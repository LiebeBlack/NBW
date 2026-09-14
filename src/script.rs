//! Sandboxed ECMAScript execution for documents.
//!
//! Scripts run on the fetch worker, never on the UI thread. The runtime is
//! intentionally isolated from the filesystem, process, sockets and Win32;
//! page scripts receive only standard ECMAScript built-ins until a safe DOM
//! binding is added.

use boa_engine::{Context, Source};

const MAX_SCRIPT_BYTES: usize = 512 * 1024;
const MAX_SCRIPTS: usize = 32;

#[derive(Debug, Default, Clone)]
pub struct ScriptReport {
    pub executed: usize,
    pub failed: usize,
    pub bytes: usize,
}

/// Execute inline page scripts with a fresh context for each document.
/// Errors are reported to the caller and never abort document loading.
pub fn execute_inline(scripts: &[String]) -> ScriptReport {
    let mut report = ScriptReport::default();
    let mut context = Context::default();
    for source in scripts.iter().take(MAX_SCRIPTS) {
        if report.bytes >= MAX_SCRIPT_BYTES {
            break;
        }
        let remaining = MAX_SCRIPT_BYTES - report.bytes;
        let source = if source.len() > remaining {
            let end = source
                .char_indices()
                .take_while(|(index, _)| *index < remaining)
                .map(|(index, character)| index + character.len_utf8())
                .last()
                .unwrap_or(0)
                .min(remaining);
            &source[..end]
        } else {
            source.as_str()
        };
        report.bytes += source.len();
        if context.eval(Source::from_bytes(source)).is_ok() {
            report.executed += 1;
        } else {
            report.failed += 1;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executes_safe_ecmascript_without_host_access() {
        let report = execute_inline(&["1 + 1;".into(), "throw new Error('expected');".into()]);
        assert_eq!(report.executed, 1);
        assert_eq!(report.failed, 1);
    }
}
