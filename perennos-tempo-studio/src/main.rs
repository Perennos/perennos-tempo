use serde_json::Value;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tempo_studio::config::{Config, Engine};
use tempo_studio::master::MasterSettings;
use tempo_studio::ops::{self, Ctx, MasterArgs, MixArgs};
use tempo_studio::reaper::{self, Os, SystemRunner, Target, Wait};
use tempo_studio::util::{fmt_db, Roots};
use tempo_studio::wav::OutFormat;
use tempo_studio::{master, mcp};

const HELP: &str = "Perennos Tempo studio: open REAPER and mix and master songs.

USAGE:
    tempo-studio analyze <files...> [--json]
    tempo-studio master <in.wav> [--preset P | --target LUFS --ceiling DBTP] [--out F]
                        [--engine auto|reaper|builtin] [--float] [--force] [--json]
    tempo-studio mix <stems-folder | files...> --out F [--master-preset P] [--reference LUFS]
                     [--engine auto|reaper|builtin] [--float] [--force] [--json]
    tempo-studio reaper locate
    tempo-studio reaper open <file>
    tempo-studio reaper script <plan.json> --out <session.lua> [--render] [--close]
    tempo-studio reaper render <project.rpp> [--preset P | --target LUFS --ceiling DBTP] [--out F]
    tempo-studio mcp [--allow-root DIR]...

Plan A is REAPER (found by --reaper, env TEMPO_REAPER, config reaper_exe, then the standard
install paths). Plan B is the built-in engine. --engine auto (the default) uses the built-in
engine and never opens a window; REAPER (which opens its window) runs only with --engine reaper
or `reaper open`. If REAPER fails, the built-in engine finishes the job.

PRESETS: streaming -14 LUFS / -1 dBTP (default), apple -16 / -1, club -9 / -0.3,
         podcast -16 / -1, broadcast -23 / -1.

Outputs are always new files: an existing name gets a -2, -3 ... suffix. With --force the old
file is moved to .tempo-trash/<date>/ next to it first; nothing is deleted, and an input is never
written over.

GLOBAL OPTIONS:
    --config PATH     JSON config (env TEMPO_STUDIO_CONFIG): reaper_exe, engine, preset,
                      allow_roots, render_timeout_s, reference_lufs, output_format
    --reaper PATH     the REAPER program
    --timeout S       seconds to wait for REAPER (default 540)
    --json            print the full JSON report
    -h, --help        this help

Exit codes: 0 ok, 1 error, 2 usage.
";

enum Fail {
    Usage(String),
    Run(String),
}

impl From<String> for Fail {
    fn from(e: String) -> Fail {
        Fail::Run(e)
    }
}

#[derive(Default)]
struct Args {
    pos: Vec<String>,
    config: Option<String>,
    reaper: Option<String>,
    engine: Option<String>,
    out: Option<String>,
    preset: Option<String>,
    target: Option<f64>,
    ceiling: Option<f64>,
    master_preset: Option<String>,
    reference: Option<f64>,
    timeout: Option<u64>,
    allow_roots: Vec<String>,
    json: bool,
    force: bool,
    float: bool,
    render: bool,
    close: bool,
    help: bool,
}

fn parse(argv: Vec<String>) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = argv.into_iter();
    while let Some(x) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        let number = |name: &str, v: String| -> Result<f64, String> {
            v.parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .ok_or_else(|| format!("{name} needs a number, got {v}"))
        };
        match x.as_str() {
            "-h" | "--help" => a.help = true,
            "--json" => a.json = true,
            "--force" => a.force = true,
            "--float" => a.float = true,
            "--render" => a.render = true,
            "--close" => a.close = true,
            "--config" => a.config = Some(val("--config")?),
            "--reaper" => a.reaper = Some(val("--reaper")?),
            "--engine" => a.engine = Some(val("--engine")?),
            "--out" | "-o" => a.out = Some(val("--out")?),
            "--preset" => a.preset = Some(val("--preset")?),
            "--master-preset" => a.master_preset = Some(val("--master-preset")?),
            "--allow-root" => a.allow_roots.push(val("--allow-root")?),
            "--target" => a.target = Some(number("--target", val("--target")?)?),
            "--ceiling" => a.ceiling = Some(number("--ceiling", val("--ceiling")?)?),
            "--reference" => a.reference = Some(number("--reference", val("--reference")?)?),
            "--timeout" => {
                let v = val("--timeout")?;
                a.timeout = Some(
                    v.parse::<u64>()
                        .ok()
                        .filter(|t| *t > 0)
                        .ok_or_else(|| format!("--timeout needs whole seconds, got {v}"))?,
                )
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => a.pos.push(x),
        }
    }
    Ok(a)
}

fn print_json(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    );
}

