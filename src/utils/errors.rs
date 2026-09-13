//! Typed error model for the whole engine (thiserror-style by hand: the
//! project's only dependency policy keeps external crates out, and a
//! hand-written Display/Error impl gives the exact same guarantees).
//!
//! Every fallible subsystem converts its failures into one of these
//! variants; no call site ever string-matches an error to branch.

use std::fmt;

/// Top-level application error. `Display` renders a user-readable line
/// suitable for the status bar; the variant is the machine-readable kind.
#[derive(Debug)]
pub enum AppError {
    /// Network layer failure (DNS, TCP, TLS, HTTP parse).
    Network(String),
    /// Local persistence failure (config read/write, serialization).
    Storage(String),
    /// Input rejected by a validator before any work was done.
    InvalidInput(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Network(m) => write!(f, "network: {m}"),
            AppError::Storage(m) => write!(f, "storage: {m}"),
            AppError::InvalidInput(m) => write!(f, "invalid input: {m}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<String> for AppError {
    fn from(m: String) -> Self {
        AppError::Network(m)
    }
}

/// Convenience alias used across the layers.
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_user_readable() {
        assert_eq!(
            AppError::Network("dns: nx".into()).to_string(),
            "network: dns: nx"
        );
        assert_eq!(
            AppError::Storage("disk full".into()).to_string(),
            "storage: disk full"
        );
        assert_eq!(
            AppError::InvalidInput("empty".into()).to_string(),
            "invalid input: empty"
        );
    }
}
