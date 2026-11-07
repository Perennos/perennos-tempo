//! Talk to Tempo, on this computer. `POST /agent/voice/transcribe` runs whisper.cpp on what you
//! said, `POST /agent/voice/speak` runs Piper on a reply, `GET /agent/voice` says what is set up.
//! The audio never leaves the machine: both programs run here, offline, and the recording is moved
//! to the trash the moment the text is out (we never delete). Only the length is logged, never
//! the words. `tempo-launch setup --voice` fetches the programs and writes `<home>/voice/voice.json`.
use crate::util;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The most audio bytes one request may carry (two minutes of 16 kHz mono WAV is 3.8 MB).
pub const MAX_AUDIO: usize = 4 << 20;
/// Longest recording we will listen to.
pub const MAX_SECONDS: f64 = 120.0;
/// Longest reply we will read aloud in one go.
pub const MAX_SPEAK_CHARS: usize = 1500;
const SPEAK_LIMIT: Duration = Duration::from_secs(60);

/// What is installed. Cheap to build, so it is looked up on every request: running
/// `tempo-launch setup --voice` takes effect without restarting the agent.
#[derive(Clone, Debug, Default)]
pub struct Voice {
    pub whisper: Option<PathBuf>,
    pub model: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub piper: Option<PathBuf>,
    pub piper_voice: Option<PathBuf>,
    /// Where recordings are written for the moment they are heard.
    pub work: PathBuf,
    /// Where they go right after.
    pub trash: PathBuf,
}

fn on_path(path: &Option<OsString>, names: &[&str]) -> Option<PathBuf> {
    let p = path.as_ref()?;
    let sfx = if cfg!(windows) { ".exe" } else { "" };
    // a relative PATH entry means "the current folder": never run a program from there
    std::env::split_paths(p)
        .filter(|d| d.is_absolute())
        .flat_map(|d| names.iter().map(move |n| d.join(format!("{n}{sfx}"))))
        .find(|f| f.is_file())
}

fn file(p: Option<&str>) -> Option<PathBuf> {
    p.filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|f| f.is_file())
}

impl Voice {
    /// The record in `<home>/voice/voice.json`, then what is on `path`. `model_env` is
    /// `TEMPO_WHISPER_MODEL`: a ggml file you put there yourself.
    pub fn discover(
        home: &Path,
        path: Option<OsString>,
        model_env: Option<PathBuf>,
        work: PathBuf,
        trash: PathBuf,
    ) -> Voice {
        let rec: Value = std::fs::read_to_string(home.join("voice").join("voice.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let r = |k: &str| file(rec.get(k).and_then(Value::as_str));
        Voice {
            whisper: r("whisper").or_else(|| on_path(&path, &["whisper-cli", "whisper-cpp"])),
            model: model_env.filter(|m| m.is_file()).or_else(|| r("model")),
            ffmpeg: on_path(&path, &["ffmpeg"]),
            piper: r("piper").or_else(|| on_path(&path, &["piper"])),
            piper_voice: r("piper_voice"),
            work,
            trash,
        }
    }

    pub fn can_listen(&self) -> bool {
        self.whisper.is_some() && self.model.is_some()
    }

    pub fn can_speak(&self) -> bool {
        self.piper.is_some() && self.piper_voice.is_some()
    }

    /// The English-only models have `.en` in their file name.
    pub fn english_only(&self) -> bool {
        self.model
            .as_ref()
            .and_then(|m| m.file_name())
            .is_some_and(|n| n.to_string_lossy().contains(".en."))
    }

    /// `GET /agent/voice`. No paths: only what the page needs to decide what to show.
    pub fn status(&self) -> Value {
        json!({
            "listen": self.can_listen(),
            "speak": self.can_speak(),
            "convert": self.ffmpeg.is_some(),
            "english_only": self.english_only(),
            "max_seconds": MAX_SECONDS,
            "max_bytes": MAX_AUDIO,
            "setup": if self.can_listen() { Value::Null } else { json!("tempo-launch setup --voice") },
        })
    }
}

/// What a WAV file says about itself.
#[derive(Debug, PartialEq)]
pub struct Wav {
    pub pcm: bool,
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub seconds: f64,
}

/// Read a RIFF/WAVE header (and find the data chunk). None when it is not a WAV.
pub fn wav_info(b: &[u8]) -> Option<Wav> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let u16_at = |i: usize| b.get(i..i + 2).map(|x| u16::from_le_bytes([x[0], x[1]]));
    let u32_at = |i: usize| {
        b.get(i..i + 4)
            .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
    };
    let (mut fmt, mut data) = (None, None);
    let mut i = 12;
    while i + 8 <= b.len() {
        let size = u32_at(i + 4)? as usize;
        match &b[i..i + 4] {
            b"fmt " => {
                fmt = Some((
                    u16_at(i + 8)?,
                    u16_at(i + 10)?,
                    u32_at(i + 12)?,
                    u16_at(i + 22)?,
                ))
            }
            b"data" => {
                // a streamed file may leave the size as 0 or 0xFFFFFFFF: take what is there
                let have = b.len() - (i + 8);
                data = Some(if size == 0 || size > have { have } else { size });
                break;
            }
            _ => {}
        }
        i = i.checked_add(8)?.checked_add(size)?.checked_add(size & 1)?;
    }
    let (tag, channels, rate, bits) = fmt?;
    let data = data?;
    let bytes_per_sec = rate as f64 * channels as f64 * (bits as f64 / 8.0);
    Some(Wav {
        pcm: tag == 1,
        channels,
        rate,
        bits,
        seconds: if bytes_per_sec > 0.0 {
            data as f64 / bytes_per_sec
        } else {
            0.0
        },
    })
}

fn is_whisper_wav(w: &Wav) -> bool {
    w.pcm && w.channels == 1 && w.rate == 16_000 && w.bits == 16
}

/// What the bytes are, by their first few: a WAV, or a container ffmpeg can open.
fn container(b: &[u8]) -> &'static str {
    if b.starts_with(b"RIFF") {
        "wav"
    } else if b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        "webm"
    } else if b.starts_with(b"OggS") {
        "ogg"
    } else if b.len() > 8 && &b[4..8] == b"ftyp" {
        "mp4"
    } else {
        ""
    }
}

