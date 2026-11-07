//! WhatsApp Cloud API (Untested against Meta: it needs a Meta app, a business phone number and a
//! public HTTPS webhook, for example Tailscale Funnel in front of the webhook port).
//!
//! The webhook has its own loopback port (`bridges.whatsapp.webhook_port`, default 8768) so only it
//! is exposed, never the room. `GET /room/whatsapp` answers Meta's verification
//! (`hub.mode=subscribe`, `hub.verify_token`, `hub.challenge`); `POST /room/whatsapp` must carry a
//! valid `X-Hub-Signature-256` (HMAC-SHA256 of the body with the app secret) or it is refused.
//! WhatsApp chats are one to one, so every text from an allowed number is for Tempo; the reply goes
//! back with `POST /{phone-number-id}/messages` and `context.message_id`.
use super::{Http, Target, WhatsAppConf};
use crate::hub::{Hub, PostReq, Speaker};
use crate::util::{cap, ct_eq, hmac_sha256_hex, log};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Meta's webhook check: echo the challenge when the verify token matches.
pub fn verify(c: &WhatsAppConf, q: &HashMap<String, String>) -> (u16, String) {
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    let challenge = get("hub.challenge");
    let safe = !challenge.is_empty()
        && challenge.len() <= 128
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if get("hub.mode") == "subscribe" && ct_eq(get("hub.verify_token"), &c.verify_token) && safe {
        (200, challenge.to_string())
    } else {
        (403, "forbidden".into())
    }
}

pub fn signature_ok(secret: &str, body: &[u8], header: &str) -> bool {
    match header.trim().strip_prefix("sha256=") {
        Some(h) => {
            !secret.is_empty() && ct_eq(&h.to_ascii_lowercase(), &hmac_sha256_hex(secret, body))
        }
        None => false,
    }
}

#[derive(Debug, PartialEq)]
pub struct Inbound {
    pub from: String,
    pub name: String,
    pub id: String,
    pub text: String,
    /// Meta's send time (unix seconds), when it gave one.
    pub ts: Option<u64>,
}

/// Longest a delivery may be late before it is treated as a replay (Meta sends at once).
const MAX_AGE_SECS: f64 = 24.0 * 3600.0;
/// Clock difference allowed for a message from "the future".
const MAX_SKEW_SECS: f64 = 600.0;

/// A signed call can be captured and sent again later: the signature has no time in it, so a
/// message older than a day, or from far in the future, is dropped.
pub fn fresh(ts: Option<u64>, now: f64) -> bool {
    match ts {
        Some(t) => {
            let t = t as f64;
            now - t <= MAX_AGE_SECS && t - now <= MAX_SKEW_SECS
        }
        None => false,
    }
}

/// More ids than a family sends in a day; past this the oldest are let go.
const MAX_SEEN: usize = 20_000;

/// Message ids already answered, with their WhatsApp time. Each is kept for as long as a message
/// counts as fresh, so a replay inside that window is caught. They are saved with the bridge's
/// state (`bridge-whatsapp.json`), so a restart does not forget them (SR-20).
pub struct Seen {
    dir: Option<PathBuf>,
    ids: Vec<(String, u64)>,
}

