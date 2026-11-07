//! The `tempo-room` command line. Commands that change rooms go through the running server when
//! there is one (so everyone sees the change live) and straight to the data folder when not.
use crate::bridges::{self, Outbound, Setting};
use crate::hub::{Hub, HubOpts};
use crate::mcp::{control_key, live_server, owner_post};
use crate::server::{self, ServeOpts};
use crate::store::{data_dir, Kind, Mode, Msg, Personality, Store, DEFAULT_PORT};
use crate::util::{log, read_json, scrub, unix_now, write_atomic};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

pub const USAGE: &str =
    "Perennos Tempo Room: a private group chat where Perennos Tempo answers when you write @tempo.

usage: tempo-room <command> [--data DIR] [options]
       (DIR: --data, else TEMPO_ROOM_DATA, else <Tempo home>/room: TEMPO_HOME, else ~/.tempo)

  serve [--port 8767] [--bind 127.0.0.1] [--allow-host H]... [--agent http://127.0.0.1:8765]
        [--public-url https://NAME]
                                 the room app and its API: only this machine unless --bind;
                                 --public-url when tailscale serve (or another proxy) is in front
  owner-link                     the link that signs you in as the owner (open it on this machine)
  rooms                          the rooms and how busy they are
  rooms add NAME                 make a room
  invite ROOM [--hours 48]       a one-time invite (link and code); makes the room if it is new
  post ROOM TEXT [--as-tempo]    post as the owner, or as Tempo
  bridges check [--live]         which bridges are on and ready (tokens are never printed)
  mcp                            the rooms as MCP tools on stdio: room_list, room_read, room_post
  version

Add --json for machine-readable output.";

const VALUE_FLAGS: &[&str] = &[
    "--data",
    "--port",
    "--bind",
    "--allow-host",
    "--agent",
    "--hours",
    "--public-url",
];
const BOOL_FLAGS: &[&str] = &["--json", "--live", "--as-tempo", "--help"];

struct Args {
    pos: Vec<String>,
    val: HashMap<String, Vec<String>>,
    on: HashSet<String>,
}

impl Args {
    fn get(&self, k: &str) -> Option<&str> {
        self.val.get(k).and_then(|v| v.last()).map(String::as_str)
    }
    fn all(&self, k: &str) -> Vec<String> {
        self.val.get(k).cloned().unwrap_or_default()
    }
    fn has(&self, k: &str) -> bool {
        self.on.contains(k)
    }
    fn pos(&self, i: usize, what: &str) -> Result<&str, String> {
        self.pos
            .get(i)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {what} (see tempo-room --help)"))
    }
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        pos: vec![],
        val: HashMap::new(),
        on: HashSet::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let s = &args[i];
        if let Some((k, v)) = s.split_once('=').filter(|(k, _)| VALUE_FLAGS.contains(k)) {
            a.val.entry(k.to_string()).or_default().push(v.to_string());
        } else if VALUE_FLAGS.contains(&s.as_str()) {
            let v = args
                .get(i + 1)
                .ok_or_else(|| format!("{s} needs a value"))?;
            a.val.entry(s.clone()).or_default().push(v.clone());
            i += 1;
        } else if BOOL_FLAGS.contains(&s.as_str()) || s == "-h" {
            a.on.insert(if s == "-h" {
                "--help".into()
            } else {
                s.clone()
            });
        } else if s.starts_with("--") {
            return Err(format!("unknown option {s} (see tempo-room --help)"));
        } else {
            a.pos.push(s.clone());
        }
        i += 1;
    }
    Ok(a)
}

fn emit(
    out: &mut dyn Write,
    a: &Args,
    j: Value,
    text: impl FnOnce() -> String,
) -> Result<(), String> {
    let s = if a.has("--json") {
        j.to_string()
    } else {
        text()
    };
    writeln!(out, "{s}").map_err(|e| e.to_string())
}

pub fn run(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let a = parse(args)?;
    let cmd = a.pos.first().map(String::as_str).unwrap_or("");
    if a.has("--help") || cmd.is_empty() || cmd == "help" {
        writeln!(out, "{USAGE}").map_err(|e| e.to_string())?;
        return Ok(());
    }
    let dir = data_dir(a.get("--data"));
    match cmd {
        "version" => emit(
            out,
            &a,
            json!({"app": "tempo-room", "version": env!("CARGO_PKG_VERSION")}),
            || format!("tempo-room {}", env!("CARGO_PKG_VERSION")),
        ),
        "serve" => serve(&a, &dir),
        "mcp" => {
            let ctx = crate::mcp::Ctx { dir };
            let stdin = std::io::stdin();
            crate::mcp::run(&ctx, stdin.lock(), std::io::stdout()).map_err(|e| e.to_string())
        }
        "owner-link" => {
            // A new one-time link (older ones stop working). Through the running server when
            // there is one, so its copy of the settings is the one that changes.
            let link = match live_server(&dir) {
                Some(addr) => {
                    let v = owner_post(&addr, &control_key(&dir)?, "owner-link", &json!({}))?;
                    v["link"].as_str().unwrap_or("").to_string()
                }
                None => {
                    let port = match a.get("--port") {
                        Some(p) => p.parse::<u16>().map_err(|_| "--port is a number")?,
                        None => read_json(&dir.join("server.json"))
                            .and_then(|v| {
                                v["addr"]
                                    .as_str()
                                    .and_then(|s| s.rsplit(':').next()?.parse().ok())
                            })
                            .unwrap_or(DEFAULT_PORT),
                    };
                    let t = Store::open(&dir)?.new_owner_link()?;
                    format!("http://127.0.0.1:{port}/#owner={t}")
                }
            };
            emit(out, &a, json!({"link": link}), || {
                format!("Open this on this machine to sign in as the owner. It works once, for a week; keep it to yourself:\n  {link}")
            })
        }
        "rooms" => rooms(&a, &dir, out),
        "invite" => invite(&a, &dir, out),
        "post" => post(&a, &dir, out),
        "bridges" => match a.pos.get(1).map(String::as_str) {
            Some("check") | None => bridges_check(&a, &dir, out),
            Some(x) => Err(format!("bridges {x}? try: tempo-room bridges check")),
        },
        _ => Err(format!("unknown command {cmd} (see tempo-room --help)")),
    }
}

fn serve(a: &Args, dir: &Path) -> Result<(), String> {
    let mut store = Store::open(dir)?;
    // A one-time owner link when nobody can sign in yet (the first start, or every link and
    // session has expired).
    let first_link = store.ensure_owner_access()?;
    let agent = a
        .get("--agent")
        .map(str::to_string)
        .unwrap_or_else(|| store.cfg.agent.clone());
    if !agent.starts_with("http://") && !agent.starts_with("https://") {
        return Err("--agent is a URL like http://127.0.0.1:8765".into());
    }
    let bind: IpAddr = a
        .get("--bind")
        .unwrap_or("127.0.0.1")
        .parse()
        .map_err(|_| {
            "--bind is an IP address (127.0.0.1, your tailnet or LAN address, or 0.0.0.0)"
        })?;
    let port: u16 = match a.get("--port") {
        Some(p) => p.parse().map_err(|_| "--port is a number")?,
        None => DEFAULT_PORT,
    };
    let public_url = a
        .get("--public-url")
        .map(str::to_string)
        .or_else(|| store.cfg.raw["public_url"].as_str().map(str::to_string))
        .filter(|u| !u.trim().is_empty());
    let bridge_cfg = store.cfg.raw["bridges"].clone();
    let checks = bridges::check(&bridge_cfg);
    let outbound = Outbound::from_config(&bridge_cfg);
    let (dc, tg) = (outbound.discord.clone(), outbound.telegram.clone());
    let hub = Hub::start(store, &agent, outbound, HubOpts::default());
    // The CLI and MCP find the running server and its control key in the data folder.
    // (If it cannot be written the room still runs; only the CLI and MCP then cannot reach it.)
    match crate::store::new_control_key(dir) {
        Ok(k) => hub.set_control_key(&k),
        Err(e) => log(&format!(
            "warning: no control key ({e}); `tempo-room post`, `invite` and MCP cannot reach this server"
        )),
    }
    let r = server::start(
        hub.clone(),
        ServeOpts {
            bind,
            port,
            allow_hosts: a.all("--allow-host"),
            public_url,
        },
    )?;
    let port = r.addr.port();
    let own = format!("127.0.0.1:{port}");
    if let Err(e) = write_atomic(
        &dir.join("server.json"),
        json!({"addr": own, "listen": r.addr.to_string(), "pid": std::process::id(), "started": unix_now()}).to_string().as_bytes(),
    ) {
        log(&format!("cannot write server.json: {e}"));
    }
    eprintln!("[Perennos Tempo Room on http://{}/]", r.addr);
    if let Some(token) = first_link {
        eprintln!("Owner link, shown once, works once (`tempo-room owner-link` makes a new one). Open it on this machine:\n  http://{own}/#owner={token}");
    } else {
        eprintln!("Owner: open the link from `tempo-room owner-link` on this machine.");
    }
    if !bind.is_loopback() {
        eprintln!(
            "WARNING: Perennos Tempo Room is listening on {} as well as on this machine.",
            r.addr
        );
        eprintln!("  Anyone who can reach that address sees the join page; only people with an invite get in,");
        eprintln!("  and only this machine can act as the owner. The traffic is plain HTTP: use Tailscale");
        eprintln!("  (encrypted) or a home network you trust, never the open internet.");
        if a.all("--allow-host").is_empty() && bind.is_unspecified() {
            eprintln!("  With --bind 0.0.0.0, add --allow-host <the name or address people use>.");
        }
    }
    eprintln!("Perennos Tempo agent: {agent}");
    for (name, state, notes) in &checks {
        if state != "off" {
            eprintln!("bridge {name}: {state}");
            if state == "not ready" {
                for n in notes {
                    eprintln!("  {n}");
                }
            }
        }
    }
    if let Some(c) = dc {
        let h = hub.clone();
        std::thread::spawn(move || bridges::discord::run(h, c));
    }
    if let Some(c) = tg {
        let h = hub.clone();
        std::thread::spawn(move || bridges::telegram::run(h, c));
    }
    if let Some(w) = r.webhook {
        eprintln!("WhatsApp webhook (Untested) on http://{w}/room/whatsapp; put Tailscale Funnel in front of this port only.");
    }
    r.wait();
    Ok(())
}

fn rooms(a: &Args, dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    if a.pos.get(1).map(String::as_str) == Some("add") {
        let name = a.pos[2..].join(" ");
        if name.trim().is_empty() {
            return Err("rooms add NAME".into());
        }
        let id = make_room(dir, &name)?;
        return emit(out, a, json!({"room": id}), || format!("made #{id}"));
    }
    let st = Store::open(dir)?;
    let list: Vec<Value> = st
        .rooms
        .iter()
        .map(|r| {
            let people = st.members.iter().filter(|m| m.rooms.contains(&r.id)).count();
            json!({"id": r.id, "name": r.name, "messages": r.next.saturating_sub(1), "mode": r.mode.as_str(), "personality": r.personality.as_str(), "members": people})
        })
        .collect();
    emit(out, a, json!({"rooms": list}), || {
        if list.is_empty() {
            return "No rooms yet. Make one: tempo-room invite family".into();
        }
        list.iter()
            .map(|r| {
                format!(
                    "#{:<20} {:<24} {:>6} messages  {} · {}  {} member(s)",
                    r["id"].as_str().unwrap_or(""),
                    r["name"].as_str().unwrap_or(""),
                    r["messages"],
                    if r["mode"] == "always" {
                        "answers all"
                    } else {
                        "@tempo"
                    },
                    r["personality"].as_str().unwrap_or(""),
                    r["members"]
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Find or make a room (through the server when it runs).
fn make_room(dir: &Path, name: &str) -> Result<String, String> {
    let st = Store::open(dir)?;
    if let Some(r) = st.find_room(name) {
        return Ok(r.id.clone());
    }
    drop(st);
    if let Some(addr) = live_server(dir) {
        let v = owner_post(&addr, &control_key(dir)?, "create", &json!({"name": name}))?;
        return Ok(v["room"]["id"].as_str().unwrap_or("").to_string());
    }
    Store::open(dir)?.create_room(name, Mode::Mention, Personality::Warm)
}

fn invite(a: &Args, dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    let name = a.pos[1..].join(" ");
    if name.trim().is_empty() {
        return Err("invite ROOM (for example: tempo-room invite family)".into());
    }
    let hours = match a.get("--hours") {
        Some(h) => Some(h.parse::<f64>().map_err(|_| "--hours is a number")?),
        None => None,
    };
    let existed = Store::open(dir)?.find_room(&name).is_some();
    let room = make_room(dir, &name)?;
    let (code, expires, link, local_only) = match live_server(dir) {
        Some(addr) => {
            let mut body = json!({"room": room});
            if let Some(h) = hours {
                body["hours"] = json!(h);
            }
            let v = owner_post(&addr, &control_key(dir)?, "invite", &body)?;
            (
                v["code"].as_str().unwrap_or("").to_string(),
                v["expires"].as_f64().unwrap_or(0.0),
                Some(v["link"].as_str().unwrap_or("").to_string()),
                v["local_only"].as_bool().unwrap_or(true),
            )
        }
        None => {
            let (code, exp) = Store::open(dir)?.new_invite(&room, hours)?;
            (code, exp, None, true)
        }
    };
    let off = crate::util::local_offset_minutes(None);
    let until = format!(
        "{} {}",
        crate::util::date_of(expires, off),
        crate::util::time_of(expires, off)
    );
    emit(
        out,
        a,
        json!({"room": room, "code": code, "link": link, "expires": expires, "local_only": local_only, "made_room": !existed}),
        || {
            let mut s = String::new();
            if !existed {
                s.push_str(&format!("Made the room #{room}.\n"));
            }
            s.push_str(&format!(
                "Invite to #{room}: works once, until {until}.\n  code: {code}\n"
            ));
            match &link {
                Some(l) => s.push_str(&format!("  link: {l}\n")),
                None => s.push_str("  link: http://<the address you serve on>/#join=CODE (start `tempo-room serve` first)\n"),
            }
            if local_only {
                s.push_str("  This room server only listens on this machine. For family or a team, serve on your tailnet:\n  tempo-room serve --bind <tailnet IP> --allow-host <machine name>\n");
            }
            s.trim_end().to_string()
        },
    )
}

fn post(a: &Args, dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    let room_q = a.pos(1, "ROOM")?;
    let text = a.pos[2..].join(" ");
    if text.trim().is_empty() {
        return Err("post ROOM TEXT".into());
    }
    let st = Store::open(dir)?;
    let room = st
        .find_room(room_q)
        .ok_or_else(|| format!("no room {room_q} (tempo-room rooms lists them)"))?
        .id
        .clone();
    let owner = st.cfg.owner_name.clone();
    drop(st);
    let as_tempo = a.has("--as-tempo");
    let (msg, live) = match live_server(dir) {
        Some(addr) => {
            let v = owner_post(
                &addr,
                &control_key(dir)?,
                "post",
                &json!({"room": room, "text": text, "via": "cli", "as_tempo": as_tempo}),
            )?;
            (v["msg"].clone(), true)
        }
        None => {
            let m = if as_tempo {
                Msg::new(&room, "tempo", "Tempo", Kind::Tempo, "cli", &scrub(&text))
            } else {
                Msg::new(&room, "owner", &owner, Kind::User, "cli", &scrub(&text))
            };
            (Store::open(dir)?.append(m)?.to_json(), false)
        }
    };
    emit(out, a, json!({"msg": msg, "live": live}), || {
        if live {
            format!("posted to #{room}")
        } else {
            format!("posted to #{room} (the room server is not running: no live update, and Tempo answers only when it runs)")
        }
    })
}

fn bridges_check(a: &Args, dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    let st = Store::open(dir)?;
    let b = st.cfg.raw["bridges"].clone();
    drop(st);
    let mut rows = bridges::check(&b);
    let mut bad = rows.iter().any(|r| r.1 == "not ready");
    if a.has("--live") {
        let http = bridges::Http::default();
        let t = Duration::from_secs(15);
        if let Setting::On(c) = bridges::discord_conf(&b) {
            let r = http.request(
                "GET",
                &format!("{}/users/@me", c.api),
                Some(&format!("Bot {}", c.token)),
                None,
                t,
                "discord check",
            );
            let line = match r {
                Ok(r) if r.status == 200 => format!(
                    "live: token accepted (bot {})",
                    r.json["username"].as_str().unwrap_or("?")
                ),
                Ok(r) => {
                    bad = true;
                    format!(
                        "live: Discord answered HTTP {} (a 401 means the token is wrong)",
                        r.status
                    )
                }
                Err(e) => {
                    bad = true;
                    format!("live: {e}")
                }
            };
            push_note(&mut rows, "discord", line);
        }
        if let Setting::On(c) = bridges::telegram_conf(&b) {
            let r = http.request(
                "GET",
                &format!("{}/bot{}/getMe", c.api, c.token),
                None,
                None,
                t,
                "telegram check",
            );
            let line = match r {
                Ok(r) if r.json["ok"] == true => format!(
                    "live: token accepted (@{})",
                    r.json["result"]["username"].as_str().unwrap_or("?")
                ),
                Ok(r) => {
                    bad = true;
                    format!(
                        "live: Telegram answered HTTP {} (a 401 means the token is wrong)",
                        r.status
                    )
                }
                Err(e) => {
                    bad = true;
                    format!("live: {e}")
                }
            };
            push_note(&mut rows, "telegram", line);
        }
        if let Setting::On(c) = bridges::whatsapp_conf(&b) {
            let url = format!(
                "{}/{}?fields=verified_name,display_phone_number",
                c.api, c.phone_number_id
            );
            let r = http.request(
                "GET",
                &url,
                Some(&format!("Bearer {}", c.token)),
                None,
                t,
                "whatsapp check",
            );
            let line = match r {
                Ok(r) if r.status == 200 => format!(
                    "live: token accepted ({})",
                    r.json["verified_name"].as_str().unwrap_or("?")
                ),
                Ok(r) => {
                    bad = true;
                    format!("live: Meta answered HTTP {}", r.status)
                }
                Err(e) => {
                    bad = true;
                    format!("live: {e}")
                }
            };
            push_note(&mut rows, "whatsapp", line);
        }
    }
    let j = json!({"bridges": rows.iter().map(|(n, s, notes)| json!({"name": n, "state": s, "notes": notes})).collect::<Vec<_>>()});
    emit(out, a, j, || {
        rows.iter()
            .map(|(n, s, notes)| {
                let mut t = format!("{n}: {s}");
                for x in notes {
                    t.push_str(&format!("\n  {x}"));
                }
                t
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if bad {
        return Err("a bridge is not ready (see above)".into());
    }
    Ok(())
}

fn push_note(rows: &mut [(String, String, Vec<String>)], name: &str, line: String) {
    if let Some(r) = rows.iter_mut().find(|r| r.0 == name) {
        r.2.push(scrub(&line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testdir::tmpdir;

    fn cli(args: &[&str]) -> Result<String, String> {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        run(&a, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn offline_commands() {
        let d = tmpdir("cli");
        let ds = d.to_str().unwrap();
        assert!(cli(&["--help"]).unwrap().contains("bridges check"));
        assert!(cli(&["version"]).unwrap().starts_with("tempo-room "));
        assert!(cli(&["rooms", "--data", ds])
            .unwrap()
            .contains("No rooms yet"));
        let inv = cli(&["invite", "Family", "--data", ds, "--hours", "1"]).unwrap();
        assert!(
            inv.contains("Made the room #family.") && inv.contains("code: "),
            "{inv}"
        );
        let j: Value =
            serde_json::from_str(&cli(&["invite", "family", "--data", ds, "--json"]).unwrap())
                .unwrap();
        assert_eq!(
            (j["room"].as_str(), j["made_room"].as_bool()),
            (Some("family"), Some(false))
        );
        assert_eq!(j["code"].as_str().unwrap().len(), 11);
        assert!(cli(&["post", "family", "hello", "there", "--data", ds])
            .unwrap()
            .contains("not running"));
        cli(&["post", "family", "beep", "--as-tempo", "--data", ds]).unwrap();
        let st = Store::open(&d).unwrap();
        let tail: Vec<(String, Kind)> = st
            .room("family")
            .unwrap()
            .tail
            .iter()
            .map(|m| (m.text.clone(), m.kind))
            .collect();
        assert_eq!(
            tail,
            [
                ("hello there".to_string(), Kind::User),
                ("beep".to_string(), Kind::Tempo)
            ]
        );
        let link = cli(&["owner-link", "--data", ds]).unwrap();
        let prefix = format!("http://127.0.0.1:{DEFAULT_PORT}/#owner=");
        let token = link
            .split(&prefix)
            .nth(1)
            .expect("the link")
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        let mut again = Store::open(&d).unwrap();
        assert!(
            again
                .redeem_owner_link(&token, crate::util::unix_now())
                .is_ok(),
            "the printed link works once"
        );
        drop(st);
        let r = cli(&["rooms", "--data", ds, "--json"]).unwrap();
        assert!(r.contains("\"messages\":2"), "{r}");
        assert_eq!(
            cli(&["rooms", "add", "Book", "club", "--data", ds])
                .unwrap()
                .trim(),
            "made #book-club"
        );
        assert!(cli(&["bridges", "check", "--data", ds])
            .unwrap()
            .contains("discord: off\ntelegram: off\nwhatsapp: off"));
        assert!(cli(&["nope"]).unwrap_err().contains("unknown command"));
        assert!(cli(&["rooms", "--bogus"])
            .unwrap_err()
            .contains("unknown option"));
        assert!(cli(&["post", "nowhere", "x", "--data", ds])
            .unwrap_err()
            .contains("no room nowhere"));
    }

    #[test]
    fn bridges_check_reports_without_tokens() {
        let d = tmpdir("cli-bridges");
        let tok = format!("{}:{}", 987_654_321, "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw");
        crate::bridges::test_env("TEMPO_T_CLI_TELEGRAM", &tok);
        std::fs::write(
            d.join("config.json"),
            json!({"bridges": {"telegram": {"enabled": true, "token_env": "TEMPO_T_CLI_TELEGRAM", "chats": {"-100": "family"}}, "discord": {"enabled": true}}}).to_string(),
        )
        .unwrap();
        let a: Vec<String> = ["bridges", "check", "--data", d.to_str().unwrap()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut out = Vec::new();
        let r = run(&a, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(r.is_err(), "discord is not ready");
        assert!(
            text.contains(
                "telegram: on\n  token: set, from the environment (TEMPO_T_CLI_TELEGRAM)"
            ),
            "{text}"
        );
        assert!(
            text.contains("discord: not ready\n  no bot token"),
            "{text}"
        );
        assert!(!text.contains(&tok));
    }
}
