//! The `tempo-room` command line. Commands that change rooms go through the running server when
//! there is one (so everyone sees the change live) and straight to the data folder when not.
use crate::bridges::{self, Outbound, Setting};
use crate::hub::{Hub, HubOpts};
use crate::mcp::{control_key, live_server, owner_get, owner_post};
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
  team list                      your teammates (named AI members) and the rooms they are in
  team add HANDLE --kind agent|engine [--name N] [--role TEXT] [--model M] [--room ROOM]...
                                 add a teammate: @HANDLE in a room asks it
  team remove HANDLE             take a teammate out (its record is kept in trash/)
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
    "--name",
    "--role",
    "--kind",
    "--model",
    "--room",
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
        "team" => team(&a, &dir, out),
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

/// `team list|add|remove`: through the running server when there is one, else the data folder.
fn team(a: &Args, dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    let sub = a.pos.get(1).map(String::as_str).unwrap_or("list");
    match sub {
        "list" => {
            let list: Vec<Value> = match live_server(dir) {
                Some(addr) => {
                    let v = owner_get(&addr, &control_key(dir)?, "team")?;
                    v["team"].as_array().cloned().unwrap_or_default()
                }
                None => {
                    let st = Store::open(dir)?;
                    st.team
                        .iter()
                        .map(|t| {
                            let mut j = t.to_json();
                            j["rooms"] = json!(st
                                .rooms
                                .iter()
                                .filter(|r| r.team.contains(&t.handle))
                                .map(|r| r.id.clone())
                                .collect::<Vec<_>>());
                            j
                        })
                        .collect()
                }
            };
            emit(out, a, json!({"team": list}), || {
                if list.is_empty() {
                    return "No teammates yet. Add one: tempo-room team add sage --kind engine --model qwen3:4b --room family".into();
                }
                list.iter()
                    .map(|t| {
                        let rooms: Vec<&str> = t["rooms"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect();
                        format!(
                            "@{:<16} {:<16} {:<7} {:<18} {}",
                            t["handle"].as_str().unwrap_or(""),
                            t["name"].as_str().unwrap_or(""),
                            t["kind"].as_str().unwrap_or(""),
                            t["model"].as_str().unwrap_or("-"),
                            if rooms.is_empty() {
                                "in no room".to_string()
                            } else {
                                format!("in #{}", rooms.join(", #"))
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        "add" => {
            let handle = a.pos(2, "HANDLE")?.trim_start_matches('@').to_lowercase();
            let name = a.get("--name").map(str::to_string).unwrap_or_else(|| {
                let mut c = handle.chars();
                c.next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default()
            });
            let kind = a.get("--kind").unwrap_or("agent").to_string();
            let role = a.get("--role").unwrap_or("").to_string();
            let model = a.get("--model").map(str::to_string);
            let rooms = a.all("--room");
            let (t, note) = match live_server(dir) {
                Some(addr) => {
                    let v = owner_post(
                        &addr,
                        &control_key(dir)?,
                        "team/add",
                        &json!({"handle": handle, "name": name, "role": role, "kind": kind, "model": model, "rooms": rooms}),
                    )?;
                    (
                        v["teammate"].clone(),
                        v["note"].as_str().map(str::to_string),
                    )
                }
                None => {
                    let mut st = Store::open(dir)?;
                    let t =
                        st.vet_teammate(&handle, &name, &role, &kind, model.as_deref(), false)?;
                    let note = match (t.kind, &t.model) {
                        (crate::team::TeamKind::Engine, Some(m)) => {
                            crate::team::verify_model(&st.cfg, m)?
                        }
                        _ => None,
                    };
                    let mut ids = vec![];
                    for r in &rooms {
                        ids.push(
                            st.find_room(r)
                                .ok_or_else(|| {
                                    format!("no room {r} (tempo-room rooms lists them)")
                                })?
                                .id
                                .clone(),
                        );
                    }
                    st.insert_teammate(t.clone(), &ids, false)?;
                    (t.to_json(), note)
                }
            };
            emit(out, a, json!({"teammate": t, "note": note}), || {
                let mut s = format!(
                    "Added @{} ({}, {}{}).",
                    t["handle"].as_str().unwrap_or(""),
                    t["name"].as_str().unwrap_or(""),
                    t["kind"].as_str().unwrap_or(""),
                    t["model"]
                        .as_str()
                        .map(|m| format!(", model {m}"))
                        .unwrap_or_default()
                );
                if rooms.is_empty() {
                    s.push_str(" Put it in a room: add --room NAME, or use People in the app.");
                } else {
                    s.push_str(&format!(" In: {}.", rooms.join(", ")));
                }
                if let Some(n) = &note {
                    s.push_str(&format!("\nNote: {n}"));
                }
                s
            })
        }
        "remove" => {
            let handle = a.pos(2, "HANDLE")?.trim_start_matches('@').to_lowercase();
            match live_server(dir) {
                Some(addr) => {
                    owner_post(
                        &addr,
                        &control_key(dir)?,
                        "team/remove",
                        &json!({"handle": handle}),
                    )?;
                }
                None => {
                    Store::open(dir)?.remove_teammate(&handle)?;
                    crate::hub::forget_convs_in(dir, &handle);
                }
            }
            emit(out, a, json!({"removed": handle}), || {
                format!("Removed @{handle}. Its record is kept in trash/.")
            })
        }
        x => Err(format!("team {x}? try: team list, team add, team remove")),
    }
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
    #[test]
    fn team_list_add_remove_offline() {
        let d = tmpdir("cli-team");
        let ds = d.to_str().unwrap();
        // Ollama is not running here: an engine teammate is kept, with a note
        std::fs::write(
            d.join("config.json"),
            json!({"ollama_url": crate::testutil::dead_url()}).to_string(),
        )
        .unwrap();
        assert!(cli(&["team", "list", "--data", ds])
            .unwrap()
            .contains("No teammates yet"));
        cli(&["rooms", "add", "Family", "--data", ds]).unwrap();
        let out = cli(&[
            "team", "add", "sage", "--kind", "engine", "--model", "mock:4b", "--name", "Sage",
            "--role", "Be kind.", "--room", "family", "--data", ds,
        ])
        .unwrap();
        assert!(
            out.contains("Added @sage (Sage, engine, model mock:4b). In: family.")
                && out.contains("Note: Ollama is not answering"),
            "{out}"
        );
        let j: Value =
            serde_json::from_str(&cli(&["team", "add", "@Coder", "--data", ds, "--json"]).unwrap())
                .unwrap();
        assert_eq!(
            (
                j["teammate"]["handle"].as_str(),
                j["teammate"]["kind"].as_str(),
                j["teammate"]["name"].as_str()
            ),
            (Some("coder"), Some("agent"), Some("Coder"))
        );
        let l: Value =
            serde_json::from_str(&cli(&["team", "list", "--data", ds, "--json"]).unwrap()).unwrap();
        assert_eq!(l["team"].as_array().unwrap().len(), 2);
        assert_eq!(l["team"][0]["rooms"], json!(["family"]));
        assert_eq!(l["team"][0]["role"], "Be kind.");
        let text = cli(&["team", "list", "--data", ds]).unwrap();
        assert!(
            text.contains("@sage")
                && text.contains("engine")
                && text.contains("in #family")
                && text.contains("in no room"),
            "{text}"
        );
        // the rules hold here too
        for bad in [
            vec!["team", "add", "Tempo"],
            vec!["team", "add", "x"],
            vec!["team", "add", "helper", "--kind", "engine"],
            vec!["team", "add", "helper", "--room", "nowhere"],
            vec!["team", "add", "sage"],
            vec!["team", "add"],
            vec!["team", "remove", "ghost"],
            vec!["team", "bogus"],
        ] {
            let mut a = bad.clone();
            a.extend(["--data", ds]);
            assert!(cli(&a).is_err(), "{bad:?}");
        }
        assert!(cli(&["team", "remove", "sage", "--data", ds])
            .unwrap()
            .contains("kept in trash"));
        assert!(std::fs::read_dir(d.join("trash"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("teammate-sage")));
        let l: Value =
            serde_json::from_str(&cli(&["team", "list", "--data", ds, "--json"]).unwrap()).unwrap();
        assert_eq!(l["team"].as_array().unwrap().len(), 1);
        assert!(Store::open(&d)
            .unwrap()
            .room("family")
            .unwrap()
            .team
            .is_empty());
    }
}
