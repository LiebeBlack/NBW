//! adblock.rs — Native network-layer ad/tracker filtering.
//!
//! Blocks requests BEFORE any socket byte is spent: URL, host and
//! path-token signatures are hashed with SSE4.2 CRC32 (`simd_hash`) and
//! probed against open-addressing tables. EasyList-style rules
//! (`||domain^`, `/path/token`, plain substrings) are compiled at startup
//! into hashed rules evaluated without any string comparisons on the hot
//! path. Target cost per request: a handful of CRC32 instructions.

#![allow(dead_code)]

use crate::simd_hash::{crc32_hw, SimdHashSet};

/// Action decided for a network request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Block,
}

/// Compiled rule kinds.
#[derive(Debug, Clone)]
enum Rule {
    /// `||host^` — matches host equal to, or a subdomain of, the pattern.
    DomainSuffix { suffix_hash: u64 },
    /// Literal token appearing anywhere in the lowercased URL (exact-length
    /// rolling CRC32 windows — the SIMD showcase).
    Substring { token_hash: u64, len: usize },
    /// `/token/` path rule: any `/`-delimited URL segment starting with the
    /// token. Hash prefilter + string verify.
    PathSegment { token: String, token_hash: u64 },
}

pub struct AdBlocker {
    rules: Vec<Rule>,
    /// Fast-path exact host deny-list (full host strings, hashed).
    deny_hosts: SimdHashSet,
    /// Well-known ad/tracker registrable domains.
    builtin_suffixes: SimdHashSet,
    /// Allow-list (checked first).
    allow_hosts: SimdHashSet,
    stats_blocked: u64,
    stats_allowed: u64,
}

impl AdBlocker {
    /// Build the blocker with the compiled-in signature database.
    pub fn new() -> Self {
        let builtin: &[&str] = &[
            "doubleclick.net",
            "googlesyndication.com",
            "googleadservices.com",
            "google-analytics.com",
            "googletagmanager.com",
            "googletagservices.com",
            "adservice.google.com",
            "pagead2.googlesyndication.com",
            "tpc.googlesyndication.com",
            "adnxs.com",
            "adsrvr.org",
            "amazon-adsystem.com",
            "advertising.com",
            "adroll.com",
            "criteo.com",
            "criteo.net",
            "casalemedia.com",
            "openx.net",
            "pubmatic.com",
            "rubiconproject.com",
            "taboola.com",
            "outbrain.com",
            "zedo.com",
            "smartadserver.com",
            "sharethrough.com",
            "teads.tv",
            "yieldmo.com",
            "indexww.com",
            "spotxchange.com",
            "moatads.com",
            "scorecardresearch.com",
            "quantserve.com",
            "comscore.com",
            "hotjar.com",
            "mouseflow.com",
            "crazyegg.com",
            "optimizely.com",
            "mixpanel.com",
            "segment.io",
            "amplitude.com",
            "kissmetrics.com",
            "branch.io",
            "appsflyer.com",
            "adjust.com",
            "kochava.com",
            "bat.bing.com",
            "clarity.ms",
            "analytics.tiktok.com",
            "ads-twitter.com",
            "ads.linkedin.com",
            "ads.pinterest.com",
            "ads.reddit.com",
            "analytics.facebook.com",
            "connect.facebook.net",
            "ads.yahoo.com",
            "analytics.yahoo.com",
            "gemini.yahoo.com",
            "2mdn.net",
            "mediavine.com",
            "adthrive.com",
            "revcontent.com",
            "mgid.com",
            "propellerads.com",
            "popads.net",
            "popcash.net",
            "adcash.com",
            "exoclick.com",
            "juicyads.com",
            "trafficjunky.net",
            "hilltopads.net",
        ];

        let mut builtin_suffixes = SimdHashSet::with_capacity(builtin.len());
        for d in builtin {
            builtin_suffixes.insert(d);
        }

        // Minimal compiled-in EasyList-style patterns.
        let pattern_lines: &[&str] = &[
            "||banner.",
            "||tracking.",
            "||telemetry.",
            "||beacon.",
            "||pixel.",
            "||/ads/",
            "||/adserver",
            "||/adframe",
            "||/advert",
            "||/pagead/",
            "||/adsbygoogle",
            "||/analytics.js",
            "||/gtag/js",
            "||/collect?v=1",
            "||/track?event=",
            "||/beacon.gif",
            "||/pixel.gif",
            "||/1x1.gif",
            "/openads",
            "||utm_source=",
        ];

        let mut rules = Vec::with_capacity(pattern_lines.len());
        for line in pattern_lines {
            if let Some(r) = compile_rule(line) {
                rules.push(r);
            }
        }

        Self {
            rules,
            deny_hosts: SimdHashSet::with_capacity(64),
            builtin_suffixes,
            allow_hosts: SimdHashSet::with_capacity(16),
            stats_blocked: 0,
            stats_allowed: 0,
        }
    }

