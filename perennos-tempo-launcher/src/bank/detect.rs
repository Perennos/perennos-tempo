//! Is it installed? A tool counts when its program is on PATH (absolute entries only), in one of
//! its known install places, or in Tempo's own folder for it. The version comes from running the
//! program with the catalog's fixed arguments, with a short time limit.

use super::catalog::Tool;
use crate::setup::json::parse;
use crate::setup::probe::run_with_timeout;
use crate::Ctx;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The computer, as detection sees it. `real()` in the program; tests give their own PATH.
#[derive(Clone, Debug)]
pub struct Sys {
    pub os: String,
    pub path: Vec<PathBuf>,
}

impl Sys {
    pub fn real() -> Sys {
        Sys {
            os: std::env::consts::OS.into(),
            path: std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
        }
    }

    fn exts(&self) -> &'static [&'static str] {
        // .cmd and .bat are left out: running one with arguments goes through cmd.exe
        if self.os == "windows" {
            &[".exe", ".com"]
        } else {
            &[""]
        }
    }

    /// `name` in the first absolute PATH folder that has it.
    pub fn which(&self, name: &str) -> Option<PathBuf> {
        self.path
            .iter()
            .filter(|d| d.is_absolute())
            .flat_map(|d| {
                self.exts()
                    .iter()
                    .map(move |e| d.join(format!("{name}{e}")))
            })
            .find(|p| p.is_file())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub bin: PathBuf,
    pub version: Option<String>,
}

/// `%VAR%` filled in from the environment; None when a variable is not set.
fn fill_vars(p: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = p;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let j = tail.find('%')?;
        out.push_str(&std::env::var(&tail[..j]).ok().filter(|v| !v.is_empty())?);
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Does a folder or file name match a pattern with at most one `*`?
fn name_matches(pat: &str, name: &str) -> bool {
    match pat.split_once('*') {
        None => pat == name,
        Some((a, b)) => name.len() >= a.len() + b.len() && name.starts_with(a) && name.ends_with(b),
    }
}

/// Every file a known path points at, `*` standing for any part of one folder or file name. The
/// later name of two (a newer version, say) comes first.
pub fn expand(pattern: &str) -> Vec<PathBuf> {
    let Some(p) = fill_vars(pattern) else {
        return vec![];
    };
    let sep = |c: char| c == '/' || c == '\\';
    let mut parts = p.split(sep).filter(|s| !s.is_empty());
    let mut level: Vec<PathBuf> = vec![if p.starts_with(sep) {
        PathBuf::from("/")
    } else {
        // a drive such as C: (Windows) is the first part and gets its slash back
        match parts.next() {
            Some(d) if d.ends_with(':') => {
                PathBuf::from(format!("{d}{}", std::path::MAIN_SEPARATOR))
            }
            Some(d) => PathBuf::from(d),
            None => return vec![],
        }
    }];
    for part in parts {
        let mut next = Vec::new();
        for base in &level {
            if part.contains('*') {
                let mut names: Vec<String> = std::fs::read_dir(base)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| name_matches(part, n))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|n| base.join(n)));
            } else {
                next.push(base.join(part));
            }
        }
        level = next;
    }
    level.retain(|f| f.is_file());
    level.reverse();
    level
}

/// Tempo's own folder for a tool it fetched: `<home>/crate/tools/<id>`.
pub fn own_dir(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.home.join("crate").join("tools").join(id)
}

fn in_own_dir(dir: &Path, names: &[String], exts: &[&str]) -> Option<PathBuf> {
    let mut level = vec![dir.to_path_buf()];
    for _ in 0..4 {
        let mut next = Vec::new();
        for d in &level {
            let Ok(rd) = std::fs::read_dir(d) else {
                continue;
            };
            let mut es: Vec<_> = rd.flatten().collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let p = e.path();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    next.push(p);
                } else if ft.is_file() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if names
                        .iter()
                        .any(|b| exts.iter().any(|x| n == format!("{b}{x}")))
                    {
                        return Some(p);
                    }
                }
            }
        }
        level = next;
    }
    None
}

