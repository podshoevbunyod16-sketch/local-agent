//! Chat session: prompt templating, KV-cache reuse, streaming generation.
//!
//! This is where the main speed win of the app lives. A naive implementation
//! re-evaluates the whole conversation on every turn:
//!
//! ```text
//! turn 5, 3k-token history: prefill 3000 tokens @ ~60 tok/s = 50 s (!)
//! ```
//!
//! We remember the token ids that are already inside the KV cache and decode
//! only the *difference*:
//!
//! ```text
//! turn 5: prefill ~80 new tokens @ ~60 tok/s = 1.3 s
//! ```
//!
//! Because a chat template renders an append-only superset while the history
//! grows, the common prefix stays valid. If the user edits or regenerates a
//! message the prefix shrinks and we truncate the cache back to it.

use std::sync::atomic::Ordering;
use std::time::Instant;

use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use tracing::{debug, warn};

use crate::api::{ChatMessage, GenParams, GenStats, StopReason};
use crate::engine::Engine;
use crate::error::{CoreError, Result};

/// What the KV cache of the engine context currently holds.
#[derive(Debug, Default)]
pub struct ChatSession {
    params: GenParams,
    /// Rendered prompt tokens already decoded into the KV cache.
    cached: Vec<LlamaToken>,
    /// Tokens dropped from the front by context sliding. Positions stay
    /// absolute (`n_past_trimmed + index`) so RoPE remains consistent.
    n_past_trimmed: u32,
    system_prompt: String,
    /// Tool/plugin instructions appended to the system prompt (not cached in
    /// `cached`, so toggling tools simply re-prefills the tail).
    tools_prompt: String,
    /// Cumulative prompt tokens that never had to be re-evaluated.
    pub cached_hits: u64,
}

impl ChatSession {
    pub fn new(system_prompt: &str, params: GenParams) -> Self {
        Self {
            params,
            cached: Vec::new(),
            n_past_trimmed: 0,
            system_prompt: system_prompt.to_string(),
            tools_prompt: String::new(),
            cached_hits: 0,
        }
    }

    pub fn params(&self) -> &GenParams {
        &self.params
    }

    pub fn set_params(&mut self, params: GenParams) {
        self.params = params;
    }

    pub fn set_system_prompt(&mut self, system: &str) {
        self.system_prompt = system.to_string();
    }

