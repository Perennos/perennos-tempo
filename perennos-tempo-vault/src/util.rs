//! Small std-only helpers: time (UTC plus a local offset), secret redaction, vault-relative path
//! checks and percent coding.
use std::time::{SystemTime, UNIX_EPOCH};

// ---- time

pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
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

/// (year, month, day) to days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` for the given unix seconds shifted by `offset_min` minutes.
pub fn date_of(secs: f64, offset_min: i64) -> String {
    let s = secs as i64 + offset_min * 60;
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `HH:MM` local.
pub fn time_of(secs: f64, offset_min: i64) -> String {
    let t = (secs as i64 + offset_min * 60).rem_euclid(86_400);
    format!("{:02}:{:02}", t / 3600, t / 60 % 60)
}

/// `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso_utc(secs: f64) -> String {
    let s = secs as i64;
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    let t = s.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

/// A sortable, filename-safe stamp with milliseconds: `20261106T064512123Z`.
pub fn stamp(secs: f64) -> String {
    let ms = ((secs.fract() * 1000.0) as i64).clamp(0, 999);
    let s = secs as i64;
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    let t = s.rem_euclid(86_400);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}{ms:03}Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

/// Parse `YYYY-MM-DD` (strictly).
pub fn parse_date(s: &str) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    let ok = (1..=12).contains(&m) && d >= 1 && {
        let next = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let len = days_from_civil(next.0, next.1, 1) - days_from_civil(y, m, 1);
        (d as i64) <= len
    };
    ok.then_some((y, m, d))
}

/// `+HH:MM`, `-HH:MM`, `+HHMM`, `Z` or plain minutes to minutes east of UTC.
pub fn parse_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("z") || s == "0" {
        return Some(0);
    }
    // Up to three digits is minutes ("-300"); four digits or a colon is hours and minutes.
    let unsigned = s.trim_start_matches(['+', '-']);
    if unsigned.len() <= 3 {
        let min = s.parse::<i64>().ok()?;
        return (min.abs() <= 14 * 60).then_some(min);
    }
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..].parse().ok()?;
    (h <= 14 && m < 60).then_some(sign * (h * 60 + m))
}

/// The machine's current UTC offset in minutes: `TEMPO_UTC_OFFSET`, then the OS, then 0.
/// Best effort: a failure anywhere means UTC, never an error.
pub fn local_offset_minutes(configured: Option<&str>) -> i64 {
    if let Some(v) = std::env::var("TEMPO_UTC_OFFSET")
        .ok()
        .and_then(|v| parse_offset(&v))
    {
        return v;
    }
    if let Some(v) = configured.and_then(parse_offset) {
        return v;
    }
    os_offset().unwrap_or(0)
}

#[cfg(unix)]
fn os_offset() -> Option<i64> {
    let out = std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()?;
    parse_offset(String::from_utf8_lossy(&out.stdout).trim())
}

#[cfg(windows)]
fn os_offset() -> Option<i64> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[int][System.TimeZoneInfo]::Local.GetUtcOffset([datetime]::Now).TotalMinutes",
        ])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(not(any(unix, windows)))]
fn os_offset() -> Option<i64> {
    None
}

// ---- redaction (no regex crate: hand-written matchers)

const KEY_WORDS: [&str; 8] = [
    "api_key",
    "api-key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
];