fn loud_line(l: &Value) -> String {
    let f = |k: &str| fmt_db(l[k].as_f64());
    format!(
        "integrated {} LUFS | momentary max {} | short-term max {} | LRA {} LU | sample peak {} dBFS | true peak {} dBTP",
        f("integrated_lufs"),
        f("momentary_max_lufs"),
        f("short_term_max_lufs"),
        f("lra_lu"),
        f("sample_peak_dbfs"),
        f("true_peak_dbtp")
    )
}

fn print_notes(v: &Value) {
    for n in v["notes"].as_array().into_iter().flatten() {
        println!("  note: {}", n.as_str().unwrap_or(""));
    }
}

fn verdict(v: &Value) -> &'static str {
    match v["within_target"].as_bool() {
        Some(true) => "within target",
        Some(false) => "NOT within target",
        None => "no target",
    }
}

fn print_master(v: &Value) {
    if v["engine"] == "reaper" {
        println!(
            "Mastered with REAPER: {} ({})",
            v["final_path"].as_str().unwrap_or("-"),
            verdict(v)
        );
        println!("  {}", loud_line(&v["final"]["loudness"]));
    } else {
        println!(
            "Mastered (built-in): {} ({}, {} pass(es))",
            v["path"].as_str().unwrap_or("-"),
            verdict(v),
            v["passes"].as_array().map(|p| p.len()).unwrap_or(0)
        );
        println!("  {}", loud_line(&v["output"]["loudness"]));
    }
    print_notes(v);
}

fn print_mix(v: &Value) {
    let stems = v["stems"].as_array().map(|s| s.len()).unwrap_or(0);
    match v["mix"]["path"].as_str() {
        Some(p) => {
            println!("Mixed {stems} stems to {p} (trim {} dB)", v["trim_db"]);
            println!("  {}", loud_line(&v["mix"]["loudness"]));
            println!("  plan: {}", v["plan_path"].as_str().unwrap_or("-"));
        }
        None => println!(
            "REAPER mixed the stems: {} ({})",
            v["final_path"].as_str().unwrap_or("-"),
            verdict(v)
        ),
    }
    if let Some(m) = v.get("master_report").filter(|m| !m.is_null()) {
        println!(
            "Mastered (built-in): {} ({})",
            m["path"].as_str().unwrap_or("-"),
            verdict(m)
        );
        println!("  {}", loud_line(&m["output"]["loudness"]));
    }
    if let Some(r) = v.get("reaper").filter(|m| !m.is_null()) {
        println!(
            "REAPER render: {} ({})",
            r["final_path"].as_str().unwrap_or("-"),
            verdict(r)
        );
        print_notes(r);
    }
    print_notes(v);
}

fn settings(a: &Args, cfg: &Config) -> Result<MasterSettings, Fail> {
    let mut s = ops::settings_from(a.preset.as_deref(), a.target, a.ceiling, &cfg.preset)
        .map_err(Fail::Usage)?;
    s.format = if a.float {
        OutFormat::Float32
    } else {
        cfg.output_format
    };
    Ok(s)
}

fn one_path(a: &Args, what: &str) -> Result<PathBuf, Fail> {
    match a.pos.as_slice() {
        [p] => Ok(PathBuf::from(p)),
        [] => Err(Fail::Usage(format!("give {what}"))),
        _ => Err(Fail::Usage(format!("give one {what}, got {}", a.pos.len()))),
    }
}

