//! `tempo-launch crate mcp`: the Crate's tools as an MCP server on stdio (JSON-RPC 2.0, one message
//! per line), for the installed tools the owner has turned on, and only those.
//!
//! Each tool runs the real program with a FIXED argument template: there is no shell and no
//! free-form command string, so what an AI sends can only fill the blanks. Every path must sit
//! inside a folder the owner allowed (the workbench, `crate folders add`, `TEMPO_CRATE_ROOTS`, or
//! the agent's own write folders), a file's size is capped, an output never replaces a file, and
//! a run that takes too long is stopped. A Blender script runs only when its name is in the
//! scripts folder and its SHA-256 is the one the owner approved with `tempo-launch crate review`.

use super::catalog::{embedded, Tool};
use super::detect::{locate, Sys};
use super::state::State;
use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::Ctx;
use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SERVER_NAME: &str = "tempo-crate";
const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;

// ---- the tools

pub struct Spec {
    pub name: &'static str,
    pub title: &'static str,
    /// Catalog ids: the tool is offered when any one of them is installed and turned on.
    pub ids: &'static [&'static str],
    /// Writes nothing: reads a file and answers.
    pub read_only: bool,
    pub description: &'static str,
    pub schema: &'static str,
}

pub const SPECS: &[Spec] = &[
    Spec {
        name: "blender_render",
        title: "Render a Blender scene",
        ids: &["blender"],
        read_only: false,
        description: "Render one frame of a .blend scene to an image with Blender, no window. Writes a new file named <output>_<frame as 4 digits>.<format> in an allowed folder; never replaces a file. Scenes cannot run their own scripts.",
        schema: r#"{"type":"object","properties":{"scene":{"type":"string","description":"path of the .blend file"},"output":{"type":"string","description":"where to write, without frame number, e.g. renders/hero. Optional: next to the scene."},"frame":{"type":"integer","minimum":0,"maximum":100000,"default":1},"format":{"type":"string","enum":["png","jpg","exr"],"default":"png"}},"required":["scene"],"additionalProperties":false}"#,
    },
    Spec {
        name: "blender_script",
        title: "Run a reviewed Blender script",
        ids: &["blender"],
        read_only: false,
        description: "Run a Python script in Blender, no window. The script must be in the Crate's scripts folder and the owner must have read and approved that exact file (`tempo-launch crate review NAME`); a changed file is refused. The script runs with the owner's own rights and can change or delete files, so only approved text runs. Returns the end of Blender's output.",
        schema: r#"{"type":"object","properties":{"script":{"type":"string","description":"file name in the scripts folder, like make_cube.py"},"scene":{"type":"string","description":"optional .blend file to open first"}},"required":["script"],"additionalProperties":false}"#,
    },
    Spec {
        name: "ffmpeg_convert",
        title: "Convert video or audio",
        ids: &["ffmpeg"],
        read_only: false,
        description: "Convert a video or audio file to another format with FFmpeg, using a ready-made preset (small, balanced or best). Writes a new file in an allowed folder; never replaces a file.",
        schema: r#"{"type":"object","properties":{"input":{"type":"string"},"format":{"type":"string","enum":["mp4","mkv","mov","webm","gif","mp3","wav","flac","ogg","m4a","opus"]},"preset":{"type":"string","enum":["small","balanced","best"],"default":"balanced"},"output":{"type":"string","description":"optional; default: next to the input"}},"required":["input","format"],"additionalProperties":false}"#,
    },
    Spec {
        name: "ffmpeg_trim",
        title: "Cut part of a video or audio file",
        ids: &["ffmpeg"],
        read_only: false,
        description: "Keep the part of a media file between start and end (seconds, or HH:MM:SS.mmm). Fast by default (cuts at the nearest keyframe); with exact true it re-encodes. Writes a new file; never replaces one.",
        schema: r#"{"type":"object","properties":{"input":{"type":"string"},"start":{"type":"string","description":"seconds or HH:MM:SS.mmm"},"end":{"type":"string","description":"seconds or HH:MM:SS.mmm"},"exact":{"type":"boolean","default":false},"output":{"type":"string","description":"optional; default: input name plus _trim"}},"required":["input","start","end"],"additionalProperties":false}"#,
    },
    Spec {
        name: "image_resize",
        title: "Resize a picture",
        ids: &["imagemagick"],
        read_only: false,
        description: "Resize a picture to fit a width and/or height with ImageMagick, keeping its shape. Writes a new file; never replaces one.",
        schema: r#"{"type":"object","properties":{"input":{"type":"string"},"width":{"type":"integer","minimum":1,"maximum":16384},"height":{"type":"integer","minimum":1,"maximum":16384},"only_shrink":{"type":"boolean","default":false},"quality":{"type":"integer","minimum":1,"maximum":100},"output":{"type":"string","description":"optional; default: input name plus the size"}},"required":["input"],"additionalProperties":false}"#,
    },
    Spec {
        name: "image_convert",
        title: "Convert a picture to another format",
        ids: &["imagemagick"],
        read_only: false,
        description: "Convert a picture to another format (PNG, JPEG, WebP, GIF, TIFF, BMP) with ImageMagick. Writes a new file; never replaces one.",
        schema: r#"{"type":"object","properties":{"input":{"type":"string"},"format":{"type":"string","enum":["png","jpg","webp","gif","tiff","bmp"]},"quality":{"type":"integer","minimum":1,"maximum":100},"output":{"type":"string"}},"required":["input","format"],"additionalProperties":false}"#,
    },
    Spec {
        name: "svg_export",
        title: "Export an SVG",
        ids: &["inkscape"],
        read_only: false,
        description: "Export an SVG drawing to PNG, PDF, EPS or PS with Inkscape, optionally at a given pixel width. Writes a new file; never replaces one.",
        schema: r#"{"type":"object","properties":{"input":{"type":"string","description":"path of the .svg file"},"format":{"type":"string","enum":["png","pdf","eps","ps"]},"width":{"type":"integer","minimum":1,"maximum":16384},"output":{"type":"string"}},"required":["input","format"],"additionalProperties":false}"#,
    },
    Spec {
        name: "ocr",
        title: "Read the text in a picture",
        ids: &["tesseract"],
        read_only: true,
        description: "Read the text in a picture (PNG, JPEG, TIFF and so on) with Tesseract and return it. Writes nothing.",
        schema: r#"{"type":"object","properties":{"image":{"type":"string"},"language":{"type":"string","description":"Tesseract language code, like eng or deu+eng","default":"eng"}},"required":["image"],"additionalProperties":false}"#,
    },
    Spec {
        name: "doc_convert",
        title: "Convert a document",
        ids: &["pandoc", "libreoffice"],
        read_only: false,
        description: "Convert a document: Markdown, HTML, Word, OpenDocument, EPUB and plain text with Pandoc; office files and anything to PDF with LibreOffice. Writes <name>.<format> next to the input; never replaces a file.",
        schema: r#"{"type":"object","properties":{"input":{"type":"string"},"format":{"type":"string","enum":["pdf","docx","odt","txt","html","md","epub","rtf"]}},"required":["input","format"],"additionalProperties":false}"#,
    },
    Spec {
        name: "transcribe",
        title: "Turn speech into text",
        ids: &["whisper-cpp"],
        read_only: true,
        description: "Write out what is said in an audio file (WAV, MP3, FLAC, OGG) with whisper.cpp, on this computer. Needs a model file in the Crate's models folder. Writes nothing; returns the text.",
        schema: r#"{"type":"object","properties":{"audio":{"type":"string"},"language":{"type":"string","description":"two or three letters like en, or auto","default":"auto"},"model":{"type":"string","description":"model file name in the models folder; default: the first one there"}},"required":["audio"],"additionalProperties":false}"#,
    },
    Spec {
        name: "speak",
        title: "Read text aloud",
        ids: &["piper"],
        read_only: false,
        description: "Turn a short text (up to 5000 characters) into a WAV file with Piper, on this computer. Needs a voice file in the Crate's models folder. Writes a new file; never replaces one.",
        schema: r#"{"type":"object","properties":{"text":{"type":"string","maxLength":5000},"voice":{"type":"string","description":"voice file name (.onnx) in the models folder; default: the first one there"},"output":{"type":"string","description":"optional .wav path; default: speech.wav in the workbench"}},"required":["text"],"additionalProperties":false}"#,
    },
    Spec {
        name: "audio_normalize",
        title: "Even out the loudness of a recording",
        ids: &["sox"],
        read_only: false,
        description: "Bring a recording's loudest point to a set level (default -1 dB) with SoX. Writes a new WAV, FLAC or OGG file; never replaces one.",
        schema: r#"{"type":"object","properties":{"input":{"type":"string"},"peak_db":{"type":"number","minimum":-20,"maximum":0,"default":-1},"output":{"type":"string","description":"optional; default: input name plus _norm"}},"required":["input"],"additionalProperties":false}"#,
    },
    Spec {
        name: "exif_read",
        title: "Read a file's hidden details",
        ids: &["exiftool"],
        read_only: true,
        description: "Read the metadata (camera, date, location, codec) of a photo, video, audio or PDF file with ExifTool and return it as JSON. Writes nothing.",
        schema: r#"{"type":"object","properties":{"file":{"type":"string"}},"required":["file"],"additionalProperties":false}"#,
    },
    Spec {
        name: "text_search",
        title: "Search the text in a folder",
        ids: &["ripgrep"],
        read_only: true,
        description: "Search the files in an allowed folder for a regular expression with ripgrep. Returns matching lines with file and line number (at most 200). Writes nothing.",
        schema: r#"{"type":"object","properties":{"pattern":{"type":"string","maxLength":200},"folder":{"type":"string"},"glob":{"type":"string","description":"optional file name filter like *.md"},"max_results":{"type":"integer","minimum":1,"maximum":200,"default":50}},"required":["pattern","folder"],"additionalProperties":false}"#,
    },
    Spec {
        name: "openscad_export",
        title: "Build a 3D model from an OpenSCAD file",
        ids: &["openscad"],
        read_only: false,
        description: "Turn a .scad file into an STL, 3MF, PNG or SVG with OpenSCAD. Writes a new file; never replaces one.",
        schema: r#"{"type":"object","properties":{"input":{"type":"string"},"format":{"type":"string","enum":["stl","3mf","png","svg"],"default":"stl"},"output":{"type":"string"}},"required":["input"],"additionalProperties":false}"#,
    },
];

