//! Input validators and sanitizers. Pure functions only: no I/O, no
//! allocation beyond what they return, every rule unit-tested.
//!
//! These guard every boundary where untrusted text enters the engine —
//! the address box, clipboard paste, `<link href>` / `<base href>`
//! attributes and search queries.

/// Truncate a string to at most `max` characters (char-boundary safe:
/// never splits a multi-byte character).
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Remove control characters (except tab) and cap the length. Applied to
/// every string before it is stored or rendered from untrusted input.
pub fn strip_control(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect();
    truncate_chars(&cleaned, max)
}

/// Sanitize a URL taken from untrusted text before it is fetched:
/// strips control characters and surrounding whitespace, enforces a
/// length bound and keeps only http/https/about schemes.
pub fn sanitize_url(raw: &str) -> Option<String> {
    let t = strip_control(raw, 2048).trim().to_string();
    if t.is_empty() {
        return None;
    }
    if t.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("about:")
    {
        Some(t)
    } else {
        None
    }
}

/// Validate a host label sequence for the address-box URL heuristic:
/// non-empty, no whitespace, no control characters.
pub fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.chars().any(|c| c.is_whitespace() || c.is_control())
}

// ---------------------------------------------------------------------------
// Address-box normalization (pure logic, shared with the UI layer)
// ---------------------------------------------------------------------------

/// Percent-encode a query for the `q=` parameter: unreserved characters pass
/// through, spaces become `+`, everything else (including each UTF-8 byte of
/// non-ASCII text) becomes `%XX`.
pub fn encode_query(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Script-free results entry point per engine index
/// (0 = Google, 1 = DuckDuckGo, 2 = Bing): this engine renders HTML/CSS
/// only and runs no JavaScript, so the JS-driven results pages would
/// arrive as empty shells.
fn engine_query_url(engine: u8, query: &str) -> String {
    let q = encode_query(query);
    match engine {
        1 => format!("https://html.duckduckgo.com/html/?q={q}"),
        2 => format!("https://www.bing.com/search?q={q}"),
        _ => format!("https://www.google.com/search?gbv=1&q={q}"),
    }
}

/// Multi-word input is always a query: the heuristic looks only at the
/// whole trimmed token, so searching "node.js tutorial" must not produce
/// the bogus URL "https://node.js tutorial".
fn looks_like_url(t: &str) -> bool {
    if t.contains("://") {
        return true;
    }
    if t.split_whitespace().count() != 1 || t.starts_with('.') {
        return false;
    }
    // The host ends at the path/query/fragment or at an explicit port, so
    // "example.com:8080" and "localhost:8080" are recognised as hosts.
    let host_end = t
        .find(|c: char| c == '/' || c == '?' || c == '#' || c == ':')
        .unwrap_or(t.len());
    let host = &t[..host_end];
    if !is_valid_host(host) {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // IPv4 literal: every dotted label is numeric.
    if host
        .split('.')
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return true;
    }
    // Otherwise require a dotted host with an alphabetic TLD of 2+ chars.
    // Bare single labels ("localhost2", "myserver") are NOT URLs: they
    // must search, not navigate to a bogus https:// host.
    match host.rsplit_once('.') {
        Some((_, tld)) => tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()),
        None => false,
    }
}

/// Turn address-box text into a URL: an explicit URL, a dotted host, or a
/// search on the engine selected by index (0 = Google, 1 = DuckDuckGo,
/// 2 = Bing). Pure and unit-tested; the UI layer maps its enum to the index.
pub fn normalize_input(raw: &str, engine: u8) -> String {
    // Bound the cleaned string first: trimming a temporary directly would
    // drop the buffer while `t` still borrows it (E0716).
    let cleaned = strip_control(raw, 2048);
    let t = cleaned.trim();
    if t.is_empty() {
        return String::new();
    }
    // about: pages are internal and must never be mistaken for a query.
    if t.to_ascii_lowercase().starts_with("about:") {
        return t.to_ascii_lowercase();
    }
    if looks_like_url(t) {
        if t.contains("://") {
            t.to_string()
        } else {
            let host_only = t.split('/').next().unwrap_or(t).split(':').next().unwrap_or(t);
            if host_only.eq_ignore_ascii_case("localhost") || host_only == "127.0.0.1" {
                format!("http://{t}")
            } else {
                format!("https://{t}")
            }
        }
    } else {
        engine_query_url(engine, t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "ññññ";
        assert_eq!(truncate_chars(s, 2).chars().count(), 2);
        // Must not panic or split a char.
        let s2 = "aé";
        assert_eq!(truncate_chars(s2, 1), "a");
    }

    #[test]
    fn control_characters_are_stripped() {
        // BEL and NUL are stripped; tab is preserved (deliberately).
        let dirty = "ab\u{0007}cd\u{0000}ef\tgh";
        assert_eq!(strip_control(dirty, 100), "abcdef\tgh");
    }

    #[test]
    fn sanitize_url_accepts_only_known_schemes() {
        assert_eq!(
            sanitize_url("  https://example.com/x "),
            Some("https://example.com/x".to_string())
        );
        assert_eq!(sanitize_url("about:home"), Some("about:home".to_string()));
        assert_eq!(sanitize_url("javascript:alert(1)"), None);
        assert_eq!(sanitize_url("data:text/html,x"), None);
        assert_eq!(sanitize_url("https://example.com/a b"), None);
        assert_eq!(sanitize_url("   "), None);
    }

    #[test]
    fn hosts_are_validated() {
        assert!(is_valid_host("example.com"));
        assert!(is_valid_host("localhost"));
        assert!(!is_valid_host(""));
        assert!(!is_valid_host("bad host"));
        assert!(!is_valid_host(&"a".repeat(300)));
    }

    #[test]
    fn address_box_normalization() {
        assert_eq!(normalize_input("localhost", 0), "http://localhost");
        assert_eq!(normalize_input("localhost:8080", 0), "http://localhost:8080");
        assert_eq!(normalize_input("example.com", 0), "https://example.com");
        assert_eq!(
            normalize_input("https://a.b/c", 0),
            "https://a.b/c"
        );
        assert_eq!(normalize_input("about:home", 0), "about:home");
        // Multi-word input is always a search, never a mangled host.
        let q = normalize_input("node.js tutorial", 0);
        assert!(q.starts_with("https://www.google.com/search?gbv=1&q="));
        assert!(q.contains("node.js+tutorial"));
        let ddg = normalize_input("rust lang", 1);
        assert!(ddg.starts_with("https://html.duckduckgo.com/html/?q="));
        // Empty input stays empty.
        assert_eq!(normalize_input("   ", 0), "");
    }

    #[test]
    fn single_labels_search_dont_navigate() {
        // A bare word without a dot or scheme is a query, not a host —
        // the previous rule sent "rust tutorial"-style single tokens to
        // https://<word>/ instead of the search engine.
        let q = normalize_input("rustlang", 0);
        assert!(q.starts_with("https://www.google.com/search?gbv=1&q="));
        assert!(normalize_input("localhost2", 0).contains("/search?"));
        // IPs and dotted hosts with alphabetic TLDs still navigate.
        assert_eq!(
            normalize_input("192.168.1.1", 0),
            "https://192.168.1.1"
        );
        assert_eq!(
            normalize_input("example.org/docs", 0),
            "https://example.org/docs"
        );
    }
}
