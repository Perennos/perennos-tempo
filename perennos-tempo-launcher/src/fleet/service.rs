//! Keeping `fleet serve` running: the same kind of entry `tempo-launch schedule` makes for the
//! learning task. `serve --quiet` ends at once when one is already running, so the entry only
//! has to start it every few minutes; it is installed when the fleet has more than one machine.
//! On a Mac, `tempo-launch start` (what sign-in runs) starts it.

use std::path::Path;
use std::process::{Command, Stdio};

pub const TASK: &str = "Perennos Tempo Fleet";
const MARK: &str = "# perennos-tempo-fleet";

/// The cron line, or the Windows task command.
pub fn entry(exe: &Path) -> Result<String, String> {
    let p = exe.display().to_string();
    if p.contains(['"', '%', '\n', '\r', '\0', '$', '`', '\\', '\'', '!']) && !cfg!(windows) {
        return Err(format!(
            "the program's folder holds a character a schedule cannot carry: {p}"
        ));
    }
    if cfg!(windows) {
        if p.contains(['"', '%', '\n', '\r']) {
            return Err(format!(
                "the task command cannot hold a quote, a % or a line break: {p}"
            ));
        }
        Ok(format!("\"{p}\" fleet serve --quiet"))
    } else {
        Ok(format!("*/5 * * * * \"{p}\" fleet serve --quiet {MARK}"))
    }
}

/// Put the entry in (or take it out). Returns what was done, in words.
pub fn install(exe: &Path, remove: bool) -> Result<String, String> {
    if cfg!(target_os = "macos") {
        return Ok(
            "on a Mac, tempo-launch start (run at sign-in) starts the fleet messenger".into(),
        );
    }
    let e = entry(exe)?;
    if cfg!(test) {
        return Ok(format!(
            "test run: would {} {e}",
            if remove { "remove" } else { "install" }
        ));
    }
    if cfg!(windows) {
        let args: Vec<String> = if remove {
            vec!["/Delete".into(), "/F".into(), "/TN".into(), TASK.into()]
        } else {
            [
                "/Create", "/F", "/TN", TASK, "/TR", &e, "/SC", "MINUTE", "/MO", "5",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect()
        };
        let out = Command::new("schtasks")
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run schtasks: {e}"))?;
        if !out.status.success() && !remove {
            return Err(format!("schtasks ended with {}", out.status));
        }
    } else {
        let cur = Command::new("crontab")
            .arg("-l")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run crontab: {e}"))?;
        let mut lines = crate::crontab_lines(
            cur.status.success(),
            &String::from_utf8_lossy(&cur.stdout),
            &String::from_utf8_lossy(&cur.stderr),
        )?;
        lines.retain(|l| !l.contains(MARK));
        if !remove {
            lines.push(e);
        }
        let mut child = Command::new("crontab")
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run crontab: {e}"))?;
        if let Some(mut si) = child.stdin.take() {
            let _ = std::io::Write::write_all(&mut si, (lines.join("\n") + "\n").as_bytes());
        }
        if !child.wait().is_ok_and(|s| s.success()) {
            return Err("crontab refused the new schedule".into());
        }
    }
    Ok(if remove {
        "removed the fleet messenger's schedule".into()
    } else {
        "the fleet messenger is started every five minutes if it is not running".into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_runs_serve_quietly_and_refuses_odd_paths() {
        let e = entry(Path::new("/opt/tempo/tempo-launch")).unwrap();
        assert!(e.contains("fleet serve --quiet"));
        if !cfg!(windows) {
            assert!(e.starts_with("*/5 * * * * ") && e.ends_with(MARK));
            assert!(entry(Path::new("/opt/te'mpo/x")).is_err());
        }
    }

    #[test]
    fn install_reports_a_simulated_change_without_touching_the_host() {
        if !cfg!(target_os = "macos") {
            let exe = Path::new("/opt/tempo/tempo-launch");
            let entry = entry(exe).unwrap();
            assert_eq!(
                install(exe, false).unwrap(),
                format!("test run: would install {entry}")
            );
            assert_eq!(
                install(exe, true).unwrap(),
                format!("test run: would remove {entry}")
            );
        }
    }
}