pub fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

// ---- what a tool may touch

/// The folders and files a call may use.
pub struct Rules {
    /// Allowed folders, resolved (links followed).
    pub roots: Vec<PathBuf>,
    /// Where a relative path starts; also the default home for output.
    pub work: PathBuf,
    pub scripts: PathBuf,
    pub models: PathBuf,
    /// LibreOffice's own settings folder, so a running office window never blocks a conversion.
    pub profile: PathBuf,
    pub reviewed: Vec<(String, String)>,
}

/// A path as the system names it, without the `\\?\` prefix Windows adds (programs dislike it).
fn plain(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

fn resolve(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok().map(plain)
}

impl Rules {
    pub fn load(ctx: &Ctx, st: &State) -> Rules {
        let base = ctx.home.join("crate");
        let work = base.join("work");
        let scripts = base.join("scripts");
        let models = base.join("models");
        for d in [&work, &scripts, &models] {
            let _ = std::fs::create_dir_all(d);
        }
        let mut raw: Vec<PathBuf> = vec![work.clone()];
        raw.extend(st.folders().into_iter().map(PathBuf::from));
        if let Some(v) = std::env::var_os("TEMPO_CRATE_ROOTS") {
            raw.extend(std::env::split_paths(&v));
        }
        // the folders the agent itself may write in
        let cfg = ctx
            .agent_config_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| parse(&t).ok());
        raw.extend(
            cfg.iter()
                .flat_map(|c| c.strs("write_roots"))
                .map(PathBuf::from),
        );
        let mut roots: Vec<PathBuf> = vec![];
        for r in raw.into_iter().filter(|r| r.is_absolute()) {
            if let Some(c) = resolve(&r).filter(|c| c.is_dir()) {
                // a filesystem root would allow everything
                if c.parent().is_some() && !roots.contains(&c) {
                    roots.push(c);
                }
            }
        }
        let reviewed = match std::fs::read_to_string(super::state::path(ctx))
            .ok()
            .and_then(|t| parse(&t).ok())
            .and_then(|j| j.get("reviewed_scripts").cloned())
        {
            Some(J::Obj(o)) => o
                .into_iter()
                .filter_map(|(k, v)| {
                    if let J::Str(s) = v {
                        Some((k, s))
                    } else {
                        None
                    }
                })
                .collect(),
            _ => vec![],
        };
        Rules {
            roots,
            work,
            scripts,
            models,
            profile: base.join("office-profile"),
            reviewed,
        }
    }

    /// An existing path inside an allowed folder.
    fn inside(&self, raw: &str) -> Result<PathBuf, String> {
        let p = clean(raw)?;
        let p = if p.is_absolute() {
            p
        } else {
            self.work.join(p)
        };
        let c = resolve(&p).ok_or_else(|| format!("{raw} does not exist"))?;
        if self.roots.iter().any(|r| c.starts_with(r)) {
            Ok(c)
        } else {
            Err(format!(
                "{raw} is outside the folders Tempo may use. Allowed: {}. The owner can add one with `tempo-launch crate folders add FOLDER`.",
                self.roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
            ))
        }
    }

    /// An existing file inside an allowed folder, with one of these extensions and at most `max` bytes.
    fn input(&self, raw: &str, exts: &[&str], max: u64) -> Result<PathBuf, String> {
        let p = self.inside(raw)?;
        let md = std::fs::metadata(&p).map_err(|e| format!("{raw}: {e}"))?;
        if !md.is_file() {
            return Err(format!("{raw} is not a file"));
        }
        if !has_ext(&p, exts) {
            return Err(format!("{raw}: this tool reads {} files", exts.join(", ")));
        }
        if md.len() > max {
            return Err(format!(
                "{raw} is {} MB; this tool takes at most {} MB",
                md.len() / MB,
                max / MB
            ));
        }
        Ok(p)
    }

    /// A file to create: its folder is inside an allowed folder, the name is plain, the
    /// extension is one of `exts`, and nothing is there yet.
    fn output(&self, raw: &str, exts: &[&str]) -> Result<PathBuf, String> {
        let p = clean(raw)?;
        let p = if p.is_absolute() {
            p
        } else {
            self.work.join(p)
        };
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{raw} has no file name"))?
            .to_string();
        let parent = p.parent().ok_or_else(|| format!("{raw} has no folder"))?;
        let dir = self.inside(&parent.to_string_lossy())?;
        if !dir.is_dir() {
            return Err(format!("{} is not a folder", parent.display()));
        }
        self.new_file(&dir, &name, exts)
    }

    fn new_file(&self, dir: &Path, name: &str, exts: &[&str]) -> Result<PathBuf, String> {
        if !plain_name(name) {
            return Err(format!(
                "\"{name}\" is not a plain file name (letters, digits, spaces, . _ -, up to 120)"
            ));
        }
        let f = dir.join(name);
        if !has_ext(&f, exts) {
            return Err(format!("{name}: the name must end in {}", exts.join(", ")));
        }
        if std::fs::symlink_metadata(&f).is_ok() {
            return Err(format!(
                "{} already exists. Tempo never replaces a file; pick another name.",
                f.display()
            ));
        }
        Ok(f)
    }

    /// `<input stem><tag>.<ext>` beside the input, unless the caller named an output.
    fn output_or(
        &self,
        raw: Option<&str>,
        input: &Path,
        tag: &str,
        ext: &str,
        exts: &[&str],
    ) -> Result<PathBuf, String> {
        match raw {
            Some(o) => self.output(o, exts),
            None => {
                let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
                self.new_file(
                    input.parent().unwrap_or(&self.work),
                    &format!("{stem}{tag}.{ext}"),
                    exts,
                )
            }
        }
    }
}

/// A path text with nothing odd in it: no control characters, no `..`, no characters that
/// ImageMagick, a shell-less Windows API or a pattern matcher would treat as more than a name.
fn clean(raw: &str) -> Result<PathBuf, String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > 1024 {
        return Err("a path is missing or too long".into());
    }
    if t.chars()
        .any(|c| c.is_control() || "*?\"<>|[]%".contains(c))
    {
        return Err(format!(
            "{raw}: a path may not hold control characters or any of * ? \" < > | [ ] %"
        ));
    }
    if t.starts_with('-') {
        return Err(format!("{raw}: a path may not start with -"));
    }
    let p = PathBuf::from(t);
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("{raw}: .. is not allowed in a path"));
    }
    Ok(p)
}

fn plain_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 120
        && !n.starts_with(['.', '-', ' '])
        && !n.ends_with([' ', '.'])
        && n.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
}

fn has_ext(p: &Path, exts: &[&str]) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

// ---- one call, as a fixed command

/// What a run gives back.
#[derive(Debug, PartialEq)]
pub enum Ret {
    /// The program's output.
    Text(usize),
    /// The last bytes of the program's output (Blender is chatty).
    Tail(usize),
    /// A file the run must have made.
    File(PathBuf),
}

#[derive(Debug)]
pub struct Job {
    /// The program, then its arguments. Never given to a shell.
    pub argv: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    pub ret: Ret,
    /// Exit codes that mean it worked (ripgrep says 1 for "no matches").
    pub ok: &'static [i32],
}

fn job(argv: Vec<String>, mins: u64, ret: Ret) -> Job {
    Job {
        argv,
        stdin: None,
        timeout: Duration::from_secs(mins * 60),
        ret,
        ok: &[0],
    }
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn text<'a>(a: &'a J, k: &str) -> Result<&'a str, String> {
    a.str(k)
        .ok_or_else(|| format!("\"{k}\" is required (text)"))
}

