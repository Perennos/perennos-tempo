//! Bridges to Discord, Telegram and WhatsApp. All optional and off by default: each is turned on
//! in `config.json` (`bridges.<name>.enabled: true`) or by its environment variables.
//!
//! Tokens come only from the environment or the local `config.json`, are registered with the
//! scrubber so no log line can carry them, and are only ever sent to the platform's own API host
//! (or to a loopback address, which is how the tests run against fake servers). Redirects are
//! never followed. A 429 is honoured by waiting `retry_after`.
pub mod discord;
pub mod telegram;
pub mod whatsapp;

#[cfg(test)]
mod flows;

use crate::util::{log, register_secret, scrub, valid_room_id};
use serde_json::Value;
use std::time::Duration;

/// Where Tempo's reply goes besides the local room.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Discord { channel: String, message: String },
    Telegram { chat: i64, message: i64 },
    WhatsApp { to: String, message: String },
}

pub const DISCORD_API: &str = "https://discord.com/api/v10";
pub const TELEGRAM_API: &str = "https://api.telegram.org";
pub const WHATSAPP_API: &str = "https://graph.facebook.com/v21.0";

/// Only the platform's own host, or loopback (fake servers in tests), may receive a token.
pub fn allowed_base(url: &str, official_host: &str) -> Result<(), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url} is not a URL"))?;
    let host_port = rest.split('/').next().unwrap_or("");
    if host_port.contains('@') {
        return Err("API URLs cannot carry a user name or password".into());
    }
    let host = if let Some(h) = host_port.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    }
    .to_ascii_lowercase();
    let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    match scheme {
        "https" if host == official_host || loopback => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(format!(
            "{url} is not allowed: tokens only go to https://{official_host} (or to loopback for testing)"
        )),
    }
}

fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    rest.starts_with("127.0.0.1") || rest.starts_with("localhost") || rest.starts_with("[::1]")
}

pub struct Resp {
    pub status: u16,
    pub json: Value,
    pub retry_after: Option<f64>,
}

/// HTTPS for the bridges: loopback goes direct, everything else may use the system proxy.
pub struct Http {
    direct: ureq::Agent,
    proxied: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        let build = |proxy: bool| -> ureq::Agent {
            let b = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .timeout_global(Some(Duration::from_secs(40)))
                // Discord asks for exactly this shape: DiscordBot ($url, $version).
                .user_agent(concat!(
                    "DiscordBot (https://perennos.com, ",
                    env!("CARGO_PKG_VERSION"),
                    ") PerennosTempoRoom"
                ));
            let b = if proxy { b } else { b.proxy(None) };
            b.build().into()
        };
        Http {
            direct: build(false),
            proxied: build(true),
        }
    }
}

impl Http {
    /// One request. `label` names it in errors (never the URL, which can hold a token).
    pub fn request(
        &self,
        method: &str,
        url: &str,
        auth: Option<&str>,
        body: Option<&Value>,
        timeout: Duration,
        label: &str,
    ) -> Result<Resp, String> {
        let a = if is_loopback_url(url) {
            &self.direct
        } else {
            &self.proxied
        };
        let mut resp = match (method, body) {
            ("GET", _) => {
                let mut r = a.get(url).config().timeout_global(Some(timeout)).build();
                if let Some(h) = auth {
                    r = r.header("Authorization", h);
                }
                r.call()
            }
            (_, b) => {
                let mut r = a
                    .post(url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
                    .header("Content-Type", "application/json");
                if let Some(h) = auth {
                    r = r.header("Authorization", h);
                }
                r.send(b.map(Value::to_string).unwrap_or_default())
            }
        }
        .map_err(|e| scrub(&format!("{label}: {e}")))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok());
        let text = resp
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_to_string()
            .unwrap_or_default();
        Ok(Resp {
            status,
            json: serde_json::from_str(&text).unwrap_or(Value::Null),
            retry_after,
        })
    }

