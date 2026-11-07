//! `<home>/welcome.html`: the first-run page. It shows what the scan found, in plain words, asks
//! whether this is one computer or several (the page is static, so each answer shows the exact
//! command to run), and gives the entries that let the person's own AI help with the setup. It
//! loads nothing from anywhere and has no script.

use super::json::J;
use super::plan::{Plan, Run};
use super::probe::Machine;
use super::propose::{APPLY_COMMAND, OLLAMA_DEFAULT_PORT};
use crate::{html_escape, MARK_SVG};
use std::path::Path;

/// The name the AI's tool entry is saved under.
pub const ENTRY_NAME: &str = "tempo-setup";

/// What this computer can run, in a sentence.
pub fn verdict(p: &Plan) -> String {
    let on = |r: Run| {
        if r == Run::Gpu {
            "graphics card"
        } else {
            "CPU"
        }
    };
    match (p.pick("heavy"), p.pick("fast")) {
        (Some(h), _) => format!(
            "This computer can run a thinking model: {} ({}), on the {}. It also gets a small quick model for short answers.",
            h.model,
            h.gb.map_or("size unknown".into(), |g| format!("{g:.1} GB")),
            on(h.run)
        ),
        (None, Some(f)) => format!(
            "This computer can run a small, quick model: {} ({}), on the {}. A bigger thinking model needs more memory than it has free; a second computer with more memory can supply one (see below).",
            f.model,
            f.gb.map_or("size unknown".into(), |g| format!("{g:.1} GB")),
            on(f.run)
        ),
        _ => "Tempo could not find a model that fits this computer yet. The scan below says why; a second computer with more memory can run the models for it (see below).".into(),
    }
}

