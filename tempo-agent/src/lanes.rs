//! Fast/heavy lane routing. The fast lane is a small model with no tools (it answers
//! chat in about a second); anything that looks like work goes to the heavy lane.
//! Port of `pick_lane`, `TOOLISH` and `HARDISH` in `tempo_agent.py`.
use crate::config::{AgentConfig, Lane};
use crate::ollama::base_name;
use crate::text::has_word;

/// Whole words (or phrases) that mean "use tools".
const TOOL_WORDS: &[&str] = &[
    "list", "ls", "dir", "folder", "directory", "read", "open", "run", "execute", "command", "write", "save", "create",
    "make", "generate", "render", "image", "picture", "delegate", "trash", "delete", "remove", "fetch", "download",
    "install", "restart", "start", "stop", "comfy", "workflow", "task list", "todo", "fix", "edit", "patch", "status of",
    "check status", "check the status", "check disk", "check the disk", "check gpu", "check the gpu",
];

/// Whole words that mean "think hard".
const HARD_WORDS: &[&str] = &[
    "analyse", "analyze", "compare", "plan", "design", "architecture", "debug", "step by step", "prove", "calculate",
    "code", "script", "refactor", "review", "draft",
];

const FILE_EXTS: &[&str] = &["py", "md", "json", "ps1", "txt", "log"];

/// `C:\` style drive paths anywhere in the text.
fn has_drive_path(text: &str) -> bool {
    text.as_bytes().windows(3).any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && w[2] == b'\\')
}

/// `.py`, `.md`, ... followed by a non-word character or the end.
fn has_file_ext(lower: &str) -> bool {
    FILE_EXTS.iter().any(|ext| {
        let pat = format!(".{ext}");
        lower.match_indices(&pat).any(|(i, _)| {
            let next = lower[i + pat.len()..].chars().next();
            !next.is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    })
}

/// Long, tool-ish or hard requests need the heavy lane.
pub fn needs_heavy(text: &str, extra_words: &[String]) -> bool {
    if text.chars().count() > 600 || has_drive_path(text) {
        return true;
    }
    let lower = text.to_lowercase();
    TOOL_WORDS.iter().chain(HARD_WORDS).any(|w| has_word(&lower, w))
        || extra_words.iter().any(|w| has_word(&lower, &w.to_lowercase()))
        || has_file_ext(&lower)
}

/// Pick the lane for a turn. `resident` (the engine's loaded models) is only asked when
/// the text alone says fast: if only the heavy model is warm, use it, because a warm
/// big model answers in about a second and swapping models costs a 10-30 s reload.
pub fn pick_lane(text: &str, cfg: &AgentConfig, resident: impl FnOnce() -> Option<Vec<String>>) -> Lane {
    if cfg.fast_models.is_empty() || needs_heavy(text, &cfg.heavy_words) {
        return Lane::Heavy;
    }
    if let (Some(h), Some(f)) = (cfg.heavy_models.first(), cfg.fast_models.first()) {
        let (h, f) = (base_name(h), base_name(f));
        if let Some(res) = resident() {
            if res.iter().any(|r| r == h) && !res.iter().any(|r| r == f) {
                return Lane::Heavy;
            }
        }
    }
    Lane::Fast
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_and_hard_words_need_heavy() {
        for t in [
            "list the downloads folder",
            "Read it",
            "please make an image of a cat",
            "update the task list",
            "check the GPU",
            "what is the status of the build",
            "can you refactor this",
            "Analyse the logs",
            "open C:\\temp",
            "what's in notes.md?",
            "see README.md",
        ] {
            assert!(needs_heavy(t, &[]), "{t}");
        }
        for t in ["hello there", "what is the capital of France", "listing prices", "notes.mdx", "thanks!"] {
            assert!(!needs_heavy(t, &[]), "{t}");
        }
        assert!(needs_heavy(&"word ".repeat(130), &[]), "long requests");
        assert!(needs_heavy("ask gpu-box about it", &["GPU-Box".to_string()]), "configured words");
    }

    #[test]
    fn sticky_heavy_when_only_it_is_resident() {
        let cfg = AgentConfig {
            heavy_models: vec!["big:latest".into()],
            fast_models: vec!["small".into()],
            ..Default::default()
        };
        let res = |names: &[&str]| Some(names.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(pick_lane("hi", &cfg, || res(&["big"])), Lane::Heavy);
        assert_eq!(pick_lane("hi", &cfg, || res(&["big", "small"])), Lane::Fast);
        assert_eq!(pick_lane("hi", &cfg, || res(&[])), Lane::Fast);
        assert_eq!(pick_lane("hi", &cfg, || None), Lane::Fast);
        assert_eq!(pick_lane("list files", &cfg, || panic!("not asked when the text decides")), Lane::Heavy);
        let no_fast = AgentConfig { fast_models: vec![], ..cfg };
        assert_eq!(pick_lane("hi", &no_fast, || None), Lane::Heavy);
    }
}
