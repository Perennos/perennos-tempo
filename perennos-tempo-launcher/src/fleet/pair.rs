//! Adding a computer: pairing it with a short code, as easy as pairing a phone.
//!
//! On the main computer `fleet invite` shows a code like `4-tiger-violet`: good for ten minutes,
//! single use, and offered only on the private interfaces `fleet serve` listens on. On the new
//! computer `fleet join` finds the main one and asks for the code. The code is the password of a
//! SPAKE2 exchange (Ed25519 group), so even a short code cannot be guessed offline: each try is
//! one live attempt, and three wrong tries burn the invitation. The key that exchange makes seals
//! what follows (ChaCha20-Poly1305): the new computer's identity and a look at its hardware go in,
//! and the fleet key and the main computer's identity come out. From then on each side pins the
//! other's Ed25519 identity, and every frame is signed by it.

use super::crypto::{hex, hmac, random, unhex, write_private};
use super::msg::Node;
use super::store::{self, now_s, Machine, Paths};
use crate::allowed_remote_host;
use crate::setup::json::{parse, J};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::SigningKey;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub const INVITE_FILE: &str = "fleet-invite.json";
pub const VALID_S: u64 = 600;
const TRIES: u32 = 3;
const ID_JOINER: &[u8] = b"tempo-fleet-joiner";
const ID_MAIN: &[u8] = b"tempo-fleet-main";

/// Plain words and music words, 256 of them: two of them and a digit make a code that is easy to
/// say across a room.
pub const WORDS: [&str; 256] = [
    "acorn",
    "amber",
    "anchor",
    "apple",
    "arrow",
    "aspen",
    "atlas",
    "badger",
    "bamboo",
    "banjo",
    "barley",
    "basil",
    "beacon",
    "beaver",
    "birch",
    "bishop",
    "bison",
    "blossom",
    "bonsai",
    "breeze",
    "bridge",
    "bronze",
    "brook",
    "buckle",
    "cactus",
    "camel",
    "candle",
    "canyon",
    "carbon",
    "cargo",
    "cedar",
    "cherry",
    "cinder",
    "circle",
    "clover",
    "cobalt",
    "comet",
    "copper",
    "coral",
    "cotton",
    "coyote",
    "crane",
    "crater",
    "crystal",
    "curtain",
    "cypress",
    "daisy",
    "delta",
    "desert",
    "diamond",
    "dolphin",
    "dragon",
    "drift",
    "eagle",
    "ember",
    "emerald",
    "falcon",
    "feather",
    "fennel",
    "fern",
    "ferry",
    "finch",
    "fjord",
    "flint",
    "forest",
    "fossil",
    "fox",
    "galaxy",
    "garden",
    "garnet",
    "geyser",
    "ginger",
    "glacier",
    "granite",
    "grove",
    "harbor",
    "hazel",
    "heron",
    "hickory",
    "hollow",
    "honey",
    "horizon",
    "iceberg",
    "indigo",
    "island",
    "ivory",
    "jade",
    "jasmine",
    "jigsaw",
    "juniper",
    "kayak",
    "kettle",
    "kite",
    "lagoon",
    "lantern",
    "laurel",
    "lemon",
    "lilac",
    "lily",
    "linen",
    "lotus",
    "lunar",
    "magnet",
    "mango",
    "maple",
    "marble",
    "meadow",
    "melon",
    "mercury",
    "meteor",
    "mint",
    "mirror",
    "monsoon",
    "mosaic",
    "nectar",
    "nickel",
    "nimbus",
    "oasis",
    "ocean",
    "olive",
    "onyx",
    "opal",
    "orchid",
    "otter",
    "oyster",
    "paddle",
    "panda",
    "papaya",
    "pebble",
    "pepper",
    "pigeon",
    "pine",
    "planet",
    "plum",
    "polar",
    "pony",
    "poppy",
    "prairie",
    "prism",
    "pumpkin",
    "quartz",
    "quill",
    "rabbit",
    "radar",
    "rain",
    "raven",
    "reef",
    "ribbon",
    "river",
    "robin",
    "rocket",
    "saddle",
    "saffron",
    "sage",
    "salmon",
    "sand",
    "sapphire",
    "scarlet",
    "seed",
    "shadow",
    "silver",
    "sketch",
    "slate",
    "snow",
    "solar",
    "sparrow",
    "spruce",
    "squirrel",
    "starling",
    "stone",
    "summit",
    "sunset",
    "swan",
    "tangerine",
    "teal",
    "thistle",
    "thunder",
    "tiger",
    "timber",
    "topaz",
    "tulip",
    "tundra",
    "turtle",
    "valley",
    "velvet",
    "violet",
    "walnut",
    "wave",
    "willow",
    "window",
    "winter",
    "wolf",
    "yarrow",
    "zephyr",
    "zinc",
    "cabin",
    "gravel",
    "harvest",
    "lighthouse",
    "magnolia",
    "mustard",
    "nutmeg",
    "orbit",
    "paprika",
    "quiver",
    "rowan",
    "saturn",
    "tidal",
    "umber",
    "vortex",
    "wheat",
    "yonder",
    "zenith",
    "pearl",
    "rust",
    "sunrise",
    "ripple",
    "basalt",
    "mesa",
    "dune",
    "thorn",
    "fable",
    "gem",
    "hush",
    "ivy",
    "jolly",
    "kiwi",
    "lark",
    "moss",
    "nook",
    "plume",
    "quest",
    "rune",
    "sprig",
    "tide",
    "urn",
    "vine",
    "wren",
    "yew",
    "zest",
    "bell",
    "cello",
    "drum",
    "flute",
    "harp",
    "horn",
    "lute",
    "oboe",
    "organ",
    "piano",
    "reed",
    "viola",
    "chord",
    "scale",
    "tempo",
    "tune",
];

