//! The real `World` for a run: this computer's ComfyUI (the agent's `comfy_url`), the Fleet as
//! the messenger tells it, the GPU lease the agent's settings name, and a cheap local check of
//! what came out (ffmpeg, from the Crate, when it is there).

use super::comfy::{Comfy, HttpComfy};
use super::route::{self, Machine};
use super::run::{Check, World};
use crate::bank::detect::{locate, Sys};
use crate::setup::json::{parse, J};
use crate::Ctx;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct RealWorld {
    paths: crate::fleet::store::Paths,
    node: std::sync::Mutex<Option<std::sync::Arc<crate::fleet::msg::Node>>>,
    settings: Option<J>,
    roots: Vec<PathBuf>,
    ffmpeg: Option<PathBuf>,
}

impl RealWorld {
    pub fn new(ctx: &Ctx) -> RealWorld {
        let settings = crate::setup::apply::pinned_agent_settings()
            .unwrap_or_else(|| crate::setup::apply::agent_settings_path(&ctx.agent_data));
        let settings = std::fs::read_to_string(settings)
            .ok()
            .and_then(|t| parse(&t).ok());
        let bank = crate::bank::state::State::load(ctx);
        let roots = crate::bank::mcp::Rules::load(ctx, &bank).roots;
        let tools = crate::bank::catalog::embedded();
        let ffmpeg =
            crate::bank::catalog::find(&tools, "ffmpeg").and_then(|t| locate(ctx, &Sys::real(), t));
        RealWorld {
            paths: crate::fleet::store::Paths::of(ctx),
            node: std::sync::Mutex::new(None),
            settings,
            roots,
            ffmpeg,
        }
    }

    fn setting(&self, k: &str) -> Option<String> {
        self.settings
            .as_ref()
            .and_then(|s| s.str(k))
            .map(String::from)
    }

    pub fn comfy_url(&self) -> String {
        self.setting("comfy_url")
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| crate::parse_remote_url(u).is_ok())
            .unwrap_or_else(|| "http://127.0.0.1:8188".into())
    }

    /// This computer's messenger, opened once. Answers come back to the running `fleet serve`,
    /// which keeps them where this one reads them.
    fn node(&self) -> Result<std::sync::Arc<crate::fleet::msg::Node>, String> {
        let mut g = crate::fleet::msg::lock(&self.node);
        if let Some(n) = g.as_ref() {
            return Ok(n.clone());
        }
        let key = crate::fleet::store::read_key(&self.paths)
            .ok_or("there is no fleet key yet: `tempo-launch fleet init`")?;
        let n = crate::fleet::msg::Node::open(&self.paths, key)?;
        *g = Some(n.clone());
        Ok(n)
    }
}

