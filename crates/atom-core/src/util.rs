//! Small shared helpers ported from main.go / session.go utilities.

use sha2::{Digest, Sha256};

/// firstLineTrunc returns the first line of s, truncated to max runes
/// with an ellipsis.
pub fn first_line_trunc(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() <= max {
        return first.to_string();
    }
    let mut out: String = first.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// formatTokens renders a token count compactly: 7600 -> "7.6K".
pub fn format_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format_trim(n as f64 / 1_000_000.0, "M")
    } else if n >= 1_000 {
        format_trim(n as f64 / 1_000.0, "K")
    } else {
        n.to_string()
    }
}

fn format_trim(v: f64, suffix: &str) -> String {
    let s = format!("{:.1}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{}{}", s, suffix)
}

/// sha256Hash returns the lowercase hex SHA-256 of data.
pub fn sha256_hash(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// openURL opens a URL with the platform opener (`open` on macOS,
/// `xdg-open` on Linux); used for clicked OSC 8 hyperlinks and any
/// other user-facing URL. Spawn failures are ignored: the URL is
/// already visible in the transcript.
pub fn open_url(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("xdg-open").arg(url).spawn()
    } else {
        return;
    };
    let _ = result;
}

/// addStreamUsage folds src into dst field-wise (Go session.go).
pub fn add_stream_usage(dst: &mut crate::types::StreamUsage, src: &crate::types::StreamUsage) {
    dst.prompt_tokens += src.prompt_tokens;
    dst.completion_tokens += src.completion_tokens;
    dst.total_tokens += src.total_tokens;
    dst.reasoning_tokens += src.reasoning_tokens;
    dst.cache_read_tokens += src.cache_read_tokens;
    dst.cache_write_tokens += src.cache_write_tokens;
    dst.cost += src.cost;
    dst.prompt_tokens_all += src.prompt_tokens;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_first_line() {
        assert_eq!(first_line_trunc("hello\nworld", 10), "hello");
        assert_eq!(first_line_trunc("abcdefghijk", 6), "abcde…");
    }

    #[test]
    fn formats_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1K");
        assert_eq!(format_tokens(7600), "7.6K");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn hashes() {
        assert_eq!(
            sha256_hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
