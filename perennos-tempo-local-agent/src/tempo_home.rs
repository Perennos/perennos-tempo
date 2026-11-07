//! One Perennos Tempo home per machine. Every product resolves it the same way.
//!
//! This file is copied, byte for byte (tests included), into each crate that needs it (the
//! launcher, Room, the agent crate and `perennos-tempo`), so a standalone crate never reaches into
//! a private crate path. Change one copy, change them all; `docs/TEMPO-HOME.md` has the rule.
//!
//! 1. `TEMPO_HOME`, if set and not empty; `PERENNOS_TEMPO_DATA` (older name) is checked second.
//! 2. Else a legacy folder, only if it already exists: `TEMPO_LEGACY_HOME` if set (any OS);
//!    otherwise, on Linux and macOS, `$XDG_STATE_HOME/perennos-tempo`, else
//!    `~/.local/state/perennos-tempo`. Windows has no built-in legacy folder.
//! 3. Else the default: Windows `%LOCALAPPDATA%\Perennos Tempo`; Linux and macOS `~/.tempo`.
//!
//! Nothing is created or moved here. Data is never moved automatically.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Which step of the rule chose the folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// The named environment variable.
    Env(&'static str),
    /// The legacy folder, because it already exists.
    Legacy,
    /// Nothing else applied.
    Default,
}

/// The chosen folder and the step that chose it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub dir: PathBuf,
    pub why: Why,
}

