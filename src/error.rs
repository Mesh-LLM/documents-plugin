use serde::Serialize;

/// A failure the caller sees as an HTTP status and a stable code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DocError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl DocError {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(413, "document_too_large", message)
    }

    pub fn timeout() -> Self {
        Self::new(
            504,
            "document_timeout",
            "PDF extraction did not finish before its deadline and was stopped",
        )
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(500, "document_plugin_error", message.to_string())
    }
}

impl std::fmt::Display for DocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for DocError {}