fn int(a: &J, k: &str, lo: i64, hi: i64) -> Result<Option<i64>, String> {
    match a.get(k) {
        None | Some(J::Null) => Ok(None),
        Some(J::Num(n)) => n
            .parse::<f64>()
            .ok()
            .filter(|v| v.fract() == 0.0 && *v >= lo as f64 && *v <= hi as f64)
            .map(|v| Some(v as i64))
            .ok_or_else(|| format!("\"{k}\" must be a whole number from {lo} to {hi}")),
        _ => Err(format!("\"{k}\" must be a number")),
    }
}

fn choice<'a>(
    a: &'a J,
    k: &str,
    allowed: &[&'a str],
    default: Option<&'a str>,
) -> Result<&'a str, String> {
    match (a.str(k), default) {
        (Some(v), _) => allowed
            .iter()
            .find(|x| x.eq_ignore_ascii_case(v))
            .copied()
            .ok_or_else(|| format!("\"{k}\" must be one of {}", allowed.join(", "))),
        (None, Some(d)) => Ok(d),
        (None, None) => Err(format!(
            "\"{k}\" is required: one of {}",
            allowed.join(", ")
        )),
    }
}

const IMAGES: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "tif", "tiff", "bmp", "heic", "avif",
];
const AUDIO: &[&str] = &["wav", "mp3", "flac", "ogg", "m4a", "aac", "opus", "aiff"];
const VIDEO: &[&str] = &[
    "mp4", "mkv", "mov", "webm", "avi", "m4v", "mpg", "mpeg", "ts", "gif",
];

fn media() -> Vec<&'static str> {
    [VIDEO, AUDIO].concat()
}

/// "12", "12.5" or "[HH:]MM:SS[.mmm]" as seconds.
pub fn seconds(t: &str) -> Result<f64, String> {
    let bad = || format!("\"{t}\" is not a time: use seconds (12.5) or HH:MM:SS.mmm");
    let parts: Vec<&str> = t.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 || t.trim().is_empty() {
        return Err(bad());
    }
    let mut total = 0.0;
    for (i, p) in parts.iter().enumerate() {
        let ok = !p.is_empty()
            && p.chars().all(|c| c.is_ascii_digit() || c == '.')
            && p.matches('.').count() <= usize::from(i == parts.len() - 1)
            && p.len() <= 12;
        let v: f64 = ok.then(|| p.parse().ok()).flatten().ok_or_else(bad)?;
        total = total * 60.0 + v;
    }
    if total > 360_000.0 {
        return Err("a time over 100 hours is not accepted".into());
    }
    Ok(total)
}

fn ffmpeg_codec(fmt: &str, preset: &str) -> Vec<String> {
    let pick = |small: &'static str, balanced: &'static str, best: &'static str| match preset {
        "small" => small,
        "best" => best,
        _ => balanced,
    };
    match fmt {
        "mp4" | "mkv" | "mov" => {
            let (speed, crf, ab) = match preset {
                "small" => ("veryfast", "28", "96k"),
                "best" => ("slow", "18", "192k"),
                _ => ("medium", "23", "128k"),
            };
            strs(&[
                "-c:v", "libx264", "-preset", speed, "-crf", crf, "-pix_fmt", "yuv420p", "-c:a",
                "aac", "-b:a", ab,
            ])
        }
        "webm" => strs(&[
            "-c:v",
            "libvpx-vp9",
            "-crf",
            pick("36", "32", "28"),
            "-b:v",
            "0",
            "-row-mt",
            "1",
            "-c:a",
            "libopus",
            "-b:a",
            pick("64k", "96k", "128k"),
        ]),
        "gif" => {
            let vf = pick(
                "fps=10,scale=360:-1:flags=lanczos",
                "fps=12,scale=480:-1:flags=lanczos",
                "fps=15,scale=640:-1:flags=lanczos",
            );
            strs(&["-an", "-vf", vf, "-loop", "0"])
        }
        "mp3" => strs(&["-vn", "-c:a", "libmp3lame", "-q:a", pick("6", "4", "2")]),
        "ogg" => strs(&["-vn", "-c:a", "libvorbis", "-q:a", pick("3", "5", "8")]),
        "m4a" => strs(&["-vn", "-c:a", "aac", "-b:a", pick("96k", "128k", "192k")]),
        "opus" => strs(&["-vn", "-c:a", "libopus", "-b:a", pick("48k", "96k", "160k")]),
        "flac" => strs(&["-vn", "-c:a", "flac"]),
        _ => strs(&["-vn", "-c:a", "pcm_s16le"]), // wav
    }
}

/// The `file://` address of a folder, for LibreOffice's settings option.
fn file_url(p: &Path) -> String {
    let t = p.to_string_lossy().replace('\\', "/");
    let mut u = String::from("file://");
    if !t.starts_with('/') {
        u.push('/');
    }
    for b in t.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~:".contains(&b) {
            u.push(b as char);
        } else {
            u.push_str(&format!("%{b:02X}"));
        }
    }
    u
}

fn first_file(dir: &Path, name: Option<&str>, ext: &str, what: &str) -> Result<PathBuf, String> {
    if let Some(n) = name {
        if !plain_name(n) || !has_ext(Path::new(n), &[ext]) {
            return Err(format!(
                "\"{n}\" is not a {what} file name (.{ext} in the models folder)"
            ));
        }
        let p = dir.join(n);
        return p
            .is_file()
            .then_some(p)
            .ok_or_else(|| format!("{n} is not in the models folder ({})", dir.display()));
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && has_ext(p, &[ext]))
        .collect();
    found.sort();
    found.into_iter().next().ok_or_else(|| {
        format!(
            "There is no {what} in the models folder yet. The owner puts one (.{ext}) in {}.",
            dir.display()
        )
    })
}

