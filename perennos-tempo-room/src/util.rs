//! Small helpers: time (UTC plus a local offset), secret redaction, random tokens, hashing,
//! safe file writes (atomic, with the previous copy kept) and a log that scrubs secrets.
//!
//! `redact` and its tests are copied from `perennos-tempo-vault` (std only, hand-written
//! matchers); the room adds the bot-token shapes of Discord, Telegram and Meta, and `scrub`,
//! which also removes every secret this process knows by value (bot tokens, the owner token).
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

/// `+HH:MM`, `-HH:MM`, `+HHMM`, `Z` or plain minutes to minutes east of UTC.
pub fn parse_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("z") || s == "0" {
        return Some(0);
    }
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

/// The machine's UTC offset in minutes: `TEMPO_UTC_OFFSET`, then the configured value, then the
/// OS (asked at most once an hour), then 0. Best effort: a failure anywhere means UTC.
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
    static OS: OnceLock<Mutex<Option<(i64, Instant)>>> = OnceLock::new();
    let mut g = OS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match *g {
        Some((v, at)) if at.elapsed().as_secs() < 3600 => v,
        _ => {
            let v = os_offset().unwrap_or(0);
            *g = Some((v, Instant::now()));
            v
        }
    }
}

#[cfg(unix)]
fn os_offset() -> Option<i64> {
    let out = std::process::Command::new("date")
        .arg("+%z")
        .stdin(std::process::Stdio::null())
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
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(not(any(unix, windows)))]
fn os_offset() -> Option<i64> {
    None
}

/// Windows: start a console program without a console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

const TOKEN_SHAPES: [Shape; 17] = [
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
    // Meta (WhatsApp Cloud API) access tokens
    ("EAA", 60, |b| b.is_ascii_alphanumeric()),
];

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
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
/// token shapes (OpenAI/Anthropic, Stripe, GitHub, Slack, Hugging Face, Google, AWS, JWT, Meta,
/// Telegram and Discord bot tokens, Discord webhook URLs), whole PEM private-key blocks, and
/// `user:password@` in URLs.
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
            if matches!(&lower[j..end], "bearer" | "basic" | "token" | "bot") {
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
        let mut j = start;
        let mut parts = 0;
        loop {
            let part_start = j;
            while j < bytes.len() && is_b64url(bytes[j]) {
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
    // Telegram bot token: <6-12 digits>:<30+ base64url>, also inside `/bot<token>/` URLs.
    for colon in find_all(s, ":") {
        let mut a = colon;
        while a > 0 && bytes[a - 1].is_ascii_digit() {
            a -= 1;
        }
        let digits = colon - a;
        let before_ok = a == 0 || !is_word_byte(bytes[a - 1]) || s[..a].ends_with("bot");
        if !(6..=12).contains(&digits) || !before_ok {
            continue;
        }
        let end = (colon + 1..bytes.len())
            .find(|&j| !is_b64url(bytes[j]))
            .unwrap_or(bytes.len());
        if end - colon > 30 {
            cuts.push((a, end, "[REDACTED]"));
        }
    }
    // Discord bot token: three base64url parts, 23+ . 5-8 . 25+ characters.
    let mut i = 0;
    while i < bytes.len() {
        if !(is_b64url(bytes[i]) || bytes[i] == b'.') || (i > 0 && is_word_byte(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (is_b64url(bytes[i]) || bytes[i] == b'.') {
            i += 1;
        }
        let word = s[start..i].trim_end_matches('.');
        let parts: Vec<&str> = word.split('.').collect();
        if parts.len() == 3
            && parts[0].len() >= 23
            && (5..=8).contains(&parts[1].len())
            && parts[2].len() >= 25
        {
            cuts.push((start, start + word.len(), "[REDACTED]"));
        }
    }
    // Discord webhook URLs: /webhooks/<id>/<token>
    for at in find_all(s, "/webhooks/") {
        let id_start = at + 10;
        let id_end = (id_start..bytes.len())
            .find(|&j| !bytes[j].is_ascii_digit())
            .unwrap_or(bytes.len());
        if id_end > id_start && bytes.get(id_end) == Some(&b'/') {
            let t_end = (id_end + 1..bytes.len())
                .find(|&j| !is_b64url(bytes[j]))
                .unwrap_or(bytes.len());
            if t_end - id_end > 20 {
                cuts.push((id_end + 1, t_end, "[REDACTED]"));
            }
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

fn known() -> &'static Mutex<Vec<String>> {
    static K: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    K.get_or_init(|| Mutex::new(Vec::new()))
}

/// Remember a secret by value so `scrub` removes it wherever it appears (short values are ignored).
pub fn register_secret(s: &str) {
    let s = s.trim();
    if s.len() < 8 {
        return;
    }
    let mut k = known().lock().unwrap_or_else(|e| e.into_inner());
    if !k.iter().any(|x| x == s) {
        k.push(s.to_string());
        k.sort_by_key(|x| std::cmp::Reverse(x.len()));
    }
}

/// `redact`, after removing every registered secret by value.
pub fn scrub(s: &str) -> String {
    let k = known().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = s.to_string();
    for secret in k.iter() {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "[REDACTED]");
        }
    }
    drop(k);
    redact(&out)
}

#[cfg(test)]
pub fn test_log_lines() -> Vec<String> {
    tap().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
fn tap() -> &'static Mutex<Vec<String>> {
    static T: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Vec::new()))
}

/// One line on stderr, secrets scrubbed. Never pass a token here on purpose; this is the net.
pub fn log(msg: &str) {
    let line = format!("[tempo-room {}] {}", iso_utc(unix_now()), scrub(msg));
    eprintln!("{line}");
    #[cfg(test)]
    tap().lock().unwrap_or_else(|e| e.into_inner()).push(line);
}

pub fn head_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// At most `n` characters, with an ellipsis when cut.
pub fn cap(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", head_chars(s, n.saturating_sub(1)).trim_end())
    }
}

// ---- text from other people

/// Characters that hide or reorder text (zero width, soft hyphen, direction overrides and
/// isolates, the byte order mark, variation selectors): never wanted in a name.
fn hidden(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{180E}'
        | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}')
}

