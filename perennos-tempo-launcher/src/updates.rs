//! Checks for a newer stable Perennos Tempo release. Downloads are never run.

use crate::setup::json::{self, J};
use crate::setup::registry::{Fetch, Fetched};
use crate::{apply_json_atomic, unix_now, FileLock};
use std::fs;
use std::io;
use std::path::Path;

pub const API_URL: &str = "https://api.github.com/repos/Perennos/perennos-tempo/releases/latest";
pub const RELEASES_URL: &str = "https://github.com/Perennos/perennos-tempo/releases/tag/";
pub const CACHE_FILE: &str = "updates.json";
const LOCK_FILE: &str = "updates.lock";
const TTL_S: u64 = 24 * 60 * 60;
const ERR_FETCH: &str = "The release check could not reach GitHub.";
const ERR_RELEASE: &str = "GitHub did not return an official stable release.";
const ERR_OLDER: &str = "GitHub returned a release older than the one already known.";
const ERR_CACHE: &str = "The saved update status could not be read.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Status,
    Check,
    Enable,
    Disable,
    Open,
    BackgroundCheck,
}

pub fn parse_args(args: &[&str]) -> Result<Action, String> {
    match args {
        [] => Ok(Action::Status),
        ["--check"] => Ok(Action::Check),
        ["--enable"] => Ok(Action::Enable),
        ["--disable"] => Ok(Action::Disable),
        ["--open"] => Ok(Action::Open),
        ["--background-check"] => Ok(Action::BackgroundCheck),
        _ => Err("updates takes --check, --enable, --disable, or --open".into()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub [u64; 3]);

impl Version {
    pub fn parse(tag: &str) -> Option<Self> {
        let parts: Vec<_> = tag.strip_prefix('v')?.split('.').collect();
        if parts.len() != 3
            || parts.iter().any(|p| {
                p.is_empty()
                    || !p.bytes().all(|b| b.is_ascii_digit())
                    || (p.len() > 1 && p.starts_with('0'))
            })
        {
            return None;
        }
        Some(Self([
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ]))
    }

    pub fn tag(self) -> String {
        format!("v{}.{}.{}", self.0[0], self.0[1], self.0[2])
    }
}

pub fn current_version() -> Version {
    Version::parse(concat!("v", env!("CARGO_PKG_VERSION"))).expect("package version is numeric")
}

/// Only the exact official stable-release URL is allowed through `Ctx::open`.
pub fn is_official_release_url(url: &str) -> bool {
    let Some(tag) = url.strip_prefix(RELEASES_URL) else {
        return false;
    };
    Version::parse(tag).is_some_and(|version| version.tag() == tag)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cache {
    pub enabled: bool,
    pub checked_at: Option<u64>,
    pub latest: Option<Version>,
    pub error: Option<String>,
}

impl Cache {
    pub fn due(&self, now: u64) -> bool {
        self.checked_at
            .is_none_or(|at| now.saturating_sub(at) >= TTL_S)
    }

    fn json(&self) -> J {
        J::Obj(vec![
            ("enabled".into(), J::Bool(self.enabled)),
            (
                "checked_at".into(),
                self.checked_at.map_or(J::Null, |n| J::Num(n.to_string())),
            ),
            (
                "latest".into(),
                self.latest.map_or(J::Null, |v| J::s(&v.tag())),
            ),
            ("error".into(), self.error.as_deref().map_or(J::Null, J::s)),
        ])
    }

    fn from_json(j: &J) -> Self {
        let latest = j
            .str("latest")
            .and_then(Version::parse)
            .filter(|v| v.tag().as_str() == j.str("latest").unwrap_or(""));
        let checked_at = match j.get("checked_at") {
            Some(J::Num(n)) => n.parse().ok(),
            _ => None,
        };
        let error = match j.str("error") {
            Some(ERR_FETCH) => Some(ERR_FETCH.to_string()),
            Some(ERR_RELEASE) => Some(ERR_RELEASE.to_string()),
            Some(ERR_OLDER) => Some(ERR_OLDER.to_string()),
            _ if j.get("error").is_some_and(|v| !matches!(v, J::Null)) => {
                Some(ERR_CACHE.to_string())
            }
            _ => None,
        };
        Self {
            enabled: j.bool("enabled").unwrap_or(false),
            checked_at,
            latest,
            error,
        }
    }
}

pub fn read_cache(home: &Path) -> Cache {
    read_cache_checked(home)
        .map(|(cache, _)| cache)
        .unwrap_or_else(|_| Cache {
            error: Some(ERR_CACHE.into()),
            ..Cache::default()
        })
}

fn save_cache(home: &Path, cache: &Cache) -> io::Result<()> {
    let path = home.join(CACHE_FILE);
    let previous = read_cache_bytes(&path)?;
    if let Some(bytes) = previous.as_deref() {
        validate_cache_bytes(bytes)?;
    }
    save_cache_expected(&path, cache, previous.as_deref())
}

fn save_cache_expected(path: &Path, cache: &Cache, expected: Option<&[u8]>) -> io::Result<()> {
    apply_json_atomic(
        path,
        expected,
        cache.json().pretty().as_bytes(),
        None,
        validate_cache_bytes,
    )
}

fn read_cache_checked(home: &Path) -> Result<(Cache, Option<Vec<u8>>), String> {
    let path = home.join(CACHE_FILE);
    let bytes = read_cache_bytes(&path).map_err(|e| format!("{ERR_CACHE} {e}"))?;
    match bytes.as_deref() {
        Some(bytes) => {
            validate_cache_bytes(bytes).map_err(|e| format!("{ERR_CACHE} {e}"))?;
            let text = std::str::from_utf8(bytes).map_err(|e| format!("{ERR_CACHE} {e}"))?;
            let json = json::parse(text).map_err(|e| format!("{ERR_CACHE} {e}"))?;
            Ok((Cache::from_json(&json), Some(bytes.to_vec())))
        }
        None => Ok((Cache::default(), None)),
    }
}

fn read_cache_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 16 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache is not a bounded regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cache is a reparse point",
            ));
        }
    }
    let bytes = fs::read(path)?;
    if bytes.len() > 16 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache exceeded its read limit",
        ));
    }
    Ok(Some(bytes))
}

