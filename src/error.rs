//! Error types shared across the crate.
//!
//! [`Error::Api`] keeps the HTTP status and AppFlowy business code as
//! structured fields so callers can branch on them (e.g. the published-views
//! 404 fallback) instead of scraping error strings.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// The AppFlowy API rejected the request: HTTP status >= 400, or an
    /// HTTP 200 envelope whose `code` is non-zero (AppFlowy reports business
    /// errors like the 1026 workspace limit that way).
    #[error("{message}")]
    Api {
        message: String,
        status: Option<u16>,
        code: Option<i64>,
    },
    /// The response was not JSON at all (proxy error pages, empty bodies).
    #[error("HTTP {status}: {body_snippet}")]
    NonJsonBody { status: u16, body_snippet: String },
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error("invalid parameters: {0}")]
    Validation(String),
    #[error("{0}")]
    NotAuthenticated(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Build an [`Error::Api`] whose message carries the Python-style suffix:
    /// `message (code 1026, HTTP 400)`.
    pub fn api(message: impl Into<String>, status: Option<u16>, code: Option<i64>) -> Self {
        let message = message.into();
        let mut parts: Vec<String> = Vec::new();
        if let Some(code) = code {
            parts.push(format!("code {code}"));
        }
        if let Some(status) = status {
            parts.push(format!("HTTP {status}"));
        }
        let message = if parts.is_empty() {
            message
        } else {
            format!("{message} ({})", parts.join(", "))
        };
        Self::Api {
            message,
            status,
            code,
        }
    }

    /// The HTTP status when this error came from a server response.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => *status,
            Self::NonJsonBody { status, .. } => Some(*status),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
