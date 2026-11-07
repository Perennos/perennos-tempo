//! Room transcripts in the Perennos Tempo Vault. Every message becomes a line in
//! `Rooms/<room>/<YYYY-MM-DD>.md` under an `## HH:MM` heading, written in batches by running
//! `tempo-vault append "<path>" "<lines>" --heading "<HH:MM>" --vault <dir>` (no console window on
//! Windows). `@tempo remember x` appends to `Rooms/<room>/Remembered.md` at once. Text is redacted
//! first. A missing `tempo-vault` is a logged warning, never a failure: the room keeps its own
//! JSONL copy of everything.
use crate::store::{Kind, Msg};
use crate::util::{date_of, head_chars, log, scrub, time_of};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "tempo-vault.exe"
    } else {
        "tempo-vault"
    }
}

/// `tempo-vault` next to `tempo-room`, else on PATH, else `TEMPO_VAULT_BIN`.
pub fn find_bin(
    exe_dir: Option<&Path>,
    path_var: Option<&OsStr>,
    env_bin: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(p) = exe_dir.map(|d| d.join(exe_name())).filter(|p| p.is_file()) {
        return Some(p);
    }
    if let Some(p) = path_var.and_then(|pv| {
        std::env::split_paths(pv)
            .map(|d| d.join(exe_name()))
            .find(|p| p.is_file())
    }) {
        return Some(p);
    }
    env_bin
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

/// The lookup above with this process's own folder, PATH and environment.
pub fn find_bin_here() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok();
    find_bin(
        exe.as_deref().and_then(Path::parent),
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("TEMPO_VAULT_BIN").as_deref(),
    )
}

/// Run `tempo-vault append`. Waits at most `timeout`; a hung run is stopped.
pub fn run_append(
    bin: &Path,
    vault: &Path,
    path: &str,
    text: &str,
    heading: Option<&str>,
    timeout: Duration,
) -> Result<(), String> {
    let mut cmd = Command::new(bin);
    cmd.arg("append").arg(path).arg(text);
    if let Some(h) = heading {
        cmd.arg("--heading").arg(h);
    }
    cmd.arg("--vault")
        .arg(vault)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::util::CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut err);
                }
                return Err(format!(
                    "tempo-vault append failed ({status}): {}",
                    head_chars(err.trim(), 300)
                ));
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("tempo-vault append took too long and was stopped".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// One transcript line for a message: `- **Mia**: text`, later lines indented, redacted.
pub fn line_for(m: &Msg) -> String {
    let who = if m.kind == Kind::Tempo {
        "Tempo".to_string()
    } else if m.kind == Kind::Teammate {
        format!(
            "{} (teammate)",
            crate::util::clean_name(&m.name).replace('*', "")
        )
    } else {
        crate::util::clean_name(&m.name).replace('*', "")
    };
    let via = match m.via.as_str() {
        "discord" | "telegram" | "whatsapp" if m.kind != Kind::Tempo => format!(" ({})", m.via),
        _ => String::new(),
    };
    // A NUL (or any control character) in an argument would make the whole batch fail to start.
    let text = scrub(&crate::util::clean_text(&m.text));
    let mut lines = text.lines();
    let mut out = format!("- **{who}**{via}: {}", lines.next().unwrap_or(""));
    for l in lines {
        out.push_str("\n  ");
        out.push_str(l);
    }
    out
}

pub struct Line {
    pub room: String,
    pub t: f64,
    pub text: String,
}

/// Group lines into `(path, heading, text)` appends: one per room, day and minute, in order, and
/// none longer than about 6,000 characters (Windows caps a command line).
pub fn group(lines: &[Line], offset_min: i64) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = vec![];
    for l in lines {
        let path = format!("Rooms/{}/{}.md", l.room, date_of(l.t, offset_min));
        let heading = time_of(l.t, offset_min);
        match out.last_mut() {
            Some((p, h, t)) if *p == path && *h == heading && t.len() + l.text.len() < 6000 => {
                t.push('\n');
                t.push_str(&l.text);
            }
            _ => out.push((path, heading, l.text.clone())),
        }
    }
    out
}

pub struct Vault {
    pub dir: PathBuf,
    pub bin: Option<PathBuf>,
    pub offset_min: i64,
    tx: Option<mpsc::Sender<Line>>,
}

static WARNED: AtomicBool = AtomicBool::new(false);

