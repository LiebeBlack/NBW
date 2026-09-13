//! Persistent user preferences (search engine, zoom, history), stored as
//! a serialized JSON document in the user profile directory.
//!
//! Design rules from the storage layer:
//!   * atomic writes — write `prefs.json.tmp`, then rename over the
//!     target, so a crash mid-write leaves the previous file intact;
//!   * bounded state — history is capped (MAX_HISTORY), so neither RAM
//!     nor disk can grow without limit;
//!   * typed IO — every failure is an [`crate::utils::AppError`], never a
//!     panic; a missing or corrupt file simply yields defaults;
//!   * save-on-change — `mark_dirty` + a debounced worker flush, so a
//!     burst of interactions costs at most one disk write per second.

use crate::utils::{AppError, AppResult};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// History entries kept in RAM and on disk. Older entries are dropped.
pub const MAX_HISTORY: usize = 100;

/// The persisted document. Field order in the JSON output is fixed and
/// the parser is position-tolerant (keyed, not positional).
#[derive(Debug, Clone, PartialEq)]
pub struct Preferences {
    /// Selected search engine: 0 = Google, 1 = DuckDuckGo, 2 = Bing.
    pub engine: u8,
    /// UI zoom, 1..=4 (matches the runtime clamp).
    pub zoom: i64,
    /// Most recent URLs, newest last.
    pub history: Vec<String>,
    /// Set once the user changes any preference; false only for fresh
    /// defaults, so DPI can pick the first-run zoom without clobbering
    /// a value the user chose in a previous session.
    pub touched: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            engine: 0,
            zoom: 2,
            history: Vec::new(),
            touched: false,
        }
    }
}

impl Preferences {
    /// Clamp every field into its legal range (corrupt or hand-edited
    /// files must never produce an invalid runtime state).
    pub fn sanitized(mut self) -> Self {
        if self.engine > 2 {
            self.engine = 0;
        }
        self.zoom = self.zoom.clamp(1, 4);
        self.history.retain(|u| {
            let t = u.trim();
            (t.starts_with("http://") || t.starts_with("https://") || t.starts_with("about:"))
                && t.len() <= 2048
        });
        self.history.truncate(MAX_HISTORY);
        self
    }

    /// Push a URL at the front-trimmed tail of the bounded history.
    pub fn push_history(&mut self, url: &str) {
        self.history.retain(|h| h != url);
        self.history.push(url.to_string());
        if self.history.len() > MAX_HISTORY {
            let drop = self.history.len() - MAX_HISTORY;
            self.history.drain(..drop);
        }
        self.touched = true;
    }

    /// True while the user has never changed any preference (first run);
    /// the UI layer may pick a DPI-derived default zoom only then.
    pub fn zoom_default(&self) -> bool {
        !self.touched
    }
}

/// Serialize to compact JSON. Strings are escaped the standard way
/// (quote, backslash, control chars as \\uXXXX).
pub fn to_json(p: &Preferences) -> String {
    let esc = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    };
    let mut out = String::with_capacity(256);
    out.push('{');
    out.push_str(&format!("\"engine\":{},", p.engine));
    out.push_str(&format!("\"zoom\":{},", p.zoom));
    out.push_str(&format!("\"touched\":{},", p.touched));
    out.push_str("\"history\":[");
    for (i, u) in p.history.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&esc(u));
        out.push('"');
    }
    out.push_str("]}");
    out
}

/// Parse the JSON produced by [`to_json`]. Tolerant of whitespace and
/// key order; unknown keys are ignored. Returns defaults for a malformed
/// document (the caller can still distinguish via `Option`).
pub fn from_json(text: &str) -> Option<Preferences> {
    // Scalar keys are searched only in the head of the document, before
    // the history array: a URL string like "https://x/?engine=1" must
    // never be mistaken for the top-level "engine" key.
    let head_end = text.find("\"history\"").unwrap_or(text.len());
    let head = &text[..head_end];
    let get_num = |key: &str| -> Option<i64> {
        let idx = head.find(&format!("\"{key}\""))?;
        let after = &head[idx + key.len() + 2..];
        let start = after.find(':')? + 1;
        let rest = after[start..].trim_start();
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '-')
            .unwrap_or(rest.len());
        rest[..end].trim().parse().ok()
    };
    let engine = get_num("engine")?.clamp(0, 2) as u8;
    let zoom = get_num("zoom")?.clamp(1, 4);
    let mut history = Vec::new();
    if let Some(hidx) = text.find("\"history\"") {
        let after = &text[hidx..];
        // Scan to the matching closing bracket while skipping over the
        // array's own strings: a history URL containing ']' (IPv6 literal
        // hosts, query strings) must not truncate the list.
        if let Some(open) = after.find('[') {
            let mut close = None;
            let mut in_string = false;
            let mut escaped = false;
            for (i, c) in after[open + 1..].char_indices() {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        in_string = false;
                    }
                    continue;
                }
                match c {
                    '"' => in_string = true,
                    ']' => {
                        close = Some(open + 1 + i);
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(close) = close {
                let items = &after[open + 1..close];
                let mut rest = items;
                while let Some(q) = rest.find('"') {
                    let body = &rest[q + 1..];
                    // Escape-aware scan for the closing quote: \" must
                    // never end the item (URLs legitimately contain
                    // quotes and backslashes).
                    let mut end = None;
                    let mut esc = false;
                    for (k, c) in body.char_indices() {
                        if esc {
                            esc = false;
                        } else if c == '\\' {
                            esc = true;
                        } else if c == '"' {
                            end = Some(k);
                            break;
                        }
                    }
                    let Some(eq) = end else { break };
                    let mut s = String::new();
                    let mut chars = body[..eq].chars();
                    while let Some(c) = chars.next() {
                        if c == '\\' {
                            match chars.next() {
                                Some('n') => s.push('\n'),
                                Some('t') => s.push('\t'),
                                Some('r') => s.push('\r'),
                                Some('u') => {
                                    let hex: String = chars.by_ref().take(4).collect();
                                    if let Ok(n) = u32::from_str_radix(&hex, 16) {
                                        if let Some(ch) = char::from_u32(n) {
                                            s.push(ch);
                                        }
                                    }
                                }
                                Some(other) => s.push(other),
                                None => break,
                            }
                        } else {
                            s.push(c);
                        }
                    }
                    history.push(s);
                    rest = &body[eq + 1..];
                }
            }
        }
    }
    Some(Preferences {
        engine,
        zoom,
        history,
        // A parsed document only exists on disk after the first save,
        // which happens on a user change; treat it as touched.
        touched: true,
    })
}

