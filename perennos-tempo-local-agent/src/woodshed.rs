//! The agent's side of the Woodshed (training): what it reads and writes in its own data folder.
//! `tempo-launch woodshed` owns everything else (see docs/WOODSHED.md).
//!
//! - `woodshed.json`: which bands the owner turned capture on for. Nothing is kept until then.
//! - `lessons/<band-or-default>.prompt.txt`: the top lessons, already picked and capped by the
//!   launcher. The agent only reads the file and puts it in the system message.
//! - `takes/takes.jsonl`: one line per judged attempt. The agent writes the owner's thumbs here.
//! - `mix/faders.json`: the per-band LoRA scales for a llama-server engine, read at every request
//!   so a fader moved while Tempo is running is heard on the next answer.

use crate::util;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// The most characters of lessons that ever go into a prompt (about 600 tokens). The launcher
/// caps by tokens before this; the cap here is only for a hand-edited file.
const LESSON_CHARS: usize = 2400;

fn read_json(p: &Path) -> Value {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// The key a band's files go under: the band's own name, or `default` for the owner's chat.
pub fn key(band: Option<&str>) -> &str {
    band.filter(|b| !b.is_empty()).unwrap_or("default")
}

/// A band, part or lesson file name that can be trusted in a path.
pub fn slug_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Whether the owner turned capture on for this band (`tempo-launch woodshed on <band>`).
pub fn capture_on(data: &Path, band: Option<&str>) -> bool {
    read_json(&data.join("woodshed.json"))["bands"][key(band)]["on"] == true
}

/// The lessons for a band, as the text that goes into the system message ("" when there are none).
pub fn lessons_text(data: &Path, band: Option<&str>) -> String {
    let k = key(band);
    if !slug_ok(k) {
        return String::new();
    }
    let t = std::fs::read_to_string(data.join("lessons").join(format!("{k}.prompt.txt")))
        .unwrap_or_default();
    util::head_chars(t.trim(), LESSON_CHARS)
}

/// The system-message part for a band's lessons, or "" when there are none.
pub fn lessons_section(data: &Path, band: Option<&str>) -> String {
    let t = lessons_text(data, band);
    if t.is_empty() {
        return String::new();
    }
    format!("\nLESSONS (short notes from earlier takes; use them when they fit, the rules above still hold):\n{t}\n")
}

/// The LoRA scales for `band` and `part` at this moment: `band/part`, else `band`, else `*`.
/// Each is `[{"id":0,"scale":1.0}, ...]`, as llama-server takes it per request.
pub fn faders(data: &Path, band: Option<&str>, part: Option<&str>) -> Option<Value> {
    let all = read_json(&data.join("mix").join("faders.json"));
    let mut keys = vec![];
    if let (Some(b), Some(p)) = (band, part) {
        keys.push(format!("{b}/{p}"));
    }
    if let Some(b) = band {
        keys.push(b.to_string());
    }
    keys.push("*".to_string());
    keys.iter()
        .filter_map(|k| all.get(k.as_str()))
        .find(|v| scales_ok(v))
        .cloned()
}

/// `[{"id": int, "scale": number}, ...]`, at most 16 adapters.
pub fn scales_ok(v: &Value) -> bool {
    v.as_array().is_some_and(|a| {
        a.len() <= 16
            && a.iter()
                .all(|e| e["id"].is_u64() && e["scale"].as_f64().is_some_and(f64::is_finite))
    })
}

/// RFC 3339, UTC, to the second.
pub fn rfc3339() -> String {
    let (y, mo, d, h, mi, s) = util::civil(util::unix_now());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// One judged attempt, appended to `takes/takes.jsonl`. A take is one line; when the same take id
/// appears again (the owner changed their mind) the later line is the one that counts.
pub fn append_take(data: &Path, take: &Value) -> Result<(), String> {
    let dir = data.join("takes");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let mut line = take.to_string();
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("takes.jsonl"))
        .map_err(|e| format!("cannot open the takes file: {e}"))?;
    // one write, so two takes written at once never share a line
    f.write_all(line.as_bytes())
        .map_err(|e| format!("cannot write the take: {e}"))
}

/// The owner's thumb on one answer, as a take. `answer` is the index of the assistant event in
/// the conversation's events and `prompt` the user event before it.
#[allow(clippy::too_many_arguments)]
pub fn owner_take(
    conv: &str,
    band: Option<&str>,
    part: Option<&str>,
    model: &str,
    engine: &str,
    task: &str,
    prompt: usize,
    answer: usize,
    up: bool,
) -> Value {
    json!({
        "v": 1,
        "take": format!("owner-{conv}-{answer}"),
        "run": format!("chat-{conv}"),
        "band": band,
        "part": part,
        "task": util::head_chars(&util::redact(task), 160),
        "engine": engine,
        "model": model,
        // the owner judged it; the words are the model's own
        "author": "local",
        "prompt_ref": format!("conversations/{conv}.json#{prompt}"),
        "answer_ref": format!("conversations/{conv}.json#{answer}"),
        "checks": {"tests": null, "soundcheck": null, "judge": null, "owner": if up { "accept" } else { "reject" }},
        "winner": up,
        "pair": null,
        "ts": rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tla-wood-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn capture_is_off_until_the_owner_turns_it_on() {
        let d = dir("capture");
        assert!(!capture_on(&d, Some("social-media")));
        std::fs::write(
            d.join("woodshed.json"),
            r#"{"bands":{"social-media":{"on":true},"default":{"on":false}}}"#,
        )
        .unwrap();
        assert!(capture_on(&d, Some("social-media")));
        assert!(!capture_on(&d, None), "default is its own switch");
        assert!(!capture_on(&d, Some("dev-team")));
    }

    #[test]
    fn lessons_come_from_the_band_file_and_are_capped() {
        let d = dir("lessons");
        assert_eq!(lessons_section(&d, Some("b")), "");
        std::fs::create_dir_all(d.join("lessons")).unwrap();
        std::fs::write(d.join("lessons/b.prompt.txt"), "- keep it short\n").unwrap();
        let s = lessons_section(&d, Some("b"));
        assert!(s.contains("- keep it short"), "{s}");
        assert_eq!(lessons_section(&d, None), "", "default has none");
        std::fs::write(d.join("lessons/default.prompt.txt"), "x".repeat(9000)).unwrap();
        assert!(lessons_text(&d, None).chars().count() <= LESSON_CHARS + 3);
        // a name that is not a plain slug never reaches the disk
        assert_eq!(lessons_text(&d, Some("../x")), "");
    }

    #[test]
    fn faders_pick_the_most_specific_and_ignore_nonsense() {
        let d = dir("faders");
        assert!(faders(&d, Some("b"), None).is_none());
        std::fs::create_dir_all(d.join("mix")).unwrap();
        std::fs::write(
            d.join("mix/faders.json"),
            r#"{"b/lead":[{"id":0,"scale":0.5}],"b":[{"id":0,"scale":1.0}],"*":[{"id":1,"scale":2}],"c":"loud"}"#,
        )
        .unwrap();
        assert_eq!(
            faders(&d, Some("b"), Some("lead")).unwrap()[0]["scale"],
            0.5
        );
        assert_eq!(
            faders(&d, Some("b"), Some("other")).unwrap()[0]["scale"],
            1.0
        );
        // a band whose entry is not a list of scales falls through to the default one
        assert_eq!(faders(&d, Some("c"), None).unwrap()[0]["id"], 1);
        assert!(!scales_ok(&json!([{"id": -1, "scale": 1.0}])));
        assert!(!scales_ok(&json!({"id": 0})));
    }

    #[test]
    fn a_thumb_is_one_take_line() {
        let d = dir("take");
        let t = owner_take(
            "c1",
            Some("b"),
            None,
            "qwen3:8b",
            "ollama",
            "write a hook",
            2,
            3,
            true,
        );
        append_take(&d, &t).unwrap();
        let t2 = owner_take("c1", Some("b"), None, "m", "e", "write a hook", 2, 3, false);
        append_take(&d, &t2).unwrap();
        let text = std::fs::read_to_string(d.join("takes/takes.jsonl")).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["take"], "owner-c1-3");
        assert_eq!(lines[0]["checks"]["owner"], "accept");
        assert_eq!(lines[0]["winner"], true);
        assert_eq!(lines[1]["checks"]["owner"], "reject");
        assert_eq!(lines[1]["winner"], false);
        assert_eq!(lines[0]["prompt_ref"], "conversations/c1.json#2");
        assert!(lines[0]["ts"].as_str().unwrap().ends_with('Z'));
    }
}
