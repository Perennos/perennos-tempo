//! Scheduled flows. A flow that runs `daily` or `weekly` becomes one entry in the system's own
//! scheduler, the way the learning task does: a Windows scheduled task, or a crontab line. Each
//! entry just runs `tempo-launch band run <band> <flow>`, with no window.
//!
//! The lines are built as plain text (so they are tested everywhere) and installed by
//! `RealScheduler`. When installing is not possible, the same lines are printed for the person.

use super::spec::{Band, Every};
use crate::{cron_word, crontab_lines, packaged, run_hidden, schedule_exe};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What follows a crontab line made for a band flow, so a re-run replaces it and nothing else.
const CRON_MARK: &str = "# perennos-tempo-band";

const WIN_DAYS: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub flow: String,
    pub every: Every,
    /// The crontab line (Linux; also works on a Mac).
    pub cron: String,
    /// The arguments for `schtasks` (Windows).
    pub task: Vec<String>,
}

impl Entry {
    /// The line to add by hand on this system.
    pub fn line(&self) -> String {
        if cfg!(windows) {
            let quoted: Vec<String> = self
                .task
                .iter()
                .map(|a| {
                    if a.contains(' ') {
                        format!("\"{}\"", a.replace('"', "\\\""))
                    } else {
                        a.clone()
                    }
                })
                .collect();
            format!("schtasks {}", quoted.join(" "))
        } else {
            self.cron.clone()
        }
    }
}

pub fn task_name(band: &str, flow: &str) -> String {
    format!("Perennos Tempo Band {band} {flow}")
}

fn win_command(exe: &Path, band: &str, flow: &str) -> Result<String, String> {
    let p = exe.display().to_string();
    if p.contains(['"', '%', '\n', '\r']) {
        return Err(format!(
            "the task command cannot hold a quote, a % or a line break; {p} has one"
        ));
    }
    let c = format!("\"{p}\" band run {band} {flow}");
    if c.len() > 261 {
        return Err("the task command is longer than Windows allows (261 characters); install Perennos Tempo in a shorter folder".into());
    }
    Ok(c)
}

