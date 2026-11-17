//! The inference engine: owns the loaded GGUF model plus one llama.cpp context.
//!
//! All mutable state is behind a `Mutex`, which makes the object `Send + Sync`
//! and therefore safe to hand to Kotlin/Java.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use llama_cpp_2::context::params::{KvCacheType, LlamaContextParams};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use parking_lot::Mutex;

use crate::api::ModelInfo;
use crate::error::{CoreError, Result};
use crate::init_backend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvType {
    F16,
    Q8_0,
    Q4_0,
}

/// How to load the model. Everything here is exposed in the app settings.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Context window. 0 = the model default, capped at 8192.
    pub n_ctx: u32,
    pub n_threads: i32,
    pub n_threads_batch: i32,
    /// Layers offloaded to a GPU backend (only meaningful for an OpenCL/Vulkan
    /// build of llama.cpp; the default CPU build uses 0).
    pub n_gpu_layers: u32,
    /// Quantised KV cache saves 1.6x (q8_0) to 3x (q4_0) of cache memory.
    pub kv_type: KvType,
    /// Keep mmap on Android: the weights stay in the page cache and the kernel
    /// can evict them under memory pressure instead of OOM-killing the app.
    pub use_mmap: bool,
    pub use_mlock: bool,
    /// Flash attention: faster + less memory for long contexts.
    pub flash_attn: bool,
    /// Size of the compute batch (prompt ingestion). 256-512 is the sweet spot
    /// in terms of memory and prefill speed on phones.
    pub n_batch: u32,
}

impl Default for LoadOptions {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4);
        // Only the big cores matter for ARM inference; using little cores makes
        // things slower because every matmul waits for the slowest thread.
        let threads = (cores - 1).clamp(2, 6);
        Self {
            n_ctx: 4096,
            n_threads: threads,
            n_threads_batch: threads,
            n_gpu_layers: 0,
            kv_type: KvType::Q8_0,
            use_mmap: true,
            use_mlock: false,
            flash_attn: true,
            n_batch: 512,
        }
    }
}

pub struct Engine {
    // IMPORTANT: field order is load-bearing. Rust drops fields in declaration
    // order, and `LlamaContext` holds a raw pointer into its model: if the model
    // is freed first, dropping the context is a use-after-free and the app dies
    // with SIGSEGV the moment the user leaves the chat (or reloads a model).
    // So the context is declared *before* the model => it is destroyed first.
    ctx: Arc<Mutex<LlamaContext<'static>>>,
    model: Arc<LlamaModel>,
    info: ModelInfo,
    opts: LoadOptions,
    cancel: Arc<AtomicBool>,
    /// Guards against a second generation running on the same context.
    busy: AtomicBool,
}

// SAFETY: `LlamaModel` is immutable and thread safe; `LlamaContext` is only
// reachable through the mutex, so only one thread touches it at a time.
unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    /// Load a GGUF file. Expensive (hundreds of ms to a few s): call it from a
    /// background thread.
    pub fn load(path: impl AsRef<Path>, opts: LoadOptions) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(CoreError::ModelNotFound(path.display().to_string()));
        }
        let backend = init_backend()?;

        let model_params = LlamaModelParams::default()
            .with_n_gpu_layers(opts.n_gpu_layers)
            .with_use_mmap(opts.use_mmap)
            .with_use_mlock(opts.use_mlock);

        let model = LlamaModel::load_from_file(backend, &path, &model_params)?;

        let n_ctx_train = model.n_ctx_train();
        let n_ctx = if opts.n_ctx == 0 {
            n_ctx_train.min(8192)
        } else {
            opts.n_ctx.min(n_ctx_train.max(2048))
        }
        .max(512);

        let mut ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(n_ctx))
            .with_n_batch(opts.n_batch.max(32).min(n_ctx))
            .with_n_ubatch(opts.n_batch.max(32).min(n_ctx))
            .with_n_threads(opts.n_threads.max(1))
            .with_n_threads_batch(opts.n_threads_batch.max(1))
            .with_offload_kqv(true);

        ctx_params = match opts.kv_type {
            KvType::F16 => ctx_params,
            KvType::Q8_0 => ctx_params
                .with_type_k(KvCacheType::Q8_0)
                .with_type_v(KvCacheType::Q8_0),
            KvType::Q4_0 => ctx_params
                .with_type_k(KvCacheType::Q4_0)
                .with_type_v(KvCacheType::Q4_0),
        };
        if opts.flash_attn {
            // `AUTO` lets llama.cpp turn FA on when the backend supports it
            // (it always does for ARM NEON / dotprod builds).
            ctx_params =
                ctx_params.with_flash_attention_policy(llama_cpp_sys_2::LLAMA_FLASH_ATTN_TYPE_AUTO);
        }

        let ctx = model.new_context(backend, ctx_params)?;
        // SAFETY: `LlamaContext` carries a borrow of `model`. We move both into
        // the same struct and never outlive `model` (Rust drops fields in
        // declaration order, so `ctx` is dropped before `model`).
        let ctx: LlamaContext<'static> = unsafe { std::mem::transmute(ctx) };

        let info = ModelInfo {
            path: path.display().to_string(),
            size_bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            n_params: model.n_params(),
            n_ctx_train,
            n_layer: model.n_layer(),
            n_embd: model.n_embd(),
            has_chat_template: model.chat_template(None).is_ok(),
            description: model
                .meta_val_str("general.description")
                .or_else(|_| model.meta_val_str("general.name"))
                .unwrap_or_else(|_| String::from("GGUF model")),
        };

        Ok(Self {
            // see the comment on the struct: context must drop before the model
            ctx: Arc::new(Mutex::new(ctx)),
            model: Arc::new(model),
            info,
            opts,
            cancel: Arc::new(AtomicBool::new(false)),
            busy: AtomicBool::new(false),
        })
    }

    pub fn info(&self) -> &ModelInfo {
        &self.info
    }

    pub fn options(&self) -> &LoadOptions {
        &self.opts
    }

    pub fn model(&self) -> &LlamaModel {
        &self.model
    }

    /// The chat template baked into the GGUF (used to pick a tool-call syntax).
    pub fn chat_template(&self) -> Option<String> {
        self.model
            .chat_template(None)
            .ok()
            .and_then(|t| t.to_str().ok().map(|s| s.to_string()))
    }

    /// Shared cancel flag: set it from another thread (the "Stop" button) and the
    /// generation loop exits within one token.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub(crate) fn with_ctx<R>(&self, f: impl FnOnce(&mut LlamaContext<'static>) -> R) -> R {
        let mut guard = self.ctx.lock();
        f(&mut guard)
    }

    /// Take the generation slot; returns an error if another generation runs.
    pub(crate) fn try_begin(&self) -> Result<GenerationGuard<'_>> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(CoreError::Invalid("generation already running".into()));
        }
        Ok(GenerationGuard { engine: self })
    }
}

pub(crate) struct GenerationGuard<'a> {
    engine: &'a Engine,
}

impl Drop for GenerationGuard<'_> {
    fn drop(&mut self) {
        self.engine.busy.store(false, Ordering::SeqCst);
    }
}
