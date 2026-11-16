use crate::FormatId;

/// Errors produced anywhere in the image pipeline.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// No registered codec recognised the data.
    #[error("unsupported or unrecognised image format")]
    UnsupportedFormat,

    /// The format is known, but no registered codec can decode it (or every one failed).
    #[error("no decoder could open this {format} image: {reason}")]
    NoDecoder { format: FormatId, reason: String },

    /// A codec recognised the data but failed while decoding it.
    #[error("{codec} failed to decode: {message}")]
    Decode { codec: &'static str, message: String },

    /// The image would need more memory than the pipeline allows for one buffer.
    #[error("image is too large: {width}x{height} needs {bytes} bytes")]
    TooLarge { width: u32, height: u32, bytes: u64 },

    /// The operation was cancelled through a [`crate::CancelToken`].
    #[error("operation cancelled")]
    Cancelled,

    /// Third-party code panicked (see [`crate::panic::catch`]); the panic was contained.
    #[error("{what} crashed: {message}")]
    Panicked { what: &'static str, message: String },
}

impl Error {
    pub fn decode(codec: &'static str, message: impl std::fmt::Display) -> Self {
        Error::Decode { codec, message: message.to_string() }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
