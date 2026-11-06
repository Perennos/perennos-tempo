//! One error type for the crate. `kind()` gives the short label the loop puts in
//! tool results (`ERROR Io: ...`), the same shape as the Python harness.
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The engine answered with a non-200 status (model swap, load error, bad request).
    Http { status: u16, body: String },
    /// The engine streamed an `{"error": ...}` object, or every engine in a lane failed.
    Engine(String),
    /// An MCP server failed to start or answered a request with a JSON-RPC error.
    Mcp(String),
    Timeout(String),
    Config(String),
    /// A tool call had missing or malformed arguments.
    Tool(String),
}

impl Error {
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Io(_) => "Io",
            Error::Json(_) => "Json",
            Error::Http { .. } => "Http",
            Error::Engine(_) => "Engine",
            Error::Mcp(_) => "Mcp",
            Error::Timeout(_) => "Timeout",
            Error::Config(_) => "Config",
            Error::Tool(_) => "Tool",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Json(e) => write!(f, "invalid JSON: {e}"),
            Error::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            Error::Engine(s) | Error::Mcp(s) | Error::Timeout(s) | Error::Config(s) | Error::Tool(s) => {
                f.write_str(s)
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