/// The first number-looking word of the first line: "Blender 4.5.1 LTS" gives 4.5.1.
pub fn parse_version(out: &str) -> Option<String> {
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    line.split_whitespace().find_map(|w| {
        let w = w.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | ':'));
        let w = match w.strip_prefix(['v', 'V']) {
            Some(r) if r.starts_with(|c: char| c.is_ascii_digit()) => r,
            _ => w,
        };
        (w.starts_with(|c: char| c.is_ascii_digit()) && w.len() <= 24).then(|| w.to_string())
    })
}

fn agent_has(ctx: &Ctx, key: &str) -> bool {
    ctx.agent_config_file()
        .or_else(|| Some(ctx.agent_data.join("agent-config.json")).filter(|p| p.is_file()))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| parse(&t).ok())
        .and_then(|j| j.str(key).map(|v| !v.trim().is_empty()))
        .unwrap_or(false)
}

/// Where the tool's program is, without asking it anything. `None`: not found.
pub fn locate(ctx: &Ctx, sys: &Sys, t: &Tool) -> Option<PathBuf> {
    let bins = t.bins(&sys.os);
    let mut bin = bins.iter().find_map(|b| sys.which(b));
    if bin.is_none() {
        bin = t
            .detect
            .paths
            .iter()
            .filter(|(os, _)| *os == sys.os)
            .flat_map(|(_, ps)| ps)
            .find_map(|p| expand(p).into_iter().next());
    }
    if bin.is_none() {
        bin = in_own_dir(&own_dir(ctx, &t.id), bins, sys.exts());
    }
    bin
}