fn validate_cache_bytes(bytes: &[u8]) -> io::Result<()> {
    let text =
        std::str::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let json =
        json::parse(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let J::Obj(fields) = &json else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache must be an object",
        ));
    };
    let mut keys = std::collections::HashSet::new();
    if fields.iter().any(|(key, _)| !keys.insert(key.as_str()))
        || fields.len() != 4
        || ["enabled", "checked_at", "latest", "error"]
            .iter()
            .any(|key| json.get(key).is_none())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache fields are incomplete or duplicated",
        ));
    }
    if !matches!(json.get("enabled"), Some(J::Bool(_)))
        || !match json.get("checked_at") {
            Some(J::Null) => true,
            Some(J::Num(value)) => value.parse::<u64>().is_ok(),
            _ => false,
        }
        || !match json.str("latest") {
            Some(tag) => Version::parse(tag).is_some_and(|v| v.tag() == tag),
            None => matches!(json.get("latest"), Some(J::Null)),
        }
        || !match json.str("error") {
            Some(ERR_FETCH | ERR_RELEASE | ERR_OLDER) => true,
            None => matches!(json.get("error"), Some(J::Null)),
        }
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache fields have invalid types or values",
        ));
    }
    Ok(())
}

fn official_release(body: &str, floor: Option<Version>) -> Result<Version, &'static str> {
    let j = json::parse(body).map_err(|_| ERR_RELEASE)?;
    let tag = j.str("tag_name").ok_or(ERR_RELEASE)?;
    let version = Version::parse(tag).ok_or(ERR_RELEASE)?;
    if j.bool("draft") != Some(false)
        || j.bool("prerelease") != Some(false)
        || j.str("html_url") != Some(format!("{RELEASES_URL}{tag}").as_str())
    {
        return Err(ERR_RELEASE);
    }
    if version < current_version() || floor.is_some_and(|known| version < known) {
        return Err(ERR_OLDER);
    }
    Ok(version)
}

/// Check once under an exclusive cache lock. A failed check keeps the last known release.
pub fn check_with(
    home: &Path,
    fetch: &dyn Fetch,
    now: u64,
    automatic: bool,
) -> Result<Cache, String> {
    let _lock = FileLock::try_acquire(&home.join(LOCK_FILE))
        .map_err(|e| format!("could not lock the update cache: {e}"))?
        .ok_or_else(|| "an update check is already running".to_string())?;
    let (mut cache, previous) = read_cache_checked(home)?;
    if automatic && (!cache.enabled || !cache.due(now)) {
        return Ok(cache);
    }
    cache.checked_at = Some(now);
    match fetch.get(API_URL, Some("Accept: application/vnd.github+json"), false) {
        Fetched::Body(body) => match official_release(&body, cache.latest) {
            Ok(version) => {
                cache.latest = Some(version);
                cache.error = None;
            }
            Err(e) => cache.error = Some(e.into()),
        },
        Fetched::Failed | Fetched::NotFound => cache.error = Some(ERR_FETCH.into()),
    }
    save_cache_expected(&home.join(CACHE_FILE), &cache, previous.as_deref())
        .map_err(|e| format!("could not save update status: {e}"))?;
    Ok(cache)
}

