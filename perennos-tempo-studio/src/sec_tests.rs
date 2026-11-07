//! Security regression tests (SEC-VAULT-STUDIO). Each one failed before its fix.
use crate::mcp::serve;
use crate::ops::testctx::{ctx, found, not_found};
use crate::reaper::testfake::{tone_at, FakeReaper};
use crate::reaper::{Cmd, Proc, Runner};
use crate::util::testutil::tmpdir;
use crate::util::Roots;
use crate::wav::testwav::{chunk, fmt_chunk, riff};
use crate::wav::{WavError, WavReader};
use serde_json::{json, Value};
use std::io::Cursor;
#[cfg(unix)]
use std::path::Path;

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
}

fn run(c: &crate::ops::Ctx, roots: &Roots, lines: &[Value]) -> Vec<Value> {
    let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let mut out = Vec::new();
    serve(c, roots, Cursor::new(input), &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn text(r: &Value) -> String {
    r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[test]
fn sec_wav_headers_with_absurd_rates_or_channel_counts_are_refused() {
    let data = |n: usize| chunk(b"data", &vec![0x40u8; n], true);
    // like `fmt_chunk`, but the byte-rate field wraps instead of overflowing the test helper
    let fmt = |ch: u16, rate: u32, bits: u16| {
        let ba = ch.wrapping_mul(bits / 8);
        let mut b = Vec::new();
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        for x in [1u16, ch] {
            b.extend_from_slice(&x.to_le_bytes());
        }
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&rate.wrapping_mul(ba as u32).to_le_bytes());
        b.extend_from_slice(&ba.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b
    };
    for (rate, ch, bits) in [
        (u32::MAX, 2u16, 16u16),
        (1_000_000_000, 1, 16),
        (1, 2, 16),
        (999, 1, 16),
        (48_000, 255 * 2, 8),
        (48_000, 257, 8),
    ] {
        let wav = riff(&[
            fmt(ch, rate, bits),
            data(2 * ch as usize * bits as usize / 8),
        ]);
        let r = WavReader::new(Cursor::new(wav));
        assert!(
            matches!(r, Err(WavError::Unsupported(_))),
            "{rate} Hz {ch} ch {bits}-bit: {:?}",
            r.err()
        );
    }
    // what real files look like stays readable
    for (rate, ch, bits) in [
        (8_000, 1, 16),
        (44_100, 2, 16),
        (96_000, 6, 24),
        (384_000, 2, 32),
        (48_000, 64, 8),
    ] {
        let wav = riff(&[
            fmt_chunk(1, ch, rate, bits),
            data(4 * ch as usize * bits as usize / 8),
        ]);
        assert!(
            WavReader::new(Cursor::new(wav)).is_ok(),
            "{rate} {ch} {bits}"
        );
    }
}

#[test]
fn sec_wav_parser_survives_mutated_headers() {
    // flip bytes of a valid header and chunk sizes at random; nothing may panic or hang
    let base = riff(&[
        fmt_chunk(1, 2, 48_000, 16),
        chunk(b"LIST", b"INFOabcd", true),
        chunk(b"data", &[1u8; 64], true),
    ]);
    let mut x: u64 = 0x2545F4914F6CDD1D;
    let mut rnd = |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n as u64) as usize
    };
    for i in 0..20_000 {
        let mut w = base.clone();
        for _ in 0..1 + rnd(4) {
            let at = rnd(w.len());
            w[at] = match rnd(4) {
                0 => 0xFF,
                1 => 0,
                2 => w[at].wrapping_add(1),
                _ => rnd(256) as u8,
            };
        }
        if i % 5 == 0 {
            w.truncate(rnd(w.len() + 1));
        }
        if let Ok(mut r) = WavReader::new(Cursor::new(w)) {
            let mut blk = Vec::new();
            let mut n = 0;
            while r.read_frames(&mut blk, 7).map(|n| n > 0).unwrap_or(false) && n < 100 {
                n += 1;
            }
            let _ = crate::loudness::Meter::for_info(r.info());
        }
    }
}

#[cfg(unix)]
#[test]
fn sec_analyzing_a_fifo_does_not_hang_the_server() {
    let d = tmpdir("fifo");
    let fifo = d.join("pipe.wav");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let (tx, rx) = std::sync::mpsc::channel();
    let p = fifo.clone();
    std::thread::spawn(move || {
        let _ = tx.send(WavReader::open(&p).map(|_| ()));
    });
    let r = rx.recv_timeout(std::time::Duration::from_secs(5));
    assert!(
        matches!(r, Ok(Err(_))),
        "opening a pipe must be refused, not wait: {r:?}"
    );
}

#[test]
fn sec_windows_device_names_and_streams_are_refused_in_allowed_roots() {
    use crate::util::windows_unsafe_name;
    for bad in [
        "CON",
        "con.wav",
        "NUL.txt",
        "Aux",
        "PRN.x",
        "COM1.wav",
        "lpt9",
        "CONIN$",
        "a.wav:stream",
        "a.wav::$DATA",
        "x.wav.",
        "x.wav ",
    ] {
        assert!(windows_unsafe_name(bad), "{bad}");
    }
    for ok in [
        "console.wav",
        "com10.wav",
        "Concerto.wav",
        "my song.wav",
        "nul-point.wav",
        "a.b.wav",
    ] {
        assert!(!windows_unsafe_name(ok), "{ok}");
    }
}

#[cfg(unix)]
#[test]
fn sec_roots_hold_against_symlinks_dotdot_and_sibling_prefixes() {
    use std::os::unix::fs::symlink;
    let d = tmpdir("roots");
    let allowed = d.join("music");
    let sibling = d.join("music2");
    let outside = d.join("secret");
    for p in [&allowed, &sibling, &outside] {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(outside.join("s.wav"), b"x").unwrap();
    symlink(&outside, allowed.join("link")).unwrap();
    symlink(outside.join("s.wav"), allowed.join("file.wav")).unwrap();
    let roots = Roots::new(std::slice::from_ref(&allowed));
    let s = |p: &Path| p.display().to_string();
    assert!(roots.check(&s(&allowed.join("ok.wav"))).is_ok());
    assert!(roots.check(&s(&allowed.join("new/dir/ok.wav"))).is_ok());
    for bad in [
        s(&sibling.join("x.wav")),
        s(&allowed.join("../secret/s.wav")),
        s(&allowed.join("link/s.wav")),
        s(&allowed.join("link/new.wav")),
        s(&allowed.join("file.wav")),
        s(&outside),
        "/etc/passwd".into(),
        "relative/x.wav".into(),
        "".into(),
        "  ".into(),
        format!("{}/\u{0}x.wav", allowed.display()),
    ] {
        assert!(roots.check(&bad).is_err(), "{bad}");
    }
    // `link/..` means the parent of the link's target to the OS but the folder holding the link to
    // the lexical resolver; the path it hands back is what is used, and that stays inside
    let r = roots
        .check(&s(&allowed.join("link/../secret/s.wav")))
        .unwrap();
    assert!(r.starts_with(&allowed), "{}", r.display());
    // the folder itself is allowed; a file named like the folder with a suffix is not
    assert!(roots.check(&s(&allowed)).is_ok());
}

#[cfg(unix)]
#[test]
fn sec_force_never_trashes_through_a_linked_trash_folder() {
    use crate::util::OutFile;
    use std::os::unix::fs::symlink;
    let d = tmpdir("trashlink");
    let out = tmpdir("trashout");
    std::fs::write(d.join("a.wav"), b"old").unwrap();
    symlink(&out, d.join(".tempo-trash")).unwrap();
    let (o, mut f) = OutFile::create(&d.join("a.wav"), true, &[]).unwrap();
    std::io::Write::write_all(&mut f, b"new").unwrap();
    drop(f);
    assert!(
        o.commit().is_err(),
        "the old file must not be moved out of the folder"
    );
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    assert_eq!(std::fs::read(d.join("a.wav")).unwrap(), b"old");
}

#[test]
fn sec_force_moves_the_old_output_to_the_dated_trash_and_never_an_input() {
    use crate::util::OutFile;
    let d = tmpdir("force");
    std::fs::write(d.join("a.wav"), b"old").unwrap();
    let (o, mut f) = OutFile::create(&d.join("a.wav"), true, &[]).unwrap();
    std::io::Write::write_all(&mut f, b"new").unwrap();
    drop(f);
    let p = o.commit().unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"new");
    let trash = std::fs::read_dir(d.join(".tempo-trash"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        trash.file_name().unwrap().to_string_lossy().len(),
        8,
        "YYYYMMDD"
    );
    assert_eq!(std::fs::read(trash.join("a.wav")).unwrap(), b"old");
    // an input is refused even with force
    assert!(OutFile::create(&d.join("a.wav"), true, &[d.join("a.wav")]).is_err());
    // without force an existing name moves on to a free one
    std::fs::write(d.join("b.wav"), b"keep").unwrap();
    let (o, f) = OutFile::create(&d.join("b.wav"), false, &[]).unwrap();
    drop(f);
    assert!(o.path.ends_with("b-2.wav"));
    assert_eq!(std::fs::read(d.join("b.wav")).unwrap(), b"keep");
}

/// Everything outside Lua string literals in a generated script.
fn without_strings(lua: &str) -> String {
    let (mut out, mut in_s, mut esc) = (String::new(), false, false);
    for c in lua.chars() {
        if in_s {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_s = false;
            }
        } else if c == '"' {
            in_s = true;
        } else {
            out.push(c);
        }
    }
    assert!(!in_s, "an unterminated string literal");
    out
}

#[test]
fn sec_hostile_names_stay_inside_lua_strings() {
    use crate::reaper::{session_script, SessionPlan, TrackPlan, SESSION_TAG};
    let evil = [
        "x]] os.execute(\"calc\") --[[",
        "a\"); os.execute(\"calc\"); (\"",
        "line1\nos.execute('calc')\n-- ",
        "\\\"; os.execute(1) --",
        "]=] os.exit() --[=[",
        "nul\0byte\r\nreturn",
        "--[==[ long comment start",
        "\u{2028}os.execute(1)\u{2029}",
    ];
    for e in evil {
        let s = SessionPlan {
            sample_rate: 48_000,
            tracks: vec![TrackPlan {
                name: e.to_string(),
                file: std::path::PathBuf::from(format!("/music/{e}.wav")),
                volume_db: 0.0,
                highpass_hz: None,
                compress: false,
                comp_threshold_db: -18.0,
            }],
            master: None,
            project_path: format!("/music/{e}.rpp").into(),
            render_path: format!("/music/{e}-out.wav").into(),
            status_path: format!("/music/{e}.status.json").into(),
            render: true,
            close: false,
        };
        let lua = session_script(&s);
        // the tag line is one comment line, whatever the paths hold
        let tag_line = lua.lines().find(|l| l.starts_with(SESSION_TAG)).unwrap();
        assert!(
            !lua[lua.find(tag_line).unwrap()..].starts_with("--[["),
            "{e:?}"
        );
        // outside its string literals, the generated data table holds no attacker text at all
        let table =
            &lua[lua.find("local P = {").unwrap()..lua.len() - crate::reaper::LUA_BODY.len()];
        let stripped = without_strings(table);
        for needle in ["os.", "calc", "return", "exit", "execute", "--", "[=", "[["] {
            assert!(
                !stripped.contains(needle),
                "{needle} escaped its string for {e:?}: {stripped}"
            );
        }
        let head: String = lua[..lua.find("local P = {").unwrap()].to_string();
        assert!(
            head.lines().all(|l| l.is_empty() || l.starts_with("--")),
            "{e:?}"
        );
        assert!(
            head.lines().count() < 6,
            "no extra lines from the tag for {e:?}"
        );
    }
}

struct Panicking;
struct NoProc;
impl Proc for NoProc {
    fn id(&self) -> u32 {
        1
    }
    fn try_wait(&mut self) -> Result<Option<i32>, String> {
        Ok(Some(0))
    }
}
impl Runner for Panicking {
    fn start(&self, _: &Cmd) -> Result<Box<dyn Proc>, String> {
        panic!("a bug deep in a tool");
    }
}

#[test]
fn sec_a_panic_in_a_tool_is_an_error_answer_not_a_dead_server() {
    let d = tmpdir("panic");
    let song = d.join("song.wav");
    tone_at(&song, -20.0);
    let c = ctx(&Panicking, &found);
    let roots = Roots::new(std::slice::from_ref(&d));
    let out = run(
        &c,
        &roots,
        &[
            call(
                1,
                "reaper_open",
                json!({"path": song.display().to_string()}),
            ),
            call(
                2,
                "studio_analyze",
                json!({"paths": [song.display().to_string()]}),
            ),
        ],
    );
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0]["result"]["isError"], true);
    assert!(
        text(&out[0]).contains("internal error"),
        "{}",
        text(&out[0])
    );
    assert_eq!(out[1]["result"]["isError"], false, "the server kept going");
    let _ = NoProc;
}