fn real_main() -> Result<(), Fail> {
    let mut a = parse(std::env::args().skip(1).collect()).map_err(Fail::Usage)?;
    if a.help {
        print!("{HELP}");
        return Ok(());
    }
    if a.pos.is_empty() {
        eprint!("{HELP}");
        return Err(Fail::Usage("no command".into()));
    }
    let cmd = a.pos.remove(0);
    let cfg = Config::load(
        a.config.as_deref(),
        std::env::var("TEMPO_STUDIO_CONFIG").ok(),
    )?;
    for w in &cfg.warnings {
        eprintln!("[warning: {w}]");
    }
    let engine = match &a.engine {
        Some(e) => Engine::parse(e).map_err(Fail::Usage)?,
        None => cfg.engine,
    };
    let runner = SystemRunner;
    let locator = |flag: Option<&str>, conf: Option<&str>| reaper::locate_here(flag, conf);
    let ctx = Ctx {
        wait: Wait {
            timeout: Duration::from_secs(a.timeout.unwrap_or(cfg.render_timeout_s)),
            poll: Duration::from_millis(250),
        },
        cfg,
        reaper_flag: a.reaper.clone(),
        runner: &runner,
        os: Os::current(),
        locator: &locator,
    };
    match cmd.as_str() {
        "analyze" => {
            if a.pos.is_empty() {
                return Err(Fail::Usage("analyze needs one or more WAV files".into()));
            }
            let files: Vec<PathBuf> = a.pos.iter().map(PathBuf::from).collect();
            let (v, ok) = ops::analyze(&files);
            if a.json {
                print_json(&v);
            } else {
                for f in v["files"].as_array().into_iter().flatten() {
                    let path = f["path"].as_str().unwrap_or("?");
                    if let Some(e) = f.get("error") {
                        println!("{path}: ERROR {}", e.as_str().unwrap_or(""));
                        continue;
                    }
                    println!(
                        "{path}: {}, {:.2} s",
                        f["format"].as_str().unwrap_or(""),
                        f["loudness"]["duration_s"].as_f64().unwrap_or(0.0)
                    );
                    println!("  {}", loud_line(&f["loudness"]));
                    for w in f["warnings"].as_array().into_iter().flatten() {
                        println!("  warning: {}", w.as_str().unwrap_or(""));
                    }
                }
            }
            if !ok {
                return Err(Fail::Run("some files could not be analyzed".into()));
            }
        }
        "master" => {
            let input = one_path(&a, "one input WAV")?;
            let s = settings(&a, &ctx.cfg)?;
            let v = ops::master(
                &ctx,
                &MasterArgs {
                    input,
                    out: a.out.as_ref().map(PathBuf::from),
                    settings: s,
                    engine,
                    force: a.force,
                },
            )?;
            if a.json {
                print_json(&v);
            } else {
                print_master(&v);
            }
        }
        "mix" => {
            if a.pos.is_empty() {
                return Err(Fail::Usage("mix needs a stems folder or stem files".into()));
            }
            let out = a
                .out
                .as_ref()
                .map(PathBuf::from)
                .ok_or_else(|| Fail::Usage("mix needs --out <mix.wav>".into()))?;
            let format = if a.float {
                OutFormat::Float32
            } else {
                ctx.cfg.output_format
            };
            let master = match &a.master_preset {
                Some(p) => {
                    let mut s =
                        MasterSettings::from_preset(&master::preset(p).map_err(Fail::Usage)?);
                    s.format = format;
                    Some(s)
                }
                None => None,
            };
            let v = ops::mix(
                &ctx,
                &MixArgs {
                    inputs: a.pos.iter().map(PathBuf::from).collect(),
                    out,
                    engine,
                    master,
                    reference_lufs: a.reference.unwrap_or(ctx.cfg.reference_lufs),
                    format,
                    force: a.force,
                },
            )?;
            if a.json {
                print_json(&v);
            } else {
                print_mix(&v);
            }
        }
        "reaper" => {
            if a.pos.is_empty() {
                return Err(Fail::Usage(
                    "reaper needs locate, open, script or render".into(),
                ));
            }
            let sub = a.pos.remove(0);
            match sub.as_str() {
                "locate" => {
                    let (v, found) = ops::reaper_locate(&ctx);
                    print_json(&v);
                    if !found {
                        return Err(Fail::Run("REAPER not found".into()));
                    }
                }
                "open" => {
                    let f = one_path(&a, "a file to open")?;
                    print_json(&ops::reaper_open(&ctx, &f)?);
                }
                "script" => {
                    let plan = one_path(&a, "a plan JSON")?;
                    let out = a
                        .out
                        .as_ref()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| ops::default_script_for(&plan));
                    let allow = |_: &std::path::Path| Ok(());
                    let s = ops::session_for(&plan, &out, a.render, a.close, &allow)?;
                    print_json(&ops::reaper_build_session(&ctx, &s, &out, false)?);
                }
                "render" => {
                    let rpp = one_path(&a, "a project (.rpp)")?;
                    let target = if a.preset.is_some() || a.target.is_some() || a.ceiling.is_some()
                    {
                        let s = settings(&a, &ctx.cfg)?;
                        Some(Target {
                            target_lufs: s.target_lufs,
                            ceiling_dbtp: s.ceiling_dbtp,
                        })
                    } else {
                        None
                    };
                    let out = a.out.as_ref().map(PathBuf::from);
                    print_json(&ops::reaper_render(&ctx, &rpp, target, out)?);
                }
                other => return Err(Fail::Usage(format!("unknown reaper command {other}"))),
            }
        }
        "mcp" => {
            let roots: Vec<PathBuf> = if !a.allow_roots.is_empty() {
                a.allow_roots.iter().map(PathBuf::from).collect()
            } else if !ctx.cfg.allow_roots.is_empty() {
                ctx.cfg.allow_roots.clone()
            } else {
                vec![std::env::current_dir().map_err(|e| e.to_string())?]
            };
            let roots = Roots::new(&roots);
            eprintln!(
                "[tempo-studio mcp: allowed folders: {}]",
                roots
                    .roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let stdin = std::io::stdin();
            mcp::serve(&ctx, &roots, stdin.lock(), std::io::stdout().lock())?;
        }
        other => return Err(Fail::Usage(format!("unknown command {other}"))),
    }
    Ok(())
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail::Usage(e)) => {
            eprintln!("usage error: {e} (see tempo-studio --help)");
            ExitCode::from(2)
        }
        Err(Fail::Run(e)) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