pub fn set_enabled(home: &Path, enabled: bool) -> Result<Cache, String> {
    let _lock = FileLock::try_acquire(&home.join(LOCK_FILE))
        .map_err(|e| format!("could not lock the update cache: {e}"))?
        .ok_or_else(|| "an update check is already running".to_string())?;
    let (mut cache, previous) = read_cache_checked(home)?;
    cache.enabled = enabled;
    save_cache_expected(&home.join(CACHE_FILE), &cache, previous.as_deref())
        .map_err(|e| format!("could not save update settings: {e}"))?;
    Ok(cache)
}

struct Offline;

impl Fetch for Offline {
    fn get(&self, _url: &str, _accept: Option<&str>, _follow: bool) -> Fetched {
        Fetched::Failed
    }
}

pub(crate) fn check_local(home: &Path, automatic: bool) -> Result<Cache, String> {
    let fetcher = crate::setup::registry::real_fetcher(&crate::setup::probe::RealHost);
    match fetcher {
        Some(f) => check_with(home, f.as_ref(), unix_now(), automatic),
        None => check_with(home, &Offline, unix_now(), automatic),
    }
}

fn elapsed(at: Option<u64>, now: u64) -> String {
    match at {
        None => "never".into(),
        Some(at) if now.saturating_sub(at) < 60 => "just now".into(),
        Some(at) if now.saturating_sub(at) < 3600 => {
            format!("{} min ago", now.saturating_sub(at) / 60)
        }
        Some(at) if now.saturating_sub(at) < TTL_S => {
            format!("{} hr ago", now.saturating_sub(at) / 3600)
        }
        Some(at) => format!("{} days ago", now.saturating_sub(at) / TTL_S),
    }
}

pub fn status_text(cache: &Cache, now: u64) -> String {
    let current = current_version();
    let mut lines = vec![format!("Current version: {}", current.tag())];
    if let Some(latest) = cache.latest {
        if latest > current {
            lines.push(format!("Update available: {}", latest.tag()));
            if cache.error.is_some() {
                lines.push("The last check failed; this is the last known release.".into());
            } else if cache.due(now) {
                lines.push("This saved check is due; the status may be out of date.".into());
            }
        } else {
            let label = if cache.error.is_some() || cache.due(now) {
                "Last known stable release"
            } else {
                "Latest stable release"
            };
            lines.push(format!("{label}: {}", latest.tag()));
            if cache.error.is_some() {
                lines.push("The last check failed; this is the last known release.".into());
            } else if cache.due(now) {
                lines.push("This saved check is due; the status may be out of date.".into());
            }
        }
    } else {
        lines.push("Release status: unknown".into());
    }
    lines.push(format!(
        "Automatic checks: {}",
        if cache.enabled { "on" } else { "off" }
    ));
    lines.push(format!("Last check: {}", elapsed(cache.checked_at, now)));
    if let Some(error) = &cache.error {
        lines.push(format!("Check note: {error}"));
    }
    lines.join("\n")
}

pub fn notice_html(home: &Path) -> String {
    let cache = read_cache(home);
    let current = current_version();
    let Some(latest) = cache.latest.filter(|v| *v > current) else {
        let detail = if cache.error.is_some() {
            "The last check failed; the release status is unknown. Run tempo-launch updates --check."
        } else if cache.checked_at.is_none() {
            if cache.enabled {
                "No check has finished yet. Automatic release checks are on."
            } else {
                "No check has finished yet. Checks are off; run tempo-launch updates --check to check now."
            }
        } else if cache.due(unix_now()) {
            "The saved release status is due for a check. Run tempo-launch updates --check."
        } else if let Some(known) = cache.latest {
            if known == current {
                "The last successful check found the current stable release."
            } else {
                "The last successful check found no newer stable release."
            }
        } else {
            "No release version is cached. Run tempo-launch updates --check."
        };
        return format!(
            "<aside class=\"small update-note\" role=\"status\" aria-label=\"Tempo version status\">Tempo {}. {detail}</aside>",
            current.tag()
        );
    };
    let stale = if cache.due(unix_now()) {
        " This is the last known release; the check is due."
    } else {
        ""
    };
    let failed = if cache.error.is_some() {
        " The last check could not finish."
    } else {
        ""
    };
    let tag = latest.tag();
    format!(
        "<aside class=\"verdict update-note\" role=\"status\" aria-label=\"Tempo update available\"><strong>Update available: {tag}</strong><p>Current version: {}.{stale}{failed}</p><p><a href=\"{RELEASES_URL}{tag}\">Release notes for {tag}</a> · Run <code>tempo-launch updates --open</code> to open them.</p></aside>",
        current.tag()
    )
}

