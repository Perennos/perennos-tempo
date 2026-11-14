//! Keeping `fleet serve` running: the same kind of entry `tempo-launch schedule` makes for the
//! learning task. `serve --quiet` ends at once when one is already running, so the entry only
//! has to start it every few minutes; it is installed when the fleet has more than one machine.
//! On a Mac, `tempo-launch start` (what sign-in runs) starts it.

use crate::{is_dev_build, DEV_BUILD_NOTE};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const TASK: &str = "Perennos Tempo Fleet";
const MARK: &str = "# perennos-tempo-fleet";

// Cargo's output root can be configured and its profile can have any name. Look for the
// profile's build structure, not a directory named `target`; canonicalize before use so an
// ordinary symlink to a build artifact does not bypass the check. This is a guard, not a
// signature or proof of an executable's origin.
fn in_build_tree(exe: &Path, is_dir: impl Fn(&Path) -> bool) -> bool {
    exe.ancestors().skip(1).any(|dir| {
        is_dir(&dir.join(".fingerprint")) && is_dir(&dir.join("deps")) && is_dir(&dir.join("build"))
    })
}

pub(super) fn background_executable(exe: &Path) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(exe)
        .map_err(|e| format!("cannot resolve the fleet executable: {e}"))?;
    if in_build_tree(&path, Path::is_dir) {
        return Err("fleet self-start and scheduling refuse Cargo build artifacts".into());
    }
    Ok(path)
}

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
    if is_dev_build(exe) {
        return Ok(DEV_BUILD_NOTE.into());
    }
    let exe = background_executable(exe)?;
    if cfg!(target_os = "macos") {
        return Ok(
            "on a Mac, tempo-launch start (run at sign-in) starts the fleet messenger".into(),
        );
    }
    let e = entry(&exe)?;
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
    fn build_profiles_are_refused_even_with_custom_target_directories() {
        for path in [
            "C:/cargo-cache/debug/deps/tempo_launch-0123456789abcdef.exe",
            "C:/cargo-cache/x86_64-pc-windows-gnu/release/tempo-launch.exe",
            "/home/a/out/custom-profile/deps/tempo_launch-0123456789abcdef",
            "/home/a/out/x86_64-unknown-linux-gnu/debug/tempo-launch",
        ] {
            let exe = Path::new(path);
            let profile = if exe.parent().unwrap().ends_with("deps") {
                exe.parent().unwrap().parent().unwrap()
            } else {
                exe.parent().unwrap()
            };
            assert!(
                in_build_tree(exe, |p| [".fingerprint", "deps", "build"]
                    .iter()
                    .any(|name| p == profile.join(name))),
                "{path}"
            );
        }
        assert!(!in_build_tree(
            Path::new("C:/Users/a/AppData/Local/Programs/Perennos Tempo/bin/tempo-launch.exe"),
            |_| false
        ));
        assert!(!in_build_tree(
            Path::new("/home/a/.local/bin/tempo-launch"),
            |_| false
        ));
    }

    #[test]
    fn actual_build_structure_refuses_self_start_without_touching_a_schedule() {
        let root = super::super::tests::tmp("build-guard");
        let profile = root.join("arbitrary-target/custom-profile");
        for dir in [".fingerprint", "deps", "build"] {
            std::fs::create_dir_all(profile.join(dir)).unwrap();
        }
        let artifact = profile.join("deps/tempo_launch-test.exe");
        std::fs::write(&artifact, b"fixture").unwrap();
        assert!(background_executable(&artifact).is_err());
        let installed = root.join("bin/tempo-launch.exe");
        std::fs::create_dir(root.join("bin")).unwrap();
        std::fs::write(&installed, b"fixture").unwrap();
        assert!(background_executable(&installed).is_ok());
        assert!(background_executable(&root.join("missing")).is_err());
    }

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
    fn cargo_outputs_are_dev_builds_and_installed_programs_are_not() {
        for p in [
            r"D:\w\target\debug\deps\tempo_launch-1a2b.exe",
            r"C:\x\target\release\tempo-launch.exe",
            "/home/x/target/debug/deps/tempo_launch-1a2b",
            "/home/x/repo/target/release/tempo-launch",
        ] {
            assert!(
                crate::in_cargo_target(Path::new(p)) && is_dev_build(Path::new(p)),
                "{p}"
            );
        }
        // under cargo test is_dev_build is true for everything, so the path rule is checked alone
        for p in [
            r"C:\Users\x\AppData\Local\Programs\Perennos Tempo\bin\tempo-launch.exe",
            "/usr/local/bin/tempo-launch",
        ] {
            assert!(!crate::in_cargo_target(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn install_of_a_dev_build_runs_nothing() {
        // returns the note at once: no schtasks, crontab or systemctl is started
        let exe = Path::new("/x/target/debug/deps/tempo_launch-1");
        assert_eq!(install(exe, false).unwrap(), DEV_BUILD_NOTE);
        assert_eq!(install(exe, true).unwrap(), DEV_BUILD_NOTE);
    }
}