fn warn_missing() {
    if !WARNED.swap(true, Ordering::SeqCst) {
        log("warning: the vault is set but tempo-vault was not found (next to tempo-room, on PATH, or TEMPO_VAULT_BIN); room transcripts stay in the room's own files");
    }
}

impl Vault {
    /// Start the batch writer: lines are written every `flush`, or sooner when 50 are waiting.
    pub fn start(dir: PathBuf, bin: Option<PathBuf>, offset_min: i64, flush: Duration) -> Vault {
        let (tx, rx) = mpsc::channel::<Line>();
        let (d, b) = (dir.clone(), bin.clone());
        std::thread::spawn(move || {
            let mut pending: Vec<Line> = vec![];
            loop {
                match rx.recv_timeout(flush) {
                    Ok(l) => {
                        pending.push(l);
                        if pending.len() < 50 {
                            continue;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        write_batch(&d, b.as_deref(), &pending, offset_min);
                        break;
                    }
                }
                write_batch(&d, b.as_deref(), &pending, offset_min);
                pending.clear();
            }
        });
        Vault {
            dir,
            bin,
            offset_min,
            tx: Some(tx),
        }
    }

    pub fn add(&self, m: &Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Line {
                room: m.room.clone(),
                t: m.t,
                text: line_for(m),
            });
        }
    }

    /// Append to `Rooms/<room>/Remembered.md` now.
    pub fn remember(&self, room: &str, line: &str) -> Result<String, String> {
        let Some(bin) = &self.bin else {
            warn_missing();
            return Err("tempo-vault was not found".into());
        };
        let path = format!("Rooms/{room}/Remembered.md");
        run_append(
            bin,
            &self.dir,
            &path,
            &scrub(&crate::util::clean_text(line)),
            None,
            Duration::from_secs(30),
        )?;
        Ok(path)
    }
}

