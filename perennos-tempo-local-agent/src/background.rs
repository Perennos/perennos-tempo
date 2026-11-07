//! Background commands: `run_command` with `"background": true` starts a process and returns at
//! once; `command_output` reads what it printed, `command_stop` ends it. Only our own children
//! are ever ended, by their handle or PID, never by name. Every one still running is ended when
//! the registry is dropped (the agent exits), so none is left behind.
use crate::tools::shell_command;
use crate::util::tail_chars;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};

/// Background commands that may be alive at once in one conversation.
pub const MAX_ALIVE: usize = 4;
/// The most output kept per command; the oldest bytes are dropped first.
pub const RING_BYTES: usize = 64 * 1024;
/// The most characters of output `command_output` returns (the end of it).
pub const TAIL_CHARS: usize = 6000;
/// Finished commands remembered per conversation (the oldest are forgotten first).
pub const KEEP_ENDED: usize = 16;

/// The last `cap` bytes a command printed.
pub struct Ring {
    cap: usize,
    buf: Vec<u8>,
}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        Ring {
            cap,
            buf: Vec::new(),
        }
    }

    /// Add output; whatever no longer fits is dropped from the front.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.cap {
            let over = self.buf.len() - self.cap;
            self.buf.drain(..over);
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The kept output as text. A character cut in half at the front is left out.
    pub fn text(&self) -> String {
        let start = self
            .buf
            .iter()
            .position(|b| b & 0xC0 != 0x80)
            .unwrap_or(self.buf.len());
        String::from_utf8_lossy(&self.buf[start..]).into_owned()
    }
}

fn lock(m: &Mutex<Ring>) -> MutexGuard<'_, Ring> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where a background command stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Exited,
    Stopped,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Exited => "exited",
            Status::Stopped => "stopped",
        }
    }
}

/// One background process.
struct Job {
    child: Child,
    out: Arc<Mutex<Ring>>,
    /// The threads copying its output into `out`.
    readers: Vec<std::thread::JoinHandle<()>>,
    /// Set once the process has ended (by itself or by `stop`): it was waited for.
    ended: Option<Status>,
    code: Option<i32>,
}

impl Job {
    /// Check whether it has ended. It is waited for (reaped) only once nothing holds its output
    /// open: until then something it started may still be running, so it counts as running, and
    /// while it is not reaped its process id (on Unix also its group's) cannot go to another
    /// process, so `stop` can still end what it left behind. All its output is read by then.
    fn poll(&mut self) -> Status {
        if let Some(s) = self.ended {
            return s;
        }
        if !self.readers.iter().all(|r| r.is_finished()) {
            return Status::Running;
        }
        if let Ok(Some(st)) = self.child.try_wait() {
            self.code = st.code();
            self.ended = Some(Status::Exited);
            return Status::Exited;
        }
        Status::Running
    }

    /// End it if it is still running. Ending one that already ended does nothing.
    fn stop(&mut self) -> Status {
        if self.poll() != Status::Running {
            return self.ended.unwrap_or(Status::Exited);
        }
        kill_tree(&self.child);
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.ended = Some(Status::Stopped);
        Status::Stopped
    }

    fn report(&mut self, id: &str) -> Value {
        let status = self.poll();
        let text = lock(&self.out).text();
        json!({"id": id, "status": status.name(), "exit_code": self.code,
               "output": tail_chars(&text, TAIL_CHARS)})
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.stop();
    }
}

/// End the process and what it started: its own process group on Unix (it leads one), its tree on
/// Windows. Only called for a child we have not yet waited for, so its PID cannot be someone
/// else's. The caller also kills the child itself, in case this finds nothing.
fn kill_tree(child: &Child) {
    let pid = child.id();
    #[cfg(unix)]
    let mut c = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", &format!("kill -KILL -{pid}")]);
        c
    };
    #[cfg(windows)]
    let mut c = {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000);
        c
    };
    let _ = c
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The jobs of one conversation.
#[derive(Default)]
struct ConvJobs {
    /// The number the next one gets (`bg-N`); never reused.
    next: u32,
    jobs: BTreeMap<String, Job>,
}

