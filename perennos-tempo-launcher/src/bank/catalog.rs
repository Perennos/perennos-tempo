//! The Crate's catalog: `catalog/crate.json`, embedded. Every tool says what it is for, its licence,
//! how each OS gets it (winget, Homebrew, apt, pipx, or a pinned download with its SHA-256), how to
//! tell it is installed, and what Tempo can do with it.

use crate::setup::json::{parse, J};

pub const EMBEDDED: &str = include_str!("../../catalog/crate.json");

/// The three systems Tempo runs on, as `std::env::consts::OS` spells them.
pub const OSES: [&str; 3] = ["windows", "macos", "linux"];

/// A file Tempo may fetch itself, checked against its SHA-256 before anything is unpacked.
#[derive(Clone, Debug, PartialEq)]
pub struct Download {
    pub url: String,
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    /// true: unpacked with the system's `tar`; false: the file is the program.
    pub archive: bool,
}

/// How one OS gets a tool. Any mix; the first manager the person has is used.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Route {
    pub winget: Option<String>,
    pub brew: Option<String>,
    pub cask: bool,
    pub apt: Option<String>,
    pub pipx: Option<String>,
    pub download: Option<Download>,
    /// Plain words for a tool no package manager has.
    pub manual: Option<String>,
}

impl Route {
    /// Whether Tempo can install it without the person doing a step by hand.
    pub fn automatic(&self) -> bool {
        self.winget.is_some()
            || self.brew.is_some()
            || self.apt.is_some()
            || self.pipx.is_some()
            || self.download.is_some()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Detect {
    pub bins: Vec<String>,
    /// Replaces `bins` on Windows (ImageMagick's `convert` there is another program).
    pub bins_windows: Option<Vec<String>>,
    /// Known install places per OS; `%VAR%` and one `*` per folder name are understood.
    pub paths: Vec<(String, Vec<String>)>,
    /// What to run to ask for the version; None when running it is not safe or not useful.
    pub version: Option<Vec<String>>,
    /// Not a program: the agent's own setting says it is there.
    pub agent_setting: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub what: String,
    pub category: String,
    pub licence: String,
    pub homepage: String,
    pub install: Vec<(String, Route)>,
    pub detect: Detect,
    pub mb: u64,
    pub tempo: String,
    /// The MCP tools it gives Tempo.
    pub tools: Vec<String>,
    pub verified: bool,
    pub note: String,
}

impl Tool {
    pub fn route(&self, os: &str) -> Option<&Route> {
        self.install.iter().find(|(o, _)| o == os).map(|(_, r)| r)
    }

    pub fn bins(&self, os: &str) -> &[String] {
        match (&self.detect.bins_windows, os) {
            (Some(b), "windows") => b,
            _ => &self.detect.bins,
        }
    }

    pub fn size_text(&self) -> String {
        if self.mb >= 1000 {
            format!("about {:.1} GB", self.mb as f64 / 1000.0)
        } else {
            format!("about {} MB", self.mb)
        }
    }
}

fn download_from(j: &J, who: &str) -> Result<Download, String> {
    let need = |k: &str| {
        j.str(k)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .ok_or_else(|| format!("{who}: a download needs \"{k}\""))
    };
    let d = Download {
        url: need("url")?,
        file: need("file")?,
        sha256: need("sha256")?.to_ascii_lowercase(),
        bytes: j
            .f64("bytes")
            .filter(|b| *b > 0.0)
            .ok_or(format!("{who}: a download needs \"bytes\""))? as u64,
        archive: j.bool("archive").unwrap_or(false),
    };
    if !d.url.starts_with("https://") {
        return Err(format!("{who}: a download must be an https address"));
    }
    if d.sha256.len() != 64 || !d.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{who}: the SHA-256 must be 64 hex digits"));
    }
    if d.file.contains(['/', '\\']) || d.file.starts_with('.') {
        return Err(format!("{who}: the download's file name must be plain"));
    }
    Ok(d)
}

fn route_from(j: &J, who: &str) -> Result<Route, String> {
    let s = |k: &str| j.str(k).filter(|v| !v.trim().is_empty()).map(String::from);
    Ok(Route {
        winget: s("winget"),
        brew: s("brew"),
        cask: j.bool("cask").unwrap_or(false),
        apt: s("apt"),
        pipx: s("pipx"),
        download: j
            .get("download")
            .map(|d| download_from(d, who))
            .transpose()?,
        manual: s("manual"),
    })
}

fn tool_from(j: &J) -> Result<Tool, String> {
    let id = j.str("id").unwrap_or("");
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "a tool has a bad id {id:?} (lowercase letters, digits, -)"
        ));
    }
    let text = |k: &str| {
        j.str(k)
            .filter(|v| !v.trim().is_empty())
            .map(String::from)
            .ok_or_else(|| format!("{id}: \"{k}\" is missing"))
    };
    let mut install = Vec::new();
    for os in OSES {
        if let Some(r) = j.get("install").and_then(|i| i.get(os)) {
            install.push((os.to_string(), route_from(r, id)?));
        }
    }
    let d = j
        .get("detect")
        .ok_or(format!("{id}: \"detect\" is missing"))?;
    let paths = OSES
        .iter()
        .filter_map(|os| {
            let p = d.get("paths").map(|p| p.strs(os)).unwrap_or_default();
            (!p.is_empty()).then(|| (os.to_string(), p))
        })
        .collect();
    let detect = Detect {
        bins: d.strs("bins"),
        bins_windows: d.get("bins_windows").map(|_| d.strs("bins_windows")),
        paths,
        version: d.get("version").map(|_| d.strs("version")),
        agent_setting: d.str("agent_setting").map(String::from),
    };
    Ok(Tool {
        id: id.to_string(),
        name: text("name")?,
        what: text("for")?,
        category: text("category")?,
        licence: text("licence")?,
        homepage: text("homepage")?,
        install,
        detect,
        mb: j
            .f64("mb")
            .filter(|m| *m > 0.0)
            .ok_or(format!("{id}: \"mb\" is missing"))? as u64,
        tempo: text("tempo")?,
        tools: j.strs("tools"),
        verified: j
            .bool("verified")
            .ok_or(format!("{id}: \"verified\" must be true or false"))?,
        note: j.str("note").unwrap_or("").to_string(),
    })
}