pub fn run(ctx: &crate::Ctx, action: Action) -> i32 {
    match action {
        Action::Status => {
            println!("{}", status_text(&read_cache(&ctx.home), unix_now()));
            0
        }
        Action::Enable | Action::Disable => {
            match set_enabled(&ctx.home, action == Action::Enable) {
                Ok(cache) => {
                    println!(
                        "Automatic release checks are {}.",
                        if cache.enabled { "on" } else { "off" }
                    );
                    0
                }
                Err(e) => {
                    eprintln!("tempo-launch: {e}");
                    1
                }
            }
        }
        Action::Check => match check_local(&ctx.home, false) {
            Ok(cache) => {
                println!("{}", status_text(&cache, unix_now()));
                if cache.error.is_some() {
                    1
                } else {
                    0
                }
            }
            Err(e) => {
                eprintln!("tempo-launch: {e}");
                1
            }
        },
        Action::Open => match read_cache(&ctx.home)
            .latest
            .filter(|v| *v > current_version())
        {
            Some(version) => ctx
                .open(&format!("{RELEASES_URL}{}", version.tag()))
                .map_or_else(
                    |e| {
                        eprintln!("tempo-launch: could not open the release page: {e}");
                        1
                    },
                    |_| 0,
                ),
            None => {
                eprintln!("No newer release is cached. Run tempo-launch updates --check first.");
                1
            }
        },
        Action::BackgroundCheck => {
            let _ = check_local(&ctx.home, true);
            0
        }
    }
}