    /// `request`, waiting out 429s (`retry_after` from the body, the header, or Telegram's
    /// `parameters`) up to three times.
    pub fn with_retry(
        &self,
        method: &str,
        url: &str,
        auth: Option<&str>,
        body: Option<&Value>,
        timeout: Duration,
        label: &str,
    ) -> Result<Resp, String> {
        for _ in 0..4 {
            let r = self.request(method, url, auth, body, timeout, label)?;
            if r.status != 429 {
                return Ok(r);
            }
            let wait = r.json["retry_after"]
                .as_f64()
                .or(r.json["parameters"]["retry_after"].as_f64())
                .or(r.retry_after)
                .unwrap_or(1.0)
                .clamp(0.05, 120.0);
            log(&format!("{label}: rate limited, waiting {wait:.2} s"));
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
        Err(format!("{label}: still rate limited after three waits"))
    }
}

#[cfg(test)]
static TEST_ENV: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Tests set a variable for this module only, under a name of their own (no process-wide
/// `set_var` while other tests run).
#[cfg(test)]
pub fn test_env(name: &str, value: &str) {
    TEST_ENV
        .lock()
        .unwrap()
        .push((name.to_string(), value.to_string()));
}

fn env_get(name: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = TEST_ENV
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(n, _)| n == name)
    {
        return Some(v.1.clone());
    }
    std::env::var(name).ok()
}

fn set(v: Option<String>) -> bool {
    v.is_some_and(|v| !v.trim().is_empty())
}

/// An environment variable's name: capital letters, digits and `_` (a pasted key is not one).
fn env_name(n: &str) -> bool {
    n.len() <= 64
        && n.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && n.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// A key a bridge needs.
struct Secret {
    value: String,
    /// The environment variable it comes from.
    var: String,
    /// It was written in config.json (and so refused).
    in_file: bool,
}

/// Keys come from the environment only, never from files: the variable is `default_var`, or the
/// one `<key>_env` names in config.json. A key written in config.json stops the bridge, with a
/// message saying where it goes instead.
fn secret(
    c: &Value,
    bridge: &str,
    key: &str,
    default_var: &str,
    problems: &mut Vec<String>,
) -> Secret {
    let var_key = format!("{key}_env");
    let var = match c[var_key.as_str()]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(v) if env_name(v) => v.to_string(),
        Some(v) => {
            register_secret(v); // it may be the key itself
            problems.push(format!("bridges.{bridge}.{var_key} must be the name of an environment variable (capital letters, digits and _), not the key"));
            default_var.to_string()
        }
        None => default_var.to_string(),
    };
    let in_file = match c[key].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => {
            register_secret(v);
            problems.push(format!("bridges.{bridge}.{key} is written in config.json, and keys never go in files: set it in the environment as {var} (or name your own variable with \"{var_key}\"), then take it out of config.json"));
            true
        }
        None => false,
    };
    let value = env_get(&var)
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    register_secret(&value);
    Secret {
        value,
        var,
        in_file,
    }
}

/// `{"<id>": "<room>"}` from config, or `id=room,id=room` from the environment.
fn conf_map(c: &Value, key: &str, env: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = c[key]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.trim().to_string(), v.as_str()?.trim().to_string())))
                .collect()
        })
        .unwrap_or_default();
    if let Some(e) = env_get(env) {
        for pair in e.split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                out.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out.retain(|(k, v)| !k.is_empty() && !v.is_empty());
    out
}

/// A bridge's settings, or why it cannot start.
pub enum Setting<T> {
    Off,
    On(T),
    Broken(Vec<String>),
}

/// On when config.json says so; otherwise when its key and its chats are both in the
/// environment. A key written in config.json turns it on too, so `bridges check` says why it
/// is refused instead of staying quiet.
fn enabled(c: &Value, token_var: &str, map_env: &str) -> bool {
    if let Some(b) = c["enabled"].as_bool() {
        return b;
    }
    if c["token"].as_str().is_some_and(|s| !s.trim().is_empty()) {
        return true;
    }
    let var = c["token_env"]
        .as_str()
        .map(str::trim)
        .filter(|v| env_name(v))
        .unwrap_or(token_var);
    set(env_get(var)) && set(env_get(map_env))
}