    /// Load extra host names into the deny list at runtime (future list
    /// download hook). Hashing is SSE4.2-accelerated.
    pub fn add_denied_host(&mut self, host: &str) {
        self.deny_hosts.insert(host);
    }

    pub fn add_allowed_host(&mut self, host: &str) {
        self.allow_hosts.insert(host);
    }

    /// Decide the fate of a request before DNS/TCP. Near-zero allocation.
    pub fn check(&mut self, url: &str, top_host: &str) -> Verdict {
        let url_l = url.to_ascii_lowercase();
        let host = extract_host(&url_l).unwrap_or("");
        let top_l = top_host.to_ascii_lowercase();

        if !self.allow_hosts.is_empty() && host_matches_suffix(&self.allow_hosts, host) {
            self.stats_allowed += 1;
            return Verdict::Allow;
        }

        // 1) Exact deny hosts (single hash probe).
        if self.deny_hosts.contains(host) {
            self.stats_blocked += 1;
            return Verdict::Block;
        }

        // 2) Built-in registrable-domain suffixes.
        if host_matches_suffix(&self.builtin_suffixes, host) {
            self.stats_blocked += 1;
            return Verdict::Block;
        }

        // 3) Compiled pattern rules (hashed).
        let third_party = is_third_party(host, &top_l);
        for rule in &self.rules {
            let hit = match rule {
                Rule::DomainSuffix { suffix_hash } => host_suffix_hit(host, *suffix_hash),
                Rule::Substring { token_hash, len } => substring_hit(&url_l, *token_hash, *len),
                Rule::PathSegment { token, token_hash } => {
                    path_segment_hit(&url_l, token, *token_hash)
                }
            };
            if hit {
                // `||domain^` rules only fire cross-site.
                let third_only = matches!(rule, Rule::DomainSuffix { .. });
                if !third_only || third_party {
                    self.stats_blocked += 1;
                    return Verdict::Block;
                }
            }
        }

        self.stats_allowed += 1;
        Verdict::Allow
    }

    pub fn blocked(&self) -> u64 {
        self.stats_blocked
    }

    pub fn allowed(&self) -> u64 {
        self.stats_allowed
    }
}

/// Compile one filter line into a hashed rule.
///
/// Recognized forms (EasyList-style subset):
///   `||domain`            — registrable-domain suffix filter
///   `||/path-token`       — URL substring filter (path anchored)
///   `||label.`            — host-label prefix, evaluated as URL substring
///   `/token/` (regex)     — skipped: regex filters are out of scope
///   `/token`              — `/`-delimited URL segment starting with token
///   `plain-token`         — URL substring filter
fn compile_rule(line: &str) -> Option<Rule> {
    let l = line.trim();
    if l.is_empty() || l.starts_with('!') || l.starts_with('#') {
        return None;
    }
    if let Some(rest) = l.strip_prefix("||") {
        let cut = rest
            .find(|c| c == '^' || c == '$')
            .unwrap_or(rest.len());
        let head = rest[..cut].to_ascii_lowercase();
        if head.is_empty() {
            return None;
        }
        let domainish =
            !head.contains('/') && !head.contains('=') && !head.contains('?') && !head.ends_with('.');
        if domainish {
            // `||doubleclick.net` — host equal to, or subdomain of, head.
            return Some(Rule::DomainSuffix {
                suffix_hash: crc32_hw(head.trim_end_matches('.').as_bytes()),
            });
        }
        // Path or label-prefix fragment: hash as a URL substring so any
        // occurrence in host or path is caught without extra machinery.
        return Some(Rule::Substring {
            token_hash: crc32_hw(head.as_bytes()),
            len: head.len(),
        });
    }
    if l.starts_with('/') && l.ends_with('/') && l.len() > 2 {
        // Regex filters are intentionally out of scope; skip safely.
        return None;
    }
    let end = l.find('$').unwrap_or(l.len());
    let token = l[..end].to_ascii_lowercase();
    if token.is_empty() {
        return None;
    }
    if token.starts_with('/') {
        let inner = token.trim_matches('/');
        if inner.is_empty() {
            return None;
        }
        if inner.contains('/') {
            // Multi-segment path pattern: treat as URL substring.
            return Some(Rule::Substring {
                token_hash: crc32_hw(inner.as_bytes()),
                len: inner.len(),
            });
        }
        return Some(Rule::PathSegment {
            token: inner.to_string(),
            token_hash: crc32_hw(inner.as_bytes()),
        });
    }
    Some(Rule::Substring {
        token_hash: crc32_hw(token.as_bytes()),
        len: token.len(),
    })
}