/// An error for the person: HTTP status and one plain sentence.
pub type Fail = (u16, String);

fn fail<T>(code: u16, why: &str) -> Result<T, Fail> {
    Err((code, why.to_string()))
}

static SERIAL: AtomicU64 = AtomicU64::new(0);

/// Held while whisper (or Piper) runs, so two people talking at once wait their turn instead of
/// starving the CPU. One turn at a time per working folder, which is one per agent.
struct Turn(PathBuf);
impl Turn {
    fn held() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
        static HELD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
        HELD.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn take(work: &Path) -> Option<Turn> {
        let mut held = Turn::held();
        if held.iter().any(|p| p == work) {
            return None;
        }
        held.push(work.to_path_buf());
        Some(Turn(work.to_path_buf()))
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        let mut held = Turn::held();
        held.retain(|p| *p != self.0);
    }
}

fn stamp() -> String {
    format!(
        "{}-{}",
        util::time_hms(),
        SERIAL.fetch_add(1, Ordering::SeqCst)
    )
}

/// Move a file into `<trash>/<date>/`. Never deletes. If the trash is on another disk and the move
/// fails, the file stays where it is (and nothing is lost).
fn to_trash(v: &Voice, f: &Path) {
    let day = v.trash.join(util::date_ymd());
    if std::fs::create_dir_all(&day).is_err() {
        return;
    }
    let name = f.file_name().map_or("voice".into(), |n| n.to_owned());
    let _ = std::fs::rename(f, day.join(name));
}

