//! TurboLLM core: a thin, thread-safe inference engine around llama.cpp/GGML.
//!
//! What actually makes token generation fast on a phone (all of it is wired up
//! in this crate):
//!
//! * **KV-cache prefix reuse** – the chat prompt is *not* re-evaluated on every
//!   turn. [`session::ChatSession`] remembers which token prefix is already in
//!   the KV cache and decodes only the new suffix. On a 3-4k token history this
//!   saves seconds per turn (10-50x faster "time to first token").
//! * **Flash attention + quantised KV cache** (`q8_0`/`q4_0`) – less memory
//!   bandwidth per token and 2-4x more context inside the same RAM budget.
//! * **Streaming** – every token is handed to the caller through a callback, so
//!   the UI renders at generation speed instead of at the end of the answer.
//! * **MTP / self-speculative decoding** – see `docs/SPEED.md`; this is the only
//!   technique that gives a genuine multiple-fold tokens/s win on phone-class
//!   hardware (1.8-3x typical), plus a stateful draft-accept loop that is kept
//!   in this crate's roadmap because llama.cpp's MTP path is model dependent.
//! * **One worker thread** – a llama.cpp context is `Send` but not `Sync`, all
//!   calls are serialised behind the engine mutex.

pub mod agent;
pub mod api;
pub mod builtin_tools;
pub mod engine;
pub mod error;
pub mod mcp;
pub mod plugins;
pub mod session;
pub mod tools;

#[cfg(feature = "jni")]
pub mod jni_bridge;

pub use api::{ChatMessage, GenParams, GenStats, ModelInfo, StopReason};
pub use engine::Engine;
pub use error::{CoreError, Result};

/// llama.cpp wants its backend initialised exactly once per process.
/// (JNI may be called from several threads, so this is a `OnceLock`.)
pub fn init_backend() -> Result<&'static llama_cpp_2::llama_backend::LlamaBackend> {
    use std::sync::OnceLock;
    static BACKEND: OnceLock<
        std::result::Result<llama_cpp_2::llama_backend::LlamaBackend, String>,
    > = OnceLock::new();

    let cell = BACKEND.get_or_init(|| {
        llama_cpp_2::llama_backend::LlamaBackend::init().map_err(|e| e.to_string())
    });
    cell.as_ref().map_err(|e| CoreError::Backend(e.clone()))
}