type Shape = (&'static str, usize, fn(u8) -> bool);

const TOKEN_SHAPES: [Shape; 16] = [
    ("sk-", 20, |b| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
    }),
    ("sk_live_", 16, |b| b.is_ascii_alphanumeric()),
    ("sk_test_", 16, |b| b.is_ascii_alphanumeric()),
    ("rk_live_", 16, |b| b.is_ascii_alphanumeric()),
    ("rk_test_", 16, |b| b.is_ascii_alphanumeric()),
    ("pk_live_", 16, |b| b.is_ascii_alphanumeric()),
    ("ghp_", 20, |b| b.is_ascii_alphanumeric()),
    ("gho_", 20, |b| b.is_ascii_alphanumeric()),
    ("ghu_", 20, |b| b.is_ascii_alphanumeric()),
    ("ghs_", 20, |b| b.is_ascii_alphanumeric()),
    ("ghr_", 20, |b| b.is_ascii_alphanumeric()),
    ("github_pat_", 20, |b| {
        b.is_ascii_alphanumeric() || b == b'_'
    }),
    ("hf_", 20, |b| b.is_ascii_alphanumeric()),
    ("AIza", 30, |b| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
    }),
    ("AKIA", 16, |b| b.is_ascii_uppercase() || b.is_ascii_digit()),
    ("xox", 12, |b| b.is_ascii_alphanumeric() || b == b'-'),
];

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn value_end(bytes: &[u8], start: usize) -> usize {
    let stop = |b: u8| b.is_ascii_whitespace() || matches!(b, b'\'' | b'"' | b',' | b';');
    (start..bytes.len())
        .find(|&j| stop(bytes[j]))
        .unwrap_or(bytes.len())
}

fn find_all<'a>(hay: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    hay.match_indices(needle).map(|(i, _)| i)
}