/// This computer's identity: an Ed25519 key, made once, in `<home>/fleet-identity` (owner only).
pub fn identity(p: &Paths) -> Result<SigningKey, String> {
    let f = p.file("fleet-identity");
    if let Ok(b) = std::fs::read(&f) {
        if let Ok(seed) = <[u8; 32]>::try_from(b.as_slice()) {
            return Ok(SigningKey::from_bytes(&seed));
        }
        return Err(format!(
            "{} is not a 32-byte key; move it aside and try again",
            f.display()
        ));
    }
    let seed: [u8; 32] = random(32)
        .try_into()
        .map_err(|_| "no random bytes".to_string())?;
    write_private(&f, &seed).map_err(|e| format!("could not write the identity: {e}"))?;
    Ok(SigningKey::from_bytes(&seed))
}

// ---- the code and its invitation

pub fn new_code() -> String {
    let r = random(3);
    format!(
        "{}-{}-{}",
        r[0] % 9 + 1,
        WORDS[r[1] as usize],
        WORDS[r[2] as usize]
    )
}

/// What a person typed, as the code: lower case, words joined by hyphens.
pub fn norm_code(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c == '-')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Invite {
    pub code: String,
    pub expires: u64,
    pub tries: u32,
}

fn read_invite(p: &Paths) -> Option<Invite> {
    let j = parse(&std::fs::read_to_string(p.file(INVITE_FILE)).ok()?).ok()?;
    Some(Invite {
        code: j.str("code")?.to_string(),
        expires: j.f64("expires")? as u64,
        tries: j.f64("tries").unwrap_or(0.0) as u32,
    })
}

fn write_invite(p: &Paths, i: &Invite) -> Result<(), String> {
    let j = J::Obj(vec![
        ("code".into(), J::s(&i.code)),
        ("expires".into(), J::n(i.expires as f64)),
        ("tries".into(), J::n(f64::from(i.tries))),
    ]);
    write_private(&p.file(INVITE_FILE), j.compact().as_bytes()).map_err(|e| e.to_string())
}

pub fn create_invite(p: &Paths, now: u64) -> Result<Invite, String> {
    let i = Invite {
        code: new_code(),
        expires: now + VALID_S,
        tries: 0,
    };
    write_invite(p, &i)?;
    Ok(i)
}