/// Resolve the preferences file path: `<exe dir>\freeweb-prefs.json` if
/// the exe directory is writable, else the temp directory. Keeping it
/// next to the binary makes the browser fully portable (no registry, no
/// AppData sprawl).
pub fn prefs_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("freeweb-prefs.json");
            // Probe writability once; fall back to temp if the folder is
            // read-only (Program Files installs).
            if fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(&candidate)
                .is_ok()
            {
                return candidate;
            }
        }
    }
    std::env::temp_dir().join("freeweb-prefs.json")
}

/// Load preferences from disk. Missing or corrupt file → defaults.
pub fn load() -> Preferences {
    let path = prefs_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return Preferences::default();
    };
    from_json(&text)
        .map(Preferences::sanitized)
        .unwrap_or_default()
}

/// Atomically write preferences: temp file in the same directory, then
/// rename over the target. Any failure is a typed Storage error.
pub fn store(prefs: &Preferences) -> AppResult<()> {
    let path = prefs_path();
    let dir = path
        .parent()
        .ok_or_else(|| AppError::Storage("prefs path has no parent".into()))?;
    let tmp = dir.join("freeweb-prefs.json.tmp");
    let json = to_json(prefs);
    fs::write(&tmp, json.as_bytes())
        .map_err(|e| AppError::Storage(format!("write {}: {e}", tmp.display())))?;
    fs::rename(&tmp, &path)
        .map_err(|e| AppError::Storage(format!("rename {}: {e}", path.display())))?;
    Ok(())
}

/// Debounced saver: call `mark_dirty` (set the flag) on every change;
/// the worker flushes at most once per second while dirty, snapshotting
/// the **current** shared preferences. Repeated calls never multiply
/// workers: one thread per process, guarded by the shared flag.
pub fn spawn_saver(shared: Arc<std::sync::Mutex<Preferences>>, dirty: Arc<AtomicBool>) {
    let flush_flag = dirty;
    std::thread::Builder::new()
        .name("freeweb-prefs-saver".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            if flush_flag.swap(false, Ordering::Relaxed) {
                let snapshot = shared
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                let _ = store(&snapshot);
            }
        })
        .ok();
}

/// Persist preferences immediately (used on exit and by tests).
pub fn store_shared(shared: &std::sync::Mutex<Preferences>) -> AppResult<()> {
    let snapshot = shared.lock().unwrap_or_else(|p| p.into_inner()).clone();
    store(&snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip() {
        let mut p = Preferences::default();
        p.engine = 1;
        p.zoom = 3;
        p.push_history("https://a.example/");
        p.push_history("https://b.example/");
        p.push_history("https://a.example/"); // dedup, moves to end
        let json = to_json(&p);
        let back = from_json(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn json_escapes_specials() {
        let mut p = Preferences::default();
        p.push_history("https://x.example/?a=\"1\"&b=\\c");
        let json = to_json(&p);
        assert!(json.contains("\\\"1\\\""));
        let back = from_json(&json).unwrap();
        assert_eq!(back.history[0], "https://x.example/?a=\"1\"&b=\\c");
    }

    #[test]
    fn malformed_json_yields_none_and_defaults_sanitize() {
        assert!(from_json("{not json").is_none());
        let p = Preferences {
            engine: 9,
            zoom: 99,
            history: vec!["javascript:alert(1)".into(), "https://ok.example/".into()],
            touched: false,
        }
        .sanitized();
        assert_eq!(p.engine, 0);
        assert_eq!(p.zoom, 4);
        assert_eq!(p.history, vec!["https://ok.example/".to_string()]);
    }

    #[test]
    fn urls_with_brackets_survive_roundtrip() {
        let mut p = Preferences::default();
        p.push_history("http://[::1]:8080/index?a=1&b=]");
        p.push_history("https://ok.example/");
        let back = from_json(&to_json(&p)).unwrap();
        assert_eq!(back.history.len(), 2);
        assert_eq!(back.history[0], "http://[::1]:8080/index?a=1&b=]");
        assert_eq!(back.history[1], "https://ok.example/");
    }

    #[test]
    fn history_is_bounded_and_deduped() {
        let mut p = Preferences::default();
        for i in 0..(MAX_HISTORY + 20) {
            p.push_history(&format!("https://h{i}.example/"));
        }
        assert_eq!(p.history.len(), MAX_HISTORY);
        assert_eq!(p.history[0], "https://h20.example/");
        p.push_history("https://h20.example/");
        assert_eq!(p.history.last().unwrap(), "https://h20.example/");
        assert_eq!(p.history.len(), MAX_HISTORY);
    }
}
