//! Running things on the machine that trains: this computer, or another one of the fleet over the
//! owner's own ssh (`fleet/ssh.rs`: BatchMode, no password asked or kept). The fleet messenger's
//! `job` topic only plays chat steps through a guest conversation (it never runs a program), so a
//! training run goes over ssh, which the fleet module already supports.
//!
//! A command's output goes to `woodshed.log` in the work folder, not through a pipe, so a trainer
//! that talks for hours never blocks on a full one. What comes back is the last few lines.

use crate::fleet::ssh;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Where {
    Local,
    /// An ssh host or `user@host`, as the fleet module names it.
    Ssh(String),
}

/// A machine and a work folder on it. On ssh the folder may start with `$HOME`.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub at: Where,
    pub dir: String,
}

#[derive(Debug)]
pub struct Done {
    pub ok: bool,
    pub tail: String,
}

pub trait Exec {
    /// Run `argv` in the work folder (made if it is not there); wait up to `limit`.
    fn run(&self, t: &Target, argv: &[String], limit: Duration) -> Result<Done, String>;
    /// Write a text file into the work folder.
    fn put(&self, t: &Target, name: &str, text: &str) -> Result<(), String>;
    /// Bring a file from the work folder to `to`.
    fn get(&self, t: &Target, name: &str, to: &Path) -> Result<(), String>;
}

/// One word for a shell: in single quotes, with any single quote inside closed and escaped.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A file name that stays in the work folder.
fn plain(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && name != ".." && name != "."
}

fn tail_of(text: &str) -> String {
    let t: Vec<&str> = text.lines().collect();
    t[t.len().saturating_sub(12)..].join("\n")
}

pub struct RealExec;

impl RealExec {
    fn local_dir(t: &Target) -> PathBuf {
        PathBuf::from(&t.dir)
    }
}