fn write_batch(dir: &Path, bin: Option<&Path>, lines: &[Line], offset_min: i64) {
    if lines.is_empty() {
        return;
    }
    let Some(bin) = bin else {
        warn_missing();
        return;
    };
    for (path, heading, text) in group(lines, offset_min) {
        if let Err(e) = run_append(
            bin,
            dir,
            &path,
            &text,
            Some(&heading),
            Duration::from_secs(30),
        ) {
            log(&format!(
                "warning: vault transcript not written ({path}): {e}"
            ));
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::util::testdir::tmpdir;

    /// A stand-in `tempo-vault` that records its arguments, one call per line (Unix only).
    #[cfg(unix)]
    pub fn fake_vault_bin(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("tempo-vault");
        let log = dir.join("calls.log");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\037' \"$a\" >> '{}'; done\nprintf '\\036\\n' >> '{}'\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// The recorded calls, each as its argument list. The fake writes one argument at a time,
    /// so only calls whose end mark is written count (a call still being written is not one).
    pub fn calls(dir: &Path) -> Vec<Vec<String>> {
        let log = std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default();
        let done = log.rfind("\x1e\n").map_or("", |i| &log[..i + 2]);
        done.split("\x1e\n")
            .filter(|c| !c.is_empty())
            .map(|c| {
                c.split('\x1f')
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }

    fn msg(name: &str, text: &str, t: f64, kind: Kind, via: &str) -> Msg {
        let mut m = Msg::new("family", "x", name, kind, via, text);
        m.t = t;
        m
    }

    #[test]
    fn a_call_still_being_written_is_not_a_call() {
        let d = crate::util::testdir::tmpdir("vault-calls");
        std::fs::write(
            d.join("calls.log"),
            "append\x1fA.md\x1e\nappend\x1fRooms/family/2026-11-07.md\x1fbody\x1f--heading\x1f",
        )
        .unwrap();
        assert_eq!(
            calls(&d),
            vec![vec!["append".to_string(), "A.md".to_string()]]
        );
    }

    #[test]
    fn lines_and_groups() {
        let key = format!("ghp_{}", "abcdefghijKLMNOPQRSTuvwxyz0123456789");
        assert_eq!(
            line_for(&msg(
                "Mia",
                &format!("hi\nkey {key}"),
                0.0,
                Kind::User,
                "telegram"
            )),
            "- **Mia** (telegram): hi\n  key [REDACTED]"
        );
        assert_eq!(
            line_for(&msg("x", "answer", 0.0, Kind::Tempo, "discord")),
            "- **Tempo**: answer"
        );
        let t0 = 1_793_966_401.0; // 2026-11-06 12:00:01 UTC
        let lines: Vec<Line> = [(0.0, "a"), (30.0, "b"), (65.0, "c"), (86_400.0, "d")]
            .iter()
            .map(|(dt, s)| Line {
                room: "family".into(),
                t: t0 + dt,
                text: s.to_string(),
            })
            .collect();
        let g = group(&lines, -300);
        assert_eq!(
            g,
            vec![
                (
                    "Rooms/family/2026-11-06.md".into(),
                    "07:00".into(),
                    "a\nb".into()
                ),
                (
                    "Rooms/family/2026-11-06.md".into(),
                    "07:01".into(),
                    "c".into()
                ),
                (
                    "Rooms/family/2026-11-07.md".into(),
                    "07:00".into(),
                    "d".into()
                ),
            ]
        );
    }

    #[test]
    fn finds_the_binary_in_order() {
        let a = tmpdir("bin-a");
        let b = tmpdir("bin-b");
        let c = tmpdir("bin-c");
        assert_eq!(find_bin(Some(&a), None, None), None);
        std::fs::write(c.join("custom-vault"), "x").unwrap();
        let env = c.join("custom-vault");
        assert_eq!(
            find_bin(Some(&a), None, Some(env.as_os_str())),
            Some(env.clone())
        );
        std::fs::write(b.join(exe_name()), "x").unwrap();
        let path = std::env::join_paths([a.clone(), b.clone()]).unwrap();
        assert_eq!(
            find_bin(Some(&a), Some(&path), Some(env.as_os_str())),
            Some(b.join(exe_name())),
            "PATH before TEMPO_VAULT_BIN"
        );
        std::fs::write(a.join(exe_name()), "x").unwrap();
        assert_eq!(
            find_bin(Some(&a), Some(&path), Some(env.as_os_str())),
            Some(a.join(exe_name())),
            "next to tempo-room first"
        );
    }

    #[cfg(unix)]
    #[test]
    fn batches_run_tempo_vault_append() {
        let d = tmpdir("vault-run");
        let bin = fake_vault_bin(&d);
        let vdir = d.join("vault");
        let v = Vault::start(
            vdir.clone(),
            Some(bin.clone()),
            0,
            Duration::from_millis(100),
        );
        let t0 = 1_793_966_401.0;
        v.add(&msg("Mia", "dinner at 7?", t0, Kind::User, "app"));
        v.add(&msg("x", "Sounds good.", t0 + 2.0, Kind::Tempo, "app"));
        let start = Instant::now();
        while calls(&d).is_empty() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let c = calls(&d);
        assert_eq!(c.len(), 1, "one batch, one call: {c:?}");
        assert_eq!(
            c[0],
            [
                "append",
                "Rooms/family/2026-11-06.md",
                "- **Mia**: dinner at 7?\n- **Tempo**: Sounds good.",
                "--heading",
                "12:00",
                "--vault",
                vdir.to_str().unwrap()
            ]
        );
        let p = v
            .remember("family", "- 2026-11-06 Mia: the wifi is upstairs")
            .unwrap();
        assert_eq!(p, "Rooms/family/Remembered.md");
        assert_eq!(calls(&d)[1][1], "Rooms/family/Remembered.md");
        // A failing binary is an error to log, and a missing one is never a crash.
        let bad = d.join("bad-vault");
        std::fs::write(&bad, "#!/bin/sh\necho nope >&2\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = run_append(&bad, &vdir, "x.md", "t", None, Duration::from_secs(5)).unwrap_err();
        assert!(e.contains("nope"), "{e}");
        let none = Vault::start(vdir, None, 0, Duration::from_millis(50));
        none.add(&msg("Mia", "hi", t0, Kind::User, "app"));
        assert!(none.remember("family", "x").is_err());
        std::thread::sleep(Duration::from_millis(200));
        assert!(crate::util::test_log_lines()
            .iter()
            .any(|l| l.contains("tempo-vault was not found")));
    }

    #[test]
    fn a_control_character_cannot_stop_a_transcript_from_starting() {
        // A NUL in an argument makes the whole `tempo-vault append` fail to start.
        let m = msg(
            "Mia\u{0}",
            "hello\u{0}\u{1b}[2J world",
            0.0,
            Kind::User,
            "app",
        );
        let l = line_for(&m);
        assert!(!l.contains('\u{0}') && !l.contains('\u{1b}'), "{l:?}");
        assert!(l.contains("hello"));
        #[cfg(unix)]
        {
            let mut cmd = Command::new("true");
            cmd.arg(&l);
            assert!(cmd.spawn().is_ok());
        }
    }
}