    /// Tool definitions rendered into the system prompt (see `tools.rs`).
    pub fn set_tools_prompt(&mut self, prompt: &str) {
        self.tools_prompt = prompt.to_string();
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Forget the KV cache ("New chat").
    pub fn reset(&mut self, engine: &Engine) {
        engine.with_ctx(|ctx| ctx.clear_kv_cache());
        self.cached.clear();
        self.n_past_trimmed = 0;
    }

    /// Render the conversation with the model's own chat template.
    fn render(&self, model: &LlamaModel, messages: &[ChatMessage]) -> Result<String> {
        let mut all: Vec<ChatMessage> = Vec::with_capacity(messages.len() + 1);
        let full_system = format!("{}{}", self.system_prompt, self.tools_prompt);
        if !full_system.trim().is_empty() {
            all.push(ChatMessage::system(full_system));
        }
        all.extend(messages.iter().cloned());

        let pairs: Vec<LlamaChatMessage> = all
            .iter()
            .map(|m| LlamaChatMessage::new(m.role.as_str().to_string(), m.content.clone()))
            .collect::<std::result::Result<_, _>>()?;

        // Prefer the template baked into the GGUF; fall back to ChatML, which is
        // what most small instruct models use.
        Ok(match model.chat_template(None) {
            Ok(tmpl) => model.apply_chat_template(&tmpl, &pairs, true)?,
            Err(_) => {
                warn!("model has no chat template, falling back to ChatML");
                let mut s = String::new();
                for m in &all {
                    s.push_str("<|im_start|>");
                    s.push_str(m.role.as_str());
                    s.push('\n');
                    s.push_str(&m.content);
                    s.push_str("<|im_end|>\n");
                }
                s.push_str("<|im_start|>assistant\n");
                s
            }
        })
    }

    /// Generate an answer for `messages` (full history *without* the new answer),
    /// streaming text pieces into `on_token`.
    pub fn generate(
        &mut self,
        engine: &Engine,
        messages: &[ChatMessage],
        mut on_token: impl FnMut(&str),
    ) -> Result<(String, GenStats)> {
        let _guard = engine.try_begin()?;
        let model = engine.model();
        let cancel = engine.cancel_flag();
        cancel.store(false, Ordering::SeqCst);

        let prompt = self.render(model, messages)?;
        let mut tokens = model.vocab().tokenize(prompt.as_bytes(), true, true);
        if tokens.is_empty() {
            return Err(CoreError::Invalid("empty prompt".into()));
        }

        let mut stats = GenStats::default();
        let n_ctx = engine.with_ctx(|ctx| ctx.n_ctx());
        let max_new = self.params.max_tokens.clamp(1, 8192) as u32;
        let budget = n_ctx
            .saturating_sub(max_new.min(n_ctx / 2))
            .max(256)
            .min(n_ctx);

        // ---------- context sliding ----------
        if tokens.len() as u32 > budget {
            let mut drop_n = (tokens.len() as u32 - budget).div_ceil(64) * 64;
            drop_n = drop_n.min(tokens.len() as u32 - 1);
            warn!(drop_n, "context window full: dropping the oldest tokens");
            tokens.drain(..drop_n as usize);
            self.n_past_trimmed += drop_n;
            engine.with_ctx(|ctx| {
                if ctx.kv_cache_seq_rm(0, Some(0), Some(drop_n)).is_err() {
                    debug!("kv_cache_seq_rm failed during slide, clearing");
                    ctx.clear_kv_cache();
                }
            });
            let common = common_prefix(&tokens, &self.cached);
            self.cached.truncate(common);
        }

        // ---------- KV-cache prefix reuse ----------
        let common = common_prefix(&tokens, &self.cached);
        if common < self.cached.len() {
            engine.with_ctx(|ctx| {
                let p0 = common as u32 + self.n_past_trimmed;
                if ctx.kv_cache_seq_rm(0, Some(p0), None).is_err() {
                    debug!("partial KV removal failed, clearing the cache");
                    ctx.clear_kv_cache();
                }
            });
            self.cached.truncate(common);
        }
        let cached_hit = self.cached.len();
        stats.cached_tokens = cached_hit as u64;
        self.cached_hits += cached_hit as u64;

        // ---------- prefill of the new suffix only ----------
        let t_prefill = Instant::now();
        let allocate = engine
            .with_ctx(|ctx| ctx.n_batch() as usize)
            .clamp(32, 2048);
        let mut batch = LlamaBatch::new(allocate, 1);
        let mut pos = cached_hit as u32 + self.n_past_trimmed;
        let mut idx = cached_hit;
        while idx < tokens.len() {
            batch.clear();
            let take = (tokens.len() - idx).min(allocate);
            let is_final_chunk = idx + take == tokens.len();
            for (i, tok) in tokens[idx..idx + take].iter().enumerate() {
                let want_logits = is_final_chunk && i + 1 == take;
                batch.add(*tok, (pos + i as u32) as i32, &[0], want_logits)?;
            }
            engine.with_ctx(|ctx| ctx.decode(&mut batch))?;
            pos += take as u32;
            idx += take;
        }
        stats.prompt_tokens = (tokens.len() - cached_hit) as u64;
        stats.prompt_ms = t_prefill.elapsed().as_secs_f64() * 1000.0;
        stats.prefill_tps = if stats.prompt_ms > 0.0 {
            stats.prompt_tokens as f64 * 1000.0 / stats.prompt_ms
        } else {
            0.0
        };
        let mut cached_now = tokens;

        // ---------- streaming decode ----------
        let t_decode = Instant::now();
        let mut sampler = build_sampler(model, &self.params);
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut out = String::new();
        let mut generated = 0u32;
        let mut stop_reason = StopReason::MaxTokens;
        let mut first_token_at: Option<Instant> = None;
        let mut one = LlamaBatch::new(1, 1);

        'gen: for _ in 0..max_new {
            if cancel.load(Ordering::SeqCst) {
                stop_reason = StopReason::Cancelled;
                break;
            }
            let token = engine.with_ctx(|ctx| {
                let mut arr = ctx.token_data_array();
                arr.apply_sampler(&mut sampler);
                arr.sample_token(self.params.seed)
            });
            sampler.accept(token);

            if model.vocab().is_eog(token) {
                stop_reason = StopReason::Eog;
                break;
            }

            let piece = model.vocab().token_to_piece(token, true, None);
            let capacity = decoder.max_utf8_buffer_length(piece.len()).unwrap_or(16);
            let mut chunk = String::with_capacity(capacity.max(8));
            let (_, _, _) = decoder.decode_to_string(&piece, &mut chunk, false);
            if !chunk.is_empty() {
                if first_token_at.is_none() {
                    first_token_at = Some(Instant::now());
                }
                out.push_str(&chunk);
                on_token(&chunk);
                for stop in &self.params.stop {
                    if !stop.is_empty() && out.ends_with(stop.as_str()) {
                        stop_reason = StopReason::StopString;
                        break 'gen;
                    }
                }
            }

            // Feed the sampled token back through the model (this is the step
            // that costs one forward pass per token).
            one.clear();
            one.add(token, pos as i32, &[0], true)?;
            engine.with_ctx(|ctx| ctx.decode(&mut one))?;
            cached_now.push(token);
            pos += 1;
            generated += 1;
        }

        // The KV cache now holds prompt + generated tokens: remember that, so the
        // next turn can reuse the whole answer instead of re-reading it.
        self.cached = cached_now;

        stats.generated_tokens = u64::from(generated);
        stats.decode_ms = t_decode.elapsed().as_secs_f64() * 1000.0;
        stats.decode_tps = if stats.decode_ms > 0.0 {
            f64::from(generated) * 1000.0 / stats.decode_ms
        } else {
            0.0
        };
        stats.ttft_ms = first_token_at
            .map(|t| stats.prompt_ms + t.duration_since(t_decode).as_secs_f64() * 1000.0)
            .unwrap_or(stats.prompt_ms);
        stats.stop_reason = Some(stop_reason);
        Ok((out, stats))
    }
}