/// Whether a code is waiting to be used.
pub fn invite_open(p: &Paths) -> bool {
    read_invite(p).is_some_and(|i| i.expires > now_s() && i.tries < TRIES)
}

/// One attempt at the invitation: counted before anything is checked, so a wrong guess costs a
/// try even if the connection drops. Returns the code to use, or why there is none.
fn take_attempt(p: &Paths, now: u64) -> Result<String, &'static str> {
    let lock = p.file("fleet-invite.lock");
    for _ in 0..200 {
        if std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .is_ok()
        {
            let r = (|| {
                let mut i = read_invite(p).ok_or("no invitation is open")?;
                if i.expires <= now {
                    let _ = std::fs::remove_file(p.file(INVITE_FILE));
                    return Err("the code ran out");
                }
                i.tries += 1;
                if i.tries >= TRIES {
                    // the last allowed try: it is used, and the invitation is gone either way
                    let _ = std::fs::remove_file(p.file(INVITE_FILE));
                } else {
                    write_invite(p, &i).map_err(|_| "could not keep count")?;
                }
                Ok(i.code)
            })();
            let _ = std::fs::remove_file(&lock);
            return r;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err("busy")
}

// ---- the wire

fn put(w: &mut impl Write, b: &[u8]) -> std::io::Result<()> {
    w.write_all(&(b.len() as u32).to_be_bytes())?;
    w.write_all(b)
}

fn get(r: &mut impl Read, max: usize) -> std::io::Result<Vec<u8>> {
    let mut n = [0u8; 4];
    r.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n) as usize;
    if n > max {
        return Err(std::io::Error::other("too long"));
    }
    let mut v = vec![0u8; n];
    r.read_exact(&mut v)?;
    Ok(v)
}

/// The two keys a pairing makes, one per direction, so each is used for one message.
fn keys(shared: &[u8]) -> ([u8; 32], [u8; 32]) {
    (
        hmac(shared, &[b"tempo pair joiner to main"]),
        hmac(shared, &[b"tempo pair main to joiner"]),
    )
}

fn seal(k: &[u8; 32], aad: &[u8], pt: &[u8]) -> Vec<u8> {
    ChaCha20Poly1305::new(Key::from_slice(k))
        .encrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: pt, aad })
        .unwrap_or_default()
}

fn open(k: &[u8; 32], aad: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    ChaCha20Poly1305::new(Key::from_slice(k))
        .decrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: ct, aad })
        .ok()
}

// ---- what a computer is, and the part it plays

/// What decides a computer's part.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hw {
    pub gpu_gb: f64,
    pub apple: bool,
    pub ram_gb: f64,
    pub cpus: u32,
    pub laptop: bool,
}

impl Hw {
    pub fn from_probe(j: &J) -> Hw {
        let gpu = j.get("gpu");
        let class = gpu.and_then(|g| g.str("class")).unwrap_or("none");
        Hw {
            gpu_gb: if class.starts_with("discrete") {
                gpu.and_then(|g| g.f64("memory_gb")).unwrap_or(0.0)
            } else {
                0.0
            },
            apple: class.starts_with("Apple"),
            ram_gb: j.f64("memory_gb_total").unwrap_or(0.0),
            cpus: j.f64("logical_cpus").unwrap_or(0.0) as u32,
            laptop: j.bool("laptop").unwrap_or(false),
        }
    }
}

/// A big graphics card (or an Apple chip with room): the engine, and the tools beside it. A
/// laptop: a client. A small box: a runner, for jobs.
pub fn choose_role(h: &Hw) -> &'static str {
    if h.gpu_gb >= 8.0 || (h.apple && h.ram_gb >= 16.0) {
        "engine"
    } else if h.laptop {
        "client"
    } else if h.ram_gb < 8.0 || h.cpus <= 4 {
        "runner"
    } else {
        "client"
    }
}

pub fn role_words(role: &str) -> &'static str {
    match role {
        "engine" => "the engine, with its tools",
        "client" => "a client",
        "runner" => "a runner",
        _ => "a player",
    }
}