/// The launcher checks in a hidden child so a network timeout cannot hold up startup.
#[cfg(not(test))]
pub fn schedule_auto_check(ctx: &crate::Ctx) {
    let cache = read_cache(&ctx.home);
    if !cache.enabled || !cache.due(unix_now()) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["updates", "--background-check"])
        .env("TEMPO_LAUNCH_HOME", &ctx.home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _ = crate::spawn_hidden(&mut cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeFetch {
        answer: Fetched,
        calls: AtomicUsize,
    }

    impl FakeFetch {
        fn new(answer: Fetched) -> Self {
            Self {
                answer,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Fetch for FakeFetch {
        fn get(&self, url: &str, accept: Option<&str>, follow: bool) -> Fetched {
            assert_eq!(url, API_URL);
            assert_eq!(accept, Some("Accept: application/vnd.github+json"));
            assert!(!follow);
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer.clone()
        }
    }

    fn release(tag: &str, url: Option<&str>, draft: bool, prerelease: bool) -> String {
        let generated;
        let url = match url {
            Some(url) => url,
            None => {
                generated = format!("{RELEASES_URL}{tag}");
                &generated
            }
        };
        J::Obj(vec![
            ("tag_name".into(), J::s(tag)),
            ("html_url".into(), J::s(url)),
            ("draft".into(), J::Bool(draft)),
            ("prerelease".into(), J::Bool(prerelease)),
        ])
        .compact()
    }

    fn home() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tempo-updates-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn numeric_versions_compare_components() {
        assert!(Version::parse("v2.10.0") > Version::parse("v2.9.99"));
        assert!(Version::parse("v3.0.0") > Version::parse("v2.99.99"));
        for bad in ["2.1.0", "v2.1", "v2.1.0-rc1", "v02.1.0", "v2.x.0"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn only_official_stable_non_downgrade_release_is_accepted() {
        let floor = Version::parse("v2.9.0");
        assert_eq!(
            official_release(&release("v2.10.0", None, false, false), floor),
            Ok(Version([2, 10, 0]))
        );
        assert_eq!(
            official_release(&release("v2.9.0", None, false, false), floor),
            Ok(Version([2, 9, 0]))
        );
        for body in [
            release("v2.10.0-rc1", None, false, false),
            release("v2.10.0", Some("https://example.com/release"), false, false),
            release("v2.10.0", None, true, false),
            release("v2.10.0", None, false, true),
            release("v2.8.9", None, false, false),
            "not json".into(),
        ] {
            assert!(official_release(&body, floor).is_err());
        }
    }

    #[test]
    fn open_action_records_only_the_cached_official_release_url() {
        let home = home();
        let cache = Cache {
            latest: Version::parse("v2.2.0"),
            ..Cache::default()
        };
        save_cache(&home, &cache).unwrap();
        let ctx = crate::Ctx {
            home: home.clone(),
            agent_data: home.join("agent"),
            vault: home.join("vault"),
            bin_dirs: vec![],
            browser: false,
            opened: std::cell::RefCell::new(Vec::new()),
        };
        assert_eq!(run(&ctx, Action::Open), 0);
        let expected = format!("{RELEASES_URL}v2.2.0");
        assert_eq!(ctx.opened.borrow().as_slice(), &[expected]);
        assert!(ctx
            .open("https://github.com/Perennos-other/perennos-tempo/releases/tag/v2.2.0")
            .is_err());
        assert!(!is_official_release_url(
            "https://github.com/Perennos/perennos-tempo/releases/tag/v2.2.0?x=1"
        ));
        assert!(!is_official_release_url(
            "https://github.com/Perennos/perennos-tempo/releases/tag/v2.2.0/extra"
        ));
        assert_eq!(ctx.opened.borrow().len(), 1);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn failed_check_keeps_release_and_caches_failure() {
        let home = home();
        let mut old = Cache {
            enabled: true,
            latest: Version::parse("v2.2.0"),
            ..Cache::default()
        };
        save_cache(&home, &old).unwrap();
        let fetch = FakeFetch::new(Fetched::Failed);
        let checked = check_with(&home, &fetch, 500, false).unwrap();
        assert_eq!(checked.latest, old.latest);
        assert_eq!(checked.checked_at, Some(500));
        assert_eq!(checked.error.as_deref(), Some(ERR_FETCH));
        assert!(
            Cache::from_json(&json::parse(&checked.json().pretty()).unwrap())
                .error
                .is_some()
        );
        let still_cached = check_with(&home, &fetch, 501, true).unwrap();
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 1);
        assert_eq!(still_cached.error.as_deref(), Some(ERR_FETCH));
        old.error = None;
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn failed_first_check_is_unknown_in_the_page_notice() {
        let home = home();
        let cache = Cache {
            checked_at: Some(500),
            error: Some(ERR_FETCH.into()),
            ..Cache::default()
        };
        save_cache(&home, &cache).unwrap();
        let notice = notice_html(&home);
        assert!(notice.contains("release status is unknown"));
        assert!(notice.contains("last check failed"));
        assert!(!notice.contains("No newer stable release is available"));
        let stale = Cache {
            latest: Some(current_version()),
            checked_at: Some(1),
            ..Cache::default()
        };
        assert!(status_text(&stale, 1 + TTL_S).contains("may be out of date"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn automatic_check_respects_opt_out_ttl_and_lock() {
        let home = home();
        let fetch = FakeFetch::new(Fetched::Body(release("v2.2.0", None, false, false)));
        assert!(check_with(&home, &fetch, 10, true)
            .unwrap()
            .checked_at
            .is_none());
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 0);
        let mut cache = Cache {
            enabled: true,
            checked_at: Some(10),
            ..Cache::default()
        };
        save_cache(&home, &cache).unwrap();
        assert!(
            check_with(&home, &fetch, 10 + TTL_S - 1, true)
                .unwrap()
                .checked_at
                == Some(10)
        );
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 0);
        let lock = crate::FileLock::try_acquire(&home.join(LOCK_FILE))
            .unwrap()
            .unwrap();
        assert!(check_with(&home, &fetch, 10 + TTL_S, true).is_err());
        drop(lock);
        cache = check_with(&home, &fetch, 10 + TTL_S, true).unwrap();
        assert_eq!(cache.latest, Version::parse("v2.2.0"));
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 1);
        assert!(home.join(LOCK_FILE).is_file());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn corrupt_update_cache_is_preserved_instead_of_replaced() {
        let home = home();
        let bad = b"{\"enabled\":true,\"enabled\":false}";
        fs::write(home.join(CACHE_FILE), bad).unwrap();
        let fetch = FakeFetch::new(Fetched::Failed);
        assert!(check_with(&home, &fetch, 99, false).is_err());
        assert_eq!(fs::read(home.join(CACHE_FILE)).unwrap(), bad);
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 0);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn orphaned_update_lock_file_does_not_block_a_new_writer() {
        let home = home();
        let lock = home.join(LOCK_FILE);
        fs::write(&lock, b"left by an exited process").unwrap();
        let fetch = FakeFetch::new(Fetched::Body(release("v2.2.0", None, false, false)));
        assert!(check_with(&home, &fetch, 99, false).is_ok());
        assert_eq!(fs::read(&lock).unwrap(), b"left by an exited process");
        fs::remove_dir_all(home).unwrap();
    }
}