/// Read a catalog and check every entry. The first problem is the answer.
pub fn load(text: &str) -> Result<Vec<Tool>, String> {
    let j = parse(text)?;
    let mut out: Vec<Tool> = Vec::new();
    for t in j.arr("tools") {
        let t = tool_from(t)?;
        if out.iter().any(|o| o.id == t.id) {
            return Err(format!("{}: listed twice", t.id));
        }
        // reachable somehow: a package, a pinned download, or by hand
        if !t
            .install
            .iter()
            .any(|(_, r)| r.automatic() || r.manual.is_some())
        {
            return Err(format!("{}: no way to install it on any OS", t.id));
        }
        if t.detect.bins.is_empty() && t.detect.agent_setting.is_none() {
            return Err(format!("{}: nothing to detect it by", t.id));
        }
        if !t.verified && t.note.trim().is_empty() {
            return Err(format!(
                "{}: verified is false, so the note must say why",
                t.id
            ));
        }
        out.push(t);
    }
    if out.is_empty() {
        return Err("the catalog is empty".into());
    }
    Ok(out)
}

pub fn embedded() -> Vec<Tool> {
    load(EMBEDDED).expect("catalog/crate.json is checked by the tests")
}

pub fn find<'a>(tools: &'a [Tool], id: &str) -> Option<&'a Tool> {
    let id = id.trim().to_ascii_lowercase();
    tools.iter().find(|t| t.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_of_the_shipped_catalog_validates() {
        let tools = load(EMBEDDED).unwrap();
        assert!(tools.len() >= 20, "{} tools", tools.len());
        for t in &tools {
            assert!(!t.licence.is_empty(), "{}", t.id);
            assert!(t.homepage.starts_with("https://"), "{}", t.id);
            assert!(
                !t.what.contains('\n') && t.what.len() < 220,
                "{}: one human line",
                t.id
            );
            let all = format!("{} {} {} {}", t.what, t.tempo, t.note, t.licence).to_lowercase();
            for w in [
                "open source",
                "open-source",
                "best-in-class",
                "seamless",
                "leverage",
            ] {
                assert!(!all.contains(w), "{}: house word: {w}", t.id);
            }
            // each tool is reachable on some OS, by a package manager or by hand
            assert!(
                t.install
                    .iter()
                    .any(|(_, r)| r.automatic() || r.manual.is_some()),
                "{}",
                t.id
            );
            for (os, r) in &t.install {
                assert!(OSES.contains(&os.as_str()), "{}: {os}", t.id);
                assert!(r.automatic() || r.manual.is_some(), "{} on {os}", t.id);
                if r.cask {
                    assert!(r.brew.is_some(), "{}: cask without a brew name", t.id);
                }
            }
            // a tool is ComfyUI-style linked, or found by a program name
            assert!(
                !t.detect.bins.is_empty() || t.detect.agent_setting.is_some(),
                "{}",
                t.id
            );
        }
        for id in [
            "blender",
            "comfyui",
            "ffmpeg",
            "imagemagick",
            "inkscape",
            "gimp",
            "sox",
            "whisper-cpp",
            "piper",
            "pandoc",
            "libreoffice",
            "tesseract",
            "yt-dlp",
            "obs",
            "godot",
            "freecad",
            "openscad",
            "kicad",
            "ollama",
            "ripgrep",
            "fd",
            "exiftool",
            "audacity",
        ] {
            assert!(find(&tools, id).is_some(), "{id} is in the Crate");
        }
        // the unsure ones say so
        assert!(tools.iter().any(|t| !t.verified));
    }

    fn one(extra: &str) -> String {
        format!(
            r#"{{"tools":[{{"id":"x","name":"X","for":"does x","category":"c","licence":"MIT","homepage":"https://x.example/",
            "detect":{{"bins":["x"]}},"mb":5,"tempo":"nothing","verified":true,{extra}}}]}}"#
        )
    }

    #[test]
    fn a_bad_entry_is_refused_with_its_reason() {
        assert!(load(&one(r#""install":{"linux":{"apt":"x"}}"#)).is_ok());
        assert!(load(&one(r#""install":{}"#))
            .unwrap_err()
            .contains("no way to install"));
        assert!(load(&one(r#""install":{"linux":{}}"#)).is_err());
        let dl = |sha: &str, url: &str| {
            one(&format!(
                r#""install":{{"linux":{{"download":{{"url":"{url}","file":"x.tgz","sha256":"{sha}","bytes":9,"archive":true}}}}}}"#
            ))
        };
        let good = "a".repeat(64);
        assert!(load(&dl(&good, "https://x.example/x.tgz")).is_ok());
        assert!(load(&dl("abc", "https://x.example/x.tgz"))
            .unwrap_err()
            .contains("SHA-256"));
        assert!(load(&dl(&good, "http://x.example/x.tgz"))
            .unwrap_err()
            .contains("https"));
        // not verified needs a reason
        let t = one(r#""install":{"linux":{"apt":"x"}}"#).replace("true", "false");
        assert!(load(&t).unwrap_err().contains("note"));
        // a missing licence is refused
        let t = one(r#""install":{"linux":{"apt":"x"}}"#).replace("\"licence\":\"MIT\",", "");
        assert!(load(&t).unwrap_err().contains("licence"));
    }
}
