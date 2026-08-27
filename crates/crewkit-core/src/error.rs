use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("command failed ({command}): {output}")]
    Cli { command: String, output: String },

    #[error("command timed out after {seconds}s: {command}")]
    CliTimeout { command: String, seconds: u64 },

    #[error("{0}")]
    Invalid(String),
}

/// Helper to attach a human-readable context to io errors.
pub fn io_ctx(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let context = context.into();
    move |source| Error::Io { context, source }
}
