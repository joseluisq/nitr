//! Typed error and result types for the Nitr library.

/// Errors returned by the Nitr library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An error raised by the Lua runtime or a script.
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),

    /// An invalid or missing configuration value.
    #[error("configuration error: {0}")]
    Config(String),

    /// A script file could not be loaded or evaluated.
    #[error("script error: {0}")]
    Script(String),

    /// An I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// An HTTP protocol error.
    #[error("http error: {0}")]
    Http(#[from] hyper::http::Error),

    /// The handler exceeded its execution budget.
    #[error("handler execution timed out")]
    Timeout,
}

/// Result type alias used across the Nitr library.
pub type Result<T = (), E = Error> = std::result::Result<T, E>;
