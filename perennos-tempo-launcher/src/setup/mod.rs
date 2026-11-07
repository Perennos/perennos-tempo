//! `tempo-launch setup`: Tempo configures itself for this computer and the user's goals. It looks
//! at the computer (`probe`), chooses models from the catalog (`plan`), and only with consent
//! fetches what is missing and writes the agent's settings (`apply`). See docs/TEMPO-SETUP.md.

pub mod apply;
pub mod catalog;
pub mod json;
pub mod page;
pub mod plan;
pub mod probe;
pub mod registry;
pub mod runtime;
pub mod sha256;

use crate::{write_atomic, Ctx};
use probe::Host;
use std::path::PathBuf;

/// The real command: this computer, the terminal for the question, a hidden `ollama serve`
/// (Tempo's own copy is started the way `tempo-launch start` starts it: with its own record).
pub fn run_setup(ctx: &Ctx, o: &apply::Opts) -> i32 {
    let port = apply::ollama_port();
    apply::run(
        ctx,
        &probe::RealHost,
        o,
        port,
        crate::unix_now(),
        &mut std::io::stdout(),
        &mut apply::ask_terminal,
        &mut |bin| {
            if bin.starts_with(ctx.home.join("runtime")) {
                return matches!(crate::ensure_owned_ollama(ctx, port, true), Ok(Some(_)));
            }
            let mut cmd = std::process::Command::new(bin);
            cmd.arg("serve")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            crate::spawn_hidden(&mut cmd).is_ok()
        },
    )
}

/// Write `<home>/setup.html`: the plan in plain words and the exact command to apply it.
pub fn write_page(ctx: &Ctx, plan: &plan::Plan, o: &apply::Opts) -> Option<PathBuf> {
    let page = ctx.home.join("setup.html");
    let html = page::setup_page(plan, &apply::apply_command(o));
    write_atomic(&page, html.as_bytes()).ok()?;
    Some(page)
}

/// First start: when the agent has no settings yet, make sure the plan page exists (the installer
/// may have written it with `setup --page`) and open it once. Nothing is fetched. A marker file
/// remembers that it was opened, so later starts do nothing.
pub fn first_start(ctx: &Ctx, host: &dyn Host, port: u16) -> Option<PathBuf> {
    let page = ctx.home.join("setup.html");
    let marker = ctx.home.join("setup.page-opened");
    let has_config = ctx.agent_config_file().is_some()
        || ctx.agent_data.join("agent-config.json").is_file()
        || std::env::var_os("TEMPO_AGENT_CONFIG").is_some();
    if has_config || marker.exists() {
        return None;
    }
    if !page.exists() {
        let c = apply::compute(
            ctx,
            host,
            &apply::Opts::default(),
            false,
            port,
            crate::unix_now(),
        )
        .ok()?;
        write_page(ctx, &c.plan, &apply::Opts::default())?;
    }
    write_atomic(&marker, b"opened once\n").ok()?;
    let _ = ctx.open(&page.to_string_lossy());
    Some(page)
}
