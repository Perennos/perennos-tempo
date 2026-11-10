//! The person's own ssh, nothing else: the system `ssh` with their keys and agent, `BatchMode=yes`
//! (it never asks for a password, so a missing key is an error and not a prompt), a five second
//! connect limit, and `StrictHostKeyChecking=accept-new` (a first host key is remembered, a
//! changed one refused). Tempo never asks for, types, reads or stores a password or a key.

use std::io::Write;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static TEST_SSH: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) struct TestSsh(
    Option<std::path::PathBuf>,
    std::marker::PhantomData<std::rc::Rc<()>>,
);

#[cfg(test)]
pub(super) fn test_ssh(path: std::path::PathBuf) -> TestSsh {
    TestSsh(
        TEST_SSH.with(|slot| slot.replace(Some(path))),
        std::marker::PhantomData,
    )
}

#[cfg(test)]
impl Drop for TestSsh {
    fn drop(&mut self) {
        TEST_SSH.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

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

/// Arguments for setup after a read-only `ssh -G` resolved the target to a private IP.
/// Unknown and changed host keys stay errors; setup never accepts a key on the fly.
pub fn strict_args_for(
    target: &str,
    destination: &str,
    port: u16,
    command: &[String],
) -> Result<Vec<String>, String> {
    if !target_ok(target)
        || destination.parse::<std::net::IpAddr>().is_err()
        || !crate::allowed_remote_host(destination)
        || port == 0
    {
        return Err("the SSH target, private destination or port is invalid".into());
    }
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "ForwardAgent=no".into(),
        "-o".into(),
        "PermitLocalCommand=no".into(),
        "-o".into(),
        "ProxyCommand=none".into(),
        "-o".into(),
        "ProxyJump=none".into(),
        "-o".into(),
        format!("HostName={destination}"),
        "-p".into(),
        port.to_string(),
        target.into(),
    ];
    args.extend(command.iter().cloned());
    Ok(args)
}

/// Resolve an SSH alias without connecting, then require every DNS answer to be private.
pub fn private_target(target: &str) -> Result<(String, u16), String> {
    if !target_ok(target) {
        return Err("That is not a valid SSH host name or private address.".into());
    }
    let cfg = run(&["-G".into(), target.into()], None, Duration::from_secs(5))
        .map_err(|e| format!("Could not read the SSH destination: {e}"))?;
    if !cfg.ok {
        return Err(format!(
            "Could not read the SSH destination: {}",
            cfg.err.lines().next().unwrap_or("no details")
        ));
    }
    let (mut host, mut port) = (None, None);
    for line in cfg.out.lines() {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("hostname"), Some(value)) => host = Some(value.to_string()),
            (Some("port"), Some(value)) => port = value.parse::<u16>().ok(),
            (Some("proxycommand" | "proxyjump"), Some(value)) if value != "none" => {
                return Err(
                    "This SSH alias uses a proxy. Bootstrap needs a direct private connection."
                        .into(),
                );
            }
            _ => {}
        }
    }
    let host = host.ok_or("SSH did not return a destination host.")?;
    let port = port
        .filter(|value| *value != 0)
        .ok_or("SSH did not return a valid port.")?;
    let ips: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("Could not resolve the SSH destination: {e}"))?
        .map(|addr| addr.ip())
        .collect();
    if ips.is_empty()
        || ips
            .iter()
            .any(|ip| !crate::allowed_remote_host(&ip.to_string()))
    {
        return Err("The SSH destination does not resolve only to private addresses.".into());
    }
    Ok((ips[0].to_string(), port))
}

pub struct Said {
    pub ok: bool,
    pub out: String,
    pub err: String,
}

/// Run `ssh` with `args`, feeding `stdin` to it, and wait up to `limit`.
pub fn run(args: &[String], stdin: Option<&str>, limit: Duration) -> Result<Said, String> {
    run_program("ssh", args, stdin.map(|s| s.as_bytes().to_vec()), limit)
}

/// The binary form used only to carry verified controls files over the authenticated SSH pipe.
pub fn run_bytes(args: &[String], stdin: &[u8], limit: Duration) -> Result<Said, String> {
    run_program("ssh", args, Some(stdin.to_vec()), limit)
}