/// Run a command line with the owner's shell and wait, at most `secs`. Its output and exit code.
fn shell(cmd: &str, secs: u64) -> (String, i32) {
    #[cfg(windows)]
    let (prog, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (prog, flag) = ("sh", "-c");
    let mut c = Command::new(prog);
    c.args([flag, cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = c.spawn() else {
        return ("could not start the command".into(), 127);
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(secs) => {
                let _ = child.kill();
                let _ = child.wait();
                return ("timed out".into(), 124);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return (e.to_string(), 1),
        }
    }
    match child.wait_with_output() {
        Ok(o) => (
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            o.status.code().unwrap_or(1),
        ),
        Err(e) => (e.to_string(), 1),
    }
}

/// Our language models off the graphics card, so ComfyUI has it (what the agent does too).
fn unload_ollama() {
    let Ok((200, body)) =
        crate::conductor::net::request(11434, "GET", "/api/ps", &[], None, Duration::from_secs(3))
    else {
        return;
    };
    let Ok(j) = parse(&body) else { return };
    let mut any = false;
    for m in j.arr("models") {
        if let Some(n) = m.str("name") {
            let b = J::Obj(vec![
                ("model".into(), J::s(n)),
                ("keep_alive".into(), J::n(0.0)),
            ])
            .compact();
            let _ = crate::conductor::net::request(
                11434,
                "POST",
                "/api/generate",
                &[],
                Some(&b),
                Duration::from_secs(10),
            );
            any = true;
        }
    }
    if any {
        std::thread::sleep(Duration::from_secs(3));
    }
}

impl World for RealWorld {
    fn now(&self) -> u64 {
        crate::unix_now()
    }

    fn random(&self) -> i64 {
        let mut b = [0u8; 8];
        let _ = getrandom::getrandom(&mut b);
        i64::from_le_bytes(b).abs()
    }

    fn local(&self) -> Option<Machine> {
        let url = self.comfy_url();
        let s = HttpComfy::new(&url).ok()?.stats().ok()?;
        Some(route::local(&url, s.gpu_gb, s.free_gb))
    }

    fn start_local(&self) -> bool {
        let Some(cmd) = self.setting("comfy_start_cmd") else {
            return false;
        };
        let _ = shell(&cmd, 240);
        for _ in 0..40 {
            if self.local().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        false
    }

    fn fleet(&self, id: &str) -> Vec<Machine> {
        let port = std::env::var("TEMPO_FLEET_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8770u16);
        match crate::conductor::net::request(
            port,
            "GET",
            "/fleet.json",
            &[],
            None,
            Duration::from_secs(2),
        ) {
            Ok((200, body)) => parse(&body)
                .map(|j| route::from_fleet(&j, id))
                .unwrap_or_default(),
            _ => vec![],
        }
    }

    fn comfy(&self, base: &str) -> Result<Box<dyn Comfy>, String> {
        Ok(Box::new(HttpComfy::new(base)?))
    }

    fn lease_begin(&self) -> Result<(), String> {
        if let Some(cmd) = self.setting("lease_status_cmd") {
            let (st, _) = shell(&cmd, 60);
            let free = st
                .find('{')
                .and_then(|i| parse(&st[i..]).ok())
                .map(|v| v.get("lease").is_none_or(|l| *l == J::Null))
                .unwrap_or(false);
            if !free {
                return Err(format!(
                    "The graphics card is on loan to another job (or its status could not be read), and Stage never takes it back. Status: {}",
                    st.chars().take(300).collect::<String>().trim()
                ));
            }
        }
        unload_ollama();
        if let Some(cmd) = self.setting("lease_acquire_cmd") {
            let (out, rc) = shell(&cmd, 120);
            if rc != 0 {
                return Err(format!(
                    "The graphics card lease was refused: {}",
                    out.chars().take(300).collect::<String>().trim()
                ));
            }
        }
        Ok(())
    }

    fn lease_end(&self) {
        if let Some(cmd) = self.setting("comfy_stop_cmd") {
            let _ = shell(&cmd, 60);
        }
        if let Some(cmd) = self.setting("lease_release_cmd") {
            let _ = shell(&cmd, 60);
        }
    }

    fn check(&self, file: &Path, kind: &str) -> Check {
        check_media(file, kind, self.ffmpeg.as_deref())
    }

    fn roots(&self) -> Option<Vec<PathBuf>> {
        Some(self.roots.clone())
    }

    fn send_inputs(
        &self,
        machine: &str,
        files: &[(String, PathBuf)],
    ) -> Result<Vec<(String, crate::fleet::blob::Sent)>, String> {
        crate::fleet::stage::send_inputs(&*self.node()?, machine, files)
    }

    fn remote_take(
        &self,
        machine: &str,
        workflow: &str,
        inputs: &[(String, crate::fleet::stage::Input)],
        pedals: &[(String, String)],
        seed: i64,
    ) -> Result<crate::fleet::stage::Done, String> {
        crate::fleet::stage::call(
            &*self.node()?,
            machine,
            workflow,
            inputs,
            pedals,
            Some(seed),
            Duration::from_secs(3 * 3600 + 600),
        )
    }
}

fn magic_ok(head: &[u8], kind: &str) -> bool {
    match kind {
        "image" => {
            head.starts_with(b"\x89PNG")
                || head.starts_with(&[0xFF, 0xD8])
                || (head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP"))
        }
        "video" => {
            head.get(4..8) == Some(b"ftyp")
                || head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
                || head.starts_with(b"RIFF")
                || head.starts_with(b"GIF8")
        }
        _ => true,
    }
}

/// The cheap local check: is it a real file of the right kind, and for a video, does it have
/// frames and a length, and is it not all black or all frozen.
pub fn check_media(file: &Path, kind: &str, ffmpeg: Option<&Path>) -> Check {
    let Ok(meta) = std::fs::metadata(file) else {
        return Check {
            ok: Some(false),
            note: "the file is not there".into(),
        };
    };
    let mut head = [0u8; 16];
    let n = std::fs::File::open(file)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    if meta.len() < 1024 || !magic_ok(&head[..n], kind) {
        return Check {
            ok: Some(false),
            note: format!("it does not look like a {kind} ({} bytes)", meta.len()),
        };
    }
    if kind != "video" {
        return Check {
            ok: Some(true),
            note: String::new(),
        };
    }
    let Some(ff) = ffmpeg else {
        return Check { ok: None, note: "ffmpeg is not installed, so only the file was checked (`tempo-launch crate add ffmpeg`)".into() };
    };
    // ffprobe lives beside ffmpeg
    let probe = ff.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    let probe_out = Command::new(&probe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames,duration",
            "-of",
            "default=nw=1",
        ])
        .arg(file)
        .stdin(Stdio::null())
        .output();
    let (frames, secs) = match probe_out {
        Ok(o) => parse_probe(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => (None, None),
    };
    if frames.is_none() {
        return Check {
            ok: None,
            note: "ffprobe could not read the clip".into(),
        };
    }
    let det = Command::new(ff)
        .args(["-v", "info", "-i"])
        .arg(file)
        .args([
            "-vf",
            "blackdetect=d=0.4:pix_th=0.10,freezedetect=n=0.003:d=1",
            "-an",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default();
    judge_clip(frames, secs, &det)
}

pub fn parse_probe(text: &str) -> (Option<u64>, Option<f64>) {
    let mut frames = None;
    let mut secs = None;
    for l in text.lines() {
        if let Some(v) = l.strip_prefix("nb_read_frames=") {
            frames = v.trim().parse().ok();
        }
        if let Some(v) = l.strip_prefix("duration=") {
            secs = v.trim().parse().ok();
        }
    }
    (frames, secs)
}

/// Frame count, length, and how much of it is black or frozen, from ffmpeg's detectors.
pub fn judge_clip(frames: Option<u64>, secs: Option<f64>, detectors: &str) -> Check {
    let (f, s) = (frames.unwrap_or(0), secs.unwrap_or(0.0));
    if f < 8 || s < 0.5 {
        return Check {
            ok: Some(false),
            note: format!("only {f} frames over {s:.1} s"),
        };
    }
    let sum = |key: &str| -> f64 {
        detectors
            .lines()
            .filter_map(|l| l.split(key).nth(1))
            .filter_map(|r| {
                r.trim_start_matches([':', ' '])
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
            })
            .sum()
    };
    let black = sum("black_duration");
    let frozen = sum("lavfi.freezedetect.freeze_duration");
    if black >= 0.8 * s {
        return Check {
            ok: Some(false),
            note: format!("black for {black:.1} of {s:.1} s"),
        };
    }
    if frozen >= 0.8 * s {
        return Check {
            ok: Some(false),
            note: format!("frozen for {frozen:.1} of {s:.1} s"),
        };
    }
    Check {
        ok: Some(true),
        note: format!("{f} frames, {s:.1} s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::detect::tests::tmp;

    #[test]
    fn a_clip_with_no_frames_or_all_black_or_frozen_fails_the_check() {
        assert_eq!(judge_clip(Some(81), Some(5.0), "").ok, Some(true));
        assert!(judge_clip(Some(3), Some(0.1), "")
            .note
            .contains("only 3 frames"));
        let black = "[blackdetect @ 0x1] black_start:0 black_end:4.6 black_duration:4.6\n";
        assert!(judge_clip(Some(81), Some(5.0), black)
            .note
            .contains("black for 4.6"));
        let frozen = "[freezedetect @ 0x2] lavfi.freezedetect.freeze_duration: 4.5\n";
        assert_eq!(judge_clip(Some(81), Some(5.0), frozen).ok, Some(false));
        // a short black blink is fine
        assert_eq!(
            judge_clip(Some(81), Some(5.0), "black_duration:0.5").ok,
            Some(true)
        );
        assert_eq!(
            parse_probe("nb_read_frames=81\nduration=5.062500\n"),
            (Some(81), Some(5.0625))
        );
    }

    #[test]
    fn a_picture_is_checked_by_its_first_bytes_and_a_video_without_ffmpeg_is_unknown() {
        let d = tmp("check");
        let png = d.join("a.png");
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.resize(3000, 0);
        std::fs::write(&png, &b).unwrap();
        assert_eq!(check_media(&png, "image", None).ok, Some(true));
        let junk = d.join("b.png");
        std::fs::write(&junk, vec![b'x'; 3000]).unwrap();
        assert_eq!(check_media(&junk, "image", None).ok, Some(false));
        let tiny = d.join("c.png");
        std::fs::write(&tiny, b"\x89PNG").unwrap();
        assert!(check_media(&tiny, "image", None).note.contains("bytes"));
        let mp4 = d.join("a.mp4");
        let mut m = vec![0, 0, 0, 24];
        m.extend_from_slice(b"ftypisom");
        m.resize(4000, 0);
        std::fs::write(&mp4, &m).unwrap();
        let c = check_media(&mp4, "video", None);
        assert_eq!(c.ok, None);
        assert!(c.note.contains("ffmpeg is not installed"));
        assert_eq!(
            check_media(&d.join("gone.mp4"), "video", None).ok,
            Some(false)
        );
    }
}