fn is_laptop() -> bool {
    if cfg!(target_os = "linux") {
        return std::fs::read_dir("/sys/class/power_supply")
            .map(|d| {
                d.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("BAT"))
            })
            .unwrap_or(false);
    }
    if cfg!(target_os = "macos") {
        return crate::setup::probe::run_with_timeout(
            "pmset",
            &["-g", "batt"],
            Duration::from_secs(2),
        )
        .is_some_and(|t| t.contains("InternalBattery"));
    }
    false
}

/// The hardware look the new computer hands over: the scan facts, a battery, ComfyUI and the MCP
/// servers by name.
pub fn probe_json(p: &Paths) -> J {
    let m = crate::setup::probe::probe(
        &crate::setup::probe::RealHost,
        &p.home,
        crate::setup::apply::ollama_port(),
    );
    let mut j = crate::setup::mcp::scan_facts(&m);
    j.set("laptop", J::Bool(is_laptop()));
    j.set(
        "ollama_models",
        J::strings(
            &m.ollama
                .models
                .iter()
                .map(|x| x.name.clone())
                .collect::<Vec<_>>(),
        ),
    );
    if let Some(t) = super::tools::advert(p, true) {
        j.set("tools", t);
    }
    j
}

// ---- the main computer's side

fn unique_name(f: &store::Fleet, want: &str) -> String {
    let base = store::clean_name(want);
    let base = if base.is_empty() {
        "computer".into()
    } else {
        base
    };
    if f.find(&base).is_none() && base != f.me {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| f.find(c).is_none())
        .unwrap_or(base)
}

/// A computer is knocking with `PAIR`. Say nothing useful to anyone who does not know the code.
pub fn serve_pair(node: &Arc<Node>, mut r: BufReader<TcpStream>, peer: SocketAddr) {
    let Ok(mut w) = r.get_ref().try_clone() else {
        return;
    };
    let _ = r.get_ref().set_read_timeout(Some(Duration::from_secs(20)));
    let refuse = |w: &mut TcpStream, why: &str| {
        let _ = w.write_all(&[0]);
        let _ = put(w, why.as_bytes());
    };
    let mut head = [0u8; 5];
    if r.read_exact(&mut head).is_err() || &head[..4] != b"PAIR" || head[4] != 1 {
        return;
    }
    if !crate::private_ip(peer.ip()) {
        return;
    }
    let code = match take_attempt(&node.paths, now_s()) {
        Ok(c) => c,
        Err(why) => return refuse(&mut w, why),
    };
    let Ok(msg_a) = get(&mut r, 128) else { return };
    let (state, msg_b) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_bytes()),
        &Identity::new(ID_JOINER),
        &Identity::new(ID_MAIN),
    );
    if w.write_all(&[1]).is_err() || put(&mut w, &msg_b).is_err() {
        return;
    }
    let Ok(shared) = state.finish(&msg_a) else {
        return refuse(&mut w, "that code was not accepted");
    };
    let (k_ab, k_ba) = keys(&shared);
    let Ok(ct) = get(&mut r, 1 << 17) else { return };
    let Some(info) = open(&k_ab, b"join", &ct)
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| parse(&t).ok())
    else {
        return refuse(&mut w, "that code was not accepted");
    };
    // the code was right: the invitation is spent
    let _ = std::fs::remove_file(node.paths.file(INVITE_FILE));
    let id = info.str("id").and_then(unhex).filter(|k| k.len() == 32);
    let addr = peer.ip().to_string();
    let (Some(id), true) = (id, allowed_remote_host(&addr)) else {
        return refuse(&mut w, "that computer's identity was not usable");
    };
    let probe = info.get("probe").cloned().unwrap_or(J::Null);
    let role = choose_role(&Hw::from_probe(&probe));
    let port = info
        .f64("port")
        .map_or(store::fleet_port(), |p| p as u16)
        .max(1);
    let wanted = info.str("name").unwrap_or("computer").to_string();
    let added = store::update(&node.paths, |f| {
        // the same computer joining again keeps its place
        let name = match f.machines.iter().position(|m| m.id == hex(&id)) {
            Some(i) => f.machines[i].name.clone(),
            None => unique_name(f, &wanted),
        };
        let mut m = Machine::new(&name, &addr, role);
        m.port = port;
        m.id = hex(&id);
        m.probe = probe.clone();
        m.tools = probe.get("tools").cloned().unwrap_or(J::Null);
        match f.machines.iter().position(|x| x.name == name) {
            Some(i) => f.machines[i] = m.clone(),
            None => f.machines.push(m.clone()),
        }
        m
    });
    let Ok(m) = added else {
        return refuse(&mut w, "the main computer could not write its fleet");
    };
    let _ = store::config_set(
        &node.paths,
        crate::setup::propose::real_pinned(),
        &m,
        true,
        now_s(),
    );
    let reply = J::Obj(vec![
        ("name".into(), J::s(&node.me)),
        ("id".into(), J::s(&node.identity_hex())),
        ("key".into(), J::s(&hex(&node.key()))),
        ("you_are".into(), J::s(&m.name)),
        ("role".into(), J::s(role)),
        ("port".into(), J::n(f64::from(store::fleet_port()))),
    ]);
    let _ = w.write_all(&[1]);
    let _ = put(&mut w, &seal(&k_ba, b"welcome", reply.compact().as_bytes()));
    println!("{} joined the band as {}.", m.name, role_words(role));
}

