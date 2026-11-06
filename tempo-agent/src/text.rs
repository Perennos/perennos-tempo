//! Text helpers ported from `tempo_agent.py`: secret redaction, result capping, keyword
//! sets and timestamps. Hand-written matchers stand in for the Python regexes so the
//! crate needs no regex dependency.
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// The first `n` characters of `s` (not bytes).
pub fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Redact, then cut to `n` characters with a note saying how much was dropped.
pub fn cap(s: &str, n: usize) -> String {
    let s = redact(s);
    let total = s.chars().count();
    if total <= n {
        return s;
    }
    format!("{}\n...[truncated {} chars]", truncate_chars(&s, n), total - n)
}

const KEY_WORDS: [&str; 8] = ["api_key", "api-key", "apikey", "token", "secret", "password", "passwd", "authorization"];

/// Known token shapes: (prefix, minimum body length, allowed body bytes).
const TOKEN_SHAPES: [(&str, usize, fn(u8) -> bool); 9] = [
    ("sk-", 20, |b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
    ("ghp_", 20, |b| b.is_ascii_alphanumeric()),
    ("gho_", 20, |b| b.is_ascii_alphanumeric()),
    ("ghu_", 20, |b| b.is_ascii_alphanumeric()),
    ("ghs_", 20, |b| b.is_ascii_alphanumeric()),
    ("ghr_", 20, |b| b.is_ascii_alphanumeric()),
    ("github_pat_", 20, |b| b.is_ascii_alphanumeric() || b == b'_'),
    ("hf_", 20, |b| b.is_ascii_alphanumeric()),
    ("AIza", 30, |b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
];

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn value_end(bytes: &[u8], start: usize) -> usize {
    let stop = |b: u8| b.is_ascii_whitespace() || matches!(b, b'\'' | b'"' | b',' | b';');
    (start..bytes.len()).find(|&j| stop(bytes[j])).unwrap_or(bytes.len())
}

/// Hide secrets in tool output before it reaches the model, the log or the screen:
/// `key: value` pairs whose key names a secret, `Bearer <token>`, and known token shapes.
pub fn redact(s: &str) -> String {
    let bytes = s.as_bytes();
    let lower = s.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let mut cuts: Vec<(usize, usize)> = Vec::new(); // byte ranges to replace, all on char boundaries

    for kw in KEY_WORDS {
        let mut from = 0;
        while let Some(off) = lower[from..].find(kw) {
            let mut j = from + off + kw.len();
            from = j;
            if j < lb.len() && (lb[j] == b'\'' || lb[j] == b'"') {
                j += 1;
            }
            while j < lb.len() && lb[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= lb.len() || !(lb[j] == b':' || lb[j] == b'=') {
                continue;
            }
            j += 1;
            while j < lb.len() && lb[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < lb.len() && (lb[j] == b'\'' || lb[j] == b'"') {
                j += 1;
            }
            let mut end = value_end(bytes, j);
            // `Authorization: Bearer <token>`: the secret is the word after the scheme.
            let word = &lower[j..end];
            if matches!(word, "bearer" | "basic" | "token") {
                let mut k = end;
                while k < lb.len() && lb[k] == b' ' {
                    k += 1;
                }
                if value_end(bytes, k) > k {
                    j = k;
                    end = value_end(bytes, k);
                }
            }
            if end - j >= 6 {
                cuts.push((j, end));
            }
        }
    }
    for (prefix, min, ok) in TOKEN_SHAPES {
        let mut from = 0;
        while let Some(off) = s[from..].find(prefix) {
            let start = from + off;
            from = start + prefix.len();
            if start > 0 && is_word_byte(bytes[start - 1]) {
                continue;
            }
            let body_end = (from..bytes.len()).find(|&j| !ok(bytes[j])).unwrap_or(bytes.len());
            if body_end - from >= min {
                cuts.push((start, body_end));
            }
        }
    }
    if cuts.is_empty() {
        return s.to_string();
    }
    cuts.sort();
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for (a, b) in cuts {
        if b <= at {
            continue;
        }
        let a = a.max(at);
        out.push_str(&s[at..a]);
        out.push_str("[REDACTED]");
        at = b;
    }
    out.push_str(&s[at..]);
    out
}

/// Lower-case ASCII alphanumeric runs of 3+ characters (the Python `tok()`).
pub fn words(s: &str) -> BTreeSet<String> {
    s.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(str::to_string)
        .collect()
}

fn is_word_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// `\bneedle\b` on already lower-cased text. `needle` may contain spaces ("task list").
pub fn has_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(off) = hay[from..].find(needle) {
        let start = from + off;
        let end = start + needle.len();
        if !is_word_char(hay[..start].chars().next_back()) && !is_word_char(hay[end..].chars().next()) {
            return true;
        }
        from = start + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Paths the file tools refuse to read or list.
pub fn is_secret_path(path: &str) -> bool {
    let p = path.to_lowercase();
    [".claude.json", ".env", "secret", "credential", "executor-token", "password", "id_rsa"].iter().any(|k| p.contains(k))
        || [".token", ".key", ".pem"].iter().any(|k| p.ends_with(k))
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// `YYYY-MM-DD HH:MM:SSZ` (UTC; std has no local time zone).
pub fn timestamp(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z", t / 3600, t / 60 % 60, t % 60)
}

pub fn now() -> String {
    timestamp(unix_now())
}

/// `06 Nov 2026`.
pub fn date_label(secs: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{d:02} {} {y}", MONTHS[(m - 1) as usize])
}

pub fn today() -> String {
    date_label(unix_now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(cap("abcdef", 4), "abcd\n...[truncated 2 chars]");
        assert_eq!(cap("abc", 4), "abc");
    }

    #[test]
    fn redacts_key_value_pairs() {
        assert_eq!(redact("api_key=abcdef123 rest"), "api_key=[REDACTED] rest");
        assert_eq!(redact(r#"{"password": "hunter22!"}"#), r#"{"password": "[REDACTED]"}"#);
        assert_eq!(redact("TOKEN : 'longvalue'"), "TOKEN : '[REDACTED]'");
        assert_eq!(redact("Authorization: Bearer abcdefghijk"), "Authorization: Bearer [REDACTED]");
        // Too short to be a secret, or not a key/value pair at all: unchanged.
        assert_eq!(redact("token=abc"), "token=abc");
        assert_eq!(redact("tokens: many words here"), "tokens: many words here");
        assert_eq!(redact("the secret is out"), "the secret is out");
    }

    #[test]
    fn redacts_token_shapes() {
        // Built at run time so the repository's leak scan never sees a token-shaped literal.
        let sk = format!("sk-{}", "A1b2".repeat(6));
        let gh = format!("ghp_{}", "x9".repeat(12));
        let text = format!("keys {sk} and ({gh}) end, é {sk}");
        assert_eq!(redact(&text), "keys [REDACTED] and ([REDACTED]) end, é [REDACTED]");
        // Inside a longer word, or too short: left alone.
        let inner = format!("mask-{}", "a".repeat(30));
        assert_eq!(redact(&inner), inner);
        assert_eq!(redact("sk-short"), "sk-short");
    }

    #[test]
    fn words_and_boundaries() {
        let w = words("List_dir: read the README.md, ok?");
        assert!(w.contains("list") && w.contains("dir") && w.contains("readme") && !w.contains("ok"));
        assert!(has_word("please list it", "list"));
        assert!(!has_word("a listing", "list"));
        assert!(has_word("update the task list.", "task list"));
        assert!(!has_word("subtask listing", "task list"));
        assert!(has_word("list", "list"));
        assert!(has_word("é list é", "list"));
    }

    #[test]
    fn secret_paths() {
        for p in ["C:/Users/x/.claude.json", "/srv/app/.env", "id_rsa", "deploy.PEM", "my-secrets/x", "api.token"] {
            assert!(is_secret_path(p), "{p}");
        }
        for p in ["notes.md", "src/tokenizer.rs", "keyboard.txt"] {
            assert!(!is_secret_path(p), "{p}");
        }
    }

    #[test]
    fn formats_utc_dates() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29 00:00:00Z");
        assert_eq!(timestamp(1_793_966_401), "2026-11-06 12:00:01Z");
        assert_eq!(date_label(1_793_966_401), "06 Nov 2026");
        assert_eq!(timestamp(-1), "1969-12-31 23:59:59Z");
    }
}
