//! utils — Utilities and security layer: typed error model, input
//! validators and URI/string sanitizers. Everything here is pure, cheap,
//! unit-testable and free of I/O.

pub mod errors;
pub mod sanitize;

pub use errors::{AppError, AppResult};
