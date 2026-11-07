//! Discord, over the REST API only (no WebSocket): every few seconds each linked channel is read
//! with `GET /channels/{id}/messages?after={last}`, new messages are mirrored into the linked room,
//! and a mention of the bot (`<@BOT_ID>`), "@tempo", or a reply to Tempo is answered in-thread with
//! `POST /channels/{id}/messages` and a `message_reference`. Tempo's replies never ping @everyone
//! or roles (`allowed_mentions`). The first look at a channel only notes where it is, so old
//! history is never answered. A 401 turns the bridge off (and says so); 429 waits `retry_after`.
use super::{load_state, save_state, DiscordConf, Http, Target};
use crate::hub::{Hub, PostReq, Speaker};
use crate::util::{cap, log};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn auth(c: &DiscordConf) -> String {
    format!("Bot {}", c.token)
}

pub fn reply(
    c: &DiscordConf,
    http: &Http,
    channel: &str,
    message: &str,
    text: &str,
) -> Result<(), String> {
    let url = format!("{}/channels/{channel}/messages", c.api);
    let body = json!({
        "content": cap(text, 1900),
        "message_reference": {"message_id": message, "channel_id": channel, "fail_if_not_exists": false},
        "allowed_mentions": {"parse": [], "replied_user": true},
    });
    let r = http.with_retry(
        "POST",
        &url,
        Some(&auth(c)),
        Some(&body),
        Duration::from_secs(20),
        "discord reply",
    )?;
    match r.status {
        200 | 201 => Ok(()),
        s => Err(format!(
            "discord reply: HTTP {s} {}{}",
            r.json["message"].as_str().unwrap_or(""),
            if r.json["code"] == 40001 {
                " (if this bot never went online, see the README: Discord, first login)"
            } else {
                ""
            }
        )),
    }
}

pub fn typing(c: &DiscordConf, http: &Http, channel: &str) {
    let url = format!("{}/channels/{channel}/typing", c.api);
    let _ = http.request(
        "POST",
        &url,
        Some(&auth(c)),
        None,
        Duration::from_secs(10),
        "discord typing",
    );
}

/// `<@BOT>` and `<@!BOT>` become "@tempo"; other user mentions become "@name".
pub fn normalize(content: &str, me: &str, mentions: &[Value]) -> String {
    let mut s = content
        .replace(&format!("<@{me}>"), "@tempo")
        .replace(&format!("<@!{me}>"), "@tempo");
    for m in mentions {
        let (Some(id), Some(name)) = (m["id"].as_str(), display_name(m)) else {
            continue;
        };
        s = s
            .replace(&format!("<@{id}>"), &format!("@{name}"))
            .replace(&format!("<@!{id}>"), &format!("@{name}"));
    }
    s
}

fn display_name(user: &Value) -> Option<String> {
    user["global_name"]
        .as_str()
        .or(user["username"].as_str())
        .map(str::to_string)
}

/// Platform message ids to local ones, for replies (bounded).
#[derive(Default)]
pub struct IdMap {
    order: VecDeque<String>,
    map: HashMap<String, u64>,
}

impl IdMap {
    pub fn put(&mut self, k: String, v: u64) {
        if self.map.insert(k.clone(), v).is_none() {
            self.order.push_back(k);
            if self.order.len() > 500 {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
    pub fn get(&self, k: &str) -> Option<u64> {
        self.map.get(k).copied()
    }
}

pub(crate) fn nap(hub: &Hub, d: Duration) {
    let until = std::time::Instant::now() + d;
    while std::time::Instant::now() < until && !hub.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50).min(d));
    }
}