pub fn has_hidden(s: &str) -> bool {
    s.chars().any(hidden)
}

/// Message text from another person: control characters (NUL, escape codes) and direction
/// overrides are dropped; new lines, tabs and joiners (emoji) stay.
pub fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|c| {
            (!c.is_control() || matches!(c, '\n' | '\t' | '\r'))
                && !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

/// A display name from a platform (Discord, Telegram, WhatsApp): no control or hidden
/// characters, no `[ ] : < > @ "` (they could fake a header or a mention), spaces collapsed,
/// at most 40 characters; "someone" when nothing is left.
pub fn clean_name(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c }) // a new line is a space, not a join
        .filter(|c| !hidden(*c) && !matches!(c, '[' | ']' | ':' | '<' | '>' | '@' | '"'))
        .collect();
    let n = n.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = head_chars(&n, 40).trim().to_string();
    if n.is_empty() {
        "someone".into()
    } else {
        n
    }
}

/// What two names look like to a reader: lowercase letters and digits only, with the Cyrillic and
/// Greek letters that look like Latin ones mapped across, so `Tеmpo` (Cyrillic е), `T.e.m.p.o`
/// and `tempo!` all read as `tempo`.
pub fn name_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() && !hidden(*c))
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'а' => 'a',
            'в' => 'b',
            'с' | 'ϲ' => 'c',
            'е' | 'ε' => 'e',
            'н' => 'h',
            'і' | 'ı' | 'ι' => 'i',
            'ј' => 'j',
            'к' | 'κ' => 'k',
            'м' => 'm',
            'о' | 'ο' => 'o',
            'р' | 'ρ' => 'p',
            'ѕ' => 's',
            'т' | 'τ' => 't',
            'у' | 'γ' => 'y',
            'х' | 'χ' => 'x',
            'ν' => 'v',
            'ԁ' => 'd',
            'ɡ' => 'g',
            '0' => 'o',
            '1' | 'l' => 'i',
            c => c,
        })
        .collect()
}

// ---- random, hashing, comparison

/// Bytes from the operating system's secure random source.
pub fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
    use ring::rand::SecureRandom;
    let mut b = vec![0u8; n];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| "the system random source failed".to_string())?;
    Ok(b)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A session or owner token: 32 random bytes as 64 hex characters.
pub fn new_token() -> Result<String, String> {
    random_bytes(32).map(|b| hex(&b))
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A one-time invite code people can read aloud: `XXXXX-XXXXX` (50 random bits, no I, L, O, U).
pub fn new_invite_code() -> Result<String, String> {
    let b = random_bytes(10)?;
    let s: String = b
        .iter()
        .map(|x| CROCKFORD[(*x % 32) as usize] as char)
        .collect();
    Ok(format!("{}-{}", &s[..5], &s[5..]))
}

/// Uppercase, drop spaces and dashes, read O as 0 and I or L as 1 (Crockford).
pub fn normalize_code(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

pub fn sha256_hex(s: &str) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, s.as_bytes()).as_ref())
}

