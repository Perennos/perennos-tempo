//! `<Tempo home>/bands/<band>/band.html`: the band's dashboard. Static, like the welcome page: a
//! strict Content-Security-Policy, no script, nothing fetched. It shows the members as cards, the
//! flows with their schedule, the latest runs with links to their notes, and how to talk to each
//! member in Room.

use super::run::{recent_runs, RunInfo};
use super::spec::{what_it_does, Band, May};
use crate::{html_escape, write_atomic, Ctx, MARK_SVG};
use std::path::{Path, PathBuf};

/// A `file:` address for a path on this computer, so a link opens the note.
pub fn file_url(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !s.starts_with('/') {
        out.push('/');
    }
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn card(m: &super::spec::Member) -> String {
    let may = match m.may {
        May::Talk => "talks with you",
        May::Draft => "talks and writes drafts",
    };
    let model = if m.model.is_empty() {
        "the agent's usual model".to_string()
    } else {
        m.model.clone()
    };
    format!(
        "<div class=\"card\"><p class=\"inst\">{}</p><h3>{}</h3><p class=\"handle\">@{}</p><p>{}</p><p class=\"small\">{} &middot; {}</p></div>",
        html_escape(&m.instrument),
        html_escape(&m.name),
        html_escape(&m.handle),
        html_escape(&what_it_does(&m.role)),
        html_escape(may),
        html_escape(&model),
    )
}

fn flow_row(b: &Band, f: &super::spec::Flow) -> String {
    let steps: Vec<String> = f
        .steps
        .iter()
        .map(|h| {
            let who = b.member(h).map_or(h.as_str(), |m| m.name.as_str());
            format!("<span class=\"chip\">{}</span>", html_escape(who))
        })
        .collect();
    format!(
        "<div class=\"flow\"><h3>{}</h3><p class=\"steps\">{}</p><p>{}</p><p class=\"small\">Runs {} &middot; notes go to <code>{}</code> in your Vault</p><pre>tempo-launch band run {} {}</pre></div>",
        html_escape(&f.name),
        steps.join(" <span class=\"arrow\">&rarr;</span> "),
        html_escape(&f.goal),
        html_escape(&f.every.words()),
        html_escape(&f.notes_to),
        html_escape(&b.name),
        html_escape(&f.name),
    )
}

fn run_row(r: &RunInfo) -> String {
    let links: Vec<String> = r
        .notes
        .iter()
        .map(|(label, path)| {
            format!(
                "<a href=\"{}\">{}</a>",
                html_escape(&file_url(path)),
                html_escape(label)
            )
        })
        .collect();
    format!(
        "<li><strong>{}</strong><br>{}</li>",
        html_escape(&r.folder),
        links.join(" &middot; ")
    )
}

/// The page for `band`, with its latest `runs`.
pub fn render(band: &Band, runs: &[RunInfo]) -> String {
    let cards: String = band.members.iter().map(card).collect();
    let flows: String = band.flows.iter().map(|f| flow_row(band, f)).collect();
    let runs_html = if runs.is_empty() {
        "<p>No runs yet. Start one with the command under a flow above.</p>".to_string()
    } else {
        format!(
            "<ul class=\"runs\">{}</ul>",
            runs.iter().map(run_row).collect::<String>()
        )
    };
    let handles: Vec<String> = band
        .members
        .iter()
        .map(|m| format!("<code>@{}</code>", html_escape(&m.handle)))
        .collect();
    let first = band.members.first().map_or("", |m| m.handle.as_str());
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'">
<title>{title} - Tempo Band</title>
<style>
:root{{color-scheme:dark}}
*{{box-sizing:border-box}}
body{{margin:0;background:#16181d;color:#ece6d8;font:15px/1.6 "Segoe UI",system-ui,-apple-system,sans-serif}}
main{{width:min(1040px,100%);margin:0 auto;padding:48px 24px 64px}}
.mark{{width:44px;height:44px;margin-bottom:28px}}
h1{{font:500 30px/1.25 "Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;margin:0 0 10px;overflow-wrap:anywhere}}
h2{{font:500 21px/1.3 "Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;margin:44px 0 12px}}
h3{{font:600 16px/1.4 system-ui,sans-serif;margin:0 0 4px}}
p,li{{color:#cfc9bd}}
.lede{{font-size:16.5px;max-width:62ch}}
code{{font:13px ui-monospace,"Cascadia Mono",Consolas,monospace;color:#e6d9bd}}
pre{{background:#0e1013;border:1px solid #2a2c31;border-radius:10px;padding:10px 14px;margin:8px 0 0;overflow:auto;font:13px/1.5 ui-monospace,"Cascadia Mono",Consolas,monospace;color:#d9dde2;user-select:all;white-space:pre}}
.cards{{display:grid;grid-template-columns:repeat(auto-fill,minmax(230px,1fr));gap:16px}}
.card,.flow{{background:#1b1e24;border:1px solid #2a2c31;border-radius:14px;padding:16px 18px;min-width:0}}
.card p{{margin:6px 0 0}}
.inst{{font:600 12px/1.2 system-ui,sans-serif;letter-spacing:.08em;text-transform:uppercase;color:#E8552F;margin:0 0 6px!important}}
.handle{{font:13px ui-monospace,"Cascadia Mono",Consolas,monospace;color:#e6d9bd}}
.flow{{margin-bottom:14px}}
.flow p{{margin:6px 0 0}}
.chip{{display:inline-block;background:#23262d;border:1px solid #2a2c31;border-radius:999px;padding:1px 12px;color:#ece6d8}}
.arrow{{color:#8a8b8f}}
.steps{{overflow-wrap:anywhere}}
.small{{font-size:12.5px;color:#8a8b8f}}
.room{{background:#1d2027;border:1px solid #2a2c31;border-left:3px solid #E8552F;border-radius:10px;padding:14px 16px;color:#ece6d8}}
.room p{{margin:0 0 6px;color:#ece6d8}}
.runs{{padding-left:0;list-style:none;margin:0}}
.runs li{{background:#1b1e24;border:1px solid #2a2c31;border-radius:12px;padding:10px 14px;margin-bottom:10px;overflow-wrap:anywhere}}
a{{color:#f0b9a6}}
@media (max-width:720px){{main{{padding:32px 16px 48px}}h1{{font-size:25px}}}}
</style></head>
<body><main>
{mark}
<h1>{title}</h1>
<p class="lede">{about}</p>

<h2>The band</h2>
<div class="cards">{cards}</div>

<h2>Flows</h2>
{flows}

<h2>Latest runs</h2>
{runs}

<h2>Talk to them in Room</h2>
<div class="room">
<p>Each member is a teammate in the room named <code>{name}</code>. Mention one like a colleague: {handles}.</p>
<p>For example, in the room: <code>@{first} what would you do first this week?</code></p>
<p class="small">If they are not in Room yet, run <code>tempo-launch band room {name}</code> while Room is running.</p>
</div>

<p class="small">Nothing here leaves this computer. Members only talk and write drafts into your Vault: they never post, send, buy or delete anything. This page is static and can be rewritten any time with <code>tempo-launch band page {name}</code>.</p>
</main></body></html>
"#,
        mark = MARK_SVG,
        title = html_escape(&band.name),
        about = html_escape(&band.about),
        cards = cards,
        flows = flows,
        runs = runs_html,
        name = html_escape(&band.name),
        handles = handles.join(" "),
        first = html_escape(first),
    )
}

/// Write `<home>/bands/<band>/band.html` with the latest runs and return its path.
pub fn write(ctx: &Ctx, band: &Band) -> Result<PathBuf, String> {
    let file = super::band_dir(ctx, &band.name).join("band.html");
    let html = render(band, &recent_runs(ctx, band));
    write_atomic(&file, html.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", file.display()))?;
    Ok(file)
}
