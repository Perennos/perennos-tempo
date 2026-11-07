//! Telegram, by long polling `getUpdates` (timeout 25 s, offset kept on disk) and replying with
//! `sendMessage` and `reply_to_message_id`. Only chats listed in `bridges.telegram.chats` are let
//! in; a message from any other chat is ignored and its chat id logged once, which is the easy
//! way to find a chat id. In groups Tempo answers when mentioned (`@BotName` or "@tempo"), when
//! someone replies to Tempo, or for a `/command`; in an allowed direct chat it answers every
//! message. The token is part of Telegram's URLs, so no URL is ever logged.
use super::discord::{nap, IdMap};
use super::{load_state, save_state, Http, Target, TelegramConf};
use crate::hub::{Hub, PostReq, Posted, Speaker};
use crate::util::{cap, log};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn url(c: &TelegramConf, method: &str) -> String {
    format!("{}/bot{}/{method}", c.api, c.token)
}

pub fn reply(
    c: &TelegramConf,
    http: &Http,
    chat: i64,
    message: i64,
    text: &str,
) -> Result<(), String> {
    let body = json!({
        "chat_id": chat,
        "text": cap(text, 4000),
        "reply_to_message_id": message,
        "allow_sending_without_reply": true,
    });
    let r = http.with_retry(
        "POST",
        &url(c, "sendMessage"),
        None,
        Some(&body),
        Duration::from_secs(20),
        "telegram sendMessage",
    )?;
    if r.json["ok"] == true {
        Ok(())
    } else {
        Err(format!(
            "telegram sendMessage: HTTP {} {}",
            r.status,
            r.json["description"].as_str().unwrap_or("")
        ))
    }
}

pub fn typing(c: &TelegramConf, http: &Http, chat: i64) {
    let _ = http.request(
        "POST",
        &url(c, "sendChatAction"),
        None,
        Some(&json!({"chat_id": chat, "action": "typing"})),
        Duration::from_secs(10),
        "telegram typing",
    );
}

/// `@BotName` (any case) becomes "@tempo".
pub fn normalize(text: &str, bot_username: &str) -> String {
    if bot_username.is_empty() {
        return text.to_string();
    }
    let needle = format!("@{}", bot_username.to_lowercase());
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        return text.to_string();
    }
    let mut out = String::new();
    let mut at = 0;
    for (i, _) in lower.match_indices(&needle) {
        out.push_str(&text[at..i]);
        out.push_str("@tempo");
        at = i + needle.len();
    }
    out.push_str(&text[at..]);
    out
}

fn person(u: &Value) -> String {
    let first = u["first_name"].as_str().unwrap_or("");
    let last = u["last_name"].as_str().unwrap_or("");
    let n = format!("{first} {last}").trim().to_string();
    if n.is_empty() {
        u["username"].as_str().unwrap_or("someone").to_string()
    } else {
        n
    }
}