// ---- the new computer's side

#[derive(Clone, Debug, PartialEq)]
pub struct Joined {
    /// The main computer's name, its identity (hex) and the fleet key.
    pub main: String,
    pub id: String,
    pub key: [u8; 32],
    pub port: u16,
    /// The name the main computer gave this one, and the part it chose.
    pub you_are: String,
    pub role: String,
}

pub fn join(
    p: &Paths,
    addr: SocketAddr,
    code: &str,
    my_name: &str,
    my_port: u16,
    probe: &J,
) -> Result<Joined, String> {
    if !crate::private_ip(addr.ip()) {
        return Err(format!(
            "{} is not a private address; Tempo only joins on your own networks",
            addr.ip()
        ));
    }
    let id = identity(p)?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| format!("could not reach {addr} ({e})"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(20)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(20)));
    let code = norm_code(code);
    let (state, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &Identity::new(ID_JOINER),
        &Identity::new(ID_MAIN),
    );
    let io = |e: std::io::Error| format!("the connection broke ({e})");
    let mut ok = [0u8; 1];
    let said =
        |s: &mut TcpStream| String::from_utf8_lossy(&get(s, 300).unwrap_or_default()).into_owned();
    // a computer that turns us away does it before it has read all we sent: its reason is
    // waiting to be read, so a failed write is followed by a read
    let sent = s.write_all(b"PAIR\x01").and_then(|()| put(&mut s, &msg_a));
    if let Err(e) = sent {
        return Err(match s.read_exact(&mut ok) {
            Ok(()) if ok[0] == 0 => said(&mut s),
            _ => io(e),
        });
    }
    s.read_exact(&mut ok).map_err(io)?;
    if ok[0] != 1 {
        return Err(said(&mut s));
    }
    let msg_b = get(&mut s, 128).map_err(io)?;
    let shared = state
        .finish(&msg_b)
        .map_err(|_| "the main computer sent something that is not a pairing".to_string())?;
    let (k_ab, k_ba) = keys(&shared);
    let info = J::Obj(vec![
        ("name".into(), J::s(my_name)),
        ("id".into(), J::s(&hex(id.verifying_key().as_bytes()))),
        ("port".into(), J::n(f64::from(my_port))),
        ("probe".into(), probe.clone()),
    ]);
    put(&mut s, &seal(&k_ab, b"join", info.compact().as_bytes())).map_err(io)?;
    s.read_exact(&mut ok).map_err(io)?;
    if ok[0] != 1 {
        return Err(said(&mut s));
    }
    let ct = get(&mut s, 1 << 16).map_err(io)?;
    let j = open(&k_ba, b"welcome", &ct)
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| parse(&t).ok())
        .ok_or("the main computer did not know the code")?;
    let key = j
        .str("key")
        .and_then(unhex)
        .and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok());
    let id_ok = j.str("id").and_then(unhex).is_some_and(|k| k.len() == 32);
    let (Some(key), true) = (key, id_ok) else {
        return Err("the welcome was not complete".into());
    };
    Ok(Joined {
        main: store::clean_name(j.str("name").unwrap_or("main")),
        id: j.str("id").unwrap_or("").to_string(),
        key,
        port: j.f64("port").map_or(store::fleet_port(), |p| p as u16),
        you_are: store::clean_name(j.str("you_are").unwrap_or(my_name)),
        role: j.str("role").unwrap_or("client").to_string(),
    })
}

