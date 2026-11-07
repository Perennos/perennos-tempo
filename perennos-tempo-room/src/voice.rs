//! Talk to Tempo in a Room. The owner, at the machine, can use the mic in any room; a member only
//! in a room where the owner turned voice on, because every word is heard by the owner's computer
//! (Room hands the audio to the agent's whisper.cpp and never keeps it). The owner's per-room
//! choice is `voice-rooms.json` in the Room folder.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The most audio a request may carry (the same cap as the agent's).
pub const MAX_AUDIO: usize = 4 << 20;

fn file(dir: &Path) -> PathBuf {
    dir.join("voice-rooms.json")
}

/// Rooms where members may use the owner's microphone-to-text.
pub fn rooms(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(file(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|j| {
            j["rooms"].as_array().map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str().map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

pub fn allowed(dir: &Path, room: &str) -> bool {
    rooms(dir).iter().any(|r| r == room)
}

pub fn set(dir: &Path, room: &str, on: bool) -> Result<(), String> {
    let mut all = rooms(dir);
    all.retain(|r| r != room);
    if on {
        all.push(room.to_string());
    }
    crate::util::write_atomic(&file(dir), json!({"rooms": all}).to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owners_choice_is_kept_per_room() {
        let d = std::env::temp_dir().join(format!("room-voice-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(!allowed(&d, "family"));
        set(&d, "family", true).unwrap();
        set(&d, "team", true).unwrap();
        assert!(allowed(&d, "family") && allowed(&d, "team") && !allowed(&d, "book"));
        set(&d, "family", false).unwrap();
        assert!(!allowed(&d, "family") && allowed(&d, "team"));
    }
}