fn run_program(
    program: &str,
    args: &[String],
    stdin: Option<Vec<u8>>,
    limit: Duration,
) -> Result<Said, String> {
    const OUTPUT_CAP: u64 = 1 << 20;
    const INPUT_CAP: usize = 64 << 20;
    if stdin.as_ref().is_some_and(|b| b.len() > INPUT_CAP) {
        return Err("SSH input exceeded the 64 MiB transfer limit".into());
    }
    let deadline = Instant::now() + limit;
    #[cfg(test)]
    let mut cmd = if program == "ssh" {
        let fake = TEST_SSH
            .with(|slot| slot.borrow().clone())
            .ok_or("test SSH requires a scoped fake executable")?;
        Command::new(fake)
    } else {
        Command::new(program)
    };
    #[cfg(not(test))]
    let mut cmd = Command::new(program);
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {program} ({e}); install OpenSSH and try again"))?;
    #[cfg(unix)]
    let mut owner = Some(ProcessGroup(child.id()));
    #[cfg(windows)]
    let mut owner = Some(match Job::attach(&child) {
        Ok(job) => job,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    });
    let mut writer = if let (Some(bytes), Some(mut si)) = (stdin, child.stdin.take()) {
        // A peer that never reads must not hold the caller past the same absolute deadline.
        Some(std::thread::spawn(move || {
            let _ = si.write_all(&bytes);
        }))
    } else {
        None
    };
    let (so, se) = (child.stdout.take(), child.stderr.take());
    fn read(
        r: Option<impl std::io::Read + Send + 'static>,
    ) -> (mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut b = Vec::with_capacity(8192);
            if let Some(r) = r {
                let _ = std::io::Read::read_to_end(&mut std::io::Read::take(r, OUTPUT_CAP), &mut b);
            }
            let _ = tx.send(String::from_utf8_lossy(&b).into_owned());
        });
        (rx, thread)
    }
    let (ho, h_out) = read(so);
    let (he, h_err) = read(se);
    let mut h_out = Some(h_out);
    let mut h_err = Some(h_err);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                drop(owner.take());
                let _ = child.kill();
                let _ = child.wait();
                if let Some(w) = writer.take() {
                    let _ = w.join();
                }
                if let Some(h) = h_out.take() {
                    let _ = h.join();
                }
                if let Some(h) = h_err.take() {
                    let _ = h.join();
                }
                break None;
            }
        }
    };
    match status {
        Some(s) => {
            let rem = deadline.saturating_duration_since(Instant::now());
            let out = match ho.recv_timeout(rem) {
                Ok(out) => out,
                Err(_) => {
                    drop(owner.take());
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(w) = writer.take() {
                        let _ = w.join();
                    }
                    if let Some(h) = h_out.take() {
                        let _ = h.join();
                    }
                    if let Some(h) = h_err.take() {
                        let _ = h.join();
                    }
                    return Err(format!(
                        "{program} output did not close before its deadline"
                    ));
                }
            };
            let err = match he.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(err) => err,
                Err(_) => {
                    drop(owner.take());
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(w) = writer.take() {
                        let _ = w.join();
                    }
                    if let Some(h) = h_out.take() {
                        let _ = h.join();
                    }
                    if let Some(h) = h_err.take() {
                        let _ = h.join();
                    }
                    return Err(format!(
                        "{program} output did not close before its deadline"
                    ));
                }
            };
            if writer.as_ref().is_some_and(|w| !w.is_finished()) {
                drop(owner.take());
                let _ = child.kill();
                let _ = child.wait();
                if let Some(w) = writer.take() {
                    let _ = w.join();
                }
                if let Some(h) = h_out.take() {
                    let _ = h.join();
                }
                if let Some(h) = h_err.take() {
                    let _ = h.join();
                }
                return Err(format!("{program} input did not close before its deadline"));
            }
            drop(owner.take());
            if let Some(w) = writer.take() {
                let _ = w.join();
            }
            if let Some(h) = h_out.take() {
                let _ = h.join();
            }
            if let Some(h) = h_err.take() {
                let _ = h.join();
            }
            Ok(Said {
                ok: s.success(),
                out,
                err,
            })
        }
        None => Err(format!(
            "{program} did not finish in {} seconds",
            limit.as_secs()
        )),
    }
}

#[cfg(unix)]
struct ProcessGroup(u32);

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        unsafe {
            let _ = kill(-(self.0 as i32), 9);
        }
    }
}

#[cfg(windows)]
struct Job(*mut std::ffi::c_void);

#[cfg(windows)]
impl Job {
    fn attach(child: &std::process::Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        #[repr(C)]
        struct Basic {
            per_process: i64,
            per_job: i64,
            flags: u32,
            min_ws: usize,
            max_ws: usize,
            active: u32,
            affinity: usize,
            priority: u32,
            schedule: u32,
        }
        #[repr(C)]
        struct Io {
            read_ops: u64,
            write_ops: u64,
            other_ops: u64,
            read_bytes: u64,
            write_bytes: u64,
            other_bytes: u64,
        }
        #[repr(C)]
        struct Extended {
            basic: Basic,
            io: Io,
            process_mem: usize,
            job_mem: usize,
            peak_process_mem: usize,
            peak_job_mem: usize,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateJobObjectW(
                attrs: *mut std::ffi::c_void,
                name: *const u16,
            ) -> *mut std::ffi::c_void;
            fn SetInformationJobObject(
                job: *mut std::ffi::c_void,
                class: i32,
                info: *const std::ffi::c_void,
                len: u32,
            ) -> i32;
            fn AssignProcessToJobObject(
                job: *mut std::ffi::c_void,
                process: *mut std::ffi::c_void,
            ) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return Err("Could not create a bounded SSH process job.".into());
        }
        let mut limits: Extended = unsafe { std::mem::zeroed() };
        limits.basic.flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        let set = unsafe {
            SetInformationJobObject(
                handle,
                9,
                (&limits as *const Extended).cast(),
                std::mem::size_of::<Extended>() as u32,
            )
        };
        let assigned = set != 0
            && unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } != 0;
        if !assigned {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err("Could not contain the SSH child in its cleanup job.".into());
        }
        Ok(Self(handle))
    }
}