/// Check the arguments and build the fixed command. `bin(id)` is the program of a catalog tool.
pub fn plan(
    tool: &str,
    a: &J,
    r: &Rules,
    bin: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<Job, String> {
    spec(tool).ok_or_else(|| format!("unknown tool {tool}"))?;
    let need = |id: &str| {
        bin(id).ok_or_else(|| format!("{id} is not installed, or not turned on for Tempo"))
    };
    match tool {
        "blender_render" => {
            let b = need("blender")?;
            let scene = r.input(text(a, "scene")?, &["blend"], 4 * GB)?;
            let mut fmt = choice(a, "format", &["png", "jpg", "exr"], Some("png"))?;
            let frame = int(a, "frame", 0, 100_000)?.unwrap_or(1);
            let target = match a.str("output") {
                Some(o) => {
                    let p = clean(o)?;
                    // a typed extension picks the format
                    match p
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(str::to_ascii_lowercase)
                        .as_deref()
                    {
                        Some("png") => fmt = "png",
                        Some("jpg" | "jpeg") => fmt = "jpg",
                        Some("exr") => fmt = "exr",
                        _ => {}
                    }
                    p.with_extension("")
                }
                None => scene.with_extension(""),
            };
            let target = if target.is_absolute() {
                target
            } else {
                r.work.join(target)
            };
            let stem = target
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("the output needs a name")?
                .to_string();
            let dir = r.inside(&target.parent().map(s).unwrap_or_default())?;
            // the frame number makes the real name; it must be free
            let made = r.new_file(&dir, &format!("{stem}_{frame:04}.{fmt}"), &[fmt])?;
            let prefix = dir.join(format!("{stem}_####"));
            let blender_fmt = match fmt {
                "jpg" => "JPEG",
                "exr" => "OPEN_EXR",
                _ => "PNG",
            };
            let mut argv = vec![s(&b)];
            argv.extend(strs(&["-noaudio", "--disable-autoexec", "-b"]));
            argv.push(s(&scene));
            argv.extend(strs(&["-o"]));
            argv.push(s(&prefix));
            argv.extend(strs(&["-F", blender_fmt, "-x", "1", "-f"]));
            argv.push(frame.to_string());
            Ok(job(argv, 120, Ret::File(made)))
        }
        "blender_script" => {
            let b = need("blender")?;
            let name = text(a, "script")?;
            let ok = plain_name(name) && has_ext(Path::new(name), &["py"]) && !name.contains(' ');
            if !ok {
                return Err(format!(
                    "\"{name}\" is not a script name: a plain file name ending in .py"
                ));
            }
            let path = r.scripts.join(name);
            let body = std::fs::read(&path).map_err(|_| {
                format!(
                    "{name} is not in the scripts folder ({})",
                    r.scripts.display()
                )
            })?;
            if body.len() > 256 * 1024 {
                return Err(format!("{name} is over 256 KB"));
            }
            let sha = sha256_hex(&body);
            match r.reviewed.iter().find(|(n, _)| n == name) {
                Some((_, want)) if *want == sha => {}
                Some(_) => return Err(format!("{name} has changed since the owner approved it. The owner reads it again with `tempo-launch crate review {name}`.")),
                None => return Err(format!("{name} has not been approved yet. The owner reads it and approves it with `tempo-launch crate review {name}`.")),
            }
            let mut argv = vec![s(&b)];
            argv.extend(strs(&["-noaudio", "--disable-autoexec", "-b"]));
            if let Some(sc) = a.str("scene") {
                argv.push(s(&r.input(sc, &["blend"], 4 * GB)?));
            }
            argv.extend(strs(&["--python-exit-code", "1", "--python"]));
            argv.push(s(&path));
            Ok(job(argv, 120, Ret::Tail(4000)))
        }
        "ffmpeg_convert" => {
            let b = need("ffmpeg")?;
            let input = r.input(text(a, "input")?, &media(), 16 * GB)?;
            let fmt = choice(
                a,
                "format",
                &[
                    "mp4", "mkv", "mov", "webm", "gif", "mp3", "wav", "flac", "ogg", "m4a", "opus",
                ],
                None,
            )?;
            let preset = choice(
                a,
                "preset",
                &["small", "balanced", "best"],
                Some("balanced"),
            )?;
            let out = r.output_or(a.str("output"), &input, "", fmt, &[fmt])?;
            let mut argv = vec![s(&b)];
            argv.extend(strs(&[
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-i",
            ]));
            argv.push(s(&input));
            argv.extend(ffmpeg_codec(fmt, preset));
            argv.push(s(&out));
            Ok(job(argv, 120, Ret::File(out)))
        }
        "ffmpeg_trim" => {
            let b = need("ffmpeg")?;
            let all = media();
            let input = r.input(text(a, "input")?, &all, 16 * GB)?;
            let (from, to) = (seconds(text(a, "start")?)?, seconds(text(a, "end")?)?);
            if to <= from {
                return Err("end must be after start".into());
            }
            let ext = input
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("mp4")
                .to_ascii_lowercase();
            let out = r.output_or(a.str("output"), &input, "_trim", &ext, &all)?;
            let exact = a.bool("exact").unwrap_or(false);
            let mut argv = vec![s(&b)];
            argv.extend(strs(&[
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-ss",
            ]));
            argv.push(format!("{from:.3}"));
            argv.extend(strs(&["-to"]));
            argv.push(format!("{to:.3}"));
            argv.extend(strs(&["-i"]));
            argv.push(s(&input));
            if !exact {
                argv.extend(strs(&["-c", "copy", "-avoid_negative_ts", "make_zero"]));
            }
            argv.push(s(&out));
            Ok(job(argv, 120, Ret::File(out)))
        }
        "image_resize" | "image_convert" => {
            let b = need("imagemagick")?;
            let input = r.input(text(a, "input")?, IMAGES, 256 * MB)?;
            let quality = int(a, "quality", 1, 100)?;
            let mut argv = vec![s(&b)];
            argv.extend(strs(&[
                "-limit", "memory", "1GiB", "-limit", "map", "2GiB", "-limit", "disk", "4GiB",
            ]));
            argv.push(s(&input));
            let out = if tool == "image_resize" {
                let (w, h) = (int(a, "width", 1, 16384)?, int(a, "height", 1, 16384)?);
                if w.is_none() && h.is_none() {
                    return Err("give a width, a height, or both".into());
                }
                let geom = format!(
                    "{}x{}{}",
                    w.map_or(String::new(), |v| v.to_string()),
                    h.map_or(String::new(), |v| v.to_string()),
                    if a.bool("only_shrink").unwrap_or(false) {
                        ">"
                    } else {
                        ""
                    }
                );
                let ext = input
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("png")
                    .to_ascii_lowercase();
                let tag = format!(
                    "_{}x{}",
                    w.map_or(String::new(), |v| v.to_string()),
                    h.map_or(String::new(), |v| v.to_string())
                );
                let out = r.output_or(a.str("output"), &input, &tag, &ext, IMAGES)?;
                argv.extend(strs(&["-auto-orient", "-resize"]));
                argv.push(geom);
                out
            } else {
                let fmt = choice(
                    a,
                    "format",
                    &["png", "jpg", "webp", "gif", "tiff", "bmp"],
                    None,
                )?;
                r.output_or(a.str("output"), &input, "", fmt, &[fmt])?
            };
            if let Some(q) = quality {
                argv.extend(strs(&["-quality"]));
                argv.push(q.to_string());
            }
            argv.push(s(&out));
            Ok(job(argv, 10, Ret::File(out)))
        }
        "svg_export" => {
            let b = need("inkscape")?;
            let input = r.input(text(a, "input")?, &["svg"], 32 * MB)?;
            let fmt = choice(a, "format", &["png", "pdf", "eps", "ps"], None)?;
            let out = r.output_or(a.str("output"), &input, "", fmt, &[fmt])?;
            let mut argv = vec![
                s(&b),
                s(&input),
                format!("--export-type={fmt}"),
                format!("--export-filename={}", s(&out)),
            ];
            if let Some(w) = int(a, "width", 1, 16384)? {
                argv.push(format!("--export-width={w}"));
            }
            Ok(job(argv, 10, Ret::File(out)))
        }
        "ocr" => {
            let b = need("tesseract")?;
            let img = r.input(text(a, "image")?, IMAGES, 128 * MB)?;
            let lang = a.str("language").unwrap_or("eng");
            let lang_ok = lang.len() <= 40
                && lang.split('+').all(|l| {
                    let (code, region) = l.split_once('_').unwrap_or((l, "a"));
                    code.len() == 3
                        && code.chars().all(|c| c.is_ascii_lowercase())
                        && region.chars().all(|c| c.is_ascii_alphabetic())
                        && !region.is_empty()
                });
            if !lang_ok {
                return Err(format!(
                    "\"{lang}\" is not a language code (like eng or deu+eng)"
                ));
            }
            let argv = vec![s(&b), s(&img), "stdout".into(), "-l".into(), lang.into()];
            Ok(job(argv, 5, Ret::Text(200_000)))
        }
        "doc_convert" => {
            const PANDOC_IN: &[&str] = &[
                "md", "markdown", "txt", "html", "htm", "docx", "odt", "rst", "tex", "epub",
            ];
            const PANDOC_OUT: &[&str] = &["md", "html", "docx", "odt", "txt", "epub"];
            const OFFICE_IN: &[&str] = &[
                "doc", "docx", "odt", "rtf", "xls", "xlsx", "ods", "ppt", "pptx", "odp", "txt",
                "html",
            ];
            const OFFICE_OUT: &[&str] = &["pdf", "docx", "odt", "txt", "html", "rtf"];
            let all = [PANDOC_IN, OFFICE_IN].concat();
            let input = r.input(text(a, "input")?, &all, 256 * MB)?;
            let fmt = choice(
                a,
                "format",
                &["pdf", "docx", "odt", "txt", "html", "md", "epub", "rtf"],
                None,
            )?;
            let ext = input
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let out = r.new_file(
                input.parent().unwrap_or(&r.work),
                &format!(
                    "{}.{fmt}",
                    input.file_stem().and_then(|x| x.to_str()).unwrap_or("out")
                ),
                &[fmt],
            )?;
            if let (Some(p), true) = (
                bin("pandoc"),
                PANDOC_IN.contains(&ext.as_str()) && PANDOC_OUT.contains(&fmt),
            ) {
                let mut argv = vec![s(&p), "--sandbox".into()];
                if fmt == "html" {
                    argv.push("-s".into());
                }
                argv.extend(strs(&["-o"]));
                argv.push(s(&out));
                argv.push(s(&input));
                return Ok(job(argv, 10, Ret::File(out)));
            }
            if let (Some(o), true) = (
                bin("libreoffice"),
                OFFICE_IN.contains(&ext.as_str()) && OFFICE_OUT.contains(&fmt),
            ) {
                let argv = vec![
                    s(&o),
                    "--headless".into(),
                    "--norestore".into(),
                    "--nolockcheck".into(),
                    format!("-env:UserInstallation={}", file_url(&r.profile)),
                    "--convert-to".into(),
                    fmt.into(),
                    "--outdir".into(),
                    s(out.parent().unwrap_or(&r.work)),
                    s(&input),
                ];
                return Ok(job(argv, 10, Ret::File(out)));
            }
            Err(format!(
                "No turned-on tool can convert .{ext} to .{fmt}. Pandoc does Markdown, HTML, Word, OpenDocument, EPUB and text (not PDF); LibreOffice does office files and PDF."
            ))
        }
        "transcribe" => {
            let b = need("whisper-cpp")?;
            let audio = r.input(text(a, "audio")?, &["wav", "mp3", "flac", "ogg"], 2 * GB)?;
            let model = first_file(&r.models, a.str("model"), "bin", "whisper model")?;
            let lang = a.str("language").unwrap_or("auto");
            if !(lang == "auto"
                || ((2..=3).contains(&lang.len()) && lang.chars().all(|c| c.is_ascii_lowercase())))
            {
                return Err(format!(
                    "\"{lang}\" is not a language: two or three letters like en, or auto"
                ));
            }
            let mut argv = vec![s(&b), "-m".into(), s(&model), "-f".into(), s(&audio)];
            argv.extend(strs(&["-nt", "-l", lang]));
            Ok(job(argv, 60, Ret::Text(200_000)))
        }
        "speak" => {
            let b = need("piper")?;
            let t = text(a, "text")?;
            if t.trim().is_empty()
                || t.chars().count() > 5000
                || t.chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
            {
                return Err(
                    "\"text\" must be 1 to 5000 characters, with no control characters".into(),
                );
            }
            let voice = first_file(&r.models, a.str("voice"), "onnx", "voice")?;
            let out = match a.str("output") {
                Some(o) => r.output(o, &["wav"])?,
                None => (1..1000)
                    .map(|n| {
                        if n == 1 {
                            "speech.wav".to_string()
                        } else {
                            format!("speech-{n}.wav")
                        }
                    })
                    .find_map(|n| r.new_file(&r.work, &n, &["wav"]).ok())
                    .ok_or("the workbench has too many speech files")?,
            };
            let argv = vec![s(&b), "-m".into(), s(&voice), "-f".into(), s(&out)];
            let mut j = job(argv, 10, Ret::File(out));
            j.stdin = Some(format!("{t}\n").into_bytes());
            Ok(j)
        }
        "audio_normalize" => {
            let b = need("sox")?;
            let input = r.input(
                text(a, "input")?,
                &["wav", "flac", "ogg", "mp3", "aiff"],
                2 * GB,
            )?;
            let db = match a.get("peak_db") {
                None | Some(J::Null) => -1.0,
                Some(J::Num(n)) => n
                    .parse::<f64>()
                    .ok()
                    .filter(|v| (-20.0..=0.0).contains(v))
                    .ok_or("\"peak_db\" must be from -20 to 0")?,
                _ => return Err("\"peak_db\" must be a number".into()),
            };
            let ext = input
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("wav")
                .to_ascii_lowercase();
            let ext = if ["wav", "flac", "ogg"].contains(&ext.as_str()) {
                ext
            } else {
                "wav".into()
            };
            let out = r.output_or(
                a.str("output"),
                &input,
                "_norm",
                &ext,
                &["wav", "flac", "ogg"],
            )?;
            let argv = vec![
                s(&b),
                "-q".into(),
                s(&input),
                s(&out),
                "norm".into(),
                format!("{db:.1}"),
            ];
            Ok(job(argv, 10, Ret::File(out)))
        }
        "exif_read" => {
            let b = need("exiftool")?;
            let all = [
                IMAGES,
                AUDIO,
                VIDEO,
                &["pdf", "heic", "raw", "cr2", "nef", "arw", "dng"],
            ]
            .concat();
            let f = r.input(text(a, "file")?, &all, 4 * GB)?;
            Ok(job(
                vec![s(&b), "-json".into(), "-g1".into(), s(&f)],
                2,
                Ret::Text(100_000),
            ))
        }
        "text_search" => {
            let b = need("ripgrep")?;
            let pat = text(a, "pattern")?;
            if pat.is_empty() || pat.chars().count() > 200 || pat.chars().any(char::is_control) {
                return Err(
                    "\"pattern\" must be 1 to 200 characters, with no control characters".into(),
                );
            }
            let dir = r.inside(text(a, "folder")?)?;
            if !dir.is_dir() {
                return Err("\"folder\" must be a folder".into());
            }
            let n = int(a, "max_results", 1, 200)?.unwrap_or(50);
            let mut argv = vec![s(&b)];
            argv.extend(strs(&[
                "--no-config",
                "--line-number",
                "--no-heading",
                "--color",
                "never",
                "--max-columns",
                "300",
                "--max-count",
            ]));
            argv.push(n.to_string());
            if let Some(g) = a.str("glob") {
                if g.is_empty()
                    || g.len() > 40
                    || g.starts_with('-')
                    || !g
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | '.' | '_' | '-'))
                {
                    return Err("\"glob\" is a file name pattern like *.md".into());
                }
                argv.extend(strs(&["--glob", g]));
            }
            argv.extend(strs(&["-e"]));
            argv.push(pat.to_string());
            argv.extend(strs(&["--"]));
            argv.push(s(&dir));
            let mut j = job(argv, 2, Ret::Text(100_000));
            j.ok = &[0, 1];
            Ok(j)
        }
        "openscad_export" => {
            let b = need("openscad")?;
            let input = r.input(text(a, "input")?, &["scad"], 16 * MB)?;
            let fmt = choice(a, "format", &["stl", "3mf", "png", "svg"], Some("stl"))?;
            let out = r.output_or(a.str("output"), &input, "", fmt, &[fmt])?;
            Ok(job(
                vec![s(&b), "-o".into(), s(&out), s(&input)],
                15,
                Ret::File(out),
            ))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

// ---- running it

fn capture(
    mut src: impl Read + Send + 'static,
    keep: usize,
    tail: bool,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut all = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = src.read(&mut buf) {
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
            // keep reading so the program never blocks, but hold only what is wanted
            if all.len() > keep * 2 {
                if tail {
                    all.drain(..all.len() - keep);
                } else {
                    all.truncate(keep);
                }
            }
        }
        if all.len() > keep {
            if tail {
                all.drain(..all.len() - keep);
            } else {
                all.truncate(keep);
            }
        }
        all
    })
}

