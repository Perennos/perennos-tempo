//! The person's own ssh, nothing else: the system `ssh` with their keys and agent, `BatchMode=yes`
//! (it never asks for a password, so a missing key is an error and not a prompt), a five second
//! connect limit, and `StrictHostKeyChecking=accept-new` (a first host key is remembered, a
//! changed one refused). Tempo never asks for, types, reads or stores a password or a key.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The arguments before the host: the three options, and nothing the remote side can change.
pub fn base_args() -> Vec<String> {
    [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A host or `user@host` that cannot be read as an option or as a second command.
pub fn target_ok(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 200
        && !t.starts_with('-')
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@._:-".contains(&b))
}

/// The complete argument list for running `command` on `target`.
pub fn args_for(target: &str, command: &[String]) -> Result<Vec<String>, String> {
    if !target_ok(target) {
        return Err(format!("{target:?} is not a host name ssh may be given"));
    }
    let mut a = base_args();
    a.push(target.to_string());
    a.extend(command.iter().cloned());
    Ok(a)
}

pub struct Said {
    pub ok: bool,
    pub out: String,
    pub err: String,
}

/// Run `ssh` with `args`, feeding `stdin` to it, and wait up to `limit`.
pub fn run(args: &[String], stdin: Option<&str>, limit: Duration) -> Result<Said, String> {
    let mut cmd = Command::new("ssh");
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run ssh ({e}); install OpenSSH and try again"))?;
    if let (Some(text), Some(mut si)) = (stdin, child.stdin.take()) {
        let _ = si.write_all(text.as_bytes());
    }
    let (so, se) = (child.stdout.take(), child.stderr.take());
    fn read(r: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            if let Some(r) = r {
                let _ = std::io::Read::read_to_end(&mut std::io::Read::take(r, 1 << 20), &mut b);
            }
            String::from_utf8_lossy(&b).into_owned()
        })
    }
    let (ho, he) = (read(so), read(se));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (out, err) = (ho.join().unwrap_or_default(), he.join().unwrap_or_default());
    match status {
        Some(s) => Ok(Said {
            ok: s.success(),
            out,
            err,
        }),
        None => Err(format!("ssh did not finish in {} seconds", limit.as_secs())),
    }
}

/// The `Host` names in an ssh config (no wildcards), to offer as machine names.
pub fn config_hosts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in text.lines() {
        let l = l.trim();
        let mut w = l.split_whitespace();
        if w.next().is_some_and(|k| k.eq_ignore_ascii_case("host")) {
            for h in w {
                if !h.contains(['*', '?', '!']) && target_ok(h) && !out.iter().any(|o| o == h) {
                    out.push(h.to_string());
                }
            }
        }
    }
    out
}

/// The hosts in `~/.ssh/config`.
pub fn user_hosts() -> Vec<String> {
    crate::home_dir()
        .and_then(|h| std::fs::read_to_string(h.join(".ssh").join("config")).ok())
        .map(|t| config_hosts(&t))
        .unwrap_or_default()
}

/// A script for a remote Unix shell: one `key=value` line per fact. It is sent on stdin to
/// `sh -s`, so the person's login shell does not matter, and it only reads.
pub const CHECK_SCRIPT: &str = r#"PATH="$PATH:$HOME/.local/bin:/usr/local/bin:/opt/homebrew/bin:/usr/local/sbin"
echo "os=$(uname -sm 2>/dev/null)"
echo "cpus=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null)"
if [ -r /proc/meminfo ]; then echo "ram_kb=$(awk '/MemTotal/{print $2}' /proc/meminfo)"; else echo "ram_kb=$(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1024 ))"; fi
if command -v nvidia-smi >/dev/null 2>&1; then echo "gpu=$(nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null | head -1)"; elif command -v system_profiler >/dev/null 2>&1; then echo "gpu=$(system_profiler SPDisplaysDataType 2>/dev/null | awk -F': ' '/Chipset Model/{print $2; exit}')"; else echo "gpu="; fi
echo "disk_kb=$(df -Pk "$HOME" 2>/dev/null | awk 'NR==2{print $4}')"
echo "launch=$(tempo-launch version 2>/dev/null)"
echo "ollama=$(ollama --version 2>/dev/null | tail -1)"
"#;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Check {
    pub os: String,
    pub cpus: u32,
    pub ram_gb: f64,
    pub gpu: String,
    pub disk_gb: f64,
    pub launch: String,
    pub ollama: String,
}

pub fn parse_check(text: &str) -> Check {
    let mut c = Check::default();
    for l in text.lines() {
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k {
            "os" => c.os = v.into(),
            "cpus" => c.cpus = v.parse().unwrap_or(0),
            "ram_kb" => c.ram_gb = v.parse::<f64>().unwrap_or(0.0) / 1024.0 / 1024.0,
            "gpu" => c.gpu = v.into(),
            "disk_kb" => c.disk_gb = v.parse::<f64>().unwrap_or(0.0) / 1024.0 / 1024.0,
            "launch" => c.launch = v.into(),
            "ollama" => c.ollama = v.into(),
            _ => {}
        }
    }
    c
}

