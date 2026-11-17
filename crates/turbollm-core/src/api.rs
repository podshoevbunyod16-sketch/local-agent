//! Plain data types shared with the Kotlin layer (serialised as JSON via JNI).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Sampling / decoding parameters. Defaults are tuned for phone CPUs:
/// small `top_k`, greedy-ish temperature and a repetition penalty keep short
/// answers coherent while producing the maximum number of accepted tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenParams {
    pub max_tokens: i32,
    pub temperature: f32,
    pub top_k: i32,
    pub top_p: f32,
    pub min_p: f32,
    pub repeat_penalty: f32,
    pub seed: u32,
    /// Hard stop strings; generation ends when any of them was emitted.
    pub stop: Vec<String>,
    /// Number of threads for the "generation" phase.
    pub n_threads: i32,
    /// Number of threads for prompt ingestion (usually == n_threads, or one
    /// less to keep the UI thread responsive).
    pub n_threads_batch: i32,
    /// Use speculative decoding when a draft model is loaded.
    pub use_speculative: bool,
    /// Draft tokens proposed per step (2-8 are sensible; 4 is the sweet spot).
    pub draft_tokens: i32,
    /// GBNF grammar that constrains the output (used by tool calling to make
    /// malformed JSON impossible). Not serialised to the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grammar: Option<String>,
}

impl Default for GenParams {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4);
        // Big cores only: on big.LITTLE SoCs using *all* cores slows ARM
        // inference down, the little cores become the bottleneck.
        let threads = (cores - 1).clamp(2, 6);
        Self {
            max_tokens: 512,
            temperature: 0.7,
            top_k: 40,
            top_p: 0.95,
            min_p: 0.05,
            repeat_penalty: 1.08,
            seed: 0x5EED,
            stop: Vec::new(),
            n_threads: threads,
            n_threads_batch: threads,
            use_speculative: true,
            draft_tokens: 4,
            grammar: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Eog,
    MaxTokens,
    StopString,
    Cancelled,
    ContextFull,
}

/// Timing counters reported to the UI after every answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenStats {
    /// Tokens the model had to ingest (new suffix only – thanks to KV reuse).
    pub prompt_tokens: u64,
    /// Tokens that were *reused* from the KV cache (the saving).
    pub cached_tokens: u64,
    pub generated_tokens: u64,
    pub prompt_ms: f64,
    pub decode_ms: f64,
    /// tokens / second during generation.
    pub decode_tps: f64,
    /// prompt tokens / second (prefill).
    pub prefill_tps: f64,
    /// Time to first token, ms.
    pub ttft_ms: f64,
    pub stop_reason: Option<StopReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub path: String,
    pub size_bytes: u64,
    pub n_params: u64,
    pub n_ctx_train: u32,
    pub n_layer: u32,
    pub n_embd: i32,
    pub has_chat_template: bool,
    pub description: String,
}