fn lossy_tail(b: &[u8], n: usize) -> String {
    let t = String::from_utf8_lossy(b);
    let t = t.trim();
    let start = t.char_indices().rev().nth(n).map_or(0, |(i, _)| i);
    t[start..].to_string()
}

/// Run the command: no shell, a time limit, the output held to a size. Returns what to tell the AI.
pub fn execute(j: &Job, cwd: &Path) -> Result<String, String> {
    let (prog, args) = j.argv.split_first().ok_or("nothing to run")?;
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(if j.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start the program: {e}"))?;
    if let (Some(data), Some(mut si)) = (j.stdin.clone(), child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let keep = match j.ret {
        Ret::Text(n) | Ret::Tail(n) => n,
        Ret::File(_) => 1000,
    };
    let out = capture(
        child.stdout.take().ok_or("no output")?,
        keep,
        matches!(j.ret, Ret::Tail(_)),
    );
    let err = capture(child.stderr.take().ok_or("no output")?, 2000, true);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() > j.timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "It ran longer than {} minutes and was stopped.",
                    j.timeout.as_secs() / 60
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(format!("could not wait for the program: {e}")),
        }
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    if !status.code().is_some_and(|c| j.ok.contains(&c)) {
        let said = lossy_tail(&err, 600);
        return Err(format!(
            "The program stopped with {status}.{}",
            if said.is_empty() {
                String::new()
            } else {
                format!(" It said: {said}")
            }
        ));
    }
    match &j.ret {
        Ret::Text(_) => {
            let t = String::from_utf8_lossy(&out).trim().to_string();
            Ok(if t.is_empty() {
                "No output (nothing found).".into()
            } else {
                t
            })
        }
        Ret::Tail(n) => Ok(lossy_tail(&out, *n)),
        Ret::File(f) => match std::fs::metadata(f) {
            Ok(m) if m.is_file() => Ok(format!(
                "Wrote {} ({} KB).",
                f.display(),
                m.len().div_ceil(1024)
            )),
            _ => Err(format!(
                "The program finished but {} was not made.{}",
                f.display(),
                {
                    let said = lossy_tail(&err, 400);
                    if said.is_empty() {
                        String::new()
                    } else {
                        format!(" It said: {said}")
                    }
                }
            )),
        },
    }
}

// ---- the server

fn o(pairs: Vec<(&str, J)>) -> J {
    J::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub fn tool_json(sp: &Spec) -> J {
    o(vec![
        ("name", J::s(sp.name)),
        ("title", J::s(sp.title)),
        ("description", J::s(sp.description)),
        ("inputSchema", parse(sp.schema).unwrap_or(J::Obj(vec![]))),
        (
            "annotations",
            o(vec![
                ("title", J::s(sp.title)),
                ("readOnlyHint", J::Bool(sp.read_only)),
                // every tool only makes a new file; a script is the exception: it can do anything
                ("destructiveHint", J::Bool(sp.name == "blender_script")),
                ("idempotentHint", J::Bool(false)),
                ("openWorldHint", J::Bool(false)),
            ]),
        ),
    ])
}

pub struct Server<'a> {
    pub ctx: &'a Ctx,
    pub sys: &'a Sys,
}

impl Server<'_> {
    /// Programs of the installed tools the owner turned on.
    fn active(&self) -> Vec<(Tool, PathBuf)> {
        let st = State::load(self.ctx);
        embedded()
            .into_iter()
            .filter(|t| st.enabled(&t.id) && !t.tools.is_empty())
            .filter_map(|t| locate(self.ctx, self.sys, &t).map(|p| (t, p)))
            .collect()
    }

    /// The tools to offer: a tool shows when one of its catalog ids is installed and on.
    pub fn tools(&self) -> Vec<&'static Spec> {
        let on: Vec<String> = self.active().into_iter().map(|(t, _)| t.id).collect();
        SPECS
            .iter()
            .filter(|s| s.ids.iter().any(|i| on.contains(&i.to_string())))
            .collect()
    }

    pub fn call(&self, name: &str, args: &J) -> Result<String, String> {
        let active = self.active();
        let sp = spec(name).ok_or_else(|| format!("unknown tool {name}"))?;
        if !sp
            .ids
            .iter()
            .any(|i| active.iter().any(|(t, _)| t.id == *i))
        {
            return Err(format!(
                "{name} is not available: {} is not installed, or the owner has not turned it on (`tempo-launch crate enable {}`).",
                sp.ids.join(" or "),
                sp.ids[0]
            ));
        }
        let st = State::load(self.ctx);
        let rules = Rules::load(self.ctx, &st);
        let bin = |id: &str| {
            active
                .iter()
                .find(|(t, _)| t.id == id)
                .map(|(_, p)| p.clone())
        };
        let j = plan(name, args, &rules, &bin)?;
        execute(&j, &rules.work)
    }

    pub fn handle(&self, msg: &J) -> Option<J> {
        let id = msg.get("id").cloned()?;
        let reply = |r: J| {
            Some(o(vec![
                ("jsonrpc", J::s("2.0")),
                ("id", id.clone()),
                ("result", r),
            ]))
        };
        let params = msg.get("params").cloned().unwrap_or(J::Obj(vec![]));
        match msg.str("method").unwrap_or("") {
            "initialize" => reply(o(vec![
                ("protocolVersion", J::s(params.str("protocolVersion").unwrap_or("2024-11-05"))),
                ("capabilities", o(vec![("tools", o(vec![("listChanged", J::Bool(false))]))])),
                ("serverInfo", o(vec![("name", J::s(SERVER_NAME)), ("version", J::s(crate::VERSION))])),
                (
                    "instructions",
                    J::s("The Crate: free tools on the owner's computer that the owner turned on for Tempo. Each tool runs one program with fixed arguments, only on files in the folders the owner allowed, and never replaces a file. Use a path inside the workbench or an allowed folder. If a tool is missing from the list, the owner has not turned it on."),
                ),
            ])),
            "ping" => reply(J::Obj(vec![])),
            "tools/list" => reply(o(vec![("tools", J::Arr(self.tools().into_iter().map(tool_json).collect()))])),
            "tools/call" => {
                let args = params.get("arguments").cloned().unwrap_or(J::Obj(vec![]));
                let (t, bad) = match self.call(params.str("name").unwrap_or(""), &args) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                reply(o(vec![
                    ("content", J::Arr(vec![o(vec![("type", J::s("text")), ("text", J::s(&t))])])),
                    ("isError", J::Bool(bad)),
                ]))
            }
            "resources/list" => reply(o(vec![("resources", J::Arr(vec![]))])),
            "prompts/list" => reply(o(vec![("prompts", J::Arr(vec![]))])),
            _ => Some(o(vec![
                ("jsonrpc", J::s("2.0")),
                ("id", id),
                ("error", o(vec![("code", J::n(-32601.0)), ("message", J::s("method not found"))])),
            ])),
        }
    }

    pub fn run(&self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let out = match parse(&line) {
                Ok(m @ J::Obj(_)) => self.handle(&m),
                _ => Some(o(vec![
                    ("jsonrpc", J::s("2.0")),
                    ("id", J::Null),
                    (
                        "error",
                        o(vec![
                            ("code", J::n(-32700.0)),
                            ("message", J::s("parse error")),
                        ]),
                    ),
                ])),
            };
            if let Some(out) = out {
                writeln!(output, "{}", out.compact())?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

/// `tempo-launch crate mcp`: serve on stdin and stdout until the client closes them.
pub fn run_stdio(ctx: &Ctx) -> i32 {
    let sys = Sys::real();
    let s = Server { ctx, sys: &sys };
    match s.run(std::io::stdin().lock(), std::io::stdout().lock()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::catalog::embedded;
    use crate::bank::detect::tests::{ctx_in, tmp};
    #[cfg(unix)]
    use crate::bank::detect::tests::{fake_bin, sys_with};

    fn rules(d: &Path) -> Rules {
        let ctx = ctx_in(&d.join("home"));
        let mut st = State::load(&ctx);
        st.set_folders(&[s(&d.join("allowed"))]);
        std::fs::create_dir_all(d.join("allowed")).unwrap();
        Rules::load(&ctx, &st)
    }

    fn args(t: &str) -> J {
        parse(t).unwrap()
    }

    fn bins(map: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<PathBuf> {
        move |id: &str| {
            map.iter()
                .find(|(k, _)| *k == id)
                .map(|(_, v)| PathBuf::from(v))
        }
    }

    fn file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn every_tool_belongs_to_a_catalog_entry_and_every_catalog_tool_exists() {
        let cat = embedded();
        for sp in SPECS {
            assert!(parse(sp.schema).is_ok(), "{}: schema", sp.name);
            assert!(!sp.ids.is_empty());
            for id in sp.ids {
                let t = cat
                    .iter()
                    .find(|t| t.id == *id)
                    .unwrap_or_else(|| panic!("{id}"));
                assert!(
                    t.tools.iter().any(|x| x == sp.name),
                    "{} is listed under {id}",
                    sp.name
                );
            }
            assert!(
                sp.description.len() > 60,
                "{}: a clear description",
                sp.name
            );
            if !sp.read_only && sp.name != "blender_script" {
                let d = sp.description.to_lowercase();
                assert!(
                    d.contains("never") || d.contains("writes a new"),
                    "{}: says it only makes new files",
                    sp.name
                );
            }
        }
        for t in &cat {
            for x in &t.tools {
                assert!(spec(x).is_some(), "{} names {x}, which is not a tool", t.id);
            }
        }
    }

    #[test]
    fn tools_carry_the_hints_the_agent_needs() {
        for sp in SPECS {
            let j = tool_json(sp);
            let a = j.get("annotations").unwrap();
            assert_eq!(a.bool("readOnlyHint"), Some(sp.read_only));
            // making a new file is not destructive; a script can do anything, and says so
            assert_eq!(
                a.bool("destructiveHint"),
                Some(sp.name == "blender_script"),
                "{}",
                sp.name
            );
        }
        for ro in ["ocr", "transcribe", "exif_read", "text_search"] {
            assert!(spec(ro).unwrap().read_only);
        }
        for rw in [
            "blender_render",
            "blender_script",
            "ffmpeg_convert",
            "image_resize",
            "speak",
        ] {
            assert!(!spec(rw).unwrap().read_only);
        }
    }

    #[test]
    fn paths_outside_the_allowed_folders_are_refused() {
        let d = tmp("roots");
        let r = rules(&d);
        let allowed = d.join("allowed");
        file(&allowed, "a.png", b"x");
        file(&d, "secret.png", b"x");
        let b = bins(&[("imagemagick", "/bin/true")]);
        let ok = plan(
            "image_convert",
            &args(&format!(
                r#"{{"input":"{}","format":"jpg"}}"#,
                allowed.join("a.png").display()
            )),
            &r,
            &b,
        );
        assert!(ok.is_ok(), "{ok:?}");
        let e = plan(
            "image_convert",
            &args(&format!(
                r#"{{"input":"{}","format":"jpg"}}"#,
                d.join("secret.png").display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("outside the folders"), "{e}");
        // dot-dot, even when the end is inside
        let sneaky = format!("{}/../secret.png", allowed.display());
        let e = plan(
            "image_convert",
            &args(&format!(r#"{{"input":"{sneaky}","format":"jpg"}}"#)),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains(".."), "{e}");
        // an output folder outside is refused too
        let e = plan(
            "image_convert",
            &args(&format!(
                r#"{{"input":"{}","format":"jpg","output":"{}/x.jpg"}}"#,
                allowed.join("a.png").display(),
                d.display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("outside"), "{e}");
        // odd characters, a leading dash and a missing file
        for bad in ["a.png;rm", "-rf.png", "a[0].png", "a%d.png", "nope.png"] {
            assert!(
                plan(
                    "image_convert",
                    &args(&format!(r#"{{"input":"{bad}","format":"jpg"}}"#)),
                    &r,
                    &b
                )
                .is_err(),
                "{bad}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_allowed_folder_is_followed_and_refused() {
        let d = tmp("link");
        let r = rules(&d);
        file(&d, "outside.png", b"x");
        std::os::unix::fs::symlink(d.join("outside.png"), d.join("allowed").join("l.png")).unwrap();
        let b = bins(&[("imagemagick", "/bin/true")]);
        let e = plan(
            "image_convert",
            &args(&format!(
                r#"{{"input":"{}/l.png","format":"jpg"}}"#,
                d.join("allowed").display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("outside"), "{e}");
    }

    #[test]
    fn sizes_extensions_and_existing_outputs_are_checked() {
        let d = tmp("limits");
        let r = rules(&d);
        let a = d.join("allowed");
        file(&a, "pic.png", b"x");
        file(&a, "notes.txt", b"x");
        file(&a, "taken.jpg", b"x");
        let b = bins(&[("imagemagick", "/bin/true")]);
        let e = plan(
            "image_convert",
            &args(&format!(
                r#"{{"input":"{}/notes.txt","format":"png"}}"#,
                a.display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("reads"), "{e}");
        // never replaces a file
        let e = plan(
            "image_convert",
            &args(&format!(
                r#"{{"input":"{}/pic.png","format":"jpg","output":"{}/taken.jpg"}}"#,
                a.display(),
                a.display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("already exists"), "{e}");
        // the output's extension must fit the format
        let e = plan(
            "image_convert",
            &args(&format!(
                r#"{{"input":"{}/pic.png","format":"jpg","output":"{}/x.exe"}}"#,
                a.display(),
                a.display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("must end in"), "{e}");
        // a size limit
        let big = a.join("big.png");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(257 * MB).unwrap();
        let e = plan(
            "image_convert",
            &args(&format!(
                r#"{{"input":"{}","format":"jpg"}}"#,
                big.display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("at most 256 MB"), "{e}");
        // numbers are range-checked
        let e = plan(
            "image_resize",
            &args(&format!(
                r#"{{"input":"{}/pic.png","width":99999}}"#,
                a.display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("whole number"), "{e}");
        let e = plan(
            "image_resize",
            &args(&format!(r#"{{"input":"{}/pic.png"}}"#, a.display())),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("width"), "{e}");
    }

    #[test]
    fn the_command_is_a_fixed_argument_list_never_a_shell_line() {
        let d = tmp("argv");
        let r = rules(&d);
        let a = d.join("allowed");
        let pic = file(&a, "my pic.png", b"x");
        let b = bins(&[
            ("imagemagick", "/opt/magick"),
            ("ffmpeg", "/opt/ffmpeg"),
            ("blender", "/opt/blender"),
            ("tesseract", "/opt/tess"),
            ("ripgrep", "/opt/rg"),
        ]);
        let pic = resolve(&pic).unwrap();
        let j = plan(
            "image_resize",
            &args(&format!(
                r#"{{"input":"{}","width":800,"only_shrink":true,"quality":90}}"#,
                pic.display()
            )),
            &r,
            &b,
        )
        .unwrap();
        let dir = resolve(&a).unwrap();
        let out = dir.join("my pic_800x.png");
        assert_eq!(
            j.argv,
            [
                "/opt/magick",
                "-limit",
                "memory",
                "1GiB",
                "-limit",
                "map",
                "2GiB",
                "-limit",
                "disk",
                "4GiB",
                &pic.display().to_string(),
                "-auto-orient",
                "-resize",
                "800x>",
                "-quality",
                "90",
                &out.display().to_string()
            ]
        );
        // a name with spaces is one argument, not two
        assert!(j.argv.iter().any(|x| x.ends_with("my pic.png")));
        // text that looks like a command stays text: it cannot even be a file name
        let e = plan(
            "image_resize",
            &args(&format!(
                r#"{{"input":"{}","width":8,"output":"x.png; rm -rf ~"}}"#,
                pic.display()
            )),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(!e.is_empty());
        // ffmpeg: fixed template with the preset's own flags
        let clip = file(&a, "clip.mov", b"x");
        let j = plan(
            "ffmpeg_convert",
            &args(&format!(
                r#"{{"input":"{}","format":"mp4","preset":"small"}}"#,
                clip.display()
            )),
            &r,
            &b,
        )
        .unwrap();
        let v: Vec<&str> = j.argv.iter().map(String::as_str).collect();
        assert_eq!(
            &v[..8],
            [
                "/opt/ffmpeg",
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-i",
                &resolve(&clip).unwrap().display().to_string()
            ]
        );
        assert!(v.windows(2).any(|w| w == ["-crf", "28"]) && v.contains(&"libx264"));
        assert!(v.last().unwrap().ends_with("clip.mp4"));
        assert!(plan(
            "ffmpeg_convert",
            &args(&format!(
                r#"{{"input":"{}","format":"exe"}}"#,
                clip.display()
            )),
            &r,
            &b
        )
        .is_err());
        // trim
        let j = plan(
            "ffmpeg_trim",
            &args(&format!(
                r#"{{"input":"{}","start":"00:00:01.5","end":"10"}}"#,
                clip.display()
            )),
            &r,
            &b,
        )
        .unwrap();
        let v: Vec<&str> = j.argv.iter().map(String::as_str).collect();
        assert!(
            v.windows(2).any(|w| w == ["-ss", "1.500"])
                && v.windows(2).any(|w| w == ["-to", "10.000"])
        );
        assert!(v.windows(2).any(|w| w == ["-c", "copy"]));
        assert!(plan(
            "ffmpeg_trim",
            &args(&format!(
                r#"{{"input":"{}","start":"5","end":"2"}}"#,
                clip.display()
            )),
            &r,
            &b
        )
        .is_err());
        assert!(plan(
            "ffmpeg_trim",
            &args(&format!(
                r#"{{"input":"{}","start":"1; ls","end":"9"}}"#,
                clip.display()
            )),
            &r,
            &b
        )
        .is_err());
        // blender render: the scene, the frame, no scripts from the scene
        let scene = file(&a, "hero.blend", b"BLENDER");
        let j = plan(
            "blender_render",
            &args(&format!(
                r#"{{"scene":"{}","frame":7,"format":"jpg"}}"#,
                scene.display()
            )),
            &r,
            &b,
        )
        .unwrap();
        let v: Vec<&str> = j.argv.iter().map(String::as_str).collect();
        assert_eq!(v[0], "/opt/blender");
        assert!(v.contains(&"--disable-autoexec") && v.contains(&"-b"));
        assert!(
            v.windows(2).any(|w| w == ["-F", "JPEG"]) && v.windows(2).any(|w| w == ["-f", "7"])
        );
        assert!(v
            .windows(2)
            .any(|w| w[0] == "-o" && w[1].ends_with("hero_####")));
        assert!(matches!(&j.ret, Ret::File(p) if p.ends_with("hero_0007.jpg")));
        // ocr
        let j = plan(
            "ocr",
            &args(&format!(
                r#"{{"image":"{}","language":"deu+eng"}}"#,
                pic.display()
            )),
            &r,
            &b,
        )
        .unwrap();
        assert_eq!(&j.argv[2..], ["stdout", "-l", "deu+eng"]);
        assert!(plan(
            "ocr",
            &args(&format!(
                r#"{{"image":"{}","language":"eng;ls"}}"#,
                pic.display()
            )),
            &r,
            &b
        )
        .is_err());
        // search: the pattern is a value after -e, the folder after --
        let j = plan(
            "text_search",
            &args(&format!(
                r#"{{"pattern":"--files","folder":"{}","glob":"*.md"}}"#,
                a.display()
            )),
            &r,
            &b,
        )
        .unwrap();
        let v: Vec<&str> = j.argv.iter().map(String::as_str).collect();
        assert!(v.windows(2).any(|w| w == ["-e", "--files"]));
        assert_eq!(v[v.len() - 2], "--");
        assert_eq!(j.ok, &[0, 1]);
        assert!(plan(
            "text_search",
            &args(&format!(
                r#"{{"pattern":"x","folder":"{}","glob":"--pre=sh"}}"#,
                a.display()
            )),
            &r,
            &b
        )
        .is_err());
    }

    #[test]
    fn a_tool_that_is_not_installed_cannot_be_planned() {
        let d = tmp("nobin");
        let r = rules(&d);
        let e = plan("ocr", &args(r#"{"image":"x.png"}"#), &r, &bins(&[])).unwrap_err();
        assert!(e.contains("not installed"), "{e}");
    }

    #[test]
    fn a_blender_script_runs_only_when_it_is_in_the_folder_and_approved() {
        let d = tmp("script");
        let ctx = ctx_in(&d.join("home"));
        let mut st = State::load(&ctx);
        let r0 = Rules::load(&ctx, &st);
        let body = b"import bpy\nprint('hi')\n";
        file(&r0.scripts, "cube.py", body);
        let b = bins(&[("blender", "/opt/blender")]);
        // not approved
        let r = Rules::load(&ctx, &st);
        let e = plan("blender_script", &args(r#"{"script":"cube.py"}"#), &r, &b).unwrap_err();
        assert!(
            e.contains("not been approved") && e.contains("crate review cube.py"),
            "{e}"
        );
        // approved
        st.set_reviewed("cube.py", &sha256_hex(body));
        st.save(&ctx).unwrap();
        let r = Rules::load(&ctx, &st);
        let j = plan("blender_script", &args(r#"{"script":"cube.py"}"#), &r, &b).unwrap();
        assert!(j
            .argv
            .windows(2)
            .any(|w| w[0] == "--python" && w[1].ends_with("cube.py")));
        assert!(j.argv.contains(&"--disable-autoexec".to_string()));
        // changed after approval
        file(&r.scripts, "cube.py", b"import os\nos.system('x')\n");
        let e = plan("blender_script", &args(r#"{"script":"cube.py"}"#), &r, &b).unwrap_err();
        assert!(e.contains("changed since"), "{e}");
        // names that try to leave the folder, or are not scripts
        for bad in [
            "../cube.py",
            "/etc/passwd",
            "cube.sh",
            "a b.py",
            "sub/cube.py",
        ] {
            assert!(
                plan(
                    "blender_script",
                    &args(&format!(r#"{{"script":"{bad}"}}"#)),
                    &r,
                    &b
                )
                .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn doc_convert_picks_the_tool_that_can_do_it() {
        let d = tmp("doc");
        let r = rules(&d);
        let a = d.join("allowed");
        let md = file(&a, "note.md", b"# hi");
        let docx = file(&a, "memo.docx", b"x");
        let both = bins(&[("pandoc", "/opt/pandoc"), ("libreoffice", "/opt/soffice")]);
        let j = plan(
            "doc_convert",
            &args(&format!(
                r#"{{"input":"{}","format":"html"}}"#,
                md.display()
            )),
            &r,
            &both,
        )
        .unwrap();
        assert_eq!(&j.argv[..2], ["/opt/pandoc", "--sandbox"]);
        assert!(j.argv.contains(&"-s".to_string()));
        let j = plan(
            "doc_convert",
            &args(&format!(
                r#"{{"input":"{}","format":"pdf"}}"#,
                docx.display()
            )),
            &r,
            &both,
        )
        .unwrap();
        assert_eq!(j.argv[0], "/opt/soffice");
        assert!(
            j.argv.contains(&"--headless".to_string())
                && j.argv
                    .iter()
                    .any(|x| x.starts_with("-env:UserInstallation=file://"))
        );
        // only pandoc on: PDF is not possible, and it says so
        let only = bins(&[("pandoc", "/opt/pandoc")]);
        let e = plan(
            "doc_convert",
            &args(&format!(r#"{{"input":"{}","format":"pdf"}}"#, md.display())),
            &r,
            &only,
        )
        .unwrap_err();
        assert!(e.contains("No turned-on tool"), "{e}");
    }

    #[test]
    fn speech_tools_need_their_model_and_say_where_to_put_it() {
        let d = tmp("models");
        let r = rules(&d);
        let a = d.join("allowed");
        let wav = file(&a, "talk.wav", b"x");
        let b = bins(&[("whisper-cpp", "/opt/whisper"), ("piper", "/opt/piper")]);
        let e = plan(
            "transcribe",
            &args(&format!(r#"{{"audio":"{}"}}"#, wav.display())),
            &r,
            &b,
        )
        .unwrap_err();
        assert!(e.contains("models folder"), "{e}");
        file(&r.models, "ggml-base.en.bin", b"m");
        let j = plan(
            "transcribe",
            &args(&format!(
                r#"{{"audio":"{}","language":"en"}}"#,
                wav.display()
            )),
            &r,
            &b,
        )
        .unwrap();
        assert!(j
            .argv
            .windows(2)
            .any(|w| w[0] == "-m" && w[1].ends_with("ggml-base.en.bin")));
        assert!(plan(
            "transcribe",
            &args(&format!(
                r#"{{"audio":"{}","model":"../x.bin"}}"#,
                wav.display()
            )),
            &r,
            &b
        )
        .is_err());
        // speak: the text goes in on stdin, never on the command line
        file(&r.models, "v.onnx", b"m");
        let j = plan("speak", &args(r#"{"text":"Hello; rm -rf /"}"#), &r, &b).unwrap();
        assert_eq!(j.stdin.as_deref(), Some(&b"Hello; rm -rf /\n"[..]));
        assert!(!j.argv.iter().any(|x| x.contains("rm -rf")));
        assert!(matches!(&j.ret, Ret::File(p) if p.ends_with("speech.wav")));
        assert!(plan(
            "speak",
            &args(&format!(r#"{{"text":"{}"}}"#, "a".repeat(5001))),
            &r,
            &b
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_server_lists_only_installed_and_enabled_tools_and_runs_the_real_program() {
        let d = tmp("server");
        let home = d.join("home");
        let ctx = ctx_in(&home);
        let bin = d.join("bin");
        // a "tesseract" that prints text, and an "ffmpeg" that makes the output file
        fake_bin(&bin, "tesseract", "Hello from the picture");
        let ff = bin.join("ffmpeg");
        std::fs::write(
            &ff,
            "#!/bin/sh\nfor last; do :; done\necho made > \"$last\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ff, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fake_bin(&bin, "rg", "x");
        let sys = sys_with("linux", &[&bin]);
        let srv = Server {
            ctx: &ctx,
            sys: &sys,
        };
        // nothing turned on: nothing offered, nothing runs
        assert!(srv.tools().is_empty());
        let e = srv.call("ocr", &args(r#"{"image":"a.png"}"#)).unwrap_err();
        assert!(e.contains("crate enable tesseract"), "{e}");
        // ffmpeg and tesseract on; ripgrep is installed but left off
        let mut st = State::load(&ctx);
        st.set("tesseract", vec![("enabled", J::Bool(true))]);
        st.set("ffmpeg", vec![("enabled", J::Bool(true))]);
        st.save(&ctx).unwrap();
        let names: Vec<&str> = srv.tools().iter().map(|s| s.name).collect();
        assert_eq!(names, ["ffmpeg_convert", "ffmpeg_trim", "ocr"]);
        // over the wire
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
        );
        let mut out = vec![];
        srv.run(input.as_bytes(), &mut out).unwrap();
        let lines: Vec<String> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines.len(), 2, "the notification gets no answer");
        let list = parse(&lines[1]).unwrap();
        let tools = list.get("result").unwrap().arr("tools");
        assert_eq!(tools.len(), 3);
        assert_eq!(
            tools[2].get("annotations").unwrap().bool("readOnlyHint"),
            Some(true)
        );
        // run: ocr on a picture in the workbench
        let work = home.join("crate").join("work");
        std::fs::create_dir_all(&work).unwrap();
        file(&work, "scan.png", b"png");
        let said = srv.call("ocr", &args(r#"{"image":"scan.png"}"#)).unwrap();
        assert_eq!(said, "Hello from the picture");
        // run: ffmpeg writes the new file, once; a second time the name is taken
        file(&work, "a.wav", b"wav");
        let said = srv
            .call(
                "ffmpeg_convert",
                &args(r#"{"input":"a.wav","format":"flac"}"#),
            )
            .unwrap();
        assert!(
            said.starts_with("Wrote ") && said.contains("a.flac"),
            "{said}"
        );
        let e = srv
            .call(
                "ffmpeg_convert",
                &args(r#"{"input":"a.wav","format":"flac"}"#),
            )
            .unwrap_err();
        assert!(e.contains("already exists"), "{e}");
        // a tool that was not turned on is refused even when asked for by name
        let e = srv
            .call("text_search", &args(r#"{"pattern":"x","folder":"."}"#))
            .unwrap_err();
        assert!(e.contains("not available"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_run_that_hangs_is_stopped_and_a_failure_is_explained() {
        let d = tmp("exec");
        let j = Job {
            argv: strs(&["/bin/sleep", "5"]),
            stdin: None,
            timeout: Duration::from_millis(200),
            ret: Ret::Text(100),
            ok: &[0],
        };
        let t = Instant::now();
        assert!(execute(&j, &d).unwrap_err().contains("stopped"));
        assert!(t.elapsed() < Duration::from_secs(3));
        let j = Job {
            argv: strs(&["/bin/sh", "-c", "echo boom >&2; exit 3"]),
            stdin: None,
            timeout: Duration::from_secs(5),
            ret: Ret::Text(100),
            ok: &[0],
        };
        let e = execute(&j, &d).unwrap_err();
        assert!(e.contains("boom"), "{e}");
        // output is held to its size
        let j = Job {
            argv: strs(&["/bin/sh", "-c", "head -c 100000 /dev/zero | tr '\\0' a"]),
            stdin: None,
            timeout: Duration::from_secs(5),
            ret: Ret::Text(1000),
            ok: &[0],
        };
        assert_eq!(execute(&j, &d).unwrap().len(), 1000);
    }

    #[test]
    fn times_parse_as_seconds() {
        assert_eq!(seconds("12.5").unwrap(), 12.5);
        assert_eq!(seconds("01:02:03.5").unwrap(), 3723.5);
        assert_eq!(seconds("2:30").unwrap(), 150.0);
        for bad in ["", "a", "1:2:3:4", "1..2", "-3", "1;2", "9999999"] {
            assert!(seconds(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_file_address_is_encoded() {
        assert_eq!(
            file_url(Path::new("/h/my crate/p")),
            "file:///h/my%20crate/p"
        );
        assert_eq!(
            file_url(Path::new(r"C:\Users\a b\p")),
            "file:///C:/Users/a%20b/p"
        );
    }
}
