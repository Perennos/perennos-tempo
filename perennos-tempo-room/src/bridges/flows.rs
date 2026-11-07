//! The bridges end to end against fake platform APIs (local TcpListener servers) and the fake
//! agent: a mention gets an in-thread reply, a 429 is waited out, unallowed chats are ignored,
//! and no token ever reaches a log line or a file.
use super::*;
use crate::hub::{Hub, HubOpts};
use crate::store::{Kind, Store};
use crate::testutil::{all_text, FakeAgent, FakeHttp, Script, Seen};
use crate::util::test_log_lines;
use crate::util::testdir::tmpdir;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn hub_with(d: &std::path::Path, agent: &str, cfg: Value) -> Arc<Hub> {
    std::fs::write(d.join("config.json"), cfg.to_string()).unwrap();
    let st = Store::open(d).unwrap();
    let out = Outbound::from_config(&st.cfg.raw["bridges"]);
    Hub::start(
        st,
        agent,
        out,
        HubOpts {
            turn: crate::hub::tests::fast(),
            watch_agent: None,
            vault_flush: Duration::from_millis(50),
            vault_bin: None,
        },
    )
}

fn room_texts(hub: &Hub, room: &str) -> Vec<(String, Kind, String, String)> {
    hub.lock()
        .room(room)
        .map(|r| {
            r.tail
                .iter()
                .map(|m| (m.name.clone(), m.kind, m.via.clone(), m.text.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn no_leak(token: &str, d: &std::path::Path) {
    assert!(
        !test_log_lines().iter().any(|l| l.contains(token)),
        "a log line carries the token"
    );
    // The token lives in the environment only: no file, config.json included, may carry it.
    let text = all_text(d);
    assert!(!text.contains(token), "a room file carries the token");
    assert!(text.contains("token_env"), "the scan reads the files");
}

fn ok(v: Value) -> crate::testutil::Reply {
    (200, vec![], v.to_string())
}

#[test]
fn discord_mention_is_answered_in_thread() {
    let agent = FakeAgent::start(vec![Script::Answer("Four, last I checked.".into())]);
    // A Discord-shaped bot token, built at run time.
    let token = format!(
        "{}.{}.{}",
        "MTA5ODc2NTQzMjEwOTg3NjU0Mw",
        "Gx1y2Z",
        "q".repeat(32)
    );
    let tk = token.clone();
    let first_batch = Arc::new(AtomicBool::new(true));
    let limited = Arc::new(AtomicBool::new(true));
    let fake = FakeHttp::start(move |r: &Seen| {
        if r.header("authorization") != Some(format!("Bot {tk}").as_str()) {
            return (
                401,
                vec![],
                json!({"message": "401: Unauthorized", "code": 0}).to_string(),
            );
        }
        let p = r.path.as_str();
        match r.method.as_str() {
            "GET" if p == "/api/v10/users/@me" => {
                ok(json!({"id": "999", "username": "tempo", "bot": true}))
            }
            "GET" if p.starts_with("/api/v10/channels/111/messages?limit=1") => ok(json!([
                {"id": "100", "type": 0, "content": "old history, never answered <@999>", "author": {"id": "5", "username": "mia"}}
            ])),
            "GET"
                if p.starts_with("/api/v10/channels/111/messages?after=100")
                    && first_batch.swap(false, Ordering::SeqCst) =>
            {
                ok(json!([
                    {"id": "104", "type": 0, "content": "an earlier reply from the bot", "author": {"id": "999", "username": "tempo", "bot": true}},
                    {"id": "103", "type": 0, "content": "beep", "author": {"id": "77", "username": "otherbot", "bot": true}},
                    {"id": "102", "type": 0, "content": "just chatting", "author": {"id": "6", "username": "bob", "global_name": "Bob"}},
                    {"id": "101", "type": 0, "content": "<@999> what is 2+2?", "author": {"id": "5", "username": "mia", "global_name": "Mia"}, "mentions": [{"id": "999", "username": "tempo"}]}
                ]))
            }
            "GET" if p.starts_with("/api/v10/channels/111/messages") => ok(json!([])),
            "GET" if p.starts_with("/api/v10/channels/222/") => (
                403,
                vec![],
                json!({"message": "Missing Access", "code": 50001}).to_string(),
            ),
            "POST" if p == "/api/v10/channels/111/typing" => (204, vec![], String::new()),
            "POST" if p == "/api/v10/channels/111/messages" => {
                if limited.swap(false, Ordering::SeqCst) {
                    (429, vec![("Retry-After".into(), "0.3".into())], json!({"message": "You are being rate limited.", "retry_after": 0.3, "global": false}).to_string())
                } else {
                    ok(json!({"id": "200"}))
                }
            }
            _ => (404, vec![], json!({"message": "Unknown"}).to_string()),
        }
    });
    let d = tmpdir("discord-flow");
    crate::bridges::test_env("TEMPO_T_FLOW_DISCORD", &token);
    let hub = hub_with(
        &d,
        &agent.url(),
        json!({"bridges": {"discord": {"enabled": true, "token_env": "TEMPO_T_FLOW_DISCORD", "channels": {"111": "Family", "222": "Secret"}, "api": format!("{}/api/v10", fake.url), "poll_secs": 0.05}}}),
    );
    let Setting::On(conf) = discord_conf(&hub.lock().cfg.raw["bridges"].clone()) else {
        panic!("discord settings")
    };
    let h = hub.clone();
    std::thread::spawn(move || discord::run(h, conf));
    let reqs = fake.wait_for("the reply", |r| {
        r.iter()
            .filter(|s| s.method == "POST" && s.path == "/api/v10/channels/111/messages")
            .count()
            >= 2
    });
    let posts: Vec<&Seen> = reqs
        .iter()
        .filter(|s| s.method == "POST" && s.path == "/api/v10/channels/111/messages")
        .collect();
    assert!(
        posts[1].at.duration_since(posts[0].at) >= Duration::from_millis(290),
        "retry_after was honoured"
    );
    let b = posts[1].json();
    assert!(
        posts[1]
            .header("user-agent")
            .is_some_and(|u| u.starts_with("DiscordBot (https://perennos.com, ")),
        "Discord's User-Agent shape"
    );
    assert_eq!(b["content"], "Four, last I checked.");
    assert_eq!(b["message_reference"]["message_id"], "101");
    assert_eq!(b["allowed_mentions"]["parse"], json!([]), "never @everyone");
    assert!(reqs
        .iter()
        .any(|s| s.path == "/api/v10/channels/111/typing"));
    let t0 = Instant::now();
    while !room_texts(&hub, "family")
        .iter()
        .any(|m| m.1 == Kind::Tempo)
    {
        assert!(t0.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    let family = room_texts(&hub, "family");
    let texts: Vec<&str> = family.iter().map(|m| m.3.as_str()).collect();
    assert_eq!(
        texts,
        [
            "@tempo what is 2+2?",
            "just chatting",
            "Four, last I checked."
        ],
        "mirrored, history and bots skipped"
    );
    assert_eq!(
        (family[0].0.as_str(), family[0].2.as_str()),
        ("Mia", "discord")
    );
    assert_eq!(agent.prompts().len(), 1, "only the mention went to Tempo");
    assert!(
        agent.prompts()[0].starts_with("[Perennos Tempo room #family] Mia: @tempo what is 2+2?")
    );
    assert!(test_log_lines()
        .iter()
        .any(|l| l.contains("channel 222 answered HTTP 403")));
    no_leak(&token, &d);
    hub.shutdown();
}

#[test]
fn telegram_answers_mentions_and_dms_and_ignores_strangers() {
    let agent = FakeAgent::start(vec![
        Script::Answer("Yes, on a clear day.".into()),
        Script::Answer("Time for tea.".into()),
    ]);
    let token = format!("{}:{}", 123_456_789, "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw");
    let base = format!("/bot{token}");
    let served = Arc::new(AtomicU32::new(0));
    let limited = Arc::new(AtomicBool::new(true));
    let fake = FakeHttp::start(move |r: &Seen| {
        let Some(m) = r.path.strip_prefix(&base) else {
            return (
                404,
                vec![],
                json!({"ok": false, "error_code": 404, "description": "Not Found"}).to_string(),
            );
        };
        let msg = |id: i64, chat: Value, from: Value, text: &str| json!({"message_id": id, "chat": chat, "from": from, "date": 1_793_966_401, "text": text});
        let family = json!({"id": -1001, "type": "supergroup", "title": "Family"});
        match m.split('?').next().unwrap_or("") {
            "/getMe" => ok(
                json!({"ok": true, "result": {"id": 777, "is_bot": true, "username": "PerennosTempoBot"}}),
            ),
            "/getUpdates" => {
                if served.fetch_add(1, Ordering::SeqCst) == 0 {
                    ok(json!({"ok": true, "result": [
                        {"update_id": 10, "message": msg(5, family.clone(), json!({"id": 1, "first_name": "Mia"}), "@PerennosTempoBot is the sky blue?")},
                        {"update_id": 11, "message": msg(6, json!({"id": -999, "type": "group", "title": "Strangers"}), json!({"id": 9, "first_name": "Eve"}), "@PerennosTempoBot hi")},
                        {"update_id": 12, "message": msg(7, family.clone(), json!({"id": 2, "first_name": "Bob"}), "just chatting")},
                        {"update_id": 13, "message": msg(8, json!({"id": 42, "type": "private", "first_name": "Riley"}), json!({"id": 42, "first_name": "Riley"}), "what time is it?")}
                    ]}))
                } else {
                    std::thread::sleep(Duration::from_millis(30));
                    ok(json!({"ok": true, "result": []}))
                }
            }
            "/sendChatAction" => ok(json!({"ok": true, "result": true})),
            "/sendMessage" => {
                if limited.swap(false, Ordering::SeqCst) {
                    (429, vec![], json!({"ok": false, "error_code": 429, "description": "Too Many Requests: retry after 1", "parameters": {"retry_after": 1}}).to_string())
                } else {
                    ok(json!({"ok": true, "result": {"message_id": 99}}))
                }
            }
            _ => (404, vec![], json!({"ok": false}).to_string()),
        }
    });
    let d = tmpdir("telegram-flow");
    crate::bridges::test_env("TEMPO_T_FLOW_TELEGRAM", &token);
    let hub = hub_with(
        &d,
        &agent.url(),
        json!({"bridges": {"telegram": {"enabled": true, "token_env": "TEMPO_T_FLOW_TELEGRAM", "chats": {"-1001": "Family", "42": "Riley DM"}, "api": fake.url, "poll_timeout": 0}}}),
    );
    let Setting::On(conf) = telegram_conf(&hub.lock().cfg.raw["bridges"].clone()) else {
        panic!("telegram settings")
    };
    let h = hub.clone();
    std::thread::spawn(move || telegram::run(h, conf));
    let is_send = |s: &Seen| s.path.ends_with("/sendMessage");
    let reqs = fake.wait_for("two replies", |r| {
        r.iter().filter(|s| is_send(s)).count() >= 3
    });
    let sends: Vec<Value> = reqs.iter().filter(|s| is_send(s)).map(Seen::json).collect();
    let times: Vec<Instant> = reqs.iter().filter(|s| is_send(s)).map(|s| s.at).collect();
    assert!(
        times[1].duration_since(times[0]) >= Duration::from_millis(990),
        "retry_after 1 s honoured"
    );
    assert_eq!(sends[1]["chat_id"], -1001);
    assert_eq!(sends[1]["reply_to_message_id"], 5);
    assert_eq!(sends[1]["text"], "Yes, on a clear day.");
    assert_eq!(sends[2]["chat_id"], 42);
    assert_eq!(sends[2]["reply_to_message_id"], 8);
    assert_eq!(sends[2]["text"], "Time for tea.");
    assert!(
        !sends.iter().any(|s| s["chat_id"] == -999),
        "strangers get nothing"
    );
    assert!(
        reqs.iter()
            .any(|s| s.path.contains("getUpdates") && s.path.contains("offset=14")),
        "the offset moved on"
    );
    assert!(reqs
        .iter()
        .all(|s| !s.path.contains("getUpdates") || s.path.contains("timeout=0")));
    let family = room_texts(&hub, "family");
    let texts: Vec<&str> = family.iter().map(|m| m.3.as_str()).collect();
    assert!(
        texts.contains(&"@tempo is the sky blue?") && texts.contains(&"just chatting"),
        "{texts:?}"
    );
    assert!(!texts.contains(&"@tempo hi"));
    assert!(
        hub.lock().find_room("Strangers").is_none(),
        "no room for an unallowed chat"
    );
    assert_eq!(
        agent.prompts().len(),
        2,
        "the group mention and the DM, not the chatter"
    );
    assert!(test_log_lines()
        .iter()
        .any(|l| l.contains("ignored a message from chat -999 (Strangers)")));
    no_leak(&token, &d);
    hub.shutdown();
}

#[test]
fn whatsapp_webhook_verifies_signs_and_replies() {
    let agent = FakeAgent::start(vec![Script::Answer("Dinner is at 7.".into())]);
    let token = format!("EAA{}", "Gm0d3lT0k3n".repeat(8));
    let tk = token.clone();
    let graph = FakeHttp::start(move |r: &Seen| {
        if r.header("authorization") != Some(format!("Bearer {tk}").as_str()) {
            return (
                401,
                vec![],
                json!({"error": {"message": "bad token"}}).to_string(),
            );
        }
        if r.method == "POST" && r.path == "/1055/messages" {
            ok(json!({"messaging_product": "whatsapp", "messages": [{"id": "wamid.R"}]}))
        } else {
            (404, vec![], "{}".into())
        }
    });
    let d = tmpdir("whatsapp-flow");
    let secret = "app-secret-for-the-test";
    crate::bridges::test_env("TEMPO_T_FLOW_WA_TOKEN", &token);
    crate::bridges::test_env("TEMPO_T_FLOW_WA_VERIFY", "verify-words-123");
    crate::bridges::test_env("TEMPO_T_FLOW_WA_SECRET", secret);
    let hub = hub_with(
        &d,
        &agent.url(),
        json!({"bridges": {"whatsapp": {"enabled": true, "token_env": "TEMPO_T_FLOW_WA_TOKEN", "phone_number_id": "1055", "verify_token_env": "TEMPO_T_FLOW_WA_VERIFY", "app_secret_env": "TEMPO_T_FLOW_WA_SECRET", "numbers": {"+1 555 123 0000": "Family"}, "api": graph.url, "webhook_port": 0}}}),
    );
    let r = crate::server::start(
        hub.clone(),
        crate::server::ServeOpts {
            port: 0,
            ..Default::default()
        },
    )
    .unwrap();
    let w = r.webhook.expect("the webhook listener");
    let raw = |req: String| -> (u16, String) {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(w).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        let code = out
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (
            code,
            out.split_once("\r\n\r\n")
                .map(|x| x.1.to_string())
                .unwrap_or_default(),
        )
    };
    let get = |q: &str| {
        raw(format!(
            "GET /room/whatsapp?{q} HTTP/1.1\r\nHost: {w}\r\n\r\n"
        ))
    };
    assert_eq!(
        get("hub.mode=subscribe&hub.verify_token=verify-words-123&hub.challenge=8675309"),
        (200, "8675309".into())
    );
    assert_eq!(
        get("hub.mode=subscribe&hub.verify_token=nope&hub.challenge=1").0,
        403
    );
    assert_eq!(
        raw(format!("GET /room/health HTTP/1.1\r\nHost: {w}\r\n\r\n")).0,
        404,
        "the webhook port serves only the webhook"
    );
    let now = crate::util::unix_now() as u64;
    let body = |id: &str, from: &str| {
        json!({"object": "whatsapp_business_account", "entry": [{"id": "1", "changes": [{"field": "messages", "value": {
        "messaging_product": "whatsapp", "metadata": {"phone_number_id": "1055"},
        "contacts": [{"profile": {"name": "Mia"}, "wa_id": from}],
        "messages": [{"from": from, "id": id, "timestamp": now.to_string(), "type": "text", "text": {"body": "@tempo when is dinner?"}}]}}]}]}).to_string()
    };
    let post = |b: &str, sig: &str| {
        raw(format!("POST /room/whatsapp HTTP/1.1\r\nHost: {w}\r\nContent-Type: application/json\r\nX-Hub-Signature-256: {sig}\r\nContent-Length: {}\r\n\r\n{b}", b.len())).0
    };
    let good = body("wamid.A", "15551230000");
    let sig = format!(
        "sha256={}",
        crate::util::hmac_sha256_hex(secret, good.as_bytes())
    );
    assert_eq!(post(&good, "sha256=00"), 403, "bad signature");
    assert_eq!(post(&good, &sig), 200);
    assert_eq!(post(&good, &sig), 200, "Meta retries are accepted...");
    let stranger = body("wamid.B", "15559990000");
    let ssig = format!(
        "sha256={}",
        crate::util::hmac_sha256_hex(secret, stranger.as_bytes())
    );
    assert_eq!(post(&stranger, &ssig), 200);
    let reqs = graph.wait_for("the reply", |r| !r.is_empty());
    std::thread::sleep(Duration::from_millis(300));
    let reqs2 = graph.requests();
    assert_eq!(
        reqs2.len(),
        1,
        "...but answered once, and strangers not at all: {reqs:?}"
    );
    let b = reqs2[0].json();
    assert_eq!(b["to"], "15551230000");
    assert_eq!(b["context"]["message_id"], "wamid.A");
    assert_eq!(b["text"]["body"], "Dinner is at 7.");
    assert_eq!(b["messaging_product"], "whatsapp");
    let family = room_texts(&hub, "family");
    assert_eq!(family[0].3, "@tempo when is dinner?");
    assert_eq!(family[0].2, "whatsapp");
    // A signed call captured earlier and sent again later (a replay) is dropped: the signature
    // is valid, the message id is new, but the message is days old.
    let before = room_texts(&hub, "family").len();
    let old =
        body("wamid.OLD", "15551230000").replace(&now.to_string(), &(now - 3 * 86_400).to_string());
    let osig = format!(
        "sha256={}",
        crate::util::hmac_sha256_hex(secret, old.as_bytes())
    );
    assert_eq!(post(&old, &osig), 200);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        room_texts(&hub, "family").len(),
        before,
        "a replayed call is not posted"
    );
    assert_eq!(graph.requests().len(), 1, "and not answered");
    no_leak(&token, &d);
    no_leak(secret, &d);
    r.stop();
    hub.shutdown();
}

#[test]
fn a_busy_webhook_port_turns_whatsapp_off_not_the_room() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let d = tmpdir("whatsapp-busy");
    crate::bridges::test_env("TEMPO_T_BUSY_WA_TOKEN", "t0k3n-for-the-busy-test");
    crate::bridges::test_env("TEMPO_T_BUSY_WA_VERIFY", "verify-words-123");
    crate::bridges::test_env("TEMPO_T_BUSY_WA_SECRET", "s3cret-busy-test");
    let hub = hub_with(
        &d,
        &crate::testutil::dead_url(),
        json!({"bridges": {"whatsapp": {"enabled": true, "token_env": "TEMPO_T_BUSY_WA_TOKEN", "phone_number_id": "1055", "verify_token_env": "TEMPO_T_BUSY_WA_VERIFY", "app_secret_env": "TEMPO_T_BUSY_WA_SECRET", "numbers": {"15551230000": "Family"}, "api": "http://127.0.0.1:9", "webhook_port": port}}}),
    );
    let r = crate::server::start(
        hub.clone(),
        crate::server::ServeOpts {
            port: 0,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(r.webhook.is_none());
    assert!(test_log_lines()
        .iter()
        .any(|l| l.contains(&format!("cannot listen on 127.0.0.1:{port}"))));
    let a = r.addr;
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(a).unwrap();
    s.write_all(format!("GET /room/health HTTP/1.1\r\nHost: {a}\r\n\r\n").as_bytes())
        .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    drop(taken);
    r.stop();
    hub.shutdown();
}

#[test]
fn refused_tokens_turn_a_bridge_off_and_the_room_runs_on() {
    let fake = FakeHttp::start(|r: &Seen| {
        if r.path.contains("getMe") {
            (
                401,
                vec![],
                json!({"ok": false, "error_code": 401, "description": "Unauthorized"}).to_string(),
            )
        } else {
            (
                401,
                vec![],
                json!({"message": "401: Unauthorized", "code": 0}).to_string(),
            )
        }
    });
    let d = tmpdir("refused");
    let tg = format!("{}:{}", 111_222_333, "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDzzz");
    let dc = format!(
        "{}.{}.{}",
        "MTA5ODc2NTQzMjEwOTg3NjU0Nw",
        "Hx1y2Z",
        "w".repeat(32)
    );
    crate::bridges::test_env("TEMPO_T_REFUSED_TELEGRAM", &tg);
    crate::bridges::test_env("TEMPO_T_REFUSED_DISCORD", &dc);
    let hub = hub_with(
        &d,
        &crate::testutil::dead_url(),
        json!({"bridges": {
            "telegram": {"enabled": true, "token_env": "TEMPO_T_REFUSED_TELEGRAM", "chats": {"-100": "Family"}, "api": fake.url, "poll_timeout": 0},
            "discord": {"enabled": true, "token_env": "TEMPO_T_REFUSED_DISCORD", "channels": {"1": "Family"}, "api": format!("{}/api/v10", fake.url)}
        }}),
    );
    let b = hub.lock().cfg.raw["bridges"].clone();
    let (Setting::On(t), Setting::On(c)) = (telegram_conf(&b), discord_conf(&b)) else {
        panic!("settings")
    };
    let (h1, h2) = (hub.clone(), hub.clone());
    let a = std::thread::spawn(move || telegram::run(h1, t));
    let b2 = std::thread::spawn(move || discord::run(h2, c));
    a.join().unwrap();
    b2.join().unwrap();
    let logs = test_log_lines();
    assert!(logs
        .iter()
        .any(|l| l.contains("telegram: the bot token was refused")));
    assert!(logs
        .iter()
        .any(|l| l.contains("discord: the bot token was refused (401)")));
    let p = hub.post(crate::hub::PostReq::new(
        "family",
        crate::hub::Speaker::owner("Owner"),
        "still here",
        "app",
    ));
    assert!(
        p.is_err(),
        "no room yet: the bridges never got far enough to make it"
    );
    hub.lock()
        .create_room(
            "Family",
            crate::store::Mode::Mention,
            crate::store::Personality::Warm,
        )
        .unwrap();
    assert!(hub
        .post(crate::hub::PostReq::new(
            "family",
            crate::hub::Speaker::owner("Owner"),
            "still here",
            "app"
        ))
        .is_ok());
    no_leak(&tg, &d);
    no_leak(&dc, &d);
    hub.shutdown();
}
