//! End to end: the real `tempo-studio` binary on synthetic WAVs, the MCP server over
//! stdin/stdout, and (on Unix) the REAPER paths with a shell-script stand-in for REAPER.
//!
//! Nothing here starts the real REAPER: every test that could reach it either forces
//! `--engine builtin`, points `--reaper` at the stand-in, or checks `reaper locate` first.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempo_studio::wav::{write_new, OutFormat};

const BIN: &str = env!("CARGO_BIN_EXE_tempo-studio");

static N: AtomicUsize = AtomicUsize::new(0);

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "tstudio-it-{}-{}-{tag}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&p).unwrap();
    // Canonical, so paths compare equal to the resolved ones in reports.
    std::fs::canonicalize(&p).unwrap()
}

/// The binary with a clean environment for REAPER lookups (an empty PATH folder, no
/// TEMPO_REAPER, no config).
fn cmd(dir: &Path) -> Command {
    let empty = dir.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let mut c = Command::new(BIN);
    c.current_dir(dir)
        .env_remove("TEMPO_REAPER")
        .env_remove("TEMPO_STUDIO_CONFIG")
        .env("PATH", &empty);
    c
}

fn run(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let o = cmd(dir).args(args).output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn run_json(dir: &Path, args: &[&str]) -> Value {
    let (code, out, err) = run(dir, args);
    assert_eq!(code, 0, "{args:?}\nstdout: {out}\nstderr: {err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

fn db(x: f64) -> f64 {
    10f64.powf(x / 20.0)
}

/// Music-like stereo: kick, bass, a chord, a lead and noise-burst hats; peak scaled to `peak_db`.
fn music(secs: f64, peak_db: f64) -> Vec<f64> {
    use std::f64::consts::PI;
    let fs = 48000.0;
    let n = (fs * secs) as usize;
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut v = Vec::with_capacity(2 * n);
    for i in 0..n {
        let t = i as f64 / fs;
        let beat = t % 0.5;
        let kick =
            (-beat / 0.06).exp() * (2.0 * PI * (50.0 + 80.0 * (-beat / 0.02).exp()) * beat).sin();
        let bass = 0.5 * (2.0 * PI * 55.0 * t).sin();
        let chord: f64 = [220.0, 261.63, 329.63]
            .iter()
            .map(|f| 0.15 * (2.0 * PI * f * t).sin())
            .sum();
        let lead = 0.2
            * (2.0
                * PI
                * if (t as u64).is_multiple_of(2) {
                    880.0
                } else {
                    1318.5
                }
                * t)
                .sin();
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let w = (seed >> 11) as f64 / (1u64 << 52) as f64 - 1.0;
        let hat = 0.3 * (-(t % 0.25) / 0.03).exp() * w;
        v.push(0.9 * kick + bass + chord + 1.2 * lead + hat);
        v.push(0.9 * kick + bass + chord + 0.8 * lead + 0.7 * hat);
    }
    let peak = v.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    v.iter().map(|x| x * db(peak_db) / peak).collect()
}

/// Stereo 1 kHz tone: integrated loudness equals its peak level.
fn tone(path: &Path, ch: u16, level_db: f64, secs: f64, freq: f64) {
    let n = (48000.0 * secs) as usize;
    let mut v = Vec::with_capacity(n * ch as usize);
    for i in 0..n {
        let fade = (i.min(n - 1 - i) as f64 / 480.0).min(1.0);
        let x =
            fade * db(level_db) * (2.0 * std::f64::consts::PI * freq * i as f64 / 48000.0).sin();
        for _ in 0..ch {
            v.push(x);
        }
    }
    write_new(path, 48000, ch, OutFormat::Pcm24, &v).unwrap();
}

fn lufs(v: &Value) -> f64 {
    v["files"][0]["loudness"]["integrated_lufs"]
        .as_f64()
        .unwrap()
}

#[test]
fn analyze_master_to_minus_14_and_check_by_re_analysis() {
    let d = tmp("master");
    write_new(
        &d.join("song.wav"),
        48000,
        2,
        OutFormat::Float32,
        &music(12.0, -12.0),
    )
    .unwrap();
    let a = run_json(&d, &["analyze", "song.wav", "--json"]);
    let before = lufs(&a);
    assert!(before < -20.0, "{before}");
    let m = run_json(
        &d,
        &[
            "master",
            "song.wav",
            "--preset",
            "streaming",
            "--out",
            "out.wav",
            "--engine",
            "builtin",
            "--json",
        ],
    );
    assert_eq!(m["within_target"], true, "{m:#}");
    assert_eq!(m["engine"], "builtin");
    // Re-analysis of the written file, by a separate run of the binary.
    let a = run_json(&d, &["analyze", "out.wav", "--json"]);
    let l = a["files"][0]["loudness"].clone();
    let i = l["integrated_lufs"].as_f64().unwrap();
    let tp = l["true_peak_dbtp"].as_f64().unwrap();
    assert!((i + 14.0).abs() <= 0.5, "integrated {i}");
    assert!(tp <= -1.0 + 0.1, "true peak {tp}");
    assert_eq!(a["files"][0]["format"], "PCM 24-bit, 2 ch, 48000 Hz");
    // The human summary.
    let (code, out, _) = run(&d, &["analyze", "out.wav"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("integrated -1") && out.contains("LUFS"),
        "{out}"
    );
    // Never overwrite: the second master gets a new name; an input is refused even with --force.
    let m2 = run_json(
        &d,
        &[
            "master", "song.wav", "--out", "out.wav", "--engine", "builtin", "--json",
        ],
    );
    assert!(
        m2["path"].as_str().unwrap().ends_with("out-2.wav"),
        "{}",
        m2["path"]
    );
    let (code, _, err) = run(
        &d,
        &[
            "master", "song.wav", "--out", "song.wav", "--force", "--engine", "builtin",
        ],
    );
    assert_eq!(code, 1);
    assert!(err.contains("over an input"), "{err}");
    // Target and ceiling instead of a preset.
    let m3 = run_json(
        &d,
        &[
            "master",
            "song.wav",
            "--target",
            "-16",
            "--ceiling",
            "-2",
            "--out",
            "t.wav",
            "--engine",
            "builtin",
            "--json",
        ],
    );
    assert_eq!(m3["settings"]["target_lufs"], -16.0);
    assert_eq!(m3["within_target"], true);
}

#[test]
fn auto_engine_uses_the_builtin_engine_with_a_note_when_reaper_is_missing() {
    let d = tmp("auto");
    tone(&d.join("song.wav"), 2, -20.0, 4.0, 1000.0);
    let (code, out, _) = run(&d, &["reaper", "locate"]);
    let loc: Value = serde_json::from_str(&out).unwrap();
    if loc["found"] == true {
        // A machine with REAPER installed: do not start it from a test; check what was found.
        assert_eq!(code, 0);
        assert!(Path::new(loc["exe"].as_str().unwrap()).is_file());
        return;
    }
    assert_eq!(code, 1);
    assert!(!loc["tried"].as_array().unwrap().is_empty());
    let mut c = cmd(&d);
    let o = c
        .env("TEMPO_REAPER", d.join("no-such-reaper"))
        .args(["master", "song.wav", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let m: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(m["engine"], "builtin");
    let note = m["notes"][0].as_str().unwrap();
    assert!(
        note.contains("REAPER not found") && note.contains("no-such-reaper"),
        "{note}"
    );
    assert!(note.contains("plan B"), "{note}");
}

#[test]
fn mix_of_three_stems_with_and_without_mastering() {
    let d = tmp("mix");
    let stems = d.join("stems");
    std::fs::create_dir_all(&stems).unwrap();
    tone(&stems.join("kick.wav"), 1, -3.0, 6.0, 60.0);
    tone(&stems.join("bass.wav"), 2, -12.0, 6.0, 110.0);
    tone(&stems.join("lead vox.wav"), 1, -6.0, 5.0, 440.0);
    let m = run_json(
        &d,
        &[
            "mix", "stems", "--out", "mix.wav", "--engine", "builtin", "--json",
        ],
    );
    assert!(d.join("mix.wav").is_file() && d.join("mix.plan.json").is_file());
    let roles: Vec<&str> = m["stems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["bass", "kick", "vocal"]);
    let peak = m["mix"]["loudness"]["sample_peak_dbfs"].as_f64().unwrap();
    assert!(peak < -6.0, "{peak}");
    // The plan on disk is what `reaper script` reads.
    let plan: Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("mix.plan.json")).unwrap()).unwrap();
    assert_eq!(plan["stems"].as_array().unwrap().len(), 3);
    let s = run_json(
        &d,
        &[
            "reaper",
            "script",
            "mix.plan.json",
            "--out",
            "session.lua",
            "--render",
        ],
    );
    assert_eq!(s["session"]["render"], true);
    let lua = std::fs::read_to_string(d.join("session.lua")).unwrap();
    assert!(
        lua.contains("ReaLimit") && lua.contains("\"lead vox\"") && lua.contains("hp_hz = 90.0000")
    );
    // With mastering: re-analysis of the master.
    let m = run_json(
        &d,
        &[
            "mix",
            "stems",
            "--out",
            "song.wav",
            "--master-preset",
            "streaming",
            "--engine",
            "builtin",
            "--json",
        ],
    );
    assert_eq!(m["master_report"]["within_target"], true, "{m:#}");
    let a = run_json(&d, &["analyze", "song-master.wav", "--json"]);
    assert!((lufs(&a) + 14.0).abs() <= 0.5);
    // The human summary.
    let (code, out, _) = run(
        &d,
        &["mix", "stems", "--out", "h.wav", "--engine", "builtin"],
    );
    assert_eq!(code, 0);
    assert!(out.contains("Mixed 3 stems"), "{out}");
}

#[test]
fn exit_codes_for_usage_and_errors() {
    let d = tmp("codes");
    tone(&d.join("a.wav"), 2, -20.0, 1.0, 1000.0);
    for (args, want) in [
        (vec![], 2),
        (vec!["--help"], 0),
        (vec!["dance"], 2),
        (vec!["analyze"], 2),
        (vec!["analyze", "a.wav", "--loud"], 2),
        (vec!["master"], 2),
        (vec!["master", "a.wav", "--preset", "nope"], 2),
        (vec!["master", "a.wav", "--target", "x"], 2),
        (vec!["mix", "a.wav"], 2),
        (vec!["reaper"], 2),
        (vec!["reaper", "fly"], 2),
        (vec!["analyze", "missing.wav"], 1),
        (vec!["master", "missing.wav", "--engine", "builtin"], 1),
        (vec!["--config", "missing.json", "analyze", "a.wav"], 1),
        (vec!["analyze", "a.wav"], 0),
    ] {
        let (code, out, err) = run(&d, &args);
        assert_eq!(code, want, "{args:?}\nstdout {out}\nstderr {err}");
    }
    // A silent file cannot be mastered: a clear error.
    write_new(
        &d.join("silence.wav"),
        48000,
        2,
        OutFormat::Pcm24,
        &vec![0.0; 48000 * 2],
    )
    .unwrap();
    let (code, _, err) = run(&d, &["master", "silence.wav", "--engine", "builtin"]);
    assert_eq!(code, 1);
    assert!(err.contains("silent"), "{err}");
}

struct Mcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    out: BufReader<std::process::ChildStdout>,
}

impl Mcp {
    fn start(dir: &Path, root: &Path) -> Mcp {
        let mut child = cmd(dir)
            .args(["mcp", "--allow-root", &root.display().to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Mcp { child, stdin, out }
    }

    fn send(&mut self, m: &Value) {
        writeln!(self.stdin, "{m}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn ask(&mut self, m: &Value) -> Value {
        self.send(m);
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    }

    fn call(&mut self, id: u64, name: &str, args: Value) -> (bool, Value) {
        let r = self.ask(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}}));
        assert_eq!(r["id"], id);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        (
            r["result"]["isError"].as_bool().unwrap(),
            serde_json::from_str(text).unwrap(),
        )
    }
}

#[test]
fn mcp_server_over_stdin_and_stdout() {
    let d = tmp("mcp");
    let root = d.join("outputs");
    std::fs::create_dir_all(root.join("stems")).unwrap();
    write_new(
        &root.join("song.wav"),
        48000,
        2,
        OutFormat::Float32,
        &music(8.0, -10.0),
    )
    .unwrap();
    tone(&root.join("stems").join("kick.wav"), 1, -3.0, 3.0, 60.0);
    tone(&root.join("stems").join("keys.wav"), 2, -9.0, 3.0, 523.0);
    tone(&d.join("outside.wav"), 2, -20.0, 1.0, 1000.0);
    let mut m = Mcp::start(&d, &root);
    let init = m.ask(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "it", "version": "1"}}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "tempo-studio");
    m.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let list = m.ask(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 6);
    let song = root.join("song.wav").display().to_string();
    let (err, a) = m.call(3, "studio_analyze", json!({"paths": [song]}));
    assert!(
        !err && a["files"][0]["loudness"]["integrated_lufs"].is_number(),
        "{a:#}"
    );
    let (err, r) = m.call(
        4,
        "studio_master",
        json!({"input": song, "preset": "streaming", "engine": "builtin"}),
    );
    assert!(!err, "{r:#}");
    assert_eq!(r["within_target"], true);
    let out = root.join("song-master.wav");
    assert_eq!(r["path"], out.display().to_string());
    let (_, a) = m.call(
        5,
        "studio_analyze",
        json!({"paths": [out.display().to_string()]}),
    );
    assert!((lufs(&a) + 14.0).abs() <= 0.5);
    let (err, mx) = m.call(
        6,
        "studio_mix",
        json!({"stems": [root.join("stems").display().to_string()],
        "output": root.join("mix.wav").display().to_string(), "engine": "builtin"}),
    );
    assert!(!err, "{mx:#}");
    assert!(mx["mix"]["loudness"]["sample_peak_dbfs"].as_f64().unwrap() < -6.0);
    let (err, e) = m.call(
        7,
        "studio_analyze",
        json!({"paths": [d.join("outside.wav").display().to_string()]}),
    );
    assert!(
        err && e["error"]
            .as_str()
            .unwrap()
            .contains("outside the allowed folders"),
        "{e}"
    );
    let (err, e) = m.call(8, "studio_master", json!({"input": "song.wav"}));
    assert!(
        err && e["error"].as_str().unwrap().contains("absolute"),
        "{e}"
    );
    let (err, e) = m.call(9, "studio_mix", json!({"stems": [d.display().to_string()], "output": root.join("x.wav").display().to_string()}));
    assert!(
        err && e["error"].as_str().unwrap().contains("outside"),
        "{e}"
    );
    let bad = m.ask(&json!({"jsonrpc": "2.0", "id": 10, "method": "nope"}));
    assert_eq!(bad["error"]["code"], -32601);
    drop(m.stdin);
    let status = m.child.wait().unwrap();
    assert!(
        status.success(),
        "the server exits cleanly when stdin closes"
    );
}

#[cfg(unix)]
mod fake_reaper {
    //! REAPER stand-in: a POSIX shell script that copies a fixture WAV where REAPER would render
    //! and answers a session script with a status file, so the real process runner is exercised.
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake(dir: &Path, name: &str, fixture: &Path) -> PathBuf {
        let p = dir.join(name);
        let script = r#"#!/bin/sh
# The binary runs with an empty PATH folder (so a real REAPER is never found); the tools
# this script uses live in the usual places.
PATH=/usr/bin:/bin:/usr/local/bin
export PATH
FIXTURE='__FIXTURE__'
echo "$*" >> "$0.log"
last=""
render=0
for a in "$@"; do
  if [ "$a" = "-renderproject" ]; then render=1; fi
  last="$a"
done
if [ "$render" = 1 ]; then
  dir=$(sed -n 's/^ *RENDER_FILE "\(.*\)"$/\1/p' "$last" | head -n 1)
  pat=$(sed -n 's/^ *RENDER_PATTERN "\(.*\)"$/\1/p' "$last" | head -n 1)
  cp "$FIXTURE" "$dir/$pat.wav"
  exit 0
fi
case "$last" in
  *.lua)
    tag=$(sed -n 's/^-- tempo-studio-session: //p' "$last")
    get() { printf '%s' "$tag" | sed "s/.*\"$1\":\"\([^\"]*\)\".*/\1/"; }
    status=$(get status); project=$(get project); out=$(get render)
    n=$(grep -c '^    { name = ' "$last")
    printf '<REAPER_PROJECT 0.1 "7.22/fake" 1\n  <TRACK {1}\n    NAME a\n    VOLPAN 1 0 -1 -1 1\n    ISBUS 0 0\n  >\n>\n' > "$project"
    cp "$FIXTURE" "$out"
    printf '{"done":true,"ok":true,"tracks":%s,"items":%s,"project":"%s","render_file":"%s","rendered":true,"steps":[],"warnings":[],"fx_missing":[]}\n' "$n" "$n" "$project" "$out" > "$status"
    ;;
esac
exit 0
"#
        .replace("__FIXTURE__", &fixture.display().to_string());
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn reaper_render_and_sessions_with_a_stand_in_reaper() {
        let d = tmp("fake");
        tone(&d.join("at-14.wav"), 2, -14.2, 3.0, 1000.0);
        tone(&d.join("at-17.wav"), 2, -17.0, 3.0, 1000.0);
        let ok = fake(&d, "reaper-ok", &d.join("at-14.wav"));
        let quiet = fake(&d, "reaper-quiet", &d.join("at-17.wav"));
        let ok_s = ok.display().to_string();
        // locate: --reaper wins.
        let l = run_json(&d, &["--reaper", &ok_s, "reaper", "locate"]);
        assert_eq!(
            (l["found"].as_bool(), l["source"].as_str()),
            (Some(true), Some("--reaper"))
        );
        // A project rendered headlessly and verified: on target the first time.
        std::fs::write(
            d.join("song.rpp"),
            "<REAPER_PROJECT 0.1 \"7.22/x\" 1\n  <TRACK {1}\n    VOLPAN 1 0 -1 -1 1\n  >\n>\n",
        )
        .unwrap();
        let r = run_json(
            &d,
            &[
                "--reaper",
                &ok_s,
                "reaper",
                "render",
                "song.rpp",
                "--preset",
                "streaming",
            ],
        );
        assert_eq!(r["within_target"], true, "{r:#}");
        assert_eq!(r["attempts"].as_array().unwrap().len(), 1);
        let log = std::fs::read_to_string(d.join("reaper-ok.log")).unwrap();
        assert!(log.contains("-newinst -nosplash -renderproject"), "{log}");
        // A REAPER that stays 3 dB short: two corrected re-renders, then plan C.
        let r = run_json(
            &d,
            &[
                "--reaper",
                &quiet.display().to_string(),
                "reaper",
                "render",
                "song.rpp",
                "--preset",
                "streaming",
            ],
        );
        assert_eq!(r["attempts"].as_array().unwrap().len(), 3, "{r:#}");
        assert_eq!(r["within_target"], true);
        assert!(r["builtin_fallback"]["path"].is_string());
        let fin = r["final_path"].as_str().unwrap().to_string();
        let a = run_json(&d, &["analyze", &fin, "--json"]);
        assert!((lufs(&a) + 14.0).abs() <= 0.5);
        // mix with --engine reaper: built-in mix and plan, then the REAPER session and render.
        let stems = d.join("stems");
        std::fs::create_dir_all(&stems).unwrap();
        tone(&stems.join("kick.wav"), 1, -3.0, 3.0, 60.0);
        tone(&stems.join("vox.wav"), 1, -9.0, 3.0, 440.0);
        let m = run_json(
            &d,
            &[
                "--reaper",
                &ok_s,
                "mix",
                "stems",
                "--out",
                "m.wav",
                "--engine",
                "reaper",
                "--master-preset",
                "streaming",
                "--json",
            ],
        );
        assert_eq!(m["reaper"]["within_target"], true, "{m:#}");
        assert_eq!(m["reaper"]["session"]["status"]["items"], 2);
        assert!(
            d.join("m.wav").is_file()
                && d.join("m-reaper.wav").is_file()
                && d.join("m-reaper-session.lua").is_file()
        );
        // master with --engine reaper.
        let r = run_json(
            &d,
            &[
                "--reaper",
                &ok_s,
                "master",
                "at-17.wav",
                "--out",
                "mm.wav",
                "--engine",
                "reaper",
                "--json",
            ],
        );
        assert_eq!(r["engine"], "reaper", "{r:#}");
        assert_eq!(r["final_path"], d.join("mm.wav").display().to_string());
        // open: visible start, not waited for.
        let o = run_json(&d, &["--reaper", &ok_s, "reaper", "open", "song.rpp"]);
        assert!(o["pid"].as_u64().unwrap() > 0);
    }
}