/// Look for one tool and ask it its version. `None`: not found.
pub fn detect(ctx: &Ctx, sys: &Sys, t: &Tool) -> Option<Found> {
    if let Some(key) = &t.detect.agent_setting {
        // linked, not installed by Tempo: there is no program to run or ask
        return agent_has(ctx, key).then(|| Found {
            bin: PathBuf::from(format!("(the agent's {key} setting)")),
            version: None,
        });
    }
    let bin = locate(ctx, sys, t)?;
    let version = t.detect.version.as_ref().and_then(|args| {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        run_with_timeout(&bin.to_string_lossy(), &a, Duration::from_secs(5))
            .and_then(|o| parse_version(&o))
    });
    Some(Found { bin, version })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::bank::catalog::embedded;

    pub fn tmp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tempo-crate-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn ctx_in(home: &Path) -> Ctx {
        Ctx {
            home: home.to_path_buf(),
            agent_data: home.join("agent"),
            vault: home.join("vault"),
            bin_dirs: vec![],
            browser: false,
            opened: Default::default(),
        }
    }

    /// A program that prints `says` when run: a script on Unix; on Windows a stand-in file only.
    pub fn fake_bin(dir: &Path, name: &str, says: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        });
        std::fs::write(&p, format!("#!/bin/sh\necho '{says}'\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    pub fn sys_with(os: &str, dirs: &[&Path]) -> Sys {
        Sys {
            os: os.into(),
            path: dirs.iter().map(|d| d.to_path_buf()).collect(),
        }
    }

    #[test]
    fn versions_are_read_from_the_first_line() {
        for (out, want) in [
            ("Blender 4.5.1 LTS\n\tbuild date: x", "4.5.1"),
            ("ffmpeg version 7.1 Copyright (c) 2000", "7.1"),
            ("sox:      SoX v14.4.2\n", "14.4.2"),
            ("rg 14.1.0\n", "14.1.0"),
            ("2025.01.01\n", "2025.01.01"),
            ("Version: ImageMagick 7.1.1-43 Q16", "7.1.1-43"),
        ] {
            assert_eq!(parse_version(out).as_deref(), Some(want), "{out}");
        }
        assert_eq!(parse_version("no numbers here"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn a_star_stands_for_part_of_one_folder_name() {
        let d = tmp("glob");
        for v in ["Blender 3.6", "Blender 4.5", "Other"] {
            let f = d.join(v);
            std::fs::create_dir_all(&f).unwrap();
            std::fs::write(f.join("blender.exe"), "").unwrap();
        }
        let hit = expand(&format!("{}/Blender */blender.exe", d.display()));
        assert_eq!(hit.len(), 2);
        assert!(
            hit[0].to_string_lossy().contains("4.5"),
            "the later version first"
        );
        assert!(expand(&format!("{}/Nope */x", d.display())).is_empty());
        assert!(expand("%NO_SUCH_VARIABLE_XYZ%/x").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fake_programs_on_path_are_found_with_their_version() {
        let d = tmp("detect");
        let ctx = ctx_in(&d.join("home"));
        let bin = d.join("bin");
        fake_bin(&bin, "ffmpeg", "ffmpeg version 7.1 Copyright (c) 2000");
        fake_bin(&bin, "rg", "ripgrep 14.1.0");
        let sys = sys_with("linux", &[&bin]);
        let tools = embedded();
        let get = |id: &str| tools.iter().find(|t| t.id == id).unwrap();
        let f = detect(&ctx, &sys, get("ffmpeg")).unwrap();
        assert_eq!(f.version.as_deref(), Some("7.1"));
        assert_eq!(f.bin, bin.join("ffmpeg"));
        assert_eq!(
            detect(&ctx, &sys, get("ripgrep"))
                .unwrap()
                .version
                .as_deref(),
            Some("14.1.0")
        );
        assert!(detect(&ctx, &sys, get("pandoc")).is_none());
        // fd is `fdfind` on Debian
        fake_bin(&bin, "fdfind", "fdfind 9.0.0");
        assert!(detect(&ctx, &sys, get("fd"))
            .unwrap()
            .bin
            .ends_with("fdfind"));
        // a relative PATH entry is never searched: a planted program would run as Tempo
        let rel = Sys {
            os: "linux".into(),
            path: vec![PathBuf::from("bin")],
        };
        assert!(detect(&ctx, &rel, get("ffmpeg")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn imagemagick_on_windows_ignores_convert_and_tempos_own_folder_counts() {
        let d = tmp("own");
        let ctx = ctx_in(&d.join("home"));
        let bin = d.join("bin");
        fake_bin(&bin, "convert", "x");
        let tools = embedded();
        let im = tools.iter().find(|t| t.id == "imagemagick").unwrap();
        assert!(detect(&ctx, &sys_with("linux", &[&bin]), im).is_some());
        // on windows only `magick` counts (and the stand-in has no .exe)
        assert!(detect(&ctx, &sys_with("windows", &[&bin]), im).is_none());
        // a tool Tempo fetched itself lives under crate/tools/<id>
        let own = own_dir(&ctx, "imagemagick").join("x").join("bin");
        fake_bin(&own, "convert", "ImageMagick 7.1.0");
        let f = detect(&ctx, &sys_with("linux", &[]), im).unwrap();
        assert!(f.bin.starts_with(own_dir(&ctx, "imagemagick")));
    }

    #[test]
    fn comfyui_is_detected_by_the_agents_setting() {
        let d = tmp("comfy");
        let ctx = ctx_in(&d);
        let tools = embedded();
        let c = tools.iter().find(|t| t.id == "comfyui").unwrap();
        let sys = sys_with("linux", &[]);
        assert!(detect(&ctx, &sys, c).is_none());
        std::fs::create_dir_all(&ctx.agent_data).unwrap();
        std::fs::write(
            ctx.agent_data.join("config.json"),
            r#"{"comfy_url":"http://127.0.0.1:8188"}"#,
        )
        .unwrap();
        assert!(detect(&ctx, &sys, c).is_some());
    }
}
