//! net — Network and protocols layer.
//!
//! Centralizes **all** network I/O of the engine behind one facade so no
//! call site ever touches sockets, TLS or retry policy directly.
//!
//! Policy (applied uniformly to every request):
//!   * automatic retry — one re-attempt per transport attempt, two full
//!     attempts total, only for transient failures (reset connection);
//!   * timeouts — 10 s per TCP address, 15 s read, bounded response size;
//!   * fallbacks — TLS: strict validation first, a single manual-validation
//!     retry for certificate-trust verdicts (surfaced as a warning, never
//!     silent), and one fresh-socket retry when a handshake is reset.
//!
//! Requests run on dedicated worker cores, so the UI thread is never
//! blocked; this module itself contains no threading code.

pub mod http;

pub use http::HttpResponse;

use crate::utils::AppError;

/// Total attempts per transport round (`1 + RETRIES`).
const RETRIES: usize = 1;

/// Classify a transport error: only connection-level breakage is worth a
/// retry; deterministic failures (DNS, URL, TLS verdicts) fail fast.
fn is_transient(err: &str) -> bool {
    err.contains("connect ")
        || err.contains("tls read")
        || err.contains("read: ")
        || err.contains("connection closed by peer")
}

/// Fetch a document with the layer policy applied. Never panics; every
/// failure is a typed [`AppError`]. The returned [`HttpResponse`] carries
/// any non-fatal transport warning (for example an accepted untrusted
/// certificate) in its `warning` field for the UI status bar.
pub fn fetch_document(url: &str) -> Result<HttpResponse, AppError> {
    let mut last_err: Option<String> = None;
    for attempt in 0..=RETRIES {
        if attempt > 0 {
            // Small backoff between full attempts; the worker thread is
            // dedicated, so a short sleep costs nothing else.
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        match http::get(url, &[]) {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                if !is_transient(&e) {
                    return Err(AppError::Network(e));
                }
                last_err = Some(e);
            }
        }
    }
    Err(AppError::Network(last_err.unwrap_or_default()))
}

/// Fetch one linked stylesheet during document parse. Best-effort by
/// design: failures degrade to "sheet missing", never to a failed load.
pub fn fetch_stylesheet(url: &str, max_bytes: usize) -> Option<String> {
    let resp = http::get(url, &[]).ok()?;
    if resp.status != 200 || resp.body.len() > max_bytes {
        return None;
    }
    Some(resp.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classification() {
        assert!(is_transient("connect example.com: os error 10061"));
        assert!(is_transient("tls read: connection reset"));
        assert!(is_transient("read: broken pipe"));
        assert!(is_transient("TLS handshake: connection closed by peer"));
        assert!(!is_transient("dns: failed lookup"));
        assert!(!is_transient("invalid URL"));
        assert!(!is_transient("TLS handshake failed: 0x80090325"));
    }

    #[test]
    fn fetch_document_rejects_bad_url_with_typed_error() {
        // An invalid URL fails on the first attempt without retrying.
        let err = fetch_document("ht tp://bad url").unwrap_err();
        assert!(matches!(err, AppError::Network(_)));
    }
}