impl Check {
    pub fn lines(&self) -> Vec<String> {
        let or = |s: &str, no: &str| {
            if s.is_empty() {
                no.to_string()
            } else {
                s.to_string()
            }
        };
        vec![
            format!("system     {}", or(&self.os, "unknown (not a Unix shell?)")),
            format!("cpu        {} threads", self.cpus),
            format!("memory     {:.1} GB", self.ram_gb),
            format!("graphics   {}", or(&self.gpu, "none found")),
            format!("free disk  {:.1} GB", self.disk_gb),
            format!(
                "tempo      {}",
                or(&self.launch, "tempo-launch is not installed")
            ),
            format!("ollama     {}", or(&self.ollama, "not installed")),
        ]
    }

    pub fn to_j(&self) -> crate::setup::json::J {
        use crate::setup::json::J;
        J::Obj(vec![
            ("os".into(), J::s(&self.os)),
            ("cpus".into(), J::n(f64::from(self.cpus))),
            ("ram_gb".into(), J::n((self.ram_gb * 10.0).round() / 10.0)),
            ("gpu".into(), J::s(&self.gpu)),
            ("disk_gb".into(), J::n((self.disk_gb * 10.0).round() / 10.0)),
            ("launch".into(), J::s(&self.launch)),
            ("ollama".into(), J::s(&self.ollama)),
        ])
    }
}

/// The commands `fleet setup` runs on the other computer, in order, each as text the person reads
/// first. `addr` is the machine's private address; `with_launch` whether tempo-launch is there.
pub struct Step {
    pub why: &'static str,
    /// What is shown (never holds the key).
    pub shown: String,
    /// The script sent to `sh -s`.
    pub script: String,
}

pub fn setup_steps(addr: &str, with_launch: bool) -> Vec<Step> {
    let unit = format!(
        r#"if ! command -v ollama >/dev/null 2>&1; then echo "Ollama is not installed here: install it from https://ollama.com/download, then run this step again"; exit 0; fi
if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
  mkdir -p "$HOME/.config/systemd/user"
  printf '[Unit]\nDescription=Ollama for Tempo (private address only)\n\n[Service]\nEnvironment=OLLAMA_HOST={addr}:11434\nExecStart=%s serve\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n' "$(command -v ollama)" > "$HOME/.config/systemd/user/tempo-ollama.service"
  systemctl --user daemon-reload && systemctl --user enable --now tempo-ollama.service && echo "Ollama now listens on {addr}:11434 (user service tempo-ollama)"
else
  echo "No user service manager here. Start Ollama yourself, listening on the private address:"
  echo "  OLLAMA_HOST={addr}:11434 ollama serve"
fi
"#
    );
    let mut v = vec![Step {
        why: "Ollama listens on this computer's private address only (a user service where there is one)",
        shown: unit.clone(),
        script: unit,
    }];
    let setup = if with_launch {
        "tempo-launch setup --apply --yes".to_string()
    } else {
        "echo \"tempo-launch is not installed there: install Perennos Tempo, then run: tempo-launch setup --apply --yes\"".to_string()
    };
    v.push(Step {
        why: "Tempo sets itself up there (fetches what is missing, asks nothing)",
        shown: setup.clone(),
        script: format!(
            "PATH=\"$PATH:$HOME/.local/bin:/usr/local/bin:/opt/homebrew/bin\"\n{setup}\n"
        ),
    });
    v
}

pub fn exists(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_are_the_three_options_then_the_host_then_the_command() {
        let a = args_for("me@10.0.0.9", &["uname".into(), "-a".into()]).unwrap();
        assert_eq!(
            a,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "me@10.0.0.9",
                "uname",
                "-a"
            ]
        );
        for bad in ["-oProxyCommand=x", "", "a b", "host;rm", "$(x)"] {
            assert!(args_for(bad, &[]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn config_hosts_skip_patterns() {
        let t = "Host *\n  User x\nhost tower nas\n  HostName 10.0.0.2\nHost *.corp !bad\nHost laptop\n\tPort 22\nHost tower\n";
        assert_eq!(config_hosts(t), ["tower", "nas", "laptop"]);
    }

    #[test]
    fn a_check_answer_is_read_into_words() {
        let c = parse_check("os=Linux x86_64\ncpus=16\nram_kb=33554432\ngpu=NVIDIA RTX 4090, 24564 MiB\ndisk_kb=209715200\nlaunch=tempo-launch 2.0.1\nollama=ollama version is 0.5.1\n");
        assert_eq!((c.cpus, c.ram_gb, c.disk_gb), (16, 32.0, 200.0));
        assert!(c.gpu.contains("4090") && c.ollama.contains("0.5.1"));
        let l = c.lines().join("\n");
        assert!(l.contains("32.0 GB") && l.contains("tempo-launch 2.0.1"));
        let none = parse_check("os=\ncpus=\ngpu=\nlaunch=\nollama=\n")
            .lines()
            .join("\n");
        assert!(none.contains("tempo-launch is not installed") && none.contains("none found"));
    }

    #[test]
    fn the_setup_steps_name_the_private_address_and_never_listen_on_all() {
        let s = setup_steps("10.0.0.9", true);
        assert!(s[0].shown.contains("OLLAMA_HOST=10.0.0.9:11434"));
        assert!(!s[0].shown.contains("0.0.0.0"));
        assert!(s[1].shown.contains("setup --apply --yes"));
        assert!(setup_steps("10.0.0.9", false)[1]
            .shown
            .contains("not installed"));
    }
}