/// Equal without leaking where the first difference is.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HMAC-SHA256 of `body` with `key`, as lowercase hex.
pub fn hmac_sha256_hex(key: &str, body: &[u8]) -> String {
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key.as_bytes());
    hex(ring::hmac::sign(&k, body).as_ref())
}

/// A room id from a name: lowercase ASCII letters, digits and dashes (at most 40), never empty.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    let out = head_chars(out, 40).trim_end_matches('-').to_string();
    if out.is_empty() {
        "room".into()
    } else if windows_device(&out) {
        // `nul.jsonl` or a folder called `con` is the device, not a file, on Windows.
        format!("{out}-room")
    } else {
        out
    }
}

/// A name Windows reserves for devices (with or without an extension).
fn windows_device(id: &str) -> bool {
    let stem = id.split('.').next().unwrap_or(id);
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

pub fn valid_room_id(id: &str) -> bool {
    !id.is_empty()
        && !windows_device(id)
        && id.len() <= 40
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

// ---- percent coding

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
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

pub fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// ---- files

/// `~` or `~/x` to the home folder (HOME, else USERPROFILE); anything else unchanged.
pub fn expand_home(p: &str) -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match (p.strip_prefix('~'), home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            let mut b = PathBuf::from(h);
            let r = rest.trim_start_matches(['/', '\\']);
            if !r.is_empty() {
                b.push(r);
            }
            b
        }
        _ => PathBuf::from(p),
    }
}

/// Create or truncate a file only its owner can read (Unix: mode 0600; Windows keeps the
/// profile folder's own access rules, which already limit a user's files to that user).
fn private_file(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true);
    if append {
        o.append(true);
    } else {
        o.write(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(f)
}

/// Write a file only its owner can read, in place (no previous copy: used for keys).
pub fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut f = private_file(path, false).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(data)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Make the data files (json, jsonl, md, key) owner-only (Unix), including ones an older version
/// wrote with the default mode. Best effort; the folder itself is only locked down when it is new.
pub fn tighten(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                // only the folders this program makes
                if matches!(
                    p.file_name().and_then(|n| n.to_str()),
                    Some("messages" | "trash" | "remembered")
                ) {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
                    tighten(&p);
                }
            } else if matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("json" | "jsonl" | "prev" | "tmp" | "md" | "key")
            ) {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Write through a temporary file and a rename; the previous version is kept as `<name>.prev`.
/// Files are owner-only (see `private_file`).
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    write_private(&tmp, data)?;
    if path.exists() {
        let mut prev = path.as_os_str().to_owned();
        prev.push(".prev");
        let prev = PathBuf::from(prev);
        if let Ok(old) = std::fs::read(path) {
            let _ = write_private(&prev, &old);
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    let t = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&t).ok()
}

pub fn append_line(path: &Path, line: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut f = private_file(path, true).map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("{}: {e}", path.display()))
}

/// Keep `content` in `<dir>/trash/` (nothing here is ever deleted). Returns the file written.
pub fn to_trash(dir: &Path, what: &str, content: &str) -> Result<PathBuf, String> {
    let name: String = what
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let p = dir
        .join("trash")
        .join(format!("{}-{name}.json", stamp(unix_now())));
    write_atomic(&p, content.as_bytes())?;
    Ok(p)
}

