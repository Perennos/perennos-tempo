//! `<home>/welcome.html`: the first-run page. It shows what the scan found, in plain words, asks
//! whether this is one computer or several (the page is static, so each answer shows the exact
//! command to run), and gives the entries that let the person's own AI help with the setup. It
//! loads nothing from anywhere and has no script.

use super::json::J;
use super::plan::{Plan, Run};
use super::probe::Machine;
use super::propose::{APPLY_COMMAND, OLLAMA_DEFAULT_PORT};
use crate::conductor::templates;
use crate::{brand, html_escape};
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

/// Step 6: the fleet, headed by the real number of machines.
pub fn fleet_block(heading: &str) -> String {
    let pre = |s: &str| format!("<pre>{}</pre>", html_escape(s));
    format!(
        "<h2>6. Your own computers</h2>\n<h3 class=\"bandname\">{}</h3>\n<p>One strong computer needs nothing here: it plays on its own, and the plan above says so when it is true. If you have more than one, they can play together. Is this your main computer, or is it joining another?</p>\n<div class=\"choices\">\n<div class=\"card\"><h3>This is my main computer</h3><p>Add a computer in a minute. This shows a short code, good for 10 minutes and once only, and only on your own network.</p>{}<p class=\"small\">Then run <code>tempo-launch fleet join</code> on the new one and type the code.</p></div>\n<div class=\"card\"><h3>This one is joining another</h3><p>Run this here. It finds your main computer on the network, asks if it is the right one, and asks for the code. Then it sets itself up: models, tools and all.</p>{}</div>\n</div>\n<p class=\"step\">Already know the address? Look around first (it explains itself and asks y/N), or add one yourself, over your own ssh if you like:</p>\n{}<p class=\"small\">Nothing leaves your own network, and no computer is added without you.</p>",
        html_escape(heading),
        pre("tempo-launch fleet invite"),
        pre("tempo-launch fleet join"),
        pre("tempo-launch fleet scan\ntempo-launch fleet add <address or name>\ntempo-launch fleet page"),
    )
}

pub fn welcome_page(m: &Machine, p: &Plan, exe: &Path) -> String {
    welcome_page_with(m, p, exe, &crate::fleet::heading(1))
}

/// `welcome_page`, with the fleet heading for the number of machines this person really has.
pub fn welcome_page_with(m: &Machine, p: &Plan, exe: &Path, fleet_heading: &str) -> String {
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
    let bands: String = templates::all()
        .iter()
        .map(|t| {
            format!(
                "<li><strong>{}</strong>: {}</li>",
                html_escape(t.id),
                html_escape(t.summary)
            )
        })
        .collect();
    format!(
        r#"{top}<p class="tt-kicker">01 — First run</p>
<h1>What's your <em>tempo</em>?</h1>
<p class="lede">Welcome to Perennos Tempo. It has just looked at this computer. Below: what it found, how to say whether this is your only computer or one of several, and how to let your own AI (Claude, or another) help you finish the setup. Nothing has been fetched, changed or sent anywhere.</p>

<h2>What this computer can run</h2>
<p class="verdict">{verdict}</p>
<ul>{fits}</ul>
{note}
<p><strong>What Tempo found</strong></p>
<ul>{scan}</ul>

<h2>Is this your only computer?</h2>
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

{ollama_tip}
<h2>Let your AI help you set up</h2>
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

<h2>What connecting an AI shares</h2>
<div class="share">
<p><strong>Only the scan facts and the plan.</strong> The helper answers with this computer's system, CPU and graphics card class, memory, free disk, whether Ollama is there, the models Tempo recommends, and which Tempo apps are running. It never sends files, notes or chats, and its answers hold no user name, folder name, computer name or address.</p>
<p>It is opt-in: nothing connects unless you add the entry to your AI yourself, and Tempo never calls out to any AI on its own. Once you do, what the helper answers becomes part of your conversation with that AI, so it reaches that AI's provider like anything else you type there. To stop, remove the entry from the AI's settings.</p>
</div>

<h2>Let the Conductor set up a band for your work</h2>
<p>The <em>Tempo Conductor</em> ties you to your workflows and to your engines. A <em>band</em> is the team it sets up for your own workflow, all on this computer: for a social media account, say, a researcher, a writer, an editor and a scheduler. Each one shows up in Room as a teammate you can talk to like a person (<code>@writer</code>), and when you raise the baton a flow walks them one after another and files what they make as linked notes in your Vault. The Conductor also knows every engine you have (this computer's Ollama and each one you added with <code>tempo-launch link</code>) and cues each member on the one that suits the part: the writers on your strongest engine, the sorters on a quick one. They only talk and write drafts: they never post, send, buy or delete anything.</p>
<p>See your engines first:</p>
{band_engines}
<p>Pick a template:</p>
<ul>{bands}</ul>
{band_template}
<p>Or say what you do, in your own words. The model on this computer fits the closest template to it; Tempo shows you the band in plain words and asks y/N before it sets anything up:</p>
{band_describe}
<p>Then run a flow, and see the engines, bands, last runs and apps on one page:</p>
{band_run}
<p>Your own AI can design a band with you too: the setup helper from step 3 has <code>conductor_templates</code> (read-only) and <code>conductor_propose</code>, and you set a proposal up yourself with <code>tempo-launch conductor apply-proposal</code>.</p>

{fleet}

{crate_section}
<p class="small">The longer plan, with sizes and licences, is in <code>setup.html</code> in the Tempo folder (<code>tempo-launch setup</code> shows it in the terminal). This page can be opened again with <code>tempo-launch welcome</code>. Perennos's own UMBRA models are never downloaded: Tempo only uses them where they are already on this computer.</p>
{end}"#,
        top = brand::page_top("Welcome to Perennos Tempo", true, true),
        end = brand::PAGE_END,
        crate_section = crate::bank::welcome_section(
            &m.os,
            m.ram_total_gb,
            !matches!(m.gpu, super::probe::Gpu::None)
        ),
        verdict = html_escape(&verdict(p)),
        fits = fits,
        note = note,
        scan = scan,
        ollama_tip = super::apply::ollama_tip(&m.ollama)
            .map_or(String::new(), |t| format!("<p class=\"small\">{}</p>", html_escape(&t))),
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
        bands = bands,
        band_template = pre("tempo-launch conductor new --template social-media"),
        band_describe = pre(
            "tempo-launch conductor new --describe \"I run a bakery's Instagram and want posts planned weekly\""
        ),
        band_run = pre("tempo-launch conductor run social-media weekly-plan\ntempo-launch conductor page"),
        band_engines = pre("tempo-launch conductor engines"),
        fleet = fleet_block(fleet_heading),
    )
}