/// Run the bridge until the hub stops (or the token is refused).
pub fn run(hub: Arc<Hub>, c: DiscordConf) {
    let http = Http::default();
    let dir = hub.lock().dir.clone();
    let mut backoff = 2u64;
    let me = loop {
        if hub.stop.load(Ordering::SeqCst) {
            return;
        }
        match http.with_retry(
            "GET",
            &format!("{}/users/@me", c.api),
            Some(&auth(&c)),
            None,
            Duration::from_secs(20),
            "discord login",
        ) {
            Ok(r) if r.status == 200 => match r.json["id"].as_str() {
                Some(id) => break id.to_string(),
                None => log("discord: odd answer to users/@me"),
            },
            Ok(r) if r.status == 401 => {
                log("discord: the bot token was refused (401); the Discord bridge is off until it is fixed");
                return;
            }
            Ok(r) => log(&format!("discord: users/@me answered HTTP {}", r.status)),
            Err(e) => log(&format!("discord: {e}")),
        }
        nap(&hub, Duration::from_secs(backoff));
        backoff = (backoff * 2).min(60);
    };
    let rooms: Vec<(String, String)> = c
        .channels
        .iter()
        .filter_map(|(ch, room)| Some((ch.clone(), super::ensure_room(&hub, room)?)))
        .collect();
    log(&format!(
        "discord: on, watching {} channel(s) every {:.1} s",
        rooms.len(),
        c.poll.as_secs_f64()
    ));
    let mut state = load_state(&dir, "discord");
    let mut ids = IdMap::default();
    let mut quiet: HashSet<String> = HashSet::new();
    let mut backoff = 2u64;
    while !hub.stop.load(Ordering::SeqCst) {
        let mut trouble = false;
        for (channel, room) in &rooms {
            if hub.stop.load(Ordering::SeqCst) {
                return;
            }
            let last = state["last"][channel].as_str().map(str::to_string);
            let url = match &last {
                Some(l) => format!("{}/channels/{channel}/messages?after={l}&limit=50", c.api),
                None => format!("{}/channels/{channel}/messages?limit=1", c.api),
            };
            let r = match http.with_retry(
                "GET",
                &url,
                Some(&auth(&c)),
                None,
                Duration::from_secs(20),
                "discord read",
            ) {
                Ok(r) => r,
                Err(e) => {
                    log(&format!("discord: {e}"));
                    trouble = true;
                    continue;
                }
            };
            match r.status {
                200 => {}
                401 => {
                    log("discord: the bot token was refused (401); the Discord bridge is off until it is fixed");
                    return;
                }
                s => {
                    if quiet.insert(format!("{channel}:{s}")) {
                        log(&format!(
                            "discord: channel {channel} answered HTTP {s} ({}); is the bot in that server, with View Channel and Read Message History?",
                            r.json["message"].as_str().unwrap_or("")
                        ));
                    }
                    continue;
                }
            }
            let mut msgs: Vec<Value> = r.json.as_array().cloned().unwrap_or_default();
            msgs.sort_by_key(|m| {
                m["id"]
                    .as_str()
                    .and_then(|i| i.parse::<u64>().ok())
                    .unwrap_or(0)
            });
            if last.is_none() {
                // First look: remember where the channel is; never answer old history.
                let newest = msgs
                    .last()
                    .and_then(|m| m["id"].as_str())
                    .unwrap_or("0")
                    .to_string();
                state["last"][channel] = json!(newest);
                save_state(&dir, "discord", &state);
                continue;
            }
            for m in &msgs {
                let Some(mid) = m["id"].as_str() else {
                    continue;
                };
                state["last"][channel] = json!(mid);
                handle(&hub, &me, channel, room, m, &mut ids);
            }
            if !msgs.is_empty() {
                save_state(&dir, "discord", &state);
            }
        }
        if trouble {
            nap(&hub, Duration::from_secs(backoff));
            backoff = (backoff * 2).min(60);
        } else {
            backoff = 2;
            nap(&hub, c.poll);
        }
    }
}

fn handle(hub: &Hub, me: &str, channel: &str, room: &str, m: &Value, ids: &mut IdMap) {
    let author = &m["author"];
    let Some(mid) = m["id"].as_str() else { return };
    if author["id"].as_str() == Some(me) || author["bot"] == true {
        return; // Tempo's own replies are already in the room; other bots are not people
    }
    if !matches!(m["type"].as_u64(), Some(0) | Some(19) | None) {
        return; // joins, pins and the like
    }
    let raw = m["content"].as_str().unwrap_or("");
    let mentions: Vec<Value> = m["mentions"].as_array().cloned().unwrap_or_default();
    let text = normalize(raw, me, &mentions);
    if text.trim().is_empty() {
        return; // no Message Content intent, or only an attachment
    }
    let to_tempo = raw.contains(&format!("<@{me}>"))
        || raw.contains(&format!("<@!{me}>"))
        || mentions.iter().any(|x| x["id"].as_str() == Some(me))
        || crate::bot::mentions_tempo(&text)
        || m["referenced_message"]["author"]["id"].as_str() == Some(me);
    let name = display_name(author).unwrap_or_else(|| "someone".into());
    let reference = m["message_reference"]["message_id"].as_str();
    let mut r = PostReq::new(
        room,
        Speaker::bridged("discord", author["id"].as_str().unwrap_or("?"), &name),
        &text,
        "discord",
    );
    r.reply_to = reference.and_then(|x| ids.get(x));
    r.reply_hint = m["referenced_message"]["content"].as_str().map(|t| {
        (
            display_name(&m["referenced_message"]["author"]).unwrap_or_default(),
            normalize(t, me, &mentions),
        )
    });
    if to_tempo {
        r.target = Some(Target::Discord {
            channel: channel.to_string(),
            message: mid.to_string(),
        });
        r.force_ask = true;
    }
    match hub.post(r) {
        Ok(crate::hub::Posted::Msg(local)) => ids.put(mid.to_string(), local.id),
        Ok(crate::hub::Posted::Note(_)) => {}
        Err(e) => log(&format!(
            "discord: a message from {name} was not mirrored: {}",
            e.1
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mentions_become_names() {
        let mentions = vec![json!({"id": "42", "username": "mia", "global_name": "Mia"})];
        assert_eq!(
            normalize("<@7> and <@!7>, ask <@42>", "7", &mentions),
            "@tempo and @tempo, ask @Mia"
        );
        let mut ids = IdMap::default();
        for i in 0..600u64 {
            ids.put(i.to_string(), i);
        }
        assert_eq!(ids.get("599"), Some(599));
        assert_eq!(ids.get("0"), None, "bounded");
    }
}