/// Run a program to its end, or kill it at `limit`. Returns its output (capped) and whether it
/// finished cleanly. `stdin` is fed first, if given.
fn run_limited(
    prog: &Path,
    args: &[&std::ffi::OsStr],
    stdin: Option<&str>,
    limit: Duration,
) -> Result<String, String> {
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start it: {e}"))?;
    if let (Some(text), Some(mut si)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        let _ = si.write_all(text.as_bytes());
    }
    let mut out = child.stdout.take().ok_or("no output")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out).take(256 * 1024).read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                // not joined: a stray grandchild could hold the pipe open, and we are done waiting
                return Err("took too long".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let buf = reader.join().unwrap_or_default();
    if !status.success() {
        return Err("it stopped with an error".into());
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Whisper's output as plain text: its `[BLANK_AUDIO]`-style markers and sound notes dropped.
pub fn clean(raw: &str) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let mut t = line.trim().to_string();
        // [00:00:00.000 --> 00:00:02.000]   text
        if t.starts_with('[') {
            if let Some((head, rest)) = t.split_once(']') {
                if head.contains("-->") {
                    t = rest.trim().to_string();
                }
            }
        }
        let t = strip_notes(&t);
        if !t.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&t);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove `[ ... ]` and `( ... )` notes such as `[BLANK_AUDIO]`, `[MUSIC]`, `(clears throat)`.
fn strip_notes(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// `POST /agent/voice/transcribe`: the request's bytes and its Content-Type. `lang` is `en`,
/// `auto` or a two-letter code; empty picks `en` for an English-only model, else `auto`.
pub fn transcribe(v: &Voice, ctype: &str, body: &[u8], lang: &str) -> Result<Value, Fail> {
    let (Some(whisper), Some(model)) = (&v.whisper, &v.model) else {
        return Err((
            503,
            "Voice isn't set up yet. Run tempo-launch setup --voice.".into(),
        ));
    };
    if body.is_empty() {
        return fail(400, "No audio came with that. Hold the mic and try again.");
    }
    if body.len() > MAX_AUDIO {
        return fail(
            413,
            "That recording is too long. Keep it under two minutes.",
        );
    }
    let kind = container(body);
    let ct = ctype.to_ascii_lowercase();
    if kind.is_empty() || !(ct.starts_with("audio/") || ct.starts_with("application/octet-stream"))
    {
        return fail(415, "Send audio: a 16 kHz mono WAV is best.");
    }
    let lang = match lang {
        "" if v.english_only() => "en",
        "" => "auto",
        l if l.len() <= 8 && l.bytes().all(|b| b.is_ascii_alphabetic()) => l,
        _ => return fail(400, "lang: two letters, like en, or auto"),
    };
    let lang = if v.english_only() { "en" } else { lang };
    // wav that whisper can read as it is, or something ffmpeg turns into one
    let direct = wav_info(body).filter(is_whisper_wav);
    if direct.is_none() && v.ffmpeg.is_none() {
        return fail(
            415,
            if kind == "wav" {
                "That WAV isn't 16 kHz mono 16-bit, and ffmpeg isn't here to convert it. The chat sends the right kind."
            } else {
                "This agent can't convert WebM or Ogg without ffmpeg. The chat sends 16 kHz WAV; ask it for that."
            },
        );
    }
    if let Some(w) = &direct {
        if w.seconds > MAX_SECONDS {
            return fail(
                413,
                "That recording is too long. Keep it under two minutes.",
            );
        }
    }
    let Some(_turn) = Turn::take(&v.work) else {
        return fail(
            503,
            "Tempo is still listening to the last one. Try again in a moment.",
        );
    };
    if std::fs::create_dir_all(&v.work).is_err() {
        return fail(
            500,
            "Tempo couldn't make its scratch folder for the recording.",
        );
    }
    let id = stamp();
    let raw = v.work.join(format!("voice-{id}.{kind}"));
    if std::fs::write(&raw, body).is_err() {
        return fail(500, "Tempo couldn't save the recording to listen to it.");
    }
    let mut files = vec![raw.clone()];
    let result = (|| {
        let mut wav = raw.clone();
        if direct.is_none() {
            let out = v.work.join(format!("voice-{id}-16k.wav"));
            files.push(out.clone());
            let ff = v
                .ffmpeg
                .as_ref()
                .ok_or((415, "ffmpeg isn't here.".to_string()))?;
            let (i, o) = (raw.as_os_str(), out.as_os_str());
            let args: Vec<&std::ffi::OsStr> =
                ["-nostdin", "-y", "-loglevel", "error", "-t", "125", "-i"]
                    .iter()
                    .map(std::ffi::OsStr::new)
                    .chain([i])
                    .chain(
                        ["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", "-f", "wav"]
                            .iter()
                            .map(std::ffi::OsStr::new),
                    )
                    .chain([o])
                    .collect();
            run_limited(ff, &args, None, Duration::from_secs(30)).map_err(|_| {
                (
                    400,
                    "Tempo couldn't read that audio. Try recording again.".to_string(),
                )
            })?;
            let conv = std::fs::read(&out)
                .ok()
                .and_then(|b| wav_info(&b))
                .filter(is_whisper_wav);
            match conv {
                Some(w) if w.seconds > MAX_SECONDS => {
                    return fail(
                        413,
                        "That recording is too long. Keep it under two minutes.",
                    )
                }
                Some(_) => {}
                None => return fail(400, "Tempo couldn't read that audio. Try recording again."),
            }
            wav = out;
        }
        let secs = direct
            .as_ref()
            .map(|w| w.seconds)
            .or_else(|| {
                std::fs::read(&wav)
                    .ok()
                    .and_then(|b| wav_info(&b))
                    .map(|w| w.seconds)
            })
            .unwrap_or(0.0);
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
        let t = threads.to_string();
        let args: Vec<&std::ffi::OsStr> = [
            std::ffi::OsStr::new("-m"),
            model.as_os_str(),
            std::ffi::OsStr::new("-f"),
            wav.as_os_str(),
            std::ffi::OsStr::new("-nt"),
            std::ffi::OsStr::new("-np"),
            std::ffi::OsStr::new("-l"),
            std::ffi::OsStr::new(lang),
            std::ffi::OsStr::new("-t"),
            std::ffi::OsStr::new(&t),
        ]
        .to_vec();
        let limit = Duration::from_secs_f64((30.0 + secs * 3.0).min(180.0));
        let t0 = Instant::now();
        let raw_text = run_limited(whisper, &args, None, limit).map_err(|e| {
            (
                500,
                format!("Tempo couldn't listen: whisper {e}. Try again, or a shorter one."),
            )
        })?;
        let text = clean(&raw_text);
        // only the length is ever logged
        eprintln!(
            "[voice: {secs:.1} s heard, {} characters]",
            text.chars().count()
        );
        Ok(
            json!({"text": text, "seconds": (secs * 10.0).round() / 10.0, "ms": t0.elapsed().as_millis() as u64}),
        )
    })();
    // the recording is gone from the working folder whatever happened
    for f in &files {
        to_trash(v, f);
    }
    result
}

/// `POST /agent/voice/speak`: text in, a WAV out.
pub fn speak(v: &Voice, text: &str) -> Result<Vec<u8>, Fail> {
    let (Some(piper), Some(voice)) = (&v.piper, &v.piper_voice) else {
        return Err((
            503,
            "Spoken replies aren't set up. Run tempo-launch setup --voice --piper.".into(),
        ));
    };
    let text = text.trim();
    if text.is_empty() {
        return fail(400, "Nothing to say.");
    }
    if text.chars().count() > MAX_SPEAK_CHARS {
        return fail(413, "That's too long to read aloud in one go.");
    }
    let Some(_turn) = Turn::take(&v.work) else {
        return fail(
            503,
            "Tempo is still busy with the last one. Try again in a moment.",
        );
    };
    if std::fs::create_dir_all(&v.work).is_err() {
        return fail(500, "Tempo couldn't make its scratch folder.");
    }
    let out = v.work.join(format!("voice-say-{}.wav", stamp()));
    let args = [
        std::ffi::OsStr::new("--model"),
        voice.as_os_str(),
        std::ffi::OsStr::new("--output_file"),
        out.as_os_str(),
    ];
    // one line: Piper speaks a line at a time
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let res = run_limited(piper, &args, Some(&format!("{one}\n")), SPEAK_LIMIT);
    let wav = std::fs::read(&out).ok();
    to_trash(v, &out);
    match (res, wav) {
        (Ok(_), Some(b)) if wav_info(&b).is_some() => {
            eprintln!("[voice: spoke {} characters]", one.chars().count());
            Ok(b)
        }
        _ => fail(
            500,
            "Tempo couldn't speak that. Piper didn't make any sound.",
        ),
    }
}

// the fake programs are shell scripts, so these rehearsals run on Linux and macOS
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::server::{start, ServeOpts};
    use crate::util::testutil::tmpdir;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};

    /// A 16 kHz mono 16-bit WAV of `n` samples of a quiet tone.
    fn wav(n: usize, rate: u32, channels: u16) -> Vec<u8> {
        let data = n * 2 * channels as usize;
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(((36 + data) as u32).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(channels.to_le_bytes());
        b.extend(rate.to_le_bytes());
        b.extend((rate * 2 * channels as u32).to_le_bytes());
        b.extend((2 * channels).to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend((data as u32).to_le_bytes());
        for i in 0..n * channels as usize {
            b.extend(((i % 50) as i16 * 100).to_le_bytes());
        }
        b
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// A fake whisper-cli on a PATH of its own: it checks the file it is handed exists, then
    /// prints a fixed transcript the way whisper.cpp does.
    fn fake_voice(tag: &str) -> (Voice, PathBuf) {
        let d = tmpdir(tag);
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        script(
            &bin,
            "whisper-cli",
            r#"while [ $# -gt 0 ]; do case "$1" in -f) f="$2"; shift;; -l) l="$2"; shift;; esac; shift; done
[ -f "$f" ] || exit 3
echo "[BLANK_AUDIO]"
echo " Hello Tempo, what is on my list today? lang-$l"
"#,
        );
        script(
            &bin,
            "piper",
            r#"while [ $# -gt 0 ]; do case "$1" in --output_file) o="$2"; shift;; esac; shift; done
cat > /dev/null
cp "$(dirname "$0")/../say.wav" "$o""#,
        );
        std::fs::write(d.join("say.wav"), wav(800, 22_050, 1)).unwrap();
        let model = d.join("ggml-base.en.bin");
        std::fs::write(&model, "model").unwrap();
        let voice_file = d.join("voice.onnx");
        std::fs::write(&voice_file, "onnx").unwrap();
        let v = Voice::discover(
            &d.join("home"),
            Some(bin.clone().into_os_string()),
            Some(model),
            d.join("work"),
            d.join("trash"),
        );
        (
            Voice {
                piper_voice: Some(voice_file),
                ..v
            },
            d,
        )
    }

    #[test]
    fn a_fake_whisper_on_path_is_found_and_its_text_comes_back_clean() {
        let (v, _d) = fake_voice("voice-ok");
        assert!(v.can_listen() && v.ffmpeg.is_none());
        let out = transcribe(&v, "audio/wav", &wav(16_000, 16_000, 1), "").unwrap();
        // the model is English-only, so whatever language was asked, the program is told en
        assert_eq!(
            out["text"],
            "Hello Tempo, what is on my list today? lang-en"
        );
        assert_eq!(out["seconds"], 1.0);
        let t = transcribe(&v, "audio/wav", &wav(16_000, 16_000, 1), "de").unwrap();
        assert!(t["text"].as_str().unwrap().ends_with("lang-en"), "{t}");
    }

    #[test]
    fn the_recording_is_gone_afterwards_and_sits_in_the_trash() {
        let (v, _d) = fake_voice("voice-trash");
        transcribe(&v, "audio/wav", &wav(8_000, 16_000, 1), "en").unwrap();
        let left: Vec<_> = std::fs::read_dir(&v.work).unwrap().flatten().collect();
        assert!(left.is_empty(), "nothing is left in the working folder");
        let day = std::fs::read_dir(&v.trash)
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let moved: Vec<_> = std::fs::read_dir(day).unwrap().flatten().collect();
        assert_eq!(moved.len(), 1);
        assert!(moved[0].file_name().to_string_lossy().starts_with("voice-"));
        // a failed run also lets go of the file
        let broken = Voice {
            whisper: Some(script(&v.work, "w", "exit 1")),
            ..v.clone()
        };
        assert_eq!(
            transcribe(&broken, "audio/wav", &wav(8_000, 16_000, 1), "en")
                .unwrap_err()
                .0,
            500
        );
        let left: Vec<_> = std::fs::read_dir(&v.work)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("voice-"))
            .collect();
        assert!(left.is_empty());
    }

    #[test]
    fn limits_and_kinds_are_enforced() {
        let (v, _d) = fake_voice("voice-limits");
        let big = vec![0u8; MAX_AUDIO + 1];
        assert_eq!(transcribe(&v, "audio/wav", &big, "").unwrap_err().0, 413);
        assert_eq!(transcribe(&v, "audio/wav", &[], "").unwrap_err().0, 400);
        // 130 s of 16 kHz mono is 4.16 MB: past two minutes, and past the size cap too
        let long = wav(16_000 * 125, 16_000, 1);
        assert_eq!(transcribe(&v, "audio/wav", &long, "").unwrap_err().0, 413);
        assert_eq!(
            transcribe(&v, "text/plain", b"hello", "").unwrap_err().0,
            415
        );
        // not 16 kHz and no ffmpeg: asks for the right kind
        let (c, why) = transcribe(&v, "audio/wav", &wav(100, 44_100, 2), "").unwrap_err();
        assert_eq!(c, 415);
        assert!(why.contains("16 kHz"), "{why}");
        let (c, why) =
            transcribe(&v, "audio/webm", &[0x1A, 0x45, 0xDF, 0xA3, 0, 0], "").unwrap_err();
        assert_eq!(c, 415);
        assert!(why.contains("ffmpeg"));
        assert!(transcribe(&v, "audio/wav", &wav(10, 16_000, 1), "x1").is_err());
    }

    #[test]
    fn a_slow_whisper_is_stopped() {
        let (mut v, d) = fake_voice("voice-slow");
        v.whisper = Some(script(&d, "slow", "sleep 30"));
        // the limit is 30 s plus three times the audio, so drive the runner directly
        let t0 = Instant::now();
        let r = run_limited(
            v.whisper.as_ref().unwrap(),
            &[],
            None,
            Duration::from_millis(300),
        );
        assert_eq!(r.unwrap_err(), "took too long");
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn webm_goes_through_ffmpeg_when_it_is_there() {
        let (mut v, d) = fake_voice("voice-ffmpeg");
        // a fake ffmpeg that writes a proper 1 s WAV to its last argument
        let src = d.join("one.wav");
        std::fs::write(&src, wav(16_000, 16_000, 1)).unwrap();
        v.ffmpeg = Some(script(
            &d,
            "ffmpeg",
            &format!("for a; do o=\"$a\"; done; cp {} \"$o\"", src.display()),
        ));
        let webm = [&[0x1A, 0x45, 0xDF, 0xA3][..], &[0u8; 64]].concat();
        let out = transcribe(&v, "audio/webm", &webm, "en").unwrap();
        assert!(out["text"].as_str().unwrap().starts_with("Hello Tempo"));
        assert_eq!(std::fs::read_dir(&v.work).unwrap().count(), 0);
    }

    #[test]
    fn nothing_is_set_up_says_how_and_speak_works_with_piper() {
        let none = Voice::default();
        let (c, why) = transcribe(&none, "audio/wav", b"x", "").unwrap_err();
        assert_eq!(c, 503);
        assert_eq!(
            why,
            "Voice isn't set up yet. Run tempo-launch setup --voice."
        );
        assert_eq!(none.status()["setup"], "tempo-launch setup --voice");
        let (v, _d) = fake_voice("voice-speak");
        let b = speak(&v, "Hello there").unwrap();
        assert!(b.starts_with(b"RIFF"));
        assert_eq!(speak(&v, "  ").unwrap_err().0, 400);
        assert_eq!(
            speak(&v, &"a".repeat(MAX_SPEAK_CHARS + 1)).unwrap_err().0,
            413
        );
        assert_eq!(speak(&none, "hi").unwrap_err().0, 503);
        assert_eq!(std::fs::read_dir(&v.work).unwrap().count(), 0);
    }

    #[test]
    fn wav_header_reading_and_cleaning() {
        let w = wav_info(&wav(32_000, 16_000, 1)).unwrap();
        assert_eq!((w.channels, w.rate, w.bits, w.pcm), (1, 16_000, 16, true));
        assert!((w.seconds - 2.0).abs() < 1e-9);
        assert!(wav_info(b"RIFF....WAVE").is_none());
        assert!(wav_info(b"nope").is_none());
        assert_eq!(clean("[BLANK_AUDIO]\n"), "");
        assert_eq!(
            clean("[00:00:00.000 --> 00:00:02.000]   Hi  there (laughs)\n[MUSIC]"),
            "Hi there"
        );
    }

    // ---- over HTTP: behind the key, loopback only

    struct Srv {
        addr: SocketAddr,
    }

    fn serve(v: Option<Voice>) -> Srv {
        let d = tmpdir("voice-http");
        let mut c = Config::defaults(Some(d.to_string_lossy().into_owned()));
        c.offline_probe = "127.0.0.1:9".into();
        crate::config::ensure_dirs(&c).unwrap();
        let o = ServeOpts {
            port: 0,
            key: Some("test-key-for-voice".into()),
            voice: v,
            ..ServeOpts::default()
        };
        let r = start(c, BTreeMap::new(), o).unwrap();
        Srv { addr: r.addr }
    }

    fn raw(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let head = head.trim_end_matches("\r\n");
        let req = format!(
            "{head}\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n\r\n",
            addr.port(),
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let text = String::from_utf8_lossy(&out).into_owned();
        let code = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let at = out
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(out.len(), |i| i + 4);
        (code, out[at..].to_vec())
    }

    #[test]
    fn the_endpoints_need_the_key_and_work_with_it() {
        let (v, _d) = fake_voice("voice-http-v");
        let srv = serve(Some(v.clone()));
        let key = "X-Tempo-Key: test-key-for-voice\r\n";
        let audio = wav(16_000, 16_000, 1);
        // no key, wrong key
        for k in ["", "X-Tempo-Key: nope\r\n"] {
            let (c, b) = raw(srv.addr, &format!("GET /agent/voice HTTP/1.1\r\n{k}"), b"");
            assert_eq!(c, 401, "{}", String::from_utf8_lossy(&b));
            let h =
                format!("POST /agent/voice/transcribe HTTP/1.1\r\n{k}Content-Type: audio/wav\r\n");
            assert_eq!(raw(srv.addr, &h, &audio).0, 401);
            let h = format!(
                "POST /agent/voice/speak HTTP/1.1\r\n{k}Content-Type: application/json\r\n"
            );
            assert_eq!(raw(srv.addr, &h, br#"{"text":"hi"}"#).0, 401);
        }
        // with the key
        let (c, b) = raw(
            srv.addr,
            &format!("GET /agent/voice HTTP/1.1\r\n{key}"),
            b"",
        );
        assert_eq!(c, 200);
        let st: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(
            (st["listen"].clone(), st["speak"].clone()),
            (json!(true), json!(true))
        );
        assert!(
            !String::from_utf8_lossy(&b).contains("/tmp"),
            "no paths in the status"
        );
        let h =
            format!("POST /agent/voice/transcribe HTTP/1.1\r\n{key}Content-Type: audio/wav\r\n");
        let (c, b) = raw(srv.addr, &h, &audio);
        assert_eq!(c, 200, "{}", String::from_utf8_lossy(&b));
        let t: Value = serde_json::from_slice(&b).unwrap();
        assert!(t["text"].as_str().unwrap().starts_with("Hello Tempo"));
        // a body over the size limit is refused from its Content-Length, before it is read
        let mut sock = TcpStream::connect(srv.addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let head = format!(
            "POST /agent/voice/transcribe HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{key}Content-Type: audio/wav\r\nContent-Length: {}\r\n\r\n",
            srv.addr.port(),
            MAX_AUDIO + 1
        );
        sock.write_all(head.as_bytes()).unwrap();
        let mut reply = String::new();
        let _ = sock.read_to_string(&mut reply);
        assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
        // a page on another origin cannot ask for it, key or not
        let h = format!("POST /agent/voice/transcribe HTTP/1.1\r\n{key}Origin: http://evil.example\r\nContent-Type: audio/wav\r\n");
        assert_eq!(raw(srv.addr, &h, &audio).0, 403);
        // speak returns a WAV
        let h =
            format!("POST /agent/voice/speak HTTP/1.1\r\n{key}Content-Type: application/json\r\n");
        let (c, b) = raw(srv.addr, &h, br#"{"text":"Hello there"}"#);
        assert_eq!(c, 200);
        assert!(b.starts_with(b"RIFF"));
        // nothing is set up: the plain sentence
        let bare = serve(Some(Voice::default()));
        let h =
            format!("POST /agent/voice/transcribe HTTP/1.1\r\n{key}Content-Type: audio/wav\r\n");
        let (c, b) = raw(bare.addr, &h, &audio);
        assert_eq!(c, 503);
        assert!(String::from_utf8_lossy(&b).contains("tempo-launch setup --voice"));
    }
}