pub fn run(hub: Arc<Hub>, c: TelegramConf) {
    let http = Http::default();
    let dir = hub.lock().dir.clone();
    let mut backoff = 2u64;
    let (me, username) = loop {
        if hub.stop.load(Ordering::SeqCst) {
            return;
        }
        match http.with_retry(
            "GET",
            &url(&c, "getMe"),
            None,
            None,
            Duration::from_secs(20),
            "telegram getMe",
        ) {
            Ok(r) if r.json["ok"] == true => {
                break (
                    r.json["result"]["id"].as_i64().unwrap_or(0),
                    r.json["result"]["username"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                )
            }
            Ok(r) if r.status == 401 || r.status == 404 => {
                log("telegram: the bot token was refused; the Telegram bridge is off until it is fixed (BotFather, /token)");
                return;
            }
            Ok(r) => log(&format!("telegram: getMe answered HTTP {}", r.status)),
            Err(e) => log(&format!("telegram: {e}")),
        }
        nap(&hub, Duration::from_secs(backoff));
        backoff = (backoff * 2).min(60);
    };
    let rooms: Vec<(i64, String)> = c
        .chats
        .iter()
        .filter_map(|(chat, room)| Some((*chat, super::ensure_room(&hub, room)?)))
        .collect();
    if rooms.is_empty() {
        log(&format!("telegram: on as @{username}; no chats allowed yet: message the bot (or mention it in your group) and its chat id shows up here"));
    } else {
        log(&format!(
            "telegram: on as @{username}, {} chat(s) allowed",
            rooms.len()
        ));
    }
    let mut state = load_state(&dir, "telegram");
    let mut ids = IdMap::default();
    let mut ignored: HashSet<i64> = HashSet::new();
    let mut backoff = 2u64;
    while !hub.stop.load(Ordering::SeqCst) {
        let offset = state["offset"].as_i64().unwrap_or(0);
        let u = format!(
            "{}?timeout={}&offset={offset}&allowed_updates=%5B%22message%22%5D",
            url(&c, "getUpdates"),
            c.poll_timeout
        );
        let r = match http.with_retry(
            "GET",
            &u,
            None,
            None,
            Duration::from_secs(c.poll_timeout + 15),
            "telegram getUpdates",
        ) {
            Ok(r) => r,
            Err(e) => {
                log(&format!("telegram: {e}"));
                nap(&hub, Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        if r.json["ok"] != true {
            log(&format!(
                "telegram: getUpdates answered HTTP {} {}",
                r.status,
                r.json["description"].as_str().unwrap_or("")
            ));
            if r.status == 401 || r.status == 404 {
                return;
            }
            nap(&hub, Duration::from_secs(backoff));
            backoff = (backoff * 2).min(60);
            continue;
        }
        backoff = 2;
        let updates = r.json["result"].as_array().cloned().unwrap_or_default();
        for up in &updates {
            if let Some(id) = up["update_id"].as_i64() {
                state["offset"] = json!(id + 1);
            }
            handle(
                &hub,
                &c,
                me,
                &username,
                &rooms,
                &up["message"],
                &mut ids,
                &mut ignored,
            );
        }
        if !updates.is_empty() {
            save_state(&dir, "telegram", &state);
        }
        if c.poll_timeout == 0 {
            nap(&hub, Duration::from_millis(200));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle(
    hub: &Hub,
    _c: &TelegramConf,
    me: i64,
    username: &str,
    rooms: &[(i64, String)],
    m: &Value,
    ids: &mut IdMap,
    ignored: &mut HashSet<i64>,
) {
    let Some(chat) = m["chat"]["id"].as_i64() else {
        return;
    };
    let Some(room) = rooms.iter().find(|(c, _)| *c == chat).map(|(_, r)| r) else {
        if ignored.insert(chat) {
            log(&format!(
                "telegram: ignored a message from chat {chat} ({}); add it to bridges.telegram.chats to let Tempo in",
                m["chat"]["title"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| person(&m["chat"]))
            ));
        }
        return;
    };
    if m["from"]["is_bot"] == true {
        return;
    }
    let Some(raw) = m["text"].as_str().or(m["caption"].as_str()) else {
        return;
    };
    let Some(mid) = m["message_id"].as_i64() else {
        return;
    };
    let private = m["chat"]["type"] == "private";
    let text = normalize(raw, username);
    let to_tempo = private
        || text != raw
        || crate::bot::mentions_tempo(&text)
        || text.trim_start().starts_with('/')
        || m["reply_to_message"]["from"]["id"].as_i64() == Some(me);
    let name = person(&m["from"]);
    let mut r = PostReq::new(
        room,
        Speaker::bridged(
            "telegram",
            &m["from"]["id"].as_i64().unwrap_or(0).to_string(),
            &name,
        ),
        &text,
        "telegram",
    );
    let replied = &m["reply_to_message"];
    r.reply_to = replied["message_id"]
        .as_i64()
        .and_then(|x| ids.get(&x.to_string()));
    r.reply_hint = replied["text"]
        .as_str()
        .or(replied["caption"].as_str())
        .map(|t| (person(&replied["from"]), normalize(t, username)));
    if to_tempo {
        r.target = Some(Target::Telegram { chat, message: mid });
        r.force_ask = true;
    }
    match hub.post(r) {
        Ok(Posted::Msg(local)) => ids.put(mid.to_string(), local.id),
        Ok(Posted::Note(_)) => {}
        Err(e) => log(&format!(
            "telegram: a message from {name} was not mirrored: {}",
            e.1
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_names_become_tempo() {
        assert_eq!(
            normalize(
                "hey @PerennosTempoBot, and @perennostempobot!",
                "PerennosTempoBot"
            ),
            "hey @tempo, and @tempo!"
        );
        assert_eq!(normalize("no mention", "Bot"), "no mention");
        assert_eq!(
            person(&serde_json::json!({"first_name": "Mia", "last_name": "R"})),
            "Mia R"
        );
        assert_eq!(person(&serde_json::json!({"username": "sam"})), "sam");
    }
}
