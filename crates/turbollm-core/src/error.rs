use llama_cpp_2::{
    ApplyChatTemplateError, ChatTemplateError, DecodeError, LlamaContextLoadError, LlamaCppError,
    LlamaModelLoadError, NewLlamaChatMessageError,
};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("backend init failed: {0}")]
    Backend(String),
    #[error("model file not found: {0}")]
    ModelNotFound(String),
    #[error("llama.cpp: {0}")]
    Llama(#[from] LlamaCppError),
    #[error("model load: {0}")]
    ModelLoad(#[from] LlamaModelLoadError),
    #[error("context: {0}")]
    Context(#[from] LlamaContextLoadError),
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
    #[error("batch: {0}")]
    Batch(#[from] llama_cpp_2::llama_batch::BatchAddError),
    #[error("chat template: {0}")]
    ChatTemplate(#[from] ChatTemplateError),
    #[error("apply chat template: {0}")]
    ApplyChatTemplate(#[from] ApplyChatTemplateError),
    #[error("chat message: {0}")]
    ChatMessage(#[from] NewLlamaChatMessageError),
    #[error("nul byte in string")]
    Nul(#[from] std::ffi::NulError),
    #[error("generation cancelled")]
    Cancelled,
    #[error("context window exhausted ({0} tokens)")]
    ContextFull(u32),
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl CoreError {
    /// Short machine-readable code handed to Kotlin, so the UI can map it to a
    /// localised message instead of dumping an English error chain.
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::ModelNotFound(_) => "MODEL_NOT_FOUND",
            CoreError::Cancelled => "CANCELLED",
            CoreError::ContextFull(_) => "CONTEXT_FULL",
            CoreError::Invalid(_) => "INVALID_ARGUMENT",
            CoreError::Io(_) => "IO",
            _ => "ENGINE",
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;
