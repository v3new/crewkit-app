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

    /// The kit is published behind a login and this request carried no
    /// usable token. `challenge` is the server's `WWW-Authenticate`, which
    /// tells the login flow where to authorize. Internal plumbing between
    /// the fetcher and the login step — callers see `AuthRequired`.
    #[error("{url} requires authorization")]
    Unauthorized {
        url: String,
        challenge: Option<String>,
    },

    /// A browser login is needed and the caller asked for a silent fetch
    /// (a background update check). The UI turns this into a sign-in
    /// prompt instead of an error.
    #[error("{0} — sign in to continue")]
    AuthRequired(String),

    #[error("authorization failed: {0}")]
    Auth(String),

    #[error("{0}")]
    Invalid(String),
}

/// Helper to attach a human-readable context to io errors.
pub fn io_ctx(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let context = context.into();
    move |source| Error::Io { context, source }
}