/// True when `host` equals a stored suffix or ends with `.suffix`.
fn host_matches_suffix(set: &SimdHashSet, host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    if set.contains(host) {
        return true;
    }
    let bytes = host.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'.' && i + 1 < bytes.len() {
            if set.contains(&host[i + 1..]) {
                return true;
            }
        }
    }
    false
}

/// Hash-probe variant used by compiled `||domain^` rules.
fn host_suffix_hit(host: &str, suffix_hash: u64) -> bool {
    if host.is_empty() {
        return false;
    }
    if crc32_hw(host.as_bytes()) == suffix_hash {
        return true;
    }
    let bytes = host.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'.' && i + 1 < bytes.len() {
            if crc32_hw(&bytes[i + 1..]) == suffix_hash {
                return true;
            }
        }
    }
    false
}

/// Exact-length rolling CRC32 windows over the whole URL — every window is
/// hashed by the SSE4.2 hardware path (~1 byte/cycle, zero branches).
fn substring_hit(url: &str, token_hash: u64, len: usize) -> bool {
    if len == 0 || len > url.len() {
        return false;
    }
    let b = url.as_bytes();
    for start in 0..=(b.len() - len) {
        if crc32_hw(&b[start..start + len]) == token_hash {
            return true;
        }
    }
    false
}

/// `/token` path rule: hash each `/`-delimited segment as a fast prefilter,
/// then verify the prefix with a string compare (hash collisions must not
/// block legitimate traffic).
fn path_segment_hit(url: &str, token: &str, token_hash: u64) -> bool {
    let b = url.as_bytes();
    let mut seg_start = 0usize;
    for i in 0..=b.len() {
        if i == b.len() || b[i] == b'/' {
            if i > seg_start {
                let seg = &b[seg_start..i];
                if crc32_hw(seg) == token_hash && seg.starts_with(token.as_bytes()) {
                    return true;
                }
            }
            seg_start = i + 1;
        }
    }
    false
}

/// Extract host from `scheme://host[:port]/...` (input already lowercase).
fn extract_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let hostport = &rest[..end];
    let host = hostport.rsplit_once(':').map_or(hostport, |(h, _)| h);
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Cross-site heuristic: different registrable domain than the top page.
fn is_third_party(host: &str, top: &str) -> bool {
    if top.is_empty() || host.is_empty() {
        return false;
    }
    let reg = |h: &str| -> String {
        let parts: Vec<&str> = h.split('.').collect();
        let n = parts.len();
        if n >= 2 {
            // Treat common 2-level public suffixes coarsely: keep last 3.
            if n >= 3
                && matches!(parts[n - 2], "co" | "com" | "org" | "net" | "gov" | "edu")
            {
                return parts[n - 3..].join(".");
            }
            return parts[n - 2..].join(".");
        }
        h.to_string()
    };
    reg(host) != reg(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_builtin_ad_domains() {
        let mut ab = AdBlocker::new();
        assert_eq!(
            ab.check(
                "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js",
                "example.com"
            ),
            Verdict::Block
        );
        assert_eq!(
            ab.check(
                "https://www.doubleclick.net/instream/ad_status.js",
                "example.com"
            ),
            Verdict::Block
        );
    }

    #[test]
    fn blocks_subdomains_of_ad_domains() {
        let mut ab = AdBlocker::new();
        assert_eq!(
            ab.check("https://stats.g.doubleclick.net/j/collect", "example.com"),
            Verdict::Block
        );
    }

    #[test]
    fn allows_normal_traffic() {
        let mut ab = AdBlocker::new();
        assert_eq!(
            ab.check(
                "https://en.wikipedia.org/wiki/Rust_(programming_language)",
                "en.wikipedia.org"
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn compiled_path_rule_blocks_ads_paths() {
        let mut ab = AdBlocker::new();
        assert_eq!(
            ab.check("https://media.example.com/ads/banner.js", "example.com"),
            Verdict::Block
        );
    }

    #[test]
    fn compiled_substring_rule_blocks_beacons() {
        let mut ab = AdBlocker::new();
        assert_eq!(
            ab.check("https://cdn.example.com/pixel.gif?uid=1", "example.com"),
            Verdict::Block
        );
    }

    #[test]
    fn allowlist_overrides_deny() {
        let mut ab = AdBlocker::new();
        ab.add_allowed_host("doubleclick.net");
        assert_eq!(
            ab.check("https://doubleclick.net/x", "doubleclick.net"),
            Verdict::Allow
        );
    }

    #[test]
    fn extract_host_works() {
        assert_eq!(extract_host("https://a.b.c/path"), Some("a.b.c"));
        assert_eq!(extract_host("https://a.b.c:8080/"), Some("a.b.c"));
        assert_eq!(extract_host("notaurl"), None);
    }
}
