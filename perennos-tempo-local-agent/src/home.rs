//! One Tempo home per machine, found the same way by every Perennos Tempo product:
//!
//! 1. `TEMPO_HOME` if set (`PERENNOS_TEMPO_DATA` is its older name), then the agent config's
//!    `tempo_home`;
//! 2. else a folder an earlier version used, only if it already exists: `C:\LocalAI\apps\decision-head`
//!    on Windows, `$XDG_STATE_HOME/perennos-tempo` or `~/.local/state/perennos-tempo` elsewhere;
//! 3. else `%LOCALAPPDATA%\Perennos Tempo` on Windows and `~/.tempo` elsewhere.
//!
//! The agent keeps its data in `<home>/agent` unless `TEMPO_AGENT_DATA` says otherwise. Nothing is
//! ever moved from one folder to another here.
use crate::config::{expand_home, home_dir};
use std::path::PathBuf;

/// Where an earlier version kept its files, for this platform (checked only when it exists).
fn legacy(getenv: &dyn Fn(&str) -> Option<String>, home: Option<PathBuf>) -> Vec<PathBuf> {
    if cfg!(windows) {
        return vec![PathBuf::from(r"C:\LocalAI\apps\decision-head")];
    }
    let mut out = vec![];
    if let Some(x) = getenv("XDG_STATE_HOME").filter(|s| !s.trim().is_empty()) {
        out.push(PathBuf::from(x).join("perennos-tempo"));
    }
    if let Some(h) = home {
        out.push(h.join(".local").join("state").join("perennos-tempo"));
    }
    out
}

/// The Tempo home and the reason it was chosen (for `tempo-launch home` and error messages).
pub fn tempo_home_why(
    getenv: &dyn Fn(&str) -> Option<String>,
    configured: Option<&str>,
) -> Option<(PathBuf, &'static str)> {
    let home = getenv("HOME")
        .or_else(|| getenv("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(home_dir);
    let set = |v: &str| expand_home(v.trim(), home.clone());
    for (k, why) in [
        ("TEMPO_HOME", "TEMPO_HOME is set"),
        ("PERENNOS_TEMPO_DATA", "PERENNOS_TEMPO_DATA is set"),
    ] {
        if let Some(v) = getenv(k).filter(|s| !s.trim().is_empty()) {
            return Some((set(&v), why));
        }
    }
    if let Some(v) = configured.filter(|s| !s.trim().is_empty()) {
        return Some((set(v), "the agent config names it"));
    }
    if let Some(p) = legacy(getenv, home.clone())
        .into_iter()
        .find(|p| p.is_dir())
    {
        return Some((p, "an earlier Tempo folder that already exists"));
    }
    if cfg!(windows) {
        if let Some(l) = getenv("LOCALAPPDATA").filter(|s| !s.trim().is_empty()) {
            return Some((
                PathBuf::from(l).join("Perennos Tempo"),
                "the default for Windows",
            ));
        }
    }
    home.map(|h| (h.join(".tempo"), "the default"))
}

pub fn tempo_home(
    getenv: &dyn Fn(&str) -> Option<String>,
    configured: Option<&str>,
) -> Option<PathBuf> {
    tempo_home_why(getenv, configured).map(|(p, _)| p)
}

/// The agent's data folder: `TEMPO_AGENT_DATA`, else `<home>/agent` (else `./tempo-agent-data`
/// when not even a home folder can be found).
pub fn agent_data(
    getenv: &dyn Fn(&str) -> Option<String>,
    configured_home: Option<&str>,
) -> PathBuf {
    if let Some(d) = getenv("TEMPO_AGENT_DATA").filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(d);
    }
    tempo_home(getenv, configured_home)
        .map(|h| h.join("agent"))
        .unwrap_or_else(|| PathBuf::from("./tempo-agent-data"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn the_home_is_found_in_order() {
        let d = std::env::temp_dir().join(format!("tla-home-{}", std::process::id()));
        let h = d.join("user");
        std::fs::create_dir_all(&h).unwrap();
        let hs = h.to_string_lossy().into_owned();
        // 1. TEMPO_HOME, then the older name, then the config
        let e = env(&[
            ("HOME", hs.clone()),
            ("TEMPO_HOME", "/t/home".into()),
            ("PERENNOS_TEMPO_DATA", "/old".into()),
        ]);
        assert_eq!(
            tempo_home_why(&e, Some("/cfg")).unwrap(),
            (PathBuf::from("/t/home"), "TEMPO_HOME is set")
        );
        let e = env(&[("HOME", hs.clone()), ("PERENNOS_TEMPO_DATA", "/old".into())]);
        assert_eq!(tempo_home(&e, Some("/cfg")).unwrap(), PathBuf::from("/old"));
        let e = env(&[("HOME", hs.clone())]);
        assert_eq!(tempo_home(&e, Some("~/mine")).unwrap(), h.join("mine"));
        if !cfg!(windows) {
            // 3. the default, until an earlier folder exists; then that one
            assert_eq!(
                tempo_home_why(&e, None).unwrap(),
                (h.join(".tempo"), "the default")
            );
            let old = h.join(".local/state/perennos-tempo");
            std::fs::create_dir_all(&old).unwrap();
            assert_eq!(tempo_home(&e, None).unwrap(), old);
            let xdg = d.join("xdg");
            std::fs::create_dir_all(xdg.join("perennos-tempo")).unwrap();
            let e = env(&[
                ("HOME", hs.clone()),
                ("XDG_STATE_HOME", xdg.to_string_lossy().into_owned()),
            ]);
            assert_eq!(tempo_home(&e, None).unwrap(), xdg.join("perennos-tempo"));
            // the agent's data: TEMPO_AGENT_DATA, else <home>/agent
            assert_eq!(
                agent_data(&e, None),
                xdg.join("perennos-tempo").join("agent")
            );
            let e = env(&[("HOME", hs), ("TEMPO_AGENT_DATA", "/a/d".into())]);
            assert_eq!(agent_data(&e, None), PathBuf::from("/a/d"));
        }
    }
}