fn common_prefix(a: &[LlamaToken], b: &[LlamaToken]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn build_sampler(model: &LlamaModel, p: &GenParams) -> LlamaSampler {
    let n_vocab = model.n_vocab();
    let mut chain = Vec::with_capacity(7);
    if let Some(g) = &p.grammar {
        match LlamaSampler::grammar(model, g, "root") {
            Ok(s) => chain.push(s),
            Err(e) => warn!("grammar rejected, falling back to free sampling: {e:?}"),
        }
    }
    if p.repeat_penalty > 1.0 {
        chain.push(LlamaSampler::penalties(
            n_vocab,
            256,
            p.repeat_penalty,
            0.0,
            0.0,
        ));
    }
    if p.top_k > 0 {
        chain.push(LlamaSampler::top_k(p.top_k));
    }
    if p.top_p > 0.0 && p.top_p < 1.0 {
        chain.push(LlamaSampler::top_p(p.top_p, 1));
    }
    if p.min_p > 0.0 {
        chain.push(LlamaSampler::min_p(p.min_p, 1));
    }
    if p.temperature <= 0.0 {
        chain.push(LlamaSampler::greedy());
    } else {
        chain.push(LlamaSampler::temp(p.temperature));
        chain.push(LlamaSampler::dist(p.seed));
    }
    LlamaSampler::chain_simple(chain)
}