#[cfg(windows)]
#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_process_that_does_not_read_stdin_is_bounded() {
        let args = ["-c".into(), "sleep 2".into()];
        let bytes = vec![b'x'; 8 << 20];
        let start = Instant::now();
        let result = run_program("sh", &args, Some(bytes), Duration::from_millis(120));
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn a_descendant_holding_output_open_is_bounded() {
        let args = ["-c".into(), "sleep 2 & exit 0".into()];
        let start = Instant::now();
        let result = run_program("sh", &args, None, Duration::from_millis(120));
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn strict_setup_disables_local_commands_and_proxy_overrides() {
        let args = strict_args_for("node", "10.0.0.9", 22, &[]).unwrap();
        assert!(args
            .windows(2)
            .any(|w| w == ["-o", "PermitLocalCommand=no"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ProxyCommand=none"]));
        assert!(args.contains(&"StrictHostKeyChecking=yes".into()));
        assert!(!args.contains(&"StrictHostKeyChecking=accept-new".into()));
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

pub fn setup_steps(_addr: &str, with_launch: bool) -> Vec<Step> {
    let script = if with_launch {
        "PATH=\"$PATH:$HOME/.local/bin:/usr/local/bin:/opt/homebrew/bin\"\nnohup tempo-launch fleet serve --quiet >/dev/null 2>&1 </dev/null &\n".to_string()
    } else {
        "echo 'Bootstrap pending: install Perennos Tempo on this computer, then prepare the fleet link again.'\n".to_string()
    };
    vec![Step {
        why: "start only the fleet messenger; models and engine settings stay as they are",
        shown: script.clone(),
        script,
    }]
}
pub fn exists(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_is_scoped_thread_local_and_unwinds_without_host_ssh() {
        assert!(run(&[], None, Duration::ZERO)
            .err()
            .unwrap()
            .contains("scoped fake executable"));
        let first = std::path::PathBuf::from("first-fake");
        let _first = test_ssh(first.clone());
        assert_eq!(
            TEST_SSH.with(|slot| slot.borrow().clone()),
            Some(first.clone())
        );
        assert!(
            std::thread::spawn(|| TEST_SSH.with(|slot| slot.borrow().is_none()))
                .join()
                .unwrap()
        );
        let unwind = std::panic::catch_unwind(|| {
            let _nested = test_ssh(std::path::PathBuf::from("nested-fake"));
            panic!("fixture panic");
        });
        assert!(unwind.is_err());
        assert_eq!(TEST_SSH.with(|slot| slot.borrow().clone()), Some(first));
    }

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
    fn bootstrap_arguments_keep_host_key_checking_strict() {
        let a = strict_args_for("nas", "10.0.0.9", 22, &["sh".into(), "-s".into()]).unwrap();
        assert!(a.iter().any(|x| x == "StrictHostKeyChecking=yes"));
        assert!(a.iter().any(|x| x == "HostName=10.0.0.9"));
        assert!(!a.iter().any(|x| x.contains("accept-new")));
        assert!(strict_args_for("nas", "8.8.8.8", 22, &[]).is_err());
        assert!(strict_args_for("nas", "bad host", 22, &[]).is_err());
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
    fn setup_steps_start_only_the_fleet_messenger() {
        let s = setup_steps("10.0.0.9", true);
        assert_eq!(s.len(), 1);
        assert!(s[0].shown.contains("fleet serve --quiet"));
        assert!(!s[0].shown.contains("OLLAMA_HOST") && !s[0].shown.contains("setup --apply"));
        assert!(setup_steps("10.0.0.9", false)[0]
            .shown
            .contains("Bootstrap pending"));
    }

    #[test]
    fn a_child_that_refuses_input_cannot_outlive_the_transfer_deadline() {
        let (program, args): (&str, Vec<String>) = if cfg!(windows) {
            (
                "powershell.exe",
                vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    "Start-Sleep -Seconds 10".into(),
                ],
            )
        } else {
            ("sh", vec!["-c".into(), "sleep 10".into()])
        };
        let started = Instant::now();
        let result = run_program(
            program,
            &args,
            Some(vec![0; 4 * 1024 * 1024]),
            Duration::from_millis(300),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_descendant_holding_output_is_cleaned_at_the_same_deadline() {
        let (program, args): (&str, Vec<String>) = if cfg!(windows) {
            (
                "powershell.exe",
                vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    "& $env:ComSpec /c 'start /b ping -n 15 127.0.0.1 >nul'; exit 0".into(),
                ],
            )
        } else {
            ("sh", vec!["-c".into(), "sleep 10 & exit 0".into()])
        };
        let started = Instant::now();
        let result = run_program(program, &args, None, Duration::from_millis(500));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