impl Seen {
    /// The ids an earlier run saved in `dir` (none without a folder or a file).
    pub fn load(dir: Option<&Path>) -> Seen {
        let saved = dir.map(|d| super::load_state(d, "whatsapp"));
        let ids = saved
            .as_ref()
            .and_then(|v| v["seen"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|x| Some((x[0].as_str()?.to_string(), x[1].as_u64()?)))
            .collect();
        Seen {
            dir: dir.map(Path::to_path_buf),
            ids,
        }
    }

    /// True the first time `id` comes, and it is remembered; false when it was answered before.
    pub fn first_time(&mut self, id: &str, ts: u64, now: f64) -> bool {
        // older than any message `fresh` lets through: no longer needed to catch a replay
        self.ids
            .retain(|(_, t)| now - *t as f64 <= MAX_AGE_SECS + MAX_SKEW_SECS);
        if self.ids.iter().any(|(x, _)| x == id) {
            return false;
        }
        self.ids.push((id.to_string(), ts));
        if self.ids.len() > MAX_SEEN {
            self.ids.remove(0);
        }
        if let Some(d) = &self.dir {
            let mut state = super::load_state(d, "whatsapp");
            state["seen"] = Value::Array(self.ids.iter().map(|(i, t)| json!([i, t])).collect());
            super::save_state(d, "whatsapp", &state);
        }
        true
    }
}

/// Text messages in a webhook call (other types, and status updates, are skipped).
pub fn parse(v: &Value) -> Vec<Inbound> {
    let mut out = vec![];
    for e in v["entry"].as_array().into_iter().flatten() {
        for ch in e["changes"].as_array().into_iter().flatten() {
            let val = &ch["value"];
            let names: HashMap<&str, &str> = val["contacts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| Some((c["wa_id"].as_str()?, c["profile"]["name"].as_str()?)))
                .collect();
            for m in val["messages"].as_array().into_iter().flatten() {
                let (Some(from), Some(id)) = (m["from"].as_str(), m["id"].as_str()) else {
                    continue;
                };
                let Some(text) = m["text"]["body"].as_str() else {
                    continue;
                };
                out.push(Inbound {
                    from: from.to_string(),
                    name: names.get(from).copied().unwrap_or(from).to_string(),
                    id: id.to_string(),
                    text: text.to_string(),
                    ts: m["timestamp"]
                        .as_str()
                        .and_then(|t| t.parse().ok())
                        .or(m["timestamp"].as_u64()),
                });
            }
        }
    }
    out
}

/// Handle one webhook POST. Returns the HTTP status for Meta.
pub fn inbound(hub: &Hub, sig: &str, body: &[u8], seen: &Mutex<Seen>) -> u16 {
    let Some(c) = &hub.bridges.whatsapp else {
        return 404;
    };
    if !signature_ok(&c.app_secret, body, sig) {
        log("whatsapp: refused a webhook call without a valid signature");
        return 403;
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return 400;
    };
    for m in parse(&v) {
        if !fresh(m.ts, crate::util::unix_now()) {
            log("whatsapp: dropped a message that is too old (or has no time): a replayed call");
            continue;
        }
        let first = seen.lock().unwrap_or_else(|e| e.into_inner()).first_time(
            &m.id,
            m.ts.unwrap_or(0),
            crate::util::unix_now(),
        );
        if !first {
            continue; // Meta retries, or a replay (even after a restart): answer once
        }
        let Some(room) = c
            .numbers
            .iter()
            .find(|(n, _)| *n == m.from)
            .and_then(|(_, r)| super::ensure_room(hub, r))
        else {
            log(&format!(
                "whatsapp: ignored a message from a number not in bridges.whatsapp.numbers (ending {})",
                &m.from[m.from.len().saturating_sub(4)..]
            ));
            continue;
        };
        let mut r = PostReq::new(
            &room,
            Speaker::bridged("whatsapp", &m.from, &m.name),
            &m.text,
            "whatsapp",
        );
        r.target = Some(Target::WhatsApp {
            to: m.from.clone(),
            message: m.id.clone(),
        });
        r.force_ask = true;
        if let Err(e) = hub.post(r) {
            log(&format!("whatsapp: a message was not mirrored: {}", e.1));
        }
    }
    200
}

pub fn reply(
    c: &WhatsAppConf,
    http: &Http,
    to: &str,
    message: &str,
    text: &str,
) -> Result<(), String> {
    let url = format!("{}/{}/messages", c.api, c.phone_number_id);
    let body = json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "text",
        "text": {"preview_url": false, "body": cap(text, 4000)},
        "context": {"message_id": message},
    });
    let r = http.with_retry(
        "POST",
        &url,
        Some(&format!("Bearer {}", c.token)),
        Some(&body),
        Duration::from_secs(20),
        "whatsapp send",
    )?;
    if (200..300).contains(&r.status) {
        Ok(())
    } else {
        Err(format!(
            "whatsapp send: HTTP {} {}",
            r.status,
            r.json["error"]["message"].as_str().unwrap_or("")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conf() -> WhatsAppConf {
        WhatsAppConf {
            token: "t".into(),
            token_from: "TEMPO_WHATSAPP_TOKEN".into(),
            phone_number_id: "1055".into(),
            verify_token: "my-verify-words".into(),
            app_secret: "s3cret-app".into(),
            numbers: vec![("15551230000".into(), "family".into())],
            api: "http://127.0.0.1:9".into(),
            webhook_port: 0,
            public_host: String::new(),
        }
    }

    #[test]
    fn answered_ids_survive_a_restart_for_as_long_as_a_message_is_fresh() {
        let d = crate::util::testdir::tmpdir("wa-seen");
        // other state the bridge keeps in the same file stays
        super::super::save_state(&d, "whatsapp", &json!({"other": 1}));
        let now = 1_793_966_400.0;
        let mut s = Seen::load(Some(&d));
        assert!(s.first_time("wamid.A", now as u64, now));
        assert!(
            !s.first_time("wamid.A", now as u64, now + 1.0),
            "Meta's retry"
        );
        // a restart: the id is still known, so a replay inside the day is not answered again
        let mut again = Seen::load(Some(&d));
        assert!(!again.first_time("wamid.A", now as u64, now + 3600.0));
        assert!(again.first_time("wamid.B", now as u64 + 3600, now + 3600.0));
        let saved = super::super::load_state(&d, "whatsapp");
        assert_eq!(saved["other"], 1);
        assert_eq!(saved["seen"].as_array().unwrap().len(), 2);
        // once a message would be too old to pass `fresh`, its id is let go
        let mut later = Seen::load(Some(&d));
        assert!(later.first_time("wamid.C", now as u64 + 2 * 86_400, now + 2.0 * 86_400.0));
        let saved = super::super::load_state(&d, "whatsapp");
        assert_eq!(saved["seen"], json!([["wamid.C", now as u64 + 2 * 86_400]]));
        // without a folder nothing is written, and it still answers once
        let mut mem = Seen::load(None);
        assert!(mem.first_time("x", 1, 1.0) && !mem.first_time("x", 1, 1.0));
    }

    #[test]
    fn webhook_verification() {
        let c = conf();
        let q = |m: &str, t: &str, ch: &str| -> HashMap<String, String> {
            [
                ("hub.mode", m),
                ("hub.verify_token", t),
                ("hub.challenge", ch),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
        };
        assert_eq!(
            verify(&c, &q("subscribe", "my-verify-words", "1158201444")),
            (200, "1158201444".into())
        );
        assert_eq!(verify(&c, &q("subscribe", "wrong", "1")).0, 403);
        assert_eq!(verify(&c, &q("unsubscribe", "my-verify-words", "1")).0, 403);
        assert_eq!(
            verify(&c, &q("subscribe", "my-verify-words", "<script>")).0,
            403,
            "never reflect markup"
        );
    }

    #[test]
    fn inbound_parsing_and_signatures() {
        let body = json!({"object": "whatsapp_business_account", "entry": [{"id": "1", "changes": [{"field": "messages", "value": {
        "messaging_product": "whatsapp",
        "metadata": {"display_phone_number": "15550000000", "phone_number_id": "1055"},
        "contacts": [{"profile": {"name": "Mia"}, "wa_id": "15551230000"}],
        "messages": [
            {"from": "15551230000", "id": "wamid.A", "timestamp": "1793966401", "type": "text", "text": {"body": "what's for dinner?"}},
            {"from": "15551230000", "id": "wamid.B", "type": "image", "image": {"id": "x"}}
        ]}}]}]});
        assert_eq!(
            parse(&body),
            vec![Inbound {
                from: "15551230000".into(),
                name: "Mia".into(),
                id: "wamid.A".into(),
                text: "what's for dinner?".into(),
                ts: Some(1_793_966_401)
            }]
        );
        let raw = body.to_string();
        let good = format!("sha256={}", hmac_sha256_hex("s3cret-app", raw.as_bytes()));
        assert!(signature_ok("s3cret-app", raw.as_bytes(), &good));
        assert!(!signature_ok("other", raw.as_bytes(), &good));
        assert!(!signature_ok("s3cret-app", b"tampered", &good));
        assert!(!signature_ok("s3cret-app", raw.as_bytes(), "md5=abc"));
        assert!(!signature_ok("", raw.as_bytes(), "sha256="));
    }
}