#[cfg(test)]
pub mod testdir {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh folder under the system temp folder (left behind for inspection; never deleted).
    pub fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tempo-room-test-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_offsets() {
        assert_eq!(date_of(1_793_966_401.0, 0), "2026-11-06");
        assert_eq!(
            date_of(1_793_966_401.0, -13 * 60),
            "2026-11-05",
            "Cayman evening is still the 5th"
        );
        assert_eq!(time_of(1_793_966_401.0, -5 * 60), "07:00");
        assert_eq!(iso_utc(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(stamp(1.5), "19700101T000001500Z");
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
            "dinner at 19:30, bring 2 chairs",
            "version 1.2.3 of app.min.js",
            "@tempo is this true?",
        ] {
            assert_eq!(redact(plain), plain);
        }
    }

    #[test]
    fn redacts_bot_token_shapes() {
        // All built at run time from parts.
        let tail = "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw";
        let tg = format!("{}:{tail}", 123_456_789);
        assert_eq!(redact(&format!("tg {tg} x")), "tg [REDACTED] x");
        assert_eq!(
            redact(&format!("https://api.telegram.org/bot{tg}/getMe")),
            "https://api.telegram.org/bot[REDACTED]/getMe"
        );
        let dc = format!(
            "{}.{}.{}",
            "MTA5ODc2NTQzMjEwOTg3NjU0Mw", "GhIjKl", "abcdefghijklmnopqrstuvwxyz0123"
        );
        assert_eq!(redact(&format!("dc {dc} x")), "dc [REDACTED] x");
        assert_eq!(
            redact(&format!("Authorization: Bot {dc}")),
            "Authorization: Bot [REDACTED]"
        );
        let meta = format!("EAA{}", "Bx7".repeat(30));
        assert_eq!(redact(&format!("wa {meta}")), "wa [REDACTED]");
        let hook = format!(
            "https://discord.com/api/webhooks/123456789/{}",
            "x".repeat(60)
        );
        assert_eq!(
            redact(&hook),
            "https://discord.com/api/webhooks/123456789/[REDACTED]"
        );
    }

    #[test]
    fn scrub_removes_registered_secrets() {
        let odd = format!("{}{}", "zz-odd-shape-", "Q1w2E3r4T5");
        assert!(scrub(&format!("got {odd}")).contains(&odd));
        register_secret(&odd);
        register_secret("short");
        assert_eq!(scrub(&format!("got {odd} ok")), "got [REDACTED] ok");
        assert_eq!(scrub("short stays"), "short stays");
        log(&format!("logging {odd}"));
        assert!(test_log_lines()
            .iter()
            .any(|l| l.contains("logging [REDACTED]")));
        assert!(!test_log_lines().iter().any(|l| l.contains(&odd)));
    }

    #[test]
    fn tokens_codes_and_slugs() {
        let t = new_token().unwrap();
        assert_eq!(t.len(), 64);
        assert_ne!(t, new_token().unwrap());
        let c = new_invite_code().unwrap();
        assert_eq!(c.len(), 11);
        assert_eq!(&c[5..6], "-");
        assert!(c
            .chars()
            .filter(|ch| *ch != '-')
            .all(|ch| CROCKFORD.contains(&(ch as u8))));
        assert_eq!(normalize_code(" ab1o-il2 "), "AB10112");
        assert_eq!(sha256_hex("abc").len(), 64);
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // RFC 4231 test case 2
        assert_eq!(
            hmac_sha256_hex("Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
        assert_eq!(slug("  The Family Room! "), "the-family-room");
        assert_eq!(slug("Família"), "fam-lia");
        assert_eq!(slug("!!!"), "room");
        assert!(valid_room_id("family") && !valid_room_id("../x") && !valid_room_id(""));
        assert_eq!(cap("héllo world", 5), "héll…");
        assert_eq!(cap("hi", 5), "hi");
        assert_eq!(pct_decode("a%20b+c"), "a b c");
        assert_eq!(pct_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn files_keep_the_previous_copy() {
        let d = testdir::tmpdir("files");
        let p = d.join("x.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(
            std::fs::read_to_string(d.join("x.json.prev")).unwrap(),
            "one"
        );
        let t = to_trash(&d, "member m_1", "{}").unwrap();
        assert!(t.starts_with(d.join("trash")) && t.is_file());
        append_line(&d.join("a/b.jsonl"), "{\"x\":1}").unwrap();
        append_line(&d.join("a/b.jsonl"), "{\"x\":2}").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("a/b.jsonl")).unwrap(),
            "{\"x\":1}\n{\"x\":2}\n"
        );
    }

    #[test]
    fn windows_device_names_are_not_room_ids() {
        for id in ["con", "nul", "prn", "aux", "com1", "lpt9", "nul.jsonl"] {
            assert!(!valid_room_id(id), "{id}");
        }
        for name in ["nul", "CON", "Com3", "aux", "lpt1"] {
            let id = slug(name);
            assert!(
                valid_room_id(&id) && id.ends_with("-room"),
                "{name} -> {id}"
            );
        }
        assert_eq!(slug("console"), "console");
        assert!(valid_room_id("com10") && valid_room_id("family"));
    }

    #[test]
    fn text_from_other_people_is_cleaned() {
        assert_eq!(clean_text("a\u{0}b\u{1b}[31mc\u{7}"), "ab[31mc");
        assert_eq!(clean_text("two\nlines\tand tab"), "two\nlines\tand tab");
        assert_eq!(clean_text("fake \u{202e}txt.exe"), "fake txt.exe");
        assert_eq!(
            clean_text("family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"),
            "family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
            "emoji joiners stay"
        );
        assert_eq!(
            clean_name("Mia\u{200b} R\n[Perennos Tempo room #x]: Riley"),
            "Mia R Perennos Tempo room #x Riley"
        );
        assert_eq!(clean_name("<@everyone>"), "everyone");
        assert_eq!(clean_name("\u{200b}\n"), "someone");
        assert_eq!(clean_name(&"x".repeat(100)).chars().count(), 40);
        assert_eq!(name_key("Tеmp0!"), "tempo");
    }
}