/// Keep what joining gave: the fleet key, the main computer's identity and place in the fleet,
/// and this computer's name as the main computer knows it.
pub fn keep(p: &Paths, j: &Joined, main_addr: &str) -> Result<(), String> {
    if let Some(old) = store::read_key(p) {
        if old != j.key {
            let _ = std::fs::copy(p.file("fleet.key"), p.file("fleet.key.old"));
        }
    }
    write_private(&p.file("fleet.key"), &j.key)
        .map_err(|e| format!("could not write fleet.key: {e}"))?;
    store::update(p, |f| {
        f.me = j.you_are.clone();
        let mut m = Machine::new(&j.main, main_addr, "both");
        m.port = j.port;
        m.id = j.id.clone();
        match f.machines.iter().position(|x| x.name == j.main) {
            Some(i) => f.machines[i] = m,
            None => f.machines.push(m),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_word_list_is_256_distinct_words() {
        let mut w = WORDS.to_vec();
        w.sort_unstable();
        w.dedup();
        assert_eq!(w.len(), 256);
        assert!(WORDS
            .iter()
            .all(|x| x.len() >= 3 && x.bytes().all(|b| b.is_ascii_lowercase())));
    }

    #[test]
    fn a_code_is_a_digit_and_two_words_and_survives_how_people_type_it() {
        for _ in 0..50 {
            let c = new_code();
            let parts: Vec<&str> = c.split('-').collect();
            assert_eq!(parts.len(), 3, "{c}");
            assert!(
                ("1"..="9").contains(&parts[0])
                    && WORDS.contains(&parts[1])
                    && WORDS.contains(&parts[2])
            );
            assert_eq!(
                norm_code(&format!("  {}  ", c.replace('-', " ").to_uppercase())),
                c
            );
        }
        assert_ne!(new_code(), new_code());
    }

    #[test]
    fn the_part_follows_the_hardware() {
        let hw = |gpu: f64, apple: bool, ram: f64, cpus: u32, laptop: bool| Hw {
            gpu_gb: gpu,
            apple,
            ram_gb: ram,
            cpus,
            laptop,
        };
        assert_eq!(choose_role(&hw(24.0, false, 64.0, 16, false)), "engine");
        assert_eq!(
            choose_role(&hw(8.0, false, 16.0, 8, true)),
            "engine",
            "a gaming laptop with a big card is still an engine"
        );
        assert_eq!(choose_role(&hw(0.0, true, 32.0, 10, true)), "engine");
        assert_eq!(choose_role(&hw(0.0, true, 8.0, 8, true)), "client");
        assert_eq!(choose_role(&hw(0.0, false, 16.0, 8, true)), "client");
        assert_eq!(choose_role(&hw(4.0, false, 32.0, 12, false)), "client");
        assert_eq!(choose_role(&hw(0.0, false, 4.0, 4, false)), "runner");
        assert_eq!(choose_role(&hw(0.0, false, 16.0, 2, false)), "runner");
        let j = parse(
            r#"{"gpu":{"class":"discrete NVIDIA","memory_gb":24.0},"memory_gb_total":64.0,"logical_cpus":16}"#,
        )
        .unwrap();
        assert_eq!(choose_role(&Hw::from_probe(&j)), "engine");
    }
}
