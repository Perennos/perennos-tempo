//! Stage from a room: "@stage golden hour", "@stage re-angle the last clip with a slow orbit", and
//! the same words said to "@tempo" (which is how a spoken phrase arrives). Only the owner, at the
//! machine, can; anyone else is told so. The words become a plan (`tempo-launch stage cue --plan`:
//! the workflow, the Pedals, which machine and about how long), the owner answers y or n in the
//! room, and on y it plays and the result path is posted to the owner. The words are read by
//! `stage_phrase.rs`, the same file Stage itself uses (copied byte for byte). Room never touches
//! ComfyUI or a file: `tempo-launch` plays it, with all its own safety rules.

use crate::bot;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What a play gave back.
#[derive(Debug, Clone, PartialEq)]
pub struct Played {
    /// "Re-angled on the studio box, 41 s. Result: /path/clip.mp4"
    pub text: String,
    pub files: Vec<String>,
}

pub trait StageRunner: Send + Sync {
    /// What the words mean, in a sentence the owner can say y or n to.
    fn plan(&self, phrase: &str) -> Result<String, String>;
    fn play(&self, phrase: &str) -> Result<Played, String>;
}

/// What a message in a room means for Stage.
#[derive(Debug, PartialEq)]
pub enum Hook {
    /// Someone who is not the owner asked: they are told it is the owner's.
    Refuse,
    /// The owner asked, with these words.
    Cue(String),
    /// The owner answered a plan waiting for a y or n.
    Answer(bool),
}

pub fn yes_no(text: &str) -> Option<bool> {
    match text
        .trim()
        .trim_end_matches(['.', '!'])
        .to_ascii_lowercase()
        .as_str()
    {
        "y" | "yes" | "yep" | "go" | "play it" | "do it" => Some(true),
        "n" | "no" | "nope" | "cancel" | "stop" => Some(false),
        _ => None,
    }
}

/// What `text` asks of Stage, if anything. `owner_ok`: the owner's own session at the machine.
/// `team`: the handles of the teammates in the room (one called `stage` keeps its name).
pub fn hook_of(text: &str, team: &[String], owner_ok: bool, pending: bool) -> Option<Hook> {
    if owner_ok && pending {
        if let Some(y) = yes_no(text) {
            return Some(Hook::Answer(y));
        }
    }
    if !bot::mention_ranges_of(text, "stage").is_empty() && !team.iter().any(|h| h == "stage") {
        return Some(if owner_ok {
            Hook::Cue(bot::strip_mention_of(text, "stage"))
        } else {
            Hook::Refuse
        });
    }
    if owner_ok && bot::mentions_tempo(text) {
        let rest = bot::strip_mention(text);
        if crate::stage_phrase::parse(&rest, false).is_some() {
            return Some(Hook::Cue(rest));
        }
    }
    None
}

// ---- the real runner: `tempo-launch stage cue`

/// Where `tempo-launch` is: next to this program, or in a folder `TEMPO_BIN` names (never a
/// search of PATH: a program planted there would run as Tempo).
pub fn find_launcher() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "tempo-launch.exe"
    } else {
        "tempo-launch"
    };
    let mut dirs: Vec<PathBuf> = vec![];
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
    {
        dirs.push(d);
    }
    if let Some(v) = std::env::var_os("TEMPO_BIN").filter(|v| !v.is_empty()) {
        dirs.extend(std::env::split_paths(&v).filter(|d| d.is_absolute()));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

pub struct Launcher(pub Option<PathBuf>);

impl Launcher {
    pub fn real() -> Launcher {
        Launcher(find_launcher())
    }

    fn call(
        &self,
        flags: &[&str],
        phrase: &str,
        limit: Duration,
    ) -> Result<serde_json::Value, String> {
        let bin = self.0.as_ref().ok_or(
            "I can't find tempo-launch next to Room, so I can't reach Stage. Run Room from the same folder as tempo-launch.",
        )?;
        let mut c = Command::new(bin);
        c.args(["stage", "cue", "--json"])
            .args(flags)
            // after the words, so a phrase can never be read as a flag
            .arg("--")
            .arg(phrase)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000);
        }
        let mut child = c
            .spawn()
            .map_err(|e| format!("could not start tempo-launch ({e})"))?;
        let mut out = child.stdout.take().ok_or("no output")?;
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = (&mut out).take(1 << 20).read_to_string(&mut s);
            s
        });
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() > limit => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("Stage took too long to answer".into());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(e.to_string()),
            }
        }
        let text = reader.join().unwrap_or_default();
        text.lines()
            .rev()
            .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .ok_or_else(|| "Stage gave no answer".to_string())
    }
}