/// Hide secrets: `key: value` pairs whose key names a secret (and the token after `Bearer`), known
/// token shapes (OpenAI/Anthropic, Stripe, GitHub, Slack, Hugging Face, Google, AWS, JWT), whole PEM
/// private-key blocks, and `user:password@` in URLs.
pub fn redact(s: &str) -> String {
    let bytes = s.as_bytes();
    let lower = s.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let mut cuts: Vec<(usize, usize, &'static str)> = Vec::new();

    // PEM private keys: from BEGIN to END (or to the end when a cap cut it off).
    for start in find_all(s, "-----BEGIN ") {
        let head_end = match s[start..]
            .find("-----\n")
            .or_else(|| s[start..].find("-----\r\n"))
        {
            Some(e) => start + e,
            None => continue,
        };
        if !s[start..head_end].ends_with("PRIVATE KEY") {
            continue;
        }
        let end = s[head_end..]
            .find("-----END ")
            .and_then(|e| {
                s[head_end + e..]
                    .find("PRIVATE KEY-----")
                    .map(|k| head_end + e + k + 16)
            })
            .unwrap_or(s.len());
        cuts.push((start, end, "[REDACTED PRIVATE KEY]"));
    }
    // scheme://user:password@host
    for sep in find_all(s, "://") {
        let from = sep + 3;
        let host_end = (from..bytes.len())
            .find(|&j| matches!(bytes[j], b'/' | b' ' | b'\n' | b'\t' | b'"' | b'\''))
            .unwrap_or(bytes.len());
        if let Some(at) = s[from..host_end].rfind('@') {
            if s[from..from + at].contains(':') {
                cuts.push((from, from + at, "[REDACTED]"));
            }
        }
    }
    for kw in KEY_WORDS {
        let mut from = 0;
        while let Some(off) = lower[from..].find(kw) {
            let mut j = from + off + kw.len();
            from = j;
            if j < lb.len() && (lb[j] == b'\'' || lb[j] == b'"') {
                j += 1;
            }
            while j < lb.len() && lb[j].is_ascii_whitespace() && lb[j] != b'\n' {
                j += 1;
            }
            if j >= lb.len() || !(lb[j] == b':' || lb[j] == b'=') {
                continue;
            }
            j += 1;
            while j < lb.len() && lb[j].is_ascii_whitespace() && lb[j] != b'\n' {
                j += 1;
            }
            if j < lb.len() && (lb[j] == b'\'' || lb[j] == b'"') {
                j += 1;
            }
            let mut end = value_end(bytes, j);
            if matches!(&lower[j..end], "bearer" | "basic" | "token") {
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
                cuts.push((j, end, "[REDACTED]"));
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
            if prefix == "xox"
                && !(from < bytes.len()
                    && matches!(bytes[from], b'a' | b'b' | b'p' | b'r' | b's')
                    && bytes.get(from + 1) == Some(&b'-'))
            {
                continue;
            }
            let body_end = (from..bytes.len())
                .find(|&j| !ok(bytes[j]))
                .unwrap_or(bytes.len());
            if body_end - from >= min {
                cuts.push((start, body_end, "[REDACTED]"));
            }
        }
    }
    // JWT: eyJ<b64url>.<b64url>.<b64url>
    for start in find_all(s, "eyJ") {
        if start > 0 && is_word_byte(bytes[start - 1]) {
            continue;
        }
        let b64 = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        let mut j = start;
        let mut parts = 0;
        loop {
            let part_start = j;
            while j < bytes.len() && b64(bytes[j]) {
                j += 1;
            }
            if j - part_start < 8 {
                break;
            }
            parts += 1;
            if parts == 3 || j >= bytes.len() || bytes[j] != b'.' {
                break;
            }
            j += 1;
        }
        if parts == 3 {
            cuts.push((start, j, "[REDACTED]"));
        }
    }
    if cuts.is_empty() {
        return s.to_string();
    }
    cuts.sort_by_key(|c| (c.0, std::cmp::Reverse(c.1)));
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for (a, b, rep) in cuts {
        if b <= at {
            continue;
        }
        let a = a.max(at);
        out.push_str(&s[at..a]);
        out.push_str(rep);
        at = b;
    }
    out.push_str(&s[at..]);
    out
}

pub fn head_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

// ---- paths

/// A date in a moment.js-style format, as Obsidian templates use (`{{date:YYYY-MM-DD}}`):
/// YYYY YY MMMM MMM MM M DD D dddd ddd HH H mm ss; text in [brackets] is kept as is.
pub fn moment(fmt: &str, secs: f64, offset_min: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let t = secs.floor() as i64 + offset_min * 60;
    let days = t.div_euclid(86_400);
    let sod = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let wd = (days + 4).rem_euclid(7) as usize; // 1970-01-01 was a Thursday
    let (hh, mi, ss) = (sod / 3600, (sod / 60) % 60, sod % 60);
    let mut out = String::new();
    let mut rest = fmt;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('[') {
            let e = r.find(']').unwrap_or(r.len());
            out.push_str(&r[..e]);
            rest = r.get(e + 1..).unwrap_or("");
            continue;
        }
        let toks: [(&str, String); 15] = [
            ("YYYY", format!("{y:04}")),
            ("YY", format!("{:02}", y.rem_euclid(100))),
            ("MMMM", MONTHS[(m - 1) as usize].to_string()),
            ("MMM", MONTHS[(m - 1) as usize][..3].to_string()),
            ("MM", format!("{m:02}")),
            ("M", m.to_string()),
            (
                "Do",
                format!(
                    "{d}{}",
                    match (d % 10, d % 100) {
                        (1, x) if x != 11 => "st",
                        (2, x) if x != 12 => "nd",
                        (3, x) if x != 13 => "rd",
                        _ => "th",
                    }
                ),
            ),
            ("DD", format!("{d:02}")),
            ("D", d.to_string()),
            ("dddd", DAYS[wd].to_string()),
            ("ddd", DAYS[wd][..3].to_string()),
            ("HH", format!("{hh:02}")),
            ("H", hh.to_string()),
            ("mm", format!("{mi:02}")),
            ("ss", format!("{ss:02}")),
        ];
        match toks.iter().find(|(k, _)| rest.starts_with(k)) {
            Some((k, v)) => {
                out.push_str(v);
                rest = &rest[k.len()..];
            }
            None => {
                let c = rest.chars().next().unwrap_or(' ');
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// `~` or `~/x` to the home folder (HOME, else USERPROFILE); anything else unchanged.
pub fn expand_home(p: &str) -> std::path::PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match (p.strip_prefix('~'), home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            let mut b = std::path::PathBuf::from(h);
            let r = rest.trim_start_matches(['/', '\\']);
            if !r.is_empty() {
                b.push(r);
            }
            b
        }
        _ => std::path::PathBuf::from(p),
    }
}

/// A safe vault-relative path: forward slashes, no empty, `.` or `..` segments, no hidden
/// (dot) segments, no drive letters or leading slash, no control characters. Returns it normalised.
pub fn clean_rel(p: &str) -> Option<String> {
    let p = p.trim().replace('\\', "/");
    if p.is_empty() || p.starts_with('/') || p.len() > 1024 || p.chars().any(|c| c.is_control()) {
        return None;
    }
    if p.as_bytes().get(1) == Some(&b':') {
        return None; // C:/...
    }
    let segs: Vec<&str> = p.split('/').collect();
    // Names Windows cannot store are refused everywhere (a vault may be opened on Windows).
    let ok = segs.iter().all(|s| {
        !s.is_empty()
            && !s.starts_with('.')
            && !s.ends_with('.')
            && s.trim() == *s
            && !s.contains(['<', '>', ':', '"', '|', '?', '*'])
            && !windows_reserved(s)
    });
    ok.then(|| segs.join("/"))
}

/// Windows device names (`CON`, `NUL`, `COM1`, ...), with or without an extension.
fn windows_reserved(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or("").trim_end();
    let up = stem.to_ascii_uppercase();
    if matches!(
        up.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let mut ch = up.chars();
    let (a, b, c, d, e) = (ch.next(), ch.next(), ch.next(), ch.next(), ch.next());
    let _ = (a, b, c);
    e.is_none()
        && (up.starts_with("COM") || up.starts_with("LPT"))
        && d.is_some_and(|d| matches!(d, '1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'))
}

/// Lexically resolve `rel` (which may contain `..`) against the folder `base` (vault-relative).
/// `None` if it climbs above the vault root.
pub fn join_rel(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if base.is_empty() {
        vec![]
    } else {
        base.split('/').collect()
    };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

pub fn parent_of(rel: &str) -> &str {
    rel.rfind('/').map_or("", |i| &rel[..i])
}

pub fn file_name(rel: &str) -> &str {
    rel.rfind('/').map_or(rel, |i| &rel[i + 1..])
}

/// File name without `.md` (case-insensitive extension).
pub fn stem(rel: &str) -> &str {
    let f = file_name(rel);
    strip_md(f)
}

pub fn strip_md(s: &str) -> &str {
    if is_md(s) {
        &s[..s.len() - 3]
    } else {
        s
    }
}

/// Ends in `.md`. Compares bytes: the last three bytes need not be a character boundary.
pub fn is_md(rel: &str) -> bool {
    let b = rel.as_bytes();
    b.len() > 3 && b[b.len() - 3..].eq_ignore_ascii_case(b".md")
}

/// Path of `to` relative to the folder `from_dir` (both vault-relative).
pub fn relative_path(from_dir: &str, to: &str) -> String {
    let a: Vec<&str> = if from_dir.is_empty() {
        vec![]
    } else {
        from_dir.split('/').collect()
    };
    let b: Vec<&str> = to.split('/').collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out: Vec<&str> = std::iter::repeat_n("..", a.len() - common).collect();
    out.extend(&b[common..]);
    out.join("/")
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode for a Markdown link destination (keeps `/` and unreserved characters).
pub fn pct_encode_path(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// FNV-1a 64-bit, for change detection and de-duplication (not for security).
pub fn fnv64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        for days in [-1, 0, 11_016, 20_398, 40_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(date_of(1_793_966_401.0, 0), "2026-11-06");
        assert_eq!(
            date_of(1_793_966_401.0, -13 * 60),
            "2026-11-05",
            "Cayman evening is still the 5th"
        );
        assert_eq!(time_of(1_793_966_401.0, -5 * 60), "07:00");
        assert_eq!(iso_utc(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(stamp(1.5), "19700101T000001500Z");
        assert_eq!(parse_date("2026-02-29"), None);
        assert_eq!(parse_date("2028-02-29"), Some((2028, 2, 29)));
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("26-01-01"), None);
    }

    #[test]
    fn moment_formats() {
        // 2026-11-06 12:00:01 UTC was a Friday
        assert_eq!(
            moment("YYYY-MM-DD HH:mm:ss", 1_793_966_401.0, 0),
            "2026-11-06 12:00:01"
        );
        assert_eq!(
            moment("dddd, MMMM Do [week] YY", 1_793_966_401.0, 0),
            "Friday, November 6th week 26"
        );
        assert_eq!(moment("ddd D MMM", 1_793_966_401.0, -13 * 60), "Thu 5 Nov");
    }

    #[test]
    fn offsets() {
        assert_eq!(parse_offset("-05:00"), Some(-300));
        assert_eq!(parse_offset("+0530"), Some(330));
        assert_eq!(parse_offset("Z"), Some(0));
        assert_eq!(parse_offset("-300"), Some(-300));
        assert_eq!(parse_offset("+25:00"), None);
        assert_eq!(parse_offset("five"), None);
    }

    #[test]
    fn redacts_every_shape() {
        let a = "abcdefghijKLMNOPQRSTuvwxyz0123456789"; // built at run time: the leak scan must never match
        for t in [
            format!("sk-{a}"),
            format!("sk_live_{a}"),
            format!("rk_test_{a}"),
            format!("ghp_{a}"),
            format!("gho_{a}"),
            format!("github_pat_11{a}"),
            format!("{}-1234-{a}", ["xo", "xb"].concat()),
            format!("hf_{a}"),
            format!("AIza{a}"),
            format!("AKIA{}", "ABCDEFGHIJKLMNOP"),
            format!("eyJ{a}.eyJ{a}.{a}"),
        ] {
            assert_eq!(
                redact(&format!("value {t} end")),
                "value [REDACTED] end",
                "{t}"
            );
        }
        assert_eq!(redact("api_key = abcdef123456"), "api_key = [REDACTED]");
        assert_eq!(
            redact("Authorization: Bearer abcdefghijk"),
            "Authorization: Bearer [REDACTED]"
        );
        let pem = format!(
            "x\n-----BEGIN RSA {}-----\nMIIE{a}\n-----END RSA {}-----\ny",
            "PRIVATE KEY", "PRIVATE KEY"
        );
        assert_eq!(redact(&pem), "x\n[REDACTED PRIVATE KEY]\ny");
        let cut = format!("-----BEGIN {}-----\nMIIE{a}", "PRIVATE KEY");
        assert_eq!(redact(&cut), "[REDACTED PRIVATE KEY]");
        assert_eq!(
            redact("https://riley:hunter2pass@git.example.com/r"),
            "https://[REDACTED]@git.example.com/r"
        );
        for plain in [
            "see https://example.com/a?b=1",
            "mail me at a@b.com",
            "tokens: many words",
            "the xox game",
            "eyJ alone",
        ] {
            assert_eq!(redact(plain), plain);
        }
    }

    #[test]
    fn rel_paths() {
        assert_eq!(
            clean_rel("Notes\\Today.md").as_deref(),
            Some("Notes/Today.md")
        );
        for bad in [
            "",
            "/etc/passwd",
            "../x.md",
            "a/../b.md",
            "a/./b.md",
            "C:/x.md",
            ".obsidian/app.json",
            "a//b.md",
            "a/.trash/x.md",
            "a /x.md",
            "a./x.md",
            "what?.md",
            "x|y.md",
        ] {
            assert_eq!(clean_rel(bad), None, "{bad}");
        }
        assert_eq!(
            clean_rel(" x.md ").as_deref(),
            Some("x.md"),
            "outer spaces are trimmed"
        );
        assert_eq!(join_rel("a/b", "../c.md").as_deref(), Some("a/c.md"));
        assert_eq!(join_rel("", "../c.md"), None);
        assert_eq!(relative_path("a/b", "a/c/d.md"), "../c/d.md");
        assert_eq!(relative_path("", "x/y.md"), "x/y.md");
        assert_eq!(relative_path("x", "x/y.md"), "y.md");
        assert_eq!(stem("Projects/Tempo Plan.MD"), "Tempo Plan");
        assert_eq!(pct_decode("My%20Note.md"), "My Note.md");
        assert_eq!(pct_encode_path("My Note (1).md"), "My%20Note%20%281%29.md");
    }
}