impl ConvJobs {
    /// Forget the oldest finished commands beyond `KEEP_ENDED`, so a long conversation does not
    /// keep the output of every command it ever ran. Running ones are never forgotten.
    fn forget_old(&mut self) {
        let mut ended: Vec<(u32, String)> = Vec::new();
        for (id, j) in self.jobs.iter_mut() {
            if j.poll() != Status::Running {
                let n = id.trim_start_matches("bg-").parse().unwrap_or(0);
                ended.push((n, id.clone()));
            }
        }
        if ended.len() <= KEEP_ENDED {
            return;
        }
        ended.sort();
        let drop = ended.len() - KEEP_ENDED;
        for (_, id) in ended.into_iter().take(drop) {
            self.jobs.remove(&id);
        }
    }
}

/// Every conversation's background commands. Dropping it ends them all.
#[derive(Default)]
pub struct Background {
    convs: HashMap<String, ConvJobs>,
}

impl Background {
    /// True when the conversation already has `MAX_ALIVE` commands running.
    pub fn full(&mut self, conv: &str) -> bool {
        let Some(c) = self.convs.get_mut(conv) else {
            return false;
        };
        c.jobs
            .values_mut()
            .map(|j| j.poll())
            .filter(|s| *s == Status::Running)
            .count()
            >= MAX_ALIVE
    }

