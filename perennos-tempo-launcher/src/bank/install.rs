//! `crate add` and `crate remove`: show the exact command, ask, then run it with the person's own
//! package manager (winget, Homebrew, pipx). Anything that needs admin rights (apt) is printed
//! and left to the person: Tempo never runs sudo. A pinned download is checked against its
//! SHA-256 before anything is unpacked or run, and what Tempo fetched itself goes to the trash,
//! never deleted.

use super::catalog::{find, Download, Route, Tool};
use super::detect::{detect, own_dir, Found, Sys};
use super::state::State;
use crate::setup::json::J;
use crate::setup::registry::Fetch;
use crate::setup::runtime::{download_limit_secs, to_trash};
use crate::setup::sha256::file_sha256;
use crate::{stamp, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The package managers, and what they are asked to do. A mock stands in for tests.
pub trait Pm {
    fn which(&self, prog: &str) -> bool;
    /// Run a command with the person watching its output. Ok when it exits 0.
    fn run(&self, argv: &[String]) -> Result<(), String>;
    fn fetcher(&self) -> Option<Arc<dyn Fetch>>;
}

pub struct RealPm<'a>(pub &'a Sys);

impl Pm for RealPm<'_> {
    fn which(&self, prog: &str) -> bool {
        self.0.which(prog).is_some()
    }
    fn run(&self, argv: &[String]) -> Result<(), String> {
        let (prog, args) = argv.split_first().ok_or("nothing to run")?;
        let st = std::process::Command::new(prog)
            .args(args)
            .status()
            .map_err(|e| format!("could not start {prog}: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("{prog} stopped with {st}"))
        }
    }
    fn fetcher(&self) -> Option<Arc<dyn Fetch>> {
        crate::setup::registry::real_fetcher(&crate::setup::probe::RealHost)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    Winget,
    Brew,
    Apt,
    Pipx,
    Download,
}

impl How {
    pub fn label(self) -> &'static str {
        match self {
            How::Winget => "winget",
            How::Brew => "brew",
            How::Apt => "apt",
            How::Pipx => "pipx",
            How::Download => "tempo",
        }
    }

    fn from_label(s: &str) -> Option<How> {
        [How::Winget, How::Brew, How::Apt, How::Pipx, How::Download]
            .into_iter()
            .find(|h| h.label() == s)
    }

    /// The program that has to exist.
    fn program(self) -> Option<&'static str> {
        match self {
            How::Winget => Some("winget"),
            How::Brew => Some("brew"),
            How::Apt => Some("apt-get"),
            How::Pipx => Some("pipx"),
            How::Download => None,
        }
    }

    fn present(self, r: &Route) -> bool {
        match self {
            How::Winget => r.winget.is_some(),
            How::Brew => r.brew.is_some(),
            How::Apt => r.apt.is_some(),
            How::Pipx => r.pipx.is_some(),
            How::Download => r.download.is_some(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub how: How,
    /// What is run; empty for a download (Tempo does that part itself).
    pub argv: Vec<String>,
    /// Needs admin rights, so it is shown and not run.
    pub admin: bool,
    pub download: Option<Download>,
}

/// The order a person on this OS most likely wants.
fn order(os: &str) -> [How; 4] {
    match os {
        "windows" => [How::Winget, How::Download, How::Pipx, How::Apt],
        "macos" => [How::Brew, How::Download, How::Pipx, How::Apt],
        _ => [How::Apt, How::Download, How::Pipx, How::Brew],
    }
}

fn words(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn install_argv(how: How, r: &Route) -> Vec<String> {
    let name = |o: &Option<String>| o.clone().unwrap_or_default();
    match how {
        How::Winget => {
            let mut a = words(&["winget", "install", "--id"]);
            a.push(name(&r.winget));
            a.extend(words(&[
                "-e",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ]));
            a
        }
        How::Brew => {
            let mut a = words(&["brew", "install"]);
            if r.cask {
                a.push("--cask".into());
            }
            a.push(name(&r.brew));
            a
        }
        How::Apt => {
            let mut a = words(&["sudo", "apt-get", "install", "-y"]);
            a.push(name(&r.apt));
            a
        }
        How::Pipx => {
            let mut a = words(&["pipx", "install"]);
            a.push(name(&r.pipx));
            a
        }
        How::Download => vec![],
    }
}

fn remove_argv(how: How, r: &Route) -> Vec<String> {
    let name = |o: &Option<String>| o.clone().unwrap_or_default();
    match how {
        How::Winget => {
            let mut a = words(&["winget", "uninstall", "--id"]);
            a.push(name(&r.winget));
            a.push("-e".into());
            a
        }
        How::Brew => {
            let mut a = words(&["brew", "uninstall"]);
            if r.cask {
                a.push("--cask".into());
            }
            a.push(name(&r.brew));
            a
        }
        How::Apt => {
            let mut a = words(&["sudo", "apt-get", "remove"]);
            a.push(name(&r.apt));
            a
        }
        How::Pipx => {
            let mut a = words(&["pipx", "uninstall"]);
            a.push(name(&r.pipx));
            a
        }
        How::Download => vec![],
    }
}

fn no_route(t: &Tool, os: &str, r: Option<&Route>) -> String {
    match r.and_then(|r| r.manual.as_deref()) {
        Some(m) => format!(
            "{} cannot be set up for you on {os}. {m} Homepage: {}",
            t.name, t.homepage
        ),
        None => format!(
            "{} has no install for {os} in the Crate. Homepage: {}",
            t.name, t.homepage
        ),
    }
}

/// How this person would get the tool: the first way their computer can do.
pub fn plan_add(t: &Tool, os: &str, pm: &dyn Pm) -> Result<Plan, String> {
    let r = t.route(os);
    let Some(route) = r.filter(|r| r.automatic()) else {
        return Err(no_route(t, os, r));
    };
    for how in order(os) {
        if !how.present(route) {
            continue;
        }
        if how.program().is_some_and(|p| !pm.which(p)) {
            continue;
        }
        if how == How::Download && pm.fetcher().is_none() {
            continue;
        }
        return Ok(Plan {
            how,
            argv: install_argv(how, route),
            admin: how == How::Apt,
            download: route.download.clone().filter(|_| how == How::Download),
        });
    }
    let needs: Vec<&str> = order(os)
        .into_iter()
        .filter(|h| h.present(route))
        .filter_map(|h| h.program())
        .collect();
    Err(format!(
        "{} comes through {}, and none of that is on this computer. Install one, or get {} from {}.",
        t.name,
        needs.join(" or "),
        t.name,
        t.homepage
    ))
}

/// The same tool taken away again: the way it came, if Tempo knows; else the way it would come.
pub fn plan_remove(t: &Tool, os: &str, pm: &dyn Pm, came: Option<&str>) -> Result<Plan, String> {
    let route = t
        .route(os)
        .ok_or_else(|| format!("{} has no install for {os} in the Crate.", t.name))?;
    let how = match came.and_then(How::from_label) {
        Some(h) => h,
        None => plan_add(t, os, pm)?.how,
    };
    if how == How::Download {
        return Ok(Plan {
            how,
            argv: vec![],
            admin: false,
            download: None,
        });
    }
    if !how.present(route) {
        return Err(format!(
            "{} came in a way this catalog has no uninstall for. Remove it the way you installed it.",
            t.name
        ));
    }
    Ok(Plan {
        how,
        argv: remove_argv(how, route),
        admin: how == How::Apt,
        download: None,
    })
}

/// A command as the person would type it.
pub fn show(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Opts {
    /// Answer y to "run it now".
    pub yes: bool,
    /// Answer y to "let Tempo use it".
    pub enable: bool,
}

fn untar_argv(os: &str, file: &Path, dest: &Path) -> Vec<String> {
    let prog = if os == "windows" {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        format!(r"{}\System32\tar.exe", root.trim_end_matches('\\'))
    } else {
        "tar".into()
    };
    vec![
        prog,
        "-xf".into(),
        file.to_string_lossy().into(),
        "-C".into(),
        dest.to_string_lossy().into(),
    ]
}

/// Fetch a pinned file, check its SHA-256, and only then unpack it into Tempo's own folder for
/// the tool. A file that does not match is moved to the trash and nothing is unpacked.
fn fetch_pinned(
    ctx: &Ctx,
    pm: &dyn Pm,
    os: &str,
    t: &Tool,
    dl: &Download,
    now: u64,
    out: &mut dyn Write,
) -> Result<PathBuf, String> {
    let home = &ctx.home;
    let fetch = pm
        .fetcher()
        .ok_or("this computer has no curl, so Tempo cannot fetch the file")?;
    let dir = home.join("crate").join("downloads");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let have = dir.join(&dl.file);
    let part = dir.join(format!("{}.part", dl.file));
    let sum = |p: &Path| {
        file_sha256(p, &mut |_| {}).map_err(|e| format!("could not read {}: {e}", p.display()))
    };
    if have.is_file() && sum(&have)? != dl.sha256 {
        to_trash(home, &have, now)?;
    }
    if !have.is_file() {
        if part.exists() {
            to_trash(home, &part, now)?;
        }
        let _ = writeln!(
            out,
            "Fetching {} ({} MB) from {}",
            dl.file,
            dl.bytes / 1_000_000,
            dl.url
        );
        fetch
            .download(&dl.url, &part, download_limit_secs(dl.bytes), &mut |_| {})
            .map_err(|e| format!("Could not fetch {}: {e}", dl.file))?;
        let got = sum(&part)?;
        if got != dl.sha256 {
            let moved = to_trash(home, &part, now)?;
            return Err(format!(
                "The file for {} did not match its pinned SHA-256 (wanted {}, got {got}), so Tempo refused it. It was moved to {}; nothing was unpacked or run.",
                t.name,
                dl.sha256,
                moved.display()
            ));
        }
        std::fs::rename(&part, &have)
            .map_err(|e| format!("could not rename {}: {e}", part.display()))?;
        let _ = writeln!(out, "SHA-256 matches the pin.");
    }
    let target = own_dir(ctx, &t.id);
    if target.exists() {
        return Err(format!(
            "{} already exists and Tempo never overwrites a folder. Move it aside, then try again.",
            target.display()
        ));
    }
    let partial = target.with_extension("partial");
    if partial.exists() {
        to_trash(home, &partial, now)?;
    }
    std::fs::create_dir_all(&partial)
        .map_err(|e| format!("could not make {}: {e}", partial.display()))?;
    if dl.archive {
        pm.run(&untar_argv(os, &have, &partial))?;
    } else {
        let dest = partial.join(&dl.file);
        std::fs::copy(&have, &dest).map_err(|e| format!("could not copy {}: {e}", dl.file))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
        }
    }
    std::fs::rename(&partial, &target)
        .map_err(|e| format!("could not finish {}: {e}", target.display()))?;
    Ok(target)
}

fn remember(ctx: &Ctx, id: &str, found: Option<&Found>, how: Option<How>, now: u64) -> State {
    let mut st = State::load(ctx);
    let mut f = vec![
        ("installed", J::Bool(found.is_some())),
        ("checked_at", J::s(&stamp(now))),
        (
            "version",
            found
                .and_then(|f| f.version.as_deref())
                .map_or(J::Null, J::s),
        ),
    ];
    if let Some(h) = how {
        f.push(("how", J::s(h.label())));
    }
    st.set(id, f);
    st
}

fn save(ctx: &Ctx, st: &State, out: &mut dyn Write) -> i32 {
    match st.save(ctx) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            1
        }
    }
}

/// `crate add ID`. Returns the exit code.
#[allow(clippy::too_many_arguments)]
pub fn add(
    ctx: &Ctx,
    sys: &Sys,
    pm: &dyn Pm,
    tools: &[Tool],
    id: &str,
    o: Opts,
    now: u64,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let Some(t) = find(tools, id) else {
        let _ = writeln!(
            out,
            "There is no \"{id}\" in the Crate. `tempo-launch crate` lists what there is."
        );
        return 1;
    };
    let mut how = None;
    let mut found = detect(ctx, sys, t);
    if let Some(f) = &found {
        let _ = writeln!(
            out,
            "{} is already here{}.",
            t.name,
            f.version
                .as_deref()
                .map_or(String::new(), |v| format!(" ({v})"))
        );
    } else {
        let plan = match plan_add(t, &sys.os, pm) {
            Ok(p) => p,
            Err(e) => {
                let _ = writeln!(out, "{e}");
                return 1;
            }
        };
        let _ = writeln!(out, "{} is {}.", t.name, t.size_text());
        match &plan.download {
            Some(d) => {
                let _ = writeln!(
                    out,
                    "Tempo will fetch {} ({} MB) from {}\nIt must match SHA-256 {} before anything is unpacked.",
                    d.file,
                    d.bytes / 1_000_000,
                    d.url,
                    d.sha256
                );
            }
            None => {
                let _ = writeln!(out, "The command:\n\n  {}\n", show(&plan.argv));
            }
        }
        if plan.admin {
            let _ = writeln!(
                out,
                "That needs admin rights, so Tempo does not run it. Run it yourself, then run `tempo-launch crate add {}` again.",
                t.id
            );
            return 0;
        }
        if !(o.yes || ask("Run it now? [y/N] ")) {
            let _ = writeln!(out, "Nothing was installed.");
            return 0;
        }
        let ran = match &plan.download {
            Some(d) => fetch_pinned(ctx, pm, &sys.os, t, d, now, out).map(|_| ()),
            None => pm.run(&plan.argv),
        };
        if let Err(e) = ran {
            let _ = writeln!(out, "{e}");
            return 1;
        }
        how = Some(plan.how);
        found = detect(ctx, sys, t);
        if found.is_none() {
            let _ = writeln!(
                out,
                "The install finished, but {} is not on the path yet. Open a new terminal (the path updates after an install), then run `tempo-launch crate check`.",
                t.name
            );
            return save(ctx, &remember(ctx, &t.id, None, how, now), out);
        }
        let _ = writeln!(out, "{} is installed.", t.name);
    }
    let mut st = remember(ctx, &t.id, found.as_ref(), how, now);
    if t.tools.is_empty() {
        let _ = writeln!(out, "Tempo does not play {} yet: {}", t.name, t.tempo);
    } else if st.enabled(&t.id) {
        let _ = writeln!(out, "Tempo can already use it.");
    } else if o.enable
        || ask(&format!(
            "Let Tempo use {}? Your AI could then run it, only on the folders you allow, and you are asked each time unless you trust it. [y/N] ",
            t.name
        ))
    {
        st.set(&t.id, vec![("enabled", J::Bool(true))]);
        let _ = writeln!(out, "On. Tempo can use {}: {}", t.name, t.tools.join(", "));
    } else {
        let _ = writeln!(
            out,
            "Left off. `tempo-launch crate enable {}` turns it on.",
            t.id
        );
    }
    save(ctx, &st, out)
}

/// `crate remove ID`. Returns the exit code.
#[allow(clippy::too_many_arguments)]
pub fn remove(
    ctx: &Ctx,
    sys: &Sys,
    pm: &dyn Pm,
    tools: &[Tool],
    id: &str,
    o: Opts,
    now: u64,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let Some(t) = find(tools, id) else {
        let _ = writeln!(out, "There is no \"{id}\" in the Crate.");
        return 1;
    };
    if detect(ctx, sys, t).is_none() {
        let _ = writeln!(
            out,
            "{} is not installed, so there is nothing to remove.",
            t.name
        );
        return 0;
    }
    let came = State::load(ctx).how(&t.id);
    let plan = match plan_remove(t, &sys.os, pm, came.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    };
    if plan.how == How::Download {
        let dir = own_dir(ctx, &t.id);
        let _ = writeln!(
            out,
            "Tempo fetched {} itself. It moves {} to the trash folder; nothing is deleted.",
            t.name,
            dir.display()
        );
        if !(o.yes || ask("Move it now? [y/N] ")) {
            let _ = writeln!(out, "Nothing was removed.");
            return 0;
        }
        if let Err(e) = to_trash(&ctx.home, &dir, now) {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    } else {
        let _ = writeln!(out, "The command:\n\n  {}\n", show(&plan.argv));
        if plan.admin {
            let _ = writeln!(
                out,
                "That needs admin rights, so Tempo does not run it. Run it yourself."
            );
            return 0;
        }
        if !(o.yes || ask("Run it now? [y/N] ")) {
            let _ = writeln!(out, "Nothing was removed.");
            return 0;
        }
        if let Err(e) = pm.run(&plan.argv) {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    }
    let mut st = State::load(ctx);
    st.set(
        &t.id,
        vec![
            ("installed", J::Bool(false)),
            ("enabled", J::Bool(false)),
            ("checked_at", J::s(&stamp(now))),
        ],
    );
    let code = save(ctx, &st, out);
    let _ = writeln!(out, "{} is removed, and Tempo no longer uses it.", t.name);
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::catalog::embedded;
    #[cfg(unix)]
    use crate::bank::catalog::load;
    use crate::bank::detect::tests::{ctx_in, fake_bin, sys_with, tmp};
    #[cfg(unix)]
    use crate::setup::registry::Fetched;
    #[cfg(unix)]
    use crate::setup::sha256::sha256_hex;
    use std::cell::RefCell;

    /// A package manager that records what it was asked and, for an install, drops a fake program.
    struct Mock {
        has: Vec<&'static str>,
        ran: RefCell<Vec<Vec<String>>>,
        /// (program, folder) to create when a command runs, as an install would.
        drops: Option<(&'static str, PathBuf)>,
        fetch: Option<Arc<dyn Fetch>>,
    }

    impl Pm for Mock {
        fn which(&self, prog: &str) -> bool {
            self.has.contains(&prog)
        }
        fn run(&self, argv: &[String]) -> Result<(), String> {
            self.ran.borrow_mut().push(argv.to_vec());
            if let Some((name, dir)) = &self.drops {
                fake_bin(dir, name, "ffmpeg version 7.1");
            }
            Ok(())
        }
        fn fetcher(&self) -> Option<Arc<dyn Fetch>> {
            self.fetch.clone()
        }
    }

    fn mock(has: &[&'static str]) -> Mock {
        Mock {
            has: has.to_vec(),
            ran: RefCell::new(vec![]),
            drops: None,
            fetch: None,
        }
    }

    fn say(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_command_comes_from_the_package_manager_the_person_has() {
        let tools = embedded();
        let ff = find(&tools, "ffmpeg").unwrap();
        let p = plan_add(ff, "windows", &mock(&["winget"])).unwrap();
        assert_eq!(p.how, How::Winget);
        assert_eq!(
            show(&p.argv),
            "winget install --id Gyan.FFmpeg -e --accept-source-agreements --accept-package-agreements"
        );
        let p = plan_add(ff, "macos", &mock(&["brew"])).unwrap();
        assert_eq!(show(&p.argv), "brew install ffmpeg");
        let bl = find(&tools, "blender").unwrap();
        assert_eq!(
            show(&plan_add(bl, "macos", &mock(&["brew"])).unwrap().argv),
            "brew install --cask blender"
        );
        let p = plan_add(ff, "linux", &mock(&["apt-get"])).unwrap();
        assert!(p.admin);
        assert_eq!(show(&p.argv), "sudo apt-get install -y ffmpeg");
        // a person with none of them is told what to install, not given a wrong command
        let e = plan_add(ff, "windows", &mock(&[])).unwrap_err();
        assert!(
            e.contains("winget") && e.contains("https://ffmpeg.org/"),
            "{e}"
        );
        // piper is pipx on every OS
        let pi = find(&tools, "piper").unwrap();
        assert_eq!(
            show(&plan_add(pi, "linux", &mock(&["pipx"])).unwrap().argv),
            "pipx install piper-tts"
        );
        // by hand: the words, not a command
        let c = find(&tools, "comfyui").unwrap();
        assert!(plan_add(c, "linux", &mock(&["apt-get"]))
            .unwrap_err()
            .contains("homepage"));
    }

    #[test]
    fn add_shows_the_command_asks_and_runs_it_only_on_yes() {
        let d = tmp("add");
        let ctx = ctx_in(&d.join("home"));
        let bin = d.join("bin");
        let sys = sys_with("macos", &[&bin]);
        let tools = embedded();
        // no: nothing runs, nothing is recorded
        let pm = mock(&["brew"]);
        let mut out = vec![];
        let mut asked = vec![];
        let code = add(
            &ctx,
            &sys,
            &pm,
            &tools,
            "ffmpeg",
            Opts::default(),
            1,
            &mut out,
            &mut |q| {
                asked.push(q.to_string());
                false
            },
        );
        let text = say(out);
        assert_eq!(code, 0);
        assert!(
            text.contains("brew install ffmpeg") && text.contains("Nothing was installed"),
            "{text}"
        );
        assert_eq!(asked.len(), 1);
        assert!(pm.ran.borrow().is_empty());
        assert!(!crate::bank::state::path(&ctx).exists());
        // yes: it runs the same command, finds the program, then asks before turning it on
        let pm = Mock {
            drops: Some(("ffmpeg", bin.clone())),
            ..mock(&["brew"])
        };
        let mut out = vec![];
        let mut qs = 0;
        let code = add(
            &ctx,
            &sys,
            &pm,
            &tools,
            "ffmpeg",
            Opts::default(),
            2,
            &mut out,
            &mut |_| {
                qs += 1;
                true
            },
        );
        let text = say(out);
        assert_eq!(code, 0, "{text}");
        assert_eq!(qs, 2, "install, then enable");
        assert_eq!(
            pm.ran.borrow().as_slice(),
            [words(&["brew", "install", "ffmpeg"])]
        );
        let st = State::load(&ctx);
        assert!(st.enabled("ffmpeg"));
        assert_eq!(st.how("ffmpeg").as_deref(), Some("brew"));
        assert!(text.contains("ffmpeg_convert"), "{text}");
    }

    #[test]
    fn apt_is_printed_for_the_person_to_run_and_never_run_by_tempo() {
        let d = tmp("apt");
        let ctx = ctx_in(&d.join("home"));
        let sys = sys_with("linux", &[]);
        let pm = mock(&["apt-get"]);
        let mut out = vec![];
        let code = add(
            &ctx,
            &sys,
            &pm,
            &embedded(),
            "pandoc",
            Opts {
                yes: true,
                enable: true,
            },
            1,
            &mut out,
            &mut |_| panic!("no question: it is not run"),
        );
        let text = say(out);
        assert_eq!(code, 0);
        assert!(text.contains("sudo apt-get install -y pandoc"), "{text}");
        assert!(
            text.contains("admin rights") && text.contains("Run it yourself"),
            "{text}"
        );
        assert!(
            pm.ran.borrow().is_empty(),
            "never sudo silently, not even with --yes"
        );
    }

    #[test]
    fn an_installed_tool_is_not_installed_again() {
        let d = tmp("again");
        let ctx = ctx_in(&d.join("home"));
        let bin = d.join("bin");
        fake_bin(&bin, "rg", "ripgrep 14.1.0");
        let sys = sys_with("linux", &[&bin]);
        let pm = mock(&["apt-get"]);
        let mut out = vec![];
        let code = add(
            &ctx,
            &sys,
            &pm,
            &embedded(),
            "ripgrep",
            Opts {
                yes: true,
                enable: true,
            },
            1,
            &mut out,
            &mut |_| true,
        );
        let text = say(out);
        assert_eq!(code, 0);
        assert!(text.contains("already here"), "{text}");
        // the stand-in prints its version only where a shell script can run
        if cfg!(unix) {
            assert!(text.contains("already here (14.1.0)"), "{text}");
        }
        assert!(pm.ran.borrow().is_empty());
        // ripgrep gives Tempo a tool, and --enable turned it on
        assert!(State::load(&ctx).enabled("ripgrep"));
    }

    #[test]
    fn remove_shows_the_uninstall_command_and_asks() {
        let d = tmp("remove");
        let ctx = ctx_in(&d.join("home"));
        let bin = d.join("bin");
        fake_bin(&bin, "ffmpeg", "ffmpeg version 7.1");
        let sys = sys_with("windows", &[]);
        let _ = sys;
        let sys = sys_with("macos", &[&bin]);
        let tools = embedded();
        let pm = mock(&["brew"]);
        let mut out = vec![];
        add(
            &ctx,
            &sys,
            &pm,
            &tools,
            "ffmpeg",
            Opts {
                yes: true,
                enable: true,
            },
            1,
            &mut out,
            &mut |_| true,
        );
        // no
        let mut out = vec![];
        remove(
            &ctx,
            &sys,
            &pm,
            &tools,
            "ffmpeg",
            Opts::default(),
            2,
            &mut out,
            &mut |_| false,
        );
        let text = say(out);
        assert!(
            text.contains("brew uninstall ffmpeg") && text.contains("Nothing was removed"),
            "{text}"
        );
        assert!(pm.ran.borrow().is_empty());
        assert!(State::load(&ctx).enabled("ffmpeg"));
        // yes
        let mut out = vec![];
        let code = remove(
            &ctx,
            &sys,
            &pm,
            &tools,
            "ffmpeg",
            Opts::default(),
            3,
            &mut out,
            &mut |_| true,
        );
        assert_eq!(code, 0);
        assert_eq!(
            pm.ran.borrow().as_slice(),
            [words(&["brew", "uninstall", "ffmpeg"])]
        );
        let st = State::load(&ctx);
        assert!(!st.enabled("ffmpeg"), "off once it is gone");
        // a tool that is not there has nothing to remove, and asks nothing
        let mut out = vec![];
        let sys2 = sys_with("macos", &[]);
        remove(
            &ctx,
            &sys2,
            &pm,
            &tools,
            "sox",
            Opts::default(),
            4,
            &mut out,
            &mut |_| panic!("no question"),
        );
        assert!(say(out).contains("nothing to remove"));
    }

    /// A fetcher that "downloads" fixed bytes.
    #[cfg(unix)]
    struct Bytes(Vec<u8>);
    #[cfg(unix)]
    impl Fetch for Bytes {
        fn get(&self, _: &str, _: Option<&str>, _: bool) -> Fetched {
            Fetched::Failed
        }
        fn download(
            &self,
            _: &str,
            part: &Path,
            _: u64,
            _: &mut dyn FnMut(u64),
        ) -> Result<(), String> {
            std::fs::write(part, &self.0).map_err(|e| e.to_string())
        }
    }

    #[cfg(unix)]
    fn pinned_catalog(sha: &str) -> Vec<Tool> {
        load(&format!(
            r#"{{"tools":[{{"id":"zed","name":"Zed","for":"does z","category":"c","licence":"MIT","homepage":"https://z.example/",
            "install":{{"linux":{{"download":{{"url":"https://z.example/zed","file":"zed","sha256":"{sha}","bytes":9,"archive":false}}}}}},
            "detect":{{"bins":["zed"]}},"mb":1,"tempo":"nothing","verified":true}}]}}"#
        ))
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_pinned_download_is_checked_before_anything_runs() {
        let body = b"#!/bin/sh\necho 'zed 1.0'\n".to_vec();
        let good = sha256_hex(&body);
        // matching: unpacked into Tempo's own folder, then found there
        let d = tmp("pin");
        let ctx = ctx_in(&d.join("home"));
        let sys = sys_with("linux", &[]);
        let pm = Mock {
            fetch: Some(Arc::new(Bytes(body.clone()))),
            ..mock(&[])
        };
        let mut out = vec![];
        let code = add(
            &ctx,
            &sys,
            &pm,
            &pinned_catalog(&good),
            "zed",
            Opts {
                yes: true,
                enable: false,
            },
            1,
            &mut out,
            &mut |_| false,
        );
        let text = say(out);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("SHA-256 matches the pin") && text.contains("Zed is installed"),
            "{text}"
        );
        assert!(crate::bank::detect::own_dir(&ctx, "zed")
            .join("zed")
            .is_file());
        // remove: Tempo's own copy goes to the trash, not away
        let mut out = vec![];
        remove(
            &ctx,
            &sys,
            &pm,
            &pinned_catalog(&good),
            "zed",
            Opts {
                yes: true,
                enable: false,
            },
            1,
            &mut out,
            &mut |_| false,
        );
        assert!(!crate::bank::detect::own_dir(&ctx, "zed").exists());
        let trashed = std::fs::read_dir(ctx.home.join("trash"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        assert!(std::fs::read_dir(trashed)
            .unwrap()
            .flatten()
            .any(|e| e.file_name() == "zed"));
        // not matching: refused, trashed, and nothing is placed
        let d = tmp("pin-bad");
        let ctx = ctx_in(&d.join("home"));
        let pm = Mock {
            fetch: Some(Arc::new(Bytes(b"something else".to_vec()))),
            ..mock(&[])
        };
        let mut out = vec![];
        let code = add(
            &ctx,
            &sys,
            &pm,
            &pinned_catalog(&good),
            "zed",
            Opts {
                yes: true,
                enable: false,
            },
            1,
            &mut out,
            &mut |_| false,
        );
        let text = say(out);
        assert_eq!(code, 1);
        assert!(
            text.contains("did not match its pinned SHA-256")
                && text.contains("nothing was unpacked or run"),
            "{text}"
        );
        assert!(!crate::bank::detect::own_dir(&ctx, "zed").exists());
        assert!(pm.ran.borrow().is_empty());
        assert!(ctx.home.join("trash").is_dir());
    }
}