impl Exec for RealExec {
    fn run(&self, t: &Target, argv: &[String], limit: Duration) -> Result<Done, String> {
        let Some((prog, args)) = argv.split_first() else {
            return Err("nothing to run".into());
        };
        match &t.at {
            Where::Local => {
                let dir = Self::local_dir(t);
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
                let log_path = dir.join("woodshed.log");
                let log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
                let from = log.metadata().map_or(0, |m| m.len());
                let mut cmd = Command::new(prog);
                cmd.args(args)
                    .current_dir(&dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
                    .stderr(Stdio::from(log));
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x0800_0000);
                }
                let mut child = cmd
                    .spawn()
                    .map_err(|e| format!("cannot run {prog} ({e})"))?;
                let start = Instant::now();
                let status = loop {
                    match child.try_wait() {
                        Ok(Some(s)) => break s,
                        Ok(None) if start.elapsed() < limit => {
                            std::thread::sleep(Duration::from_millis(50))
                        }
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(format!(
                                "{prog} did not finish in {} minutes",
                                limit.as_secs() / 60
                            ));
                        }
                    }
                };
                let mut mine = String::new();
                if let Ok(mut f) = std::fs::File::open(&log_path) {
                    let _ = f.seek(SeekFrom::Start(from));
                    let _ = f.take(1 << 20).read_to_string(&mut mine);
                }
                Ok(Done {
                    ok: status.success(),
                    tail: tail_of(&mine),
                })
            }
            Where::Ssh(host) => {
                let line = argv.iter().map(|a| sq(a)).collect::<Vec<_>>().join(" ");
                let script = format!(
                    "mkdir -p \"{d}\" && cd \"{d}\" && {{ {line} >> woodshed.log 2>&1; r=$?; tail -n 12 woodshed.log; exit $r; }}",
                    d = t.dir
                );
                let args = ssh::args_for(host, &[format!("sh -c {}", sq(&script))])?;
                let said = ssh::run(&args, None, limit)?;
                Ok(Done {
                    ok: said.ok,
                    tail: if said.out.trim().is_empty() {
                        tail_of(&said.err)
                    } else {
                        tail_of(&said.out)
                    },
                })
            }
        }
    }

    fn put(&self, t: &Target, name: &str, text: &str) -> Result<(), String> {
        if !plain(name) {
            return Err(format!("{name} is not a plain file name"));
        }
        match &t.at {
            Where::Local => {
                let dir = Self::local_dir(t);
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
                std::fs::write(dir.join(name), text)
                    .map_err(|e| format!("cannot write {name}: {e}"))
            }
            Where::Ssh(host) => {
                let script = format!("mkdir -p \"{d}\" && cat > \"{d}/{name}\"", d = t.dir);
                let args = ssh::args_for(host, &[format!("sh -c {}", sq(&script))])?;
                let said = ssh::run(&args, Some(text), Duration::from_secs(300))?;
                if said.ok {
                    Ok(())
                } else {
                    Err(format!(
                        "could not put {name} on {host}: {}",
                        tail_of(&said.err)
                    ))
                }
            }
        }
    }

    fn get(&self, t: &Target, name: &str, to: &Path) -> Result<(), String> {
        if !plain(name) {
            return Err(format!("{name} is not a plain file name"));
        }
        match &t.at {
            Where::Local => std::fs::copy(Self::local_dir(t).join(name), to)
                .map(|_| ())
                .map_err(|e| format!("cannot copy {name}: {e}")),
            Where::Ssh(host) => {
                let script = format!("cat \"{d}/{name}\"", d = t.dir);
                let args = ssh::args_for(host, &[format!("sh -c {}", sq(&script))])?;
                let out = std::fs::File::create(to)
                    .map_err(|e| format!("cannot write {}: {e}", to.display()))?;
                let mut cmd = Command::new("ssh");
                cmd.args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(out))
                    .stderr(Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x0800_0000);
                }
                let s = cmd.status().map_err(|e| format!("cannot run ssh ({e})"))?;
                if s.success() {
                    Ok(())
                } else {
                    Err(format!("could not bring {name} from {host}"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_for_a_shell_is_quoted_whatever_is_in_it() {
        assert_eq!(sq("plain"), "'plain'");
        assert_eq!(sq("it's"), "'it'\\''s'");
        assert_eq!(sq("a b; rm -rf /"), "'a b; rm -rf /'");
    }

    #[test]
    fn only_plain_names_leave_or_enter_the_work_folder() {
        for bad in ["", "..", ".", "a/b", "..\\x", "/etc/passwd"] {
            assert!(!plain(bad), "{bad}");
        }
        assert!(plain("adapter.gguf"));
        let t = Target {
            at: Where::Local,
            dir: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        assert!(RealExec.put(&t, "../x", "y").is_err());
        assert!(RealExec.get(&t, "a/b", Path::new("x")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_local_command_runs_in_the_work_folder_and_its_words_go_to_the_log() {
        let dir = std::env::temp_dir().join(format!("tempo-woodshed-exec-{}", std::process::id()));
        let t = Target {
            at: Where::Local,
            dir: dir.to_string_lossy().into_owned(),
        };
        RealExec.put(&t, "hello.txt", "hi").unwrap();
        let ok = RealExec
            .run(
                &t,
                &["sh".into(), "-c".into(), "pwd; ls; echo problem >&2".into()],
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(
            ok.ok && ok.tail.contains("hello.txt") && ok.tail.contains("problem"),
            "{}",
            ok.tail
        );
        let bad = RealExec
            .run(
                &t,
                &["sh".into(), "-c".into(), "echo nope; exit 3".into()],
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(!bad.ok && bad.tail.contains("nope"));
        assert!(
            !bad.tail.contains("hello.txt"),
            "only this command's words come back"
        );
        let slow = RealExec.run(
            &t,
            &["sleep".into(), "5".into()],
            Duration::from_millis(200),
        );
        assert!(slow.unwrap_err().contains("did not finish"));
        assert!(RealExec
            .run(
                &t,
                &["tempo-no-such-program".into()],
                Duration::from_secs(1)
            )
            .is_err());
        RealExec
            .get(&t, "hello.txt", &dir.join("copy.txt"))
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("copy.txt")).unwrap(), "hi");
    }
}