fn check_rooms(map: &[(String, String)], problems: &mut Vec<String>, what: &str) {
    if map.is_empty() {
        problems.push(format!("no {what} listed"));
    }
    for (_, room) in map {
        if !valid_room_id(&crate::util::slug(room)) {
            problems.push(format!("{room} is not a usable room name"));
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiscordConf {
    pub token: String,
    /// The environment variable the token came from.
    pub token_from: String,
    /// Channel id -> local room id.
    pub channels: Vec<(String, String)>,
    pub api: String,
    pub poll: Duration,
}

#[derive(Clone, Debug)]
pub struct TelegramConf {
    pub token: String,
    pub token_from: String,
    /// Chat id -> local room id. Messages from other chats are ignored.
    pub chats: Vec<(i64, String)>,
    pub api: String,
    pub poll_timeout: u64,
}

#[derive(Clone, Debug)]
pub struct WhatsAppConf {
    pub token: String,
    pub token_from: String,
    pub phone_number_id: String,
    pub verify_token: String,
    pub app_secret: String,
    /// Phone number (international, digits only) -> local room id.
    pub numbers: Vec<(String, String)>,
    pub api: String,
    pub webhook_port: u16,
    pub public_host: String,
}

pub fn discord_conf(b: &Value) -> Setting<DiscordConf> {
    let c = &b["discord"];
    if !enabled(c, "TEMPO_DISCORD_TOKEN", "TEMPO_DISCORD_CHANNELS") {
        return Setting::Off;
    }
    let mut p = vec![];
    let tok = secret(c, "discord", "token", "TEMPO_DISCORD_TOKEN", &mut p);
    let channels = conf_map(c, "channels", "TEMPO_DISCORD_CHANNELS");
    let api = c["api"]
        .as_str()
        .unwrap_or(DISCORD_API)
        .trim_end_matches('/')
        .to_string();
    if tok.value.is_empty() && !tok.in_file {
        p.push(format!("no bot token (set {} in the environment)", tok.var));
    }
    check_rooms(&channels, &mut p, "channels");
    if channels
        .iter()
        .any(|(id, _)| !id.bytes().all(|b| b.is_ascii_digit()))
    {
        p.push(
            "channel ids are numbers (Developer Mode, right-click the channel, Copy Channel ID)"
                .into(),
        );
    }
    if let Err(e) = allowed_base(&api, "discord.com") {
        p.push(e);
    }
    if !p.is_empty() {
        return Setting::Broken(p);
    }
    Setting::On(DiscordConf {
        token: tok.value,
        token_from: tok.var,
        channels,
        api,
        poll: Duration::from_secs_f64(c["poll_secs"].as_f64().unwrap_or(4.0).clamp(0.05, 60.0)),
    })
}

pub fn telegram_conf(b: &Value) -> Setting<TelegramConf> {
    let c = &b["telegram"];
    if !enabled(c, "TEMPO_TELEGRAM_TOKEN", "TEMPO_TELEGRAM_CHATS") {
        return Setting::Off;
    }
    let mut p = vec![];
    let tok = secret(c, "telegram", "token", "TEMPO_TELEGRAM_TOKEN", &mut p);
    let map = conf_map(c, "chats", "TEMPO_TELEGRAM_CHATS");
    let api = c["api"]
        .as_str()
        .unwrap_or(TELEGRAM_API)
        .trim_end_matches('/')
        .to_string();
    if tok.value.is_empty() && !tok.in_file {
        p.push(format!("no bot token (set {} in the environment)", tok.var));
    }
    // No chats yet is fine: every message is ignored and its chat id logged (that is how you
    // find the id to put here).
    if !map.is_empty() {
        check_rooms(&map, &mut p, "chats");
    }
    let chats: Vec<(i64, String)> = map
        .iter()
        .filter_map(|(k, v)| Some((k.parse::<i64>().ok()?, v.clone())))
        .collect();
    if chats.len() != map.len() {
        p.push("chat ids are numbers (groups start with -100)".into());
    }
    if let Err(e) = allowed_base(&api, "api.telegram.org") {
        p.push(e);
    }
    if !p.is_empty() {
        return Setting::Broken(p);
    }
    Setting::On(TelegramConf {
        token: tok.value,
        token_from: tok.var,
        chats,
        api,
        poll_timeout: c["poll_timeout"].as_u64().unwrap_or(25).min(50),
    })
}

pub fn whatsapp_conf(b: &Value) -> Setting<WhatsAppConf> {
    let c = &b["whatsapp"];
    if !enabled(c, "TEMPO_WHATSAPP_TOKEN", "TEMPO_WHATSAPP_NUMBERS") {
        return Setting::Off;
    }
    let mut p = vec![];
    let tok = secret(c, "whatsapp", "token", "TEMPO_WHATSAPP_TOKEN", &mut p);
    let verify = secret(
        c,
        "whatsapp",
        "verify_token",
        "TEMPO_WHATSAPP_VERIFY_TOKEN",
        &mut p,
    );
    let app = secret(
        c,
        "whatsapp",
        "app_secret",
        "TEMPO_WHATSAPP_APP_SECRET",
        &mut p,
    );
    let phone_number_id = c["phone_number_id"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let numbers: Vec<(String, String)> = conf_map(c, "numbers", "TEMPO_WHATSAPP_NUMBERS")
        .into_iter()
        .map(|(k, v)| (k.trim_start_matches('+').replace([' ', '-'], ""), v))
        .collect();
    let api = c["api"]
        .as_str()
        .unwrap_or(WHATSAPP_API)
        .trim_end_matches('/')
        .to_string();
    if tok.value.is_empty() && !tok.in_file {
        p.push(format!(
            "no access token (set {} in the environment)",
            tok.var
        ));
    }
    if verify.value.len() < 8 && !verify.in_file {
        p.push(format!("no verify_token (8+ characters, any text you choose, in the environment as {}; Meta sends it back to check the webhook)", verify.var));
    }
    if app.value.is_empty() && !app.in_file {
        p.push(format!("no app_secret (Meta app settings, Basic; in the environment as {}; it signs every webhook call)", app.var));
    }
    if phone_number_id.is_empty() || !phone_number_id.bytes().all(|b| b.is_ascii_digit()) {
        p.push("no phone_number_id (WhatsApp, API Setup in the Meta app)".into());
    }
    check_rooms(&numbers, &mut p, "numbers");
    if let Err(e) = allowed_base(&api, "graph.facebook.com") {
        p.push(e);
    }
    if !p.is_empty() {
        return Setting::Broken(p);
    }
    Setting::On(WhatsAppConf {
        token: tok.value,
        token_from: tok.var,
        phone_number_id,
        verify_token: verify.value,
        app_secret: app.value,
        numbers,
        api,
        webhook_port: c["webhook_port"].as_u64().unwrap_or(8768) as u16,
        public_host: c["public_host"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_lowercase(),
    })
}

/// Sending Tempo's replies out. Bridges that are off simply have no settings here.
#[derive(Default)]
pub struct Outbound {
    pub discord: Option<DiscordConf>,
    pub telegram: Option<TelegramConf>,
    pub whatsapp: Option<WhatsAppConf>,
    pub http: Http,
}

impl Outbound {
    pub fn from_config(bridges: &Value) -> Outbound {
        Outbound {
            discord: match discord_conf(bridges) {
                Setting::On(c) => Some(c),
                _ => None,
            },
            telegram: match telegram_conf(bridges) {
                Setting::On(c) => Some(c),
                _ => None,
            },
            whatsapp: match whatsapp_conf(bridges) {
                Setting::On(c) => Some(c),
                _ => None,
            },
            http: Http::default(),
        }
    }

    /// The room names every bridge that is on is mapped to (a bridged chat mirrors into a local
    /// room). A room with any of them can never be owner-only.
    pub fn mapped_rooms(&self) -> Vec<String> {
        let mut out = vec![];
        if let Some(c) = &self.discord {
            out.extend(c.channels.iter().map(|(_, r)| r.clone()));
        }
        if let Some(c) = &self.telegram {
            out.extend(c.chats.iter().map(|(_, r)| r.clone()));
        }
        if let Some(c) = &self.whatsapp {
            out.extend(c.numbers.iter().map(|(_, r)| r.clone()));
        }
        out
    }

    /// Reply in the platform's thread. Failures are logged; the local room already has the reply.
    pub fn send(&self, t: &Target, text: &str) {
        let r = match t {
            Target::Discord { channel, message } => match &self.discord {
                Some(c) => discord::reply(c, &self.http, channel, message, text),
                None => Err("discord is off".into()),
            },
            Target::Telegram { chat, message } => match &self.telegram {
                Some(c) => telegram::reply(c, &self.http, *chat, *message, text),
                None => Err("telegram is off".into()),
            },
            Target::WhatsApp { to, message } => match &self.whatsapp {
                Some(c) => whatsapp::reply(c, &self.http, to, message, text),
                None => Err("whatsapp is off".into()),
            },
        };
        if let Err(e) = r {
            log(&format!("bridge reply not sent: {e}"));
        }
    }

    /// "Tempo is typing" on the platform, where it has one.
    pub fn typing(&self, t: &Target) {
        match (t, &self.discord, &self.telegram) {
            (Target::Discord { channel, .. }, Some(c), _) => {
                discord::typing(c, &self.http, channel)
            }
            (Target::Telegram { chat, .. }, _, Some(c)) => telegram::typing(c, &self.http, *chat),
            _ => {}
        }
    }
}

/// `bridges check`: one line per bridge, never a token.
pub fn check(bridges: &Value) -> Vec<(String, String, Vec<String>)> {
    let mut out = vec![];
    match discord_conf(bridges) {
        Setting::Off => out.push(("discord".into(), "off".into(), vec![])),
        Setting::Broken(p) => out.push(("discord".into(), "not ready".into(), p)),
        Setting::On(c) => out.push((
            "discord".into(),
            "on".into(),
            vec![
                format!("token: set, from the environment ({})", c.token_from),
                format!(
                    "channels: {}",
                    c.channels
                        .iter()
                        .map(|(k, v)| format!("{k} -> #{}", crate::util::slug(v)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                format!("api: {}", c.api),
            ],
        )),
    }
    match telegram_conf(bridges) {
        Setting::Off => out.push(("telegram".into(), "off".into(), vec![])),
        Setting::Broken(p) => out.push(("telegram".into(), "not ready".into(), p)),
        Setting::On(c) => out.push((
            "telegram".into(),
            "on".into(),
            vec![
                format!("token: set, from the environment ({})", c.token_from),
                if c.chats.is_empty() {
                    "chats: none yet; messages are ignored and their chat ids appear in the serve log".to_string()
                } else {
                    format!(
                        "chats: {}",
                        c.chats
                            .iter()
                            .map(|(k, v)| format!("{k} -> #{}", crate::util::slug(v)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
                format!("api: {}", c.api),
            ],
        )),
    }
    match whatsapp_conf(bridges) {
        Setting::Off => out.push(("whatsapp".into(), "off".into(), vec![])),
        Setting::Broken(p) => out.push(("whatsapp".into(), "not ready".into(), p)),
        Setting::On(c) => out.push((
            "whatsapp".into(),
            "on (Untested against Meta)".into(),
            vec![
                format!("token: set, from the environment ({})", c.token_from),
                "verify_token: set; app_secret: set (both from the environment)".into(),
                format!("numbers: {}", c.numbers.len()),
                format!(
                    "webhook: http://127.0.0.1:{}/room/whatsapp (put Tailscale Funnel in front: https://{}/room/whatsapp)",
                    c.webhook_port,
                    if c.public_host.is_empty() { "<your-machine>.ts.net" } else { &c.public_host }
                ),
            ],
        )),
    }
    out
}

/// The local room a bridged chat mirrors into (made the first time it is needed).
pub fn ensure_room(hub: &crate::hub::Hub, name: &str) -> Option<String> {
    let made = {
        let mut st = hub.lock();
        if let Some(r) = st.find_room(name) {
            return Some(r.id.clone());
        }
        st.create_room(
            name,
            crate::store::Mode::Mention,
            crate::store::Personality::Warm,
        )
    };
    match made {
        Ok(id) => {
            hub.emit(&id, "room", serde_json::json!({"room": id}));
            Some(id)
        }
        Err(e) => {
            log(&format!("cannot make the room {name}: {e}"));
            None
        }
    }
}

/// Small persisted state per bridge (`bridge-<name>.json`): last seen ids, offsets.
pub fn load_state(dir: &std::path::Path, name: &str) -> Value {
    crate::util::read_json(&dir.join(format!("bridge-{name}.json")))
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

pub fn save_state(dir: &std::path::Path, name: &str, v: &Value) {
    if let Err(e) = crate::util::write_atomic(
        &dir.join(format!("bridge-{name}.json")),
        v.to_string().as_bytes(),
    ) {
        log(&format!("{name}: cannot save bridge state: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokens_only_go_to_the_platform() {
        assert!(allowed_base("https://discord.com/api/v10", "discord.com").is_ok());
        assert!(allowed_base("http://127.0.0.1:9999/api", "discord.com").is_ok());
        assert!(allowed_base("https://localhost:1/x", "discord.com").is_ok());
        assert!(allowed_base("http://[::1]:5/x", "discord.com").is_ok());
        for bad in [
            "http://discord.com/api",
            "https://discord.com.evil.example/api",
            "https://evil.example/discord.com",
            "https://user:pw@discord.com/api",
            "ftp://discord.com",
            "discord.com",
            "http://10.0.0.5:8080",
        ] {
            assert!(allowed_base(bad, "discord.com").is_err(), "{bad}");
        }
    }

    #[test]
    fn telegram_without_chats_starts_in_discovery_mode() {
        let tok = format!("{}:{}", 555_666_777, "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDxyz");
        test_env("TEMPO_T_TG_DISCOVERY", &tok);
        let b = json!({"telegram": {"enabled": true, "token_env": "TEMPO_T_TG_DISCOVERY"}});
        assert!(matches!(telegram_conf(&b), Setting::On(c) if c.chats.is_empty()));
        let out = check(&b);
        assert_eq!(out[1].1, "on");
        assert!(
            out[1].2.iter().any(|n| n.contains("chats: none yet")),
            "{out:?}"
        );
        let bad = json!({"telegram": {"enabled": true, "token_env": "TEMPO_T_TG_DISCOVERY", "chats": {"-100x": "family"}}});
        assert!(matches!(telegram_conf(&bad), Setting::Broken(_)));
    }

    #[test]
    fn settings_and_check_never_show_tokens() {
        let tok = format!(
            "{}.{}.{}",
            "MTA5ODc2NTQzMjEwOTg3NjU0Mw",
            "GhIjKl",
            "z".repeat(30)
        );
        test_env("TEMPO_T_DC_CHECK", &tok);
        let b = json!({
            "discord": {"enabled": true, "token_env": "TEMPO_T_DC_CHECK", "channels": {"123": "Family"}},
            "telegram": {"enabled": true, "token_env": "TEMPO_T_TG_NOT_SET", "chats": {"abc": "family"}},
            "whatsapp": {"enabled": false}
        });
        let out = check(&b);
        let text = format!("{out:?}");
        assert!(!text.contains(&tok), "{text}");
        assert_eq!(out[0].1, "on");
        assert!(text.contains("token: set, from the environment (TEMPO_T_DC_CHECK)"));
        assert!(text.contains("123 -> #family"));
        assert_eq!(out[1].1, "not ready");
        assert!(out[1]
            .2
            .iter()
            .any(|p| p.contains("no bot token (set TEMPO_T_TG_NOT_SET in the environment)")));
        assert!(out[1].2.iter().any(|p| p.contains("chat ids are numbers")));
        assert_eq!(out[2].1, "off");
        let evil = json!({"discord": {"enabled": true, "token_env": "TEMPO_T_DC_CHECK", "channels": {"1": "a"}, "api": "https://evil.example/api"}});
        assert!(
            matches!(discord_conf(&evil), Setting::Broken(p) if p.iter().any(|x| x.contains("not allowed")))
        );
        let wa = json!({"whatsapp": {"enabled": true, "token_env": "TEMPO_T_WA_NONE", "verify_token_env": "TEMPO_T_WA_NONE", "app_secret_env": "TEMPO_T_WA_NONE", "numbers": {"+1 555-0100": "family"}}});
        match whatsapp_conf(&wa) {
            Setting::Broken(p) => {
                assert!(p.iter().any(|x| x.contains("no access token")));
                assert!(p.iter().any(|x| x.contains("verify_token")));
                assert!(p.iter().any(|x| x.contains("app_secret")));
                assert!(p.iter().any(|x| x.contains("phone_number_id")));
            }
            _ => panic!("whatsapp needs more settings"),
        }
    }

    #[test]
    fn keys_written_in_config_json_stop_the_bridge() {
        let dc = format!(
            "{}.{}.{}",
            "MTA5ODc2NTQzMjEwOTg3NjU0OQ",
            "Ab12Cd",
            "q".repeat(30)
        );
        let tg = format!("{}:{}", 444_555_666, "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDqrs");
        let wa_secret = format!("{}{}", "0123456789abcdef", "0123456789abcdef");
        // even with the variable set too: a key in the file is refused until it is taken out
        test_env("TEMPO_T_DC_ALSO_SET", &dc);
        let b = json!({
            // no "enabled": a key in the file still turns it on, so the check can say why it is refused
            "discord": {"token": dc, "token_env": "TEMPO_T_DC_ALSO_SET", "channels": {"123": "Family"}},
            // the key pasted where the variable's name goes
            "telegram": {"enabled": true, "token_env": tg, "chats": {"-100": "family"}},
            "whatsapp": {"enabled": true, "token_env": "TEMPO_T_WA_ENV_TOKEN", "app_secret": wa_secret,
                         "verify_token_env": "TEMPO_T_WA_ENV_VERIFY", "phone_number_id": "1055", "numbers": {"15551230000": "family"}}
        });
        test_env("TEMPO_T_WA_ENV_TOKEN", "wa-token-from-the-environment");
        test_env("TEMPO_T_WA_ENV_VERIFY", "verify-words-456");
        let out = check(&b);
        let text = format!("{out:?}");
        for key in [&dc, &tg, &wa_secret] {
            assert!(!text.contains(key.as_str()), "a key is never shown: {text}");
        }
        assert_eq!(out[0].1, "not ready", "{text}");
        assert!(out[0].2.iter().any(|p| p.contains("bridges.discord.token is written in config.json, and keys never go in files: set it in the environment as TEMPO_T_DC_ALSO_SET")), "{text}");
        assert_eq!(out[1].1, "not ready");
        assert!(
            out[1].2.iter().any(|p| p.contains(
                "bridges.telegram.token_env must be the name of an environment variable"
            )),
            "{text}"
        );
        assert_eq!(out[2].1, "not ready");
        assert!(
            out[2]
                .2
                .iter()
                .any(|p| p.contains("bridges.whatsapp.app_secret is written in config.json")),
            "{text}"
        );
        assert!(
            !out[2].2.iter().any(|p| p.contains("no app_secret")),
            "one message per problem: {text}"
        );
        // nothing written in the file, everything from the environment: on
        let ok = json!({"whatsapp": {"enabled": true, "token_env": "TEMPO_T_WA_ENV_TOKEN", "app_secret_env": "TEMPO_T_WA_ENV_VERIFY",
                                     "verify_token_env": "TEMPO_T_WA_ENV_VERIFY", "phone_number_id": "1055", "numbers": {"15551230000": "family"}}});
        assert!(
            matches!(whatsapp_conf(&ok), Setting::On(c) if c.token == "wa-token-from-the-environment" && c.token_from == "TEMPO_T_WA_ENV_TOKEN")
        );
        // the usual variable name is used when none is given
        let mut p = vec![];
        test_env("TEMPO_T_DEFAULT_NAME", "from-the-usual-name");
        let s = secret(
            &json!({}),
            "discord",
            "token",
            "TEMPO_T_DEFAULT_NAME",
            &mut p,
        );
        assert_eq!(
            (s.value.as_str(), s.var.as_str(), s.in_file, p.len()),
            ("from-the-usual-name", "TEMPO_T_DEFAULT_NAME", false, 0)
        );
        assert!(env_name("TEMPO_DISCORD_TOKEN") && env_name("_X1"));
        assert!(
            !env_name("")
                && !env_name("lower_case")
                && !env_name("1ABC")
                && !env_name(&tg)
                && !env_name(&"A".repeat(65))
        );
    }
}