    /// Start `cmd` in `cwd` and return its id (`bg-N`), or why it could not start.
    pub fn start(&mut self, conv: &str, cmd: &str, cwd: &Path) -> Result<String, String> {
        if self.full(conv) {
            return Err(format!(
                "refused: {MAX_ALIVE} background commands are already running in this conversation; stop one with command_stop first"
            ));
        }
        let _ = std::fs::create_dir_all(cwd);
        let mut c = shell_command(cmd);
        c.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0); // its own group, so stopping it reaches what it started
        }
        let mut child = c
            .spawn()
            .map_err(|e| format!("error: cannot start shell: {e}"))?;
        let out = Arc::new(Mutex::new(Ring::new(RING_BYTES)));
        let pipes: [Option<Box<dyn Read + Send>>; 2] = [
            child.stdout.take().map(|p| Box::new(p) as _),
            child.stderr.take().map(|p| Box::new(p) as _),
        ];
        let mut readers = Vec::new();
        for mut pipe in pipes.into_iter().flatten() {
            let out = out.clone();
            readers.push(std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    lock(&out).push(&buf[..n]);
                }
            }));
        }
        let entry = self.convs.entry(conv.to_string()).or_default();
        entry.next += 1;
        let id = format!("bg-{}", entry.next);
        entry.jobs.insert(
            id.clone(),
            Job {
                child,
                out,
                readers,
                ended: None,
                code: None,
            },
        );
        entry.forget_old();
        Ok(id)
    }

    fn job(&mut self, conv: &str, id: &str) -> Result<&mut Job, String> {
        self.convs
            .get_mut(conv)
            .and_then(|c| c.jobs.get_mut(id))
            .ok_or_else(|| {
                format!("error: no background command {id} in this conversation (run_command with background true starts one and gives its id; only the last {KEEP_ENDED} finished ones are kept)")
            })
    }

    /// Status, exit code and the end of the output of one command.
    pub fn output(&mut self, conv: &str, id: &str) -> Result<Value, String> {
        Ok(self.job(conv, id)?.report(id))
    }

    /// End one command (nothing happens if it already ended) and report where it stands.
    pub fn stop(&mut self, conv: &str, id: &str) -> Result<Value, String> {
        let job = self.job(conv, id)?;
        job.stop();
        Ok(job.report(id))
    }

    /// End every command still running, in every conversation.
    pub fn stop_all(&mut self) {
        self.convs.clear(); // each job ends as it is dropped
    }

    /// The process id of a command (for tests).
    #[cfg(test)]
    pub fn pid(&mut self, conv: &str, id: &str) -> Option<u32> {
        self.job(conv, id).ok().map(|j| j.child.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[test]
    fn the_ring_keeps_only_the_last_64_kb() {
        let mut r = Ring::new(RING_BYTES);
        let all: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        for chunk in all.chunks(4096) {
            r.push(chunk);
        }
        assert_eq!(r.len(), RING_BYTES);
        assert_eq!(r.buf, all[all.len() - RING_BYTES..]);
        // small pushes below the cap drop nothing
        let mut s = Ring::new(10);
        s.push(b"abc");
        s.push(b"def");
        assert_eq!(s.text(), "abcdef");
        s.push(b"ghijklm");
        assert_eq!(s.text(), "defghijklm");
    }

    #[test]
    fn a_character_cut_in_half_at_the_front_is_left_out() {
        let mut r = Ring::new(5);
        r.push("ab\u{e9}\u{e9}".as_bytes()); // a, b, then two 2-byte characters: 6 bytes
        assert_eq!(r.len(), 5);
        assert_eq!(r.text(), "b\u{e9}\u{e9}");
        let mut r = Ring::new(3);
        r.push("\u{e9}\u{e9}".as_bytes());
        assert_eq!(r.text(), "\u{e9}");
    }

    #[cfg(unix)]
    fn running(pid: u32) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tempo-bg-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[cfg(unix)]
    #[test]
    fn commands_are_ended_when_the_registry_is_dropped() {
        let d = tmp("drop");
        let mut bg = Background::default();
        let id = bg.start("c", "sleep 300", &d).unwrap();
        let pid = bg.pid("c", &id).unwrap();
        assert!(running(pid));
        drop(bg);
        assert!(!running(pid), "the child outlived the registry");
    }

    #[cfg(unix)]
    fn wait_for_pid(file: &Path) -> u32 {
        let t = Instant::now();
        loop {
            if let Some(p) = std::fs::read_to_string(file)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                return p;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "no pid in {file:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn what_a_command_left_running_counts_as_running_and_is_ended_with_it() {
        // the shell starts a program in the background and exits at once; the program still
        // holds the output open, so the command is not finished and stopping it reaches the program
        let d = tmp("left");
        let mut bg = Background::default();
        let id = bg
            .start("c", "sleep 300 & echo $! > left.pid; echo started", &d)
            .unwrap();
        let gpid = wait_for_pid(&d.join("left.pid"));
        std::thread::sleep(Duration::from_millis(200));
        let r = bg.output("c", &id).unwrap();
        assert_eq!(r["status"], "running", "{r}");
        assert!(r["output"].as_str().unwrap().contains("started"), "{r}");
        assert_eq!(bg.stop("c", &id).unwrap()["status"], "stopped");
        let t = Instant::now();
        while running(gpid) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!running(gpid), "what the command left running kept running");
    }

    #[cfg(unix)]
    #[test]
    fn only_the_last_finished_commands_are_kept() {
        let d = tmp("keep");
        let mut bg = Background::default();
        let mut ids = vec![];
        for i in 0..KEEP_ENDED + 4 {
            let id = bg.start("c", &format!("echo run {i}"), &d).unwrap();
            let t = Instant::now();
            while bg.output("c", &id).unwrap()["status"] != "exited" {
                assert!(t.elapsed() < Duration::from_secs(5), "{id} never ended");
                std::thread::sleep(Duration::from_millis(10));
            }
            ids.push(id);
        }
        // one more start forgets the oldest finished ones
        let last = bg.start("c", "echo last", &d).unwrap();
        let kept = bg.convs["c"].jobs.len();
        assert!(kept <= KEEP_ENDED + 1, "{kept} kept");
        let err = bg.output("c", &ids[0]).unwrap_err();
        assert!(err.contains("only the last"), "{err}");
        assert!(bg.output("c", ids.last().unwrap()).is_ok());
        assert!(bg.output("c", &last).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn what_a_command_started_ends_with_it() {
        let d = tmp("tree");
        let mut bg = Background::default();
        let id = bg
            .start("c", "sleep 300 & echo $! > grandchild.pid; wait", &d)
            .unwrap();
        let t = Instant::now();
        let file = d.join("grandchild.pid");
        let gpid: u32 = loop {
            if let Some(p) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                break p;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "no grandchild pid");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(running(gpid));
        assert_eq!(bg.stop("c", &id).unwrap()["status"], "stopped");
        let t = Instant::now();
        while running(gpid) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!running(gpid), "the grandchild kept running");
    }
}