/// The legacy folder for this OS (it may not exist). `TEMPO_LEGACY_HOME` overrides the built-in one.
pub fn legacy_dir(
    windows: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
    user_home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(v) = env("TEMPO_LEGACY_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    if windows {
        return None;
    }
    let state = env("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| user_home.map(|h| h.join(".local").join("state")))?;
    Some(state.join("perennos-tempo"))
}

/// The default folder for this OS.
pub fn default_dir(
    windows: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
    user_home: Option<&Path>,
) -> PathBuf {
    if windows {
        let local = env("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| user_home.map(|h| h.join("AppData").join("Local")));
        if let Some(l) = local {
            return l.join("Perennos Tempo");
        }
    } else if let Some(h) = user_home {
        return h.join(".tempo");
    }
    std::env::temp_dir().join("perennos-tempo")
}

/// The rule. `env`, `user_home` and `is_dir` are injected so tests never touch the machine.
pub fn resolve(
    windows: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
    user_home: Option<&Path>,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Home {
    for name in ["TEMPO_HOME", "PERENNOS_TEMPO_DATA"] {
        if let Some(v) = env(name).filter(|v| !v.is_empty()) {
            return Home {
                dir: PathBuf::from(v),
                why: Why::Env(name),
            };
        }
    }
    if let Some(l) = legacy_dir(windows, env, user_home).filter(|l| is_dir(l)) {
        return Home {
            dir: l,
            why: Why::Legacy,
        };
    }
    Home {
        dir: default_dir(windows, env, user_home),
        why: Why::Default,
    }
}

/// The user's home folder (`HOME` on Linux and macOS, `USERPROFILE` on Windows).
pub fn user_home_from(windows: bool, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let order: [&str; 2] = if windows {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    order
        .iter()
        .filter_map(|k| env(k))
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The rule against the real machine.
pub fn tempo_home() -> Home {
    let env = |k: &str| std::env::var_os(k);
    let windows = cfg!(windows);
    let user = user_home_from(windows, &env);
    resolve(windows, &env, user.as_deref(), &|p| p.is_dir())
}

/// `<home>/agent`, unless `TEMPO_AGENT_DATA` is set.
pub fn agent_data(home: &Path, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    env("TEMPO_AGENT_DATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("agent"))
}

#[cfg(test)]
mod tempo_home_tests {
    use super::*;
    use std::collections::HashMap;

    fn run(windows: bool, env: &[(&str, &str)], user: Option<&str>, dirs: &[&str]) -> Home {
        let m: HashMap<String, OsString> = env
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        let user = user.map(PathBuf::from);
        let dirs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
        resolve(windows, &|k| m.get(k).cloned(), user.as_deref(), &|p| {
            dirs.iter().any(|d| d == p)
        })
    }

    /// The conformance table: every product's copy of the rule must pass these rows unchanged.
    #[test]
    fn conformance_table() {
        let lin_legacy = "/home/a/.local/state/perennos-tempo";
        type Row<'a> = (
            &'a str,
            bool,
            Vec<(&'a str, &'a str)>,
            Option<&'a str>,
            Vec<&'a str>,
            &'a str,
            Why,
        );
        let rows: Vec<Row> = vec![
            (
                "TEMPO_HOME wins",
                false,
                vec![("TEMPO_HOME", "/th"), ("PERENNOS_TEMPO_DATA", "/pt")],
                Some("/home/a"),
                vec![lin_legacy],
                "/th",
                Why::Env("TEMPO_HOME"),
            ),
            (
                "alias second",
                false,
                vec![("PERENNOS_TEMPO_DATA", "/pt")],
                Some("/home/a"),
                vec![lin_legacy],
                "/pt",
                Why::Env("PERENNOS_TEMPO_DATA"),
            ),
            (
                "empty TEMPO_HOME skipped",
                false,
                vec![("TEMPO_HOME", ""), ("PERENNOS_TEMPO_DATA", "/pt")],
                Some("/home/a"),
                vec![],
                "/pt",
                Why::Env("PERENNOS_TEMPO_DATA"),
            ),
            (
                "both empty, default",
                false,
                vec![("TEMPO_HOME", ""), ("PERENNOS_TEMPO_DATA", "")],
                Some("/home/a"),
                vec![],
                "/home/a/.tempo",
                Why::Default,
            ),
            (
                "linux legacy only if it exists",
                false,
                vec![],
                Some("/home/a"),
                vec![lin_legacy],
                lin_legacy,
                Why::Legacy,
            ),
            (
                "linux no legacy folder",
                false,
                vec![],
                Some("/home/a"),
                vec![],
                "/home/a/.tempo",
                Why::Default,
            ),
            (
                "linux XDG legacy exists",
                false,
                vec![("XDG_STATE_HOME", "/x")],
                Some("/home/a"),
                vec!["/x/perennos-tempo"],
                "/x/perennos-tempo",
                Why::Legacy,
            ),
            (
                "linux XDG set, only ~/.local one exists",
                false,
                vec![("XDG_STATE_HOME", "/x")],
                Some("/home/a"),
                vec![lin_legacy],
                "/home/a/.tempo",
                Why::Default,
            ),
            (
                "linux empty XDG is ignored",
                false,
                vec![("XDG_STATE_HOME", "")],
                Some("/home/a"),
                vec![lin_legacy],
                lin_legacy,
                Why::Legacy,
            ),
            (
                "mac same as linux",
                false,
                vec![],
                Some("/Users/a"),
                vec![],
                "/Users/a/.tempo",
                Why::Default,
            ),
            (
                "env beats legacy",
                false,
                vec![("TEMPO_HOME", "/th")],
                Some("/home/a"),
                vec![lin_legacy],
                "/th",
                Why::Env("TEMPO_HOME"),
            ),
            (
                "windows legacy only if it exists",
                true,
                vec![
                    ("LOCALAPPDATA", r"C:\Users\a\AppData\Local"),
                    ("TEMPO_LEGACY_HOME", "D:/old-tempo"),
                ],
                Some(r"C:\Users\a"),
                vec!["D:/old-tempo"],
                "D:/old-tempo",
                Why::Legacy,
            ),
            (
                "windows default",
                true,
                vec![("LOCALAPPDATA", r"C:\Users\a\AppData\Local")],
                Some(r"C:\Users\a"),
                vec![],
                "C:/Users/a/AppData/Local/Perennos Tempo",
                Why::Default,
            ),
            (
                "windows default without LOCALAPPDATA",
                true,
                vec![],
                Some(r"C:\Users\a"),
                vec![],
                "C:/Users/a/AppData/Local/Perennos Tempo",
                Why::Default,
            ),
            (
                "windows ignores the XDG folder",
                true,
                vec![("LOCALAPPDATA", "L"), ("XDG_STATE_HOME", "/x")],
                Some("U"),
                vec!["/x/perennos-tempo"],
                "L/Perennos Tempo",
                Why::Default,
            ),
            (
                "windows has no built-in legacy folder",
                true,
                vec![("LOCALAPPDATA", "L")],
                Some("U"),
                vec!["C:/anything"],
                "L/Perennos Tempo",
                Why::Default,
            ),
            (
                "env beats windows legacy",
                true,
                vec![
                    ("TEMPO_HOME", "D:/t"),
                    ("TEMPO_LEGACY_HOME", "D:/old-tempo"),
                ],
                Some("U"),
                vec!["D:/old-tempo"],
                "D:/t",
                Why::Env("TEMPO_HOME"),
            ),
        ];
        for (name, windows, env, user, dirs, want, why) in rows {
            let got = run(windows, &env, user, &dirs);
            // Windows rows run on every OS, so compare with one separator.
            let norm = |p: &Path| p.to_string_lossy().replace('\\', "/");
            assert_eq!(norm(&got.dir), norm(Path::new(want)), "row: {name}");
            assert_eq!(got.why, why, "row: {name}");
        }
    }

    #[test]
    fn no_user_home_falls_back_to_temp() {
        let h = run(false, &[], None, &[]);
        assert_eq!(h.why, Why::Default);
        assert_eq!(h.dir, std::env::temp_dir().join("perennos-tempo"));
    }

    #[test]
    fn user_home_order_per_os() {
        let m: HashMap<&str, OsString> = [("HOME", "/h".into()), ("USERPROFILE", "C:/u".into())]
            .into_iter()
            .collect();
        let get = |k: &str| m.get(k).cloned();
        assert_eq!(user_home_from(false, &get), Some(PathBuf::from("/h")));
        assert_eq!(user_home_from(true, &get), Some(PathBuf::from("C:/u")));
    }

    #[test]
    fn agent_data_env_beats_home_agent() {
        let none = |_: &str| None;
        assert_eq!(
            agent_data(Path::new("/h"), &none),
            PathBuf::from("/h/agent")
        );
        let set = |k: &str| (k == "TEMPO_AGENT_DATA").then(|| OsString::from("/ad"));
        assert_eq!(agent_data(Path::new("/h"), &set), PathBuf::from("/ad"));
    }
}