fn command_path(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// The entry for `claude_desktop_config.json` and for other clients that read the same shape.
pub fn desktop_entry(exe: &Path) -> String {
    J::Obj(vec![(
        "mcpServers".into(),
        J::Obj(vec![(
            ENTRY_NAME.into(),
            J::Obj(vec![
                ("command".into(), J::s(&exe.to_string_lossy())),
                ("args".into(), J::strings(&["mcp".to_string()])),
            ]),
        )]),
    )])
    .pretty()
}

/// `claude mcp add ...` for Claude Code.
pub fn code_command(exe: &Path) -> String {
    format!("claude mcp add {ENTRY_NAME} -- {} mcp", command_path(exe))
}

pub fn welcome_page(m: &Machine, p: &Plan, exe: &Path) -> String {
    let li = |items: Vec<String>| -> String {
        items
            .into_iter()
            .map(|t| format!("<li>{}</li>", html_escape(&t)))
            .collect()
    };
    let scan = li(super::plan::summary(m));
    let fits = li(p.picks.iter().map(|k| k.why.clone()).collect());
    let note = p.refused.as_ref().map_or(String::new(), |r| {
        format!("<p><strong>{}</strong></p>", html_escape(r))
    });
    let pre = |s: &str| format!("<pre>{}</pre>", html_escape(s));
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'">
<title>Welcome to Perennos Tempo</title>
<style>
:root{{color-scheme:dark}}
*{{box-sizing:border-box}}
body{{margin:0;background:#16181d;color:#ece6d8;font:15px/1.6 "Segoe UI",system-ui,-apple-system,sans-serif}}
main{{width:min(960px,100%);margin:0 auto;padding:48px 24px 64px}}
.mark{{width:44px;height:44px;margin-bottom:28px}}
h1{{font:500 30px/1.25 "Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;margin:0 0 10px}}
h2{{font:500 21px/1.3 "Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;margin:44px 0 8px}}
h3{{font:600 15px/1.4 system-ui,sans-serif;margin:0 0 6px}}
p,li{{color:#cfc9bd}}
.lede{{font-size:16.5px;max-width:62ch}}
ul{{padding-left:20px}}
code{{font:13px ui-monospace,"Cascadia Mono",Consolas,monospace;color:#e6d9bd}}
pre{{background:#0e1013;border:1px solid #2a2c31;border-radius:10px;padding:12px 14px;margin:8px 0 14px;overflow:auto;font:13px/1.5 ui-monospace,"Cascadia Mono",Consolas,monospace;color:#d9dde2;user-select:all;white-space:pre}}
.verdict{{background:#1d2027;border:1px solid #2a2c31;border-left:3px solid #E8552F;border-radius:10px;padding:14px 16px;color:#ece6d8}}
.choices{{display:grid;grid-template-columns:1fr 1fr;gap:16px;margin-top:14px;align-items:start}}
.card{{background:#1b1e24;border:1px solid #2a2c31;border-radius:14px;padding:18px 18px 6px;min-width:0}}
.card p{{margin:0 0 8px}}
.step{{font-size:13px;color:#a9a597;margin:10px 0 0}}
.small{{font-size:12.5px;color:#8a8b8f}}
.share{{border:1px solid #2a2c31;border-radius:12px;padding:6px 18px 8px;background:#1b1e24}}
@media (max-width:720px){{main{{padding:32px 16px 48px}}.choices{{grid-template-columns:1fr}}h1{{font-size:25px}}}}
</style></head>
<body><main>
{mark}
<h1>Welcome to Perennos Tempo</h1>
<p class="lede">Tempo has just looked at this computer. Below: what it found, how to say whether this is your only computer or one of several, and how to let your own AI (Claude, or another) help you finish the setup. Nothing has been fetched, changed or sent anywhere.</p>

<h2>1. What this computer can run</h2>
<p class="verdict">{verdict}</p>
<ul>{fits}</ul>
{note}
<p><strong>What Tempo found</strong></p>
<ul>{scan}</ul>

<h2>2. Is this your only computer?</h2>
<p>This page cannot take a click, so each answer shows the exact command. Run the one that fits, in a terminal.</p>
<div class="choices">
<div class="card">
<h3>Just this computer</h3>
<p>Tempo fetches the models above and sets itself up here. It asks y/N before it downloads anything.</p>
{single}
<p class="step">Run it again whenever you like: it only changes what is missing.</p>
</div>
<div class="card">
<h3>Several computers (a home server, a laptop, a GPU box...)</h3>
<p>Each computer runs its own Tempo. A computer with a big graphics card can serve the models to the others. Only addresses on your own network or tailnet are accepted, never the public internet.</p>
<p class="step">On each computer, once:</p>
{single}
<p class="step">On the computer that will serve models (the engine), use Ollama from ollama.com (Tempo's own copy answers only its own computer) and let it listen on the engine's tailnet or home-network address, then restart Ollama:</p>
{engine}
<p class="step">On the computer you sit at (the main one), add the engine. Tempo checks the address is private and contacts nothing:</p>
{link}
<p class="step">Tempo uses the main computer's own Ollama first and moves to the next address when it cannot be reached. To make the engine the first choice instead, ask your AI below to propose it.</p>
</div>
</div>

<h2>3. Let your AI help you set up</h2>
<p>Add Tempo's setup helper to the AI you already use. It can read the scan and the plan above, talk you through the choice in step 2, and write a <em>proposal</em> for your settings. It cannot change anything: you apply a proposal yourself with <code>{apply_cmd}</code>, which shows what would change and asks y/N.</p>
<h3>Claude Desktop</h3>
<p>Open <code>claude_desktop_config.json</code> (Windows: <code>%APPDATA%\Claude\</code>; Mac: <code>~/Library/Application Support/Claude/</code>), add this inside the file (keep any servers already there), and restart Claude Desktop:</p>
{desktop}
<h3>Claude Code</h3>
<p>Run this once in a terminal:</p>
{code}
<h3>Other MCP clients (Cursor, VS Code, and others that start a local program)</h3>
<p>Add a server with the transport <code>stdio</code>, the command below and one argument, <code>mcp</code>. Clients that read the same JSON shape as Claude Desktop can use the entry above as it is.</p>
{command}
<h3>ChatGPT</h3>
<p>ChatGPT's connectors need an MCP server reachable over the internet, so it cannot start a program on your computer. Instead, run this, read what it prints, and paste it into your chat yourself:</p>
{scan_cmd}

<h2>4. What connecting an AI shares</h2>
<div class="share">
<p><strong>Only the scan facts and the plan.</strong> The helper answers with this computer's system, CPU and graphics card class, memory, free disk, whether Ollama is there, the models Tempo recommends, and which Tempo apps are running. It never sends files, notes or chats, and its answers hold no user name, folder name, computer name or address.</p>
<p>It is opt-in: nothing connects unless you add the entry to your AI yourself, and Tempo never calls out to any AI on its own. Once you do, what the helper answers becomes part of your conversation with that AI, so it reaches that AI's provider like anything else you type there. To stop, remove the entry from the AI's settings.</p>
</div>

<p class="small">The longer plan, with sizes and licences, is in <code>setup.html</code> in the Tempo folder (<code>tempo-launch setup</code> shows it in the terminal). This page can be opened again with <code>tempo-launch welcome</code>. Perennos's own UMBRA models are never downloaded: Tempo only uses them where they are already on this computer.</p>
</main></body></html>
"#,
        mark = MARK_SVG,
        verdict = html_escape(&verdict(p)),
        fits = fits,
        note = note,
        scan = scan,
        single = pre("tempo-launch setup --apply"),
        engine = pre(&format!(
            "Windows:       setx OLLAMA_HOST \"<engine address>:{OLLAMA_DEFAULT_PORT}\"\nMac or Linux:  OLLAMA_HOST=<engine address>:{OLLAMA_DEFAULT_PORT} ollama serve"
        )),
        link = pre(&format!(
            "tempo-launch link http://<engine address>:{OLLAMA_DEFAULT_PORT}"
        )),
        apply_cmd = html_escape(APPLY_COMMAND),
        desktop = pre(desktop_entry(exe).trim_end()),
        code = pre(&code_command(exe)),
        command = pre(&format!("{} mcp", command_path(exe))),
        scan_cmd = pre("tempo-launch mcp --scan"),
    )
}