/// One entry for a flow that runs by itself; `Ok(None)` for a manual flow.
pub fn entry(exe: &Path, band: &str, flow: &str, every: Every) -> Result<Option<Entry>, String> {
    let (min, hour, dow, sc): (u8, u8, String, Vec<String>) = match every {
        Every::Manual => return Ok(None),
        Every::Daily(h, m) => (m, h, "*".into(), vec!["/SC".into(), "DAILY".into()]),
        Every::Weekly(d, h, m) => (
            m,
            h,
            ((d + 1) % 7).to_string(),
            vec![
                "/SC".into(),
                "WEEKLY".into(),
                "/D".into(),
                WIN_DAYS[d as usize % 7].into(),
            ],
        ),
    };
    if exe.to_string_lossy().chars().any(char::is_control) {
        return Err("the program's folder name has a control character; install Perennos Tempo in a plainer folder".into());
    }
    let cron = format!(
        "{min} {hour} * * {dow} {} band run {band} {flow} {CRON_MARK} {band}/{flow}",
        cron_word(exe)?
    );
    let mut task: Vec<String> = [
        "/Create",
        "/F",
        "/TN",
        &task_name(band, flow),
        "/TR",
        &win_command(exe, band, flow)?,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    task.extend(sc);
    task.extend(["/ST".into(), format!("{hour:02}:{min:02}")]);
    Ok(Some(Entry {
        flow: flow.to_string(),
        every,
        cron,
        task,
    }))
}

/// The entries for every flow of the band that runs by itself.
pub fn entries(band: &Band, exe: &Path) -> Result<Vec<Entry>, String> {
    let mut all = Vec::new();
    for f in &band.flows {
        if let Some(e) = entry(exe, &band.name, &f.name, f.every)? {
            all.push(e);
        }
    }
    Ok(all)
}

/// The crontab with this band's earlier lines taken out and the new ones added.
pub fn merge_cron(existing: Vec<String>, band: &str, new: &[String]) -> Vec<String> {
    let mine = format!("{CRON_MARK} {band}/");
    let mut lines: Vec<String> = existing
        .into_iter()
        .filter(|l| !l.contains(&mine))
        .collect();
    lines.extend(new.iter().cloned());
    lines
}

pub trait Scheduler {
    fn install(&self, band: &str, entries: &[Entry]) -> Result<(), String>;
}

/// This program, as a scheduled task should run it (the Store package's command name inside the
/// package).
pub fn scheduled_exe() -> PathBuf {
    let current = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tempo-launch"));
    schedule_exe(
        packaged(),
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .as_deref(),
        &current,
    )
}

pub struct RealScheduler;

impl Scheduler for RealScheduler {
    fn install(&self, band: &str, entries: &[Entry]) -> Result<(), String> {
        if cfg!(windows) {
            for e in entries {
                let out = run_hidden(Command::new("schtasks").args(&e.task))
                    .map_err(|err| format!("cannot run schtasks: {err}"))?;
                if !out.status.success() {
                    let said = [&out.stderr, &out.stdout]
                        .iter()
                        .map(|b| String::from_utf8_lossy(b).trim().to_string())
                        .find(|s| !s.is_empty())
                        .unwrap_or_else(|| "no message".into());
                    return Err(format!("schtasks ended with {}: {said}", out.status));
                }
            }
            Ok(())
        } else if cfg!(target_os = "macos") {
            Err("scheduling on a Mac is not automatic yet".into())
        } else {
            let cur = Command::new("crontab")
                .arg("-l")
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("cannot run crontab: {e}"))?;
            let existing = crontab_lines(
                cur.status.success(),
                &String::from_utf8_lossy(&cur.stdout),
                &String::from_utf8_lossy(&cur.stderr),
            )?;
            let new: Vec<String> = entries.iter().map(|e| e.cron.clone()).collect();
            let lines = merge_cron(existing, band, &new);
            let mut child = Command::new("crontab")
                .arg("-")
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|e| format!("cannot run crontab: {e}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let mut text = lines.join("\n");
                text.push('\n');
                stdin
                    .write_all(text.as_bytes())
                    .map_err(|e| e.to_string())?;
            }
            if child.wait().is_ok_and(|s| s.success()) {
                Ok(())
            } else {
                Err("crontab refused the new schedule".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from("/opt/tempo/tempo-launch")
    }

    #[test]
    fn a_manual_flow_has_no_entry() {
        assert_eq!(entry(&exe(), "b", "f", Every::Manual), Ok(None));
    }

    #[test]
    fn daily_and_weekly_become_cron_lines() {
        let d = entry(&exe(), "bakery", "morning", Every::Daily(8, 30))
            .unwrap()
            .unwrap();
        assert_eq!(
            d.cron,
            "30 8 * * * \"/opt/tempo/tempo-launch\" band run bakery morning # perennos-tempo-band bakery/morning"
        );
        // Monday is 1 in cron, Sunday 0
        let w = entry(&exe(), "bakery", "plan", Every::Weekly(0, 9, 5))
            .unwrap()
            .unwrap();
        assert!(w.cron.starts_with("5 9 * * 1 "), "{}", w.cron);
        let s = entry(&exe(), "bakery", "plan", Every::Weekly(6, 18, 0))
            .unwrap()
            .unwrap();
        assert!(s.cron.starts_with("0 18 * * 0 "), "{}", s.cron);
    }

    #[test]
    fn windows_gets_schtasks_arguments() {
        let w = entry(
            Path::new("C:\\Tempo\\tempo-launch.exe"),
            "bakery",
            "plan",
            Every::Weekly(4, 16, 0),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            w.task,
            [
                "/Create",
                "/F",
                "/TN",
                "Perennos Tempo Band bakery plan",
                "/TR",
                "\"C:\\Tempo\\tempo-launch.exe\" band run bakery plan",
                "/SC",
                "WEEKLY",
                "/D",
                "FRI",
                "/ST",
                "16:00"
            ]
        );
        let d = entry(&exe(), "b-b", "f-f", Every::Daily(7, 5))
            .unwrap()
            .unwrap();
        assert!(d
            .task
            .ends_with(&["/SC".into(), "DAILY".into(), "/ST".into(), "07:05".into()]));
        assert!(d.line().contains("band run b-b f-f"));
    }

    #[test]
    fn a_path_a_scheduler_cannot_hold_is_refused() {
        assert!(entry(Path::new("/opt/te%mpo/x"), "b", "f", Every::Daily(1, 1)).is_err());
        assert!(entry(Path::new("/opt/te\"mpo/x"), "b", "f", Every::Daily(1, 1)).is_err());
        assert!(entry(Path::new("/opt/te\nmpo/x"), "b", "f", Every::Daily(1, 1)).is_err());
        let long = format!("/{}/x", "a".repeat(300));
        assert!(entry(Path::new(&long), "b", "f", Every::Daily(1, 1))
            .unwrap_err()
            .contains("261"));
    }

    #[test]
    fn a_rerun_replaces_only_this_bands_lines() {
        let existing = vec![
            "17 * * * * tempo-launch learn # perennos-tempo-vault-learn".to_string(),
            "0 9 * * 1 x band run bakery plan # perennos-tempo-band bakery/plan".to_string(),
            "0 9 * * 1 x band run bakery-two plan # perennos-tempo-band bakery-two/plan"
                .to_string(),
            "5 5 * * * backup".to_string(),
        ];
        let merged = merge_cron(existing, "bakery", &["NEW".to_string()]);
        assert_eq!(merged.len(), 4);
        assert!(merged.iter().any(|l| l.contains("vault-learn")));
        assert!(merged.iter().any(|l| l.contains("bakery-two/plan")));
        assert!(merged.iter().any(|l| l == "5 5 * * * backup"));
        assert!(!merged.iter().any(|l| l.contains("bakery/plan")));
        assert_eq!(merged.last().unwrap(), "NEW");
    }

    #[test]
    fn entries_cover_each_scheduled_flow_of_a_band() {
        let t = crate::band::templates::find("small-business").unwrap();
        let b = t.band("shop", &crate::band::Models::default());
        let e = entries(&b, &exe()).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].flow, "morning-inbox");
        let music = crate::band::templates::find("music-release")
            .unwrap()
            .band("m", &crate::band::Models::default());
        assert!(entries(&music, &exe()).unwrap().is_empty());
    }
}