impl StageRunner for Launcher {
    fn plan(&self, phrase: &str) -> Result<String, String> {
        let j = self.call(&["--plan"], phrase, Duration::from_secs(30))?;
        let t = j["text"].as_str().unwrap_or("").to_string();
        if j["ok"].as_bool() == Some(true) {
            Ok(t)
        } else {
            Err(t)
        }
    }

    fn play(&self, phrase: &str) -> Result<Played, String> {
        let j = self.call(&["--yes"], phrase, Duration::from_secs(3 * 3600 + 900))?;
        let text = j["text"].as_str().unwrap_or("").to_string();
        if j["ok"].as_bool() != Some(true) {
            return Err(text);
        }
        Ok(Played {
            text,
            files: j["files"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|f| f.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(h: &[&str]) -> Vec<String> {
        h.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn at_stage_from_the_owner_is_a_cue_and_from_anyone_else_a_refusal() {
        let none = team(&[]);
        assert_eq!(
            hook_of("@stage golden hour", &none, true, false),
            Some(Hook::Cue("golden hour".into()))
        );
        assert_eq!(
            hook_of(
                "hey @Stage, re-angle the last clip with a slow orbit",
                &none,
                true,
                false
            ),
            Some(Hook::Cue(
                "hey re-angle the last clip with a slow orbit".into()
            ))
        );
        assert_eq!(
            hook_of("@stage golden hour", &none, false, false),
            Some(Hook::Refuse)
        );
        // an address is not a mention, and a teammate called stage keeps the name
        assert_eq!(hook_of("mail me@stage.example", &none, true, false), None);
        assert_eq!(
            hook_of("@stage golden hour", &team(&["stage"]), true, false),
            None
        );
    }

    #[test]
    fn said_to_tempo_by_the_owner_the_same_words_work_and_a_guest_gets_nothing() {
        let none = team(&[]);
        assert_eq!(
            hook_of("@tempo golden hour", &none, true, false),
            Some(Hook::Cue("golden hour".into()))
        );
        assert_eq!(
            hook_of("@tempo make it noir", &none, true, false),
            Some(Hook::Cue("make it noir".into()))
        );
        // not the owner: it is an ordinary ask, answered as a guest, with no tools
        assert_eq!(hook_of("@tempo golden hour", &none, false, false), None);
        // an ordinary question from the owner is still a question
        assert_eq!(
            hook_of("@tempo what is golden hour?", &none, true, false),
            None
        );
        assert_eq!(
            hook_of("@tempo what's for dinner", &none, true, false),
            None
        );
        assert_eq!(
            hook_of("no mention at all: golden hour", &none, true, false),
            None
        );
    }

    #[test]
    fn y_and_n_only_count_for_the_owner_and_only_while_a_plan_waits() {
        let none = team(&[]);
        assert_eq!(hook_of("y", &none, true, true), Some(Hook::Answer(true)));
        assert_eq!(hook_of("Yes.", &none, true, true), Some(Hook::Answer(true)));
        assert_eq!(hook_of("no", &none, true, true), Some(Hook::Answer(false)));
        assert_eq!(hook_of("y", &none, true, false), None, "nothing waits");
        assert_eq!(
            hook_of("y", &none, false, true),
            None,
            "a guest cannot answer"
        );
        assert_eq!(hook_of("maybe", &none, true, true), None);
    }

    #[test]
    fn a_phrase_is_never_read_as_a_flag_and_a_missing_launcher_says_so() {
        let l = Launcher(None);
        let e = l.plan("golden hour").unwrap_err();
        assert!(e.contains("tempo-launch"), "{e}");
    }

    /// The real wrapper against a stand-in program that answers like `stage cue --json`.
    #[cfg(unix)]
    #[test]
    fn the_launcher_wrapper_passes_the_words_after_a_double_dash_and_reads_one_line_of_json() {
        use std::os::unix::fs::PermissionsExt;
        let d = crate::util::testdir::tmpdir("stage-launcher");
        let log = d.join("args.txt");
        let prog = d.join("tempo-launch");
        std::fs::write(
            &prog,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\ncase \"$*\" in\n*--plan*) echo '{{\"ok\":true,\"text\":\"Plan text.\",\"files\":[]}}';;\n*) echo '{{\"ok\":true,\"text\":\"Done. Result: /w/a.mp4\",\"files\":[\"/w/a.mp4\"]}}';;\nesac\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
        let l = Launcher(Some(prog));
        assert_eq!(l.plan("--yes golden hour").unwrap(), "Plan text.");
        let p = l.play("push in").unwrap();
        assert_eq!(p.files, ["/w/a.mp4"]);
        assert!(p.text.contains("Result: /w/a.mp4"));
        let args = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        assert_eq!(lines[0], "stage cue --json --plan -- --yes golden hour");
        assert_eq!(lines[1], "stage cue --json --yes -- push in");
    }
}