#[test]
fn sec_mcp_reaper_open_never_runs_scripts_or_programs() {
    let d = tmpdir("openlua");
    let fake = FakeReaper::new(-14.0);
    let c = ctx(&fake, &found);
    let roots = Roots::new(std::slice::from_ref(&d));
    let mut calls = vec![];
    for (i, name) in [
        "evil.lua",
        "evil.eel",
        "evil.py",
        "evil.bat",
        "evil.exe",
        "evil.sh",
        "evil.js",
        "evil.dll",
        "noext",
        "evil.LUA",
        "x.wav.lua",
    ]
    .iter()
    .enumerate()
    {
        std::fs::write(d.join(name), "os.execute('calc')").unwrap();
        calls.push(call(
            i as u64,
            "reaper_open",
            json!({"path": d.join(name).display().to_string()}),
        ));
    }
    std::fs::write(d.join("ok.rpp"), "<REAPER_PROJECT 0.1\n>\n").unwrap();
    tone_at(&d.join("ok.wav"), -20.0);
    calls.push(call(
        100,
        "reaper_open",
        json!({"path": d.join("ok.rpp").display().to_string()}),
    ));
    calls.push(call(
        101,
        "reaper_open",
        json!({"path": d.join("ok.wav").display().to_string()}),
    ));
    let out = run(&c, &roots, &calls);
    for o in &out[..11] {
        assert_eq!(o["result"]["isError"], true, "{o}");
        assert!(text(o).contains("not opened from here"), "{}", text(o));
    }
    assert_eq!(out[11]["result"]["isError"], false, "{}", text(&out[11]));
    assert_eq!(out[12]["result"]["isError"], false, "{}", text(&out[12]));
    assert_eq!(
        fake.calls.borrow().len(),
        2,
        "REAPER was started only for the project and the audio"
    );
    let _ = not_found;
}
