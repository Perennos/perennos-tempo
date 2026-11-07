//! The Conductor page: one place to see everything. Static, like the welcome page: a strict
//! Content-Security-Policy, no script, nothing fetched. It shows the engines (up or down, and their
//! models), the bands (members with their instrument, role and engine, the flows and when they
//! run), the last runs with links to their Vault notes, and which apps are running.
//!
//! `conductor page` writes `<Tempo home>/conductor.html` with every band;
//! `conductor page BAND` writes `<Tempo home>/bands/<band>/band.html` with that band.

use super::engines::{size_words, Engine};
use super::run::{recent_runs, RunInfo};
use super::spec::{what_it_does, Band, May, Member};
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

/// Whether an app answers, for the page.
#[derive(Clone, Debug, PartialEq)]
pub struct AppState {
    pub title: String,
    /// `running`, `stopped` or `not installed` (Studio: `ready` when installed).
    pub state: String,
    pub detail: String,
    pub up: bool,
}

/// The apps as `tempo-launch status` sees them: installed, and answering on their port. Studio has
/// no server (the agent uses it as a tool), so it is only ever installed or not.
pub fn real_apps(ctx: &Ctx) -> Vec<AppState> {
    let mut apps: Vec<AppState> = std::thread::scope(|s| {
        let hs: Vec<_> = crate::APPS
            .iter()
            .filter(|a| a.id != "field")
            .map(|a| {
                let installed = ctx.find(a.bin).is_some();
                s.spawn(move || (a, installed, crate::answers(a, a.port)))
            })
            .collect();
        hs.into_iter()
            .filter_map(|h| h.join().ok())
            .map(|(a, installed, body)| {
                let detail = match (a.id, &body) {
                    ("vault", Some(b)) => {
                        format!("{} notes", crate::json_num(b, "notes").unwrap_or(0))
                    }
                    ("room", Some(b)) => {
                        format!("{} rooms", crate::json_num(b, "rooms").unwrap_or(0))
                    }
                    (_, Some(_)) => format!("127.0.0.1:{}", a.port),
                    _ => String::new(),
                };
                AppState {
                    title: a.title.to_string(),
                    state: match (body.is_some(), installed) {
                        (true, _) => "running",
                        (false, true) => "stopped",
                        (false, false) => "not installed",
                    }
                    .into(),
                    detail,
                    up: body.is_some(),
                }
            })
            .collect()
    });
    let studio = ctx.find("tempo-studio").is_some();
    apps.push(AppState {
        title: "Tempo Studio".into(),
        state: if studio { "ready" } else { "not installed" }.into(),
        detail: if studio {
            "works through the agent".into()
        } else {
            String::new()
        },
        up: studio,
    });
    apps
}

/// A band and its latest runs.
pub struct BandView<'a> {
    pub band: &'a Band,
    pub runs: Vec<RunInfo>,
}

fn engine_card(e: &Engine) -> String {
    let (state, line) = if e.up {
        (
            "up",
            format!(
                "{} &middot; answered in {} ms",
                html_escape(e.class().word()),
                e.ms
            ),
        )
    } else {
        ("down", html_escape(&e.why.replace("://", " ")))
    };
    let models = if !e.up {
        String::new()
    } else if e.models.is_empty() {
        "<p class=\"small\">No models yet.</p>".to_string()
    } else {
        format!(
            "<ul class=\"models\">{}</ul>",
            e.models
                .iter()
                .map(|m| format!(
                    "<li>{} <span class=\"small\">{}</span></li>",
                    html_escape(&m.name),
                    html_escape(&size_words(m.billions))
                ))
                .collect::<String>()
        )
    };
    format!(
        "<div class=\"card {state}\"><p class=\"inst {state}\">{state}</p><h3>{}</h3><p class=\"handle\">{}</p><p class=\"small\">{line}</p>{models}</div>",
        html_escape(&e.id),
        html_escape(e.at()),
    )
}

fn plays_on(m: &Member, engines: &[Engine]) -> String {
    if m.engine.is_empty() {
        return "the agent's own order".into();
    }
    match engines.iter().find(|e| e.id == m.engine) {
        Some(e) if e.up => format!("engine {}", m.engine),
        Some(_) => format!("engine {} (down now)", m.engine),
        None => format!("engine {} (not one of yours now)", m.engine),
    }
}

fn card(m: &Member, engines: &[Engine]) -> String {
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
        "<div class=\"card\"><p class=\"inst\">{}</p><h3>{}</h3><p class=\"handle\">@{}</p><p>{}</p><p class=\"small\">{}</p><p class=\"small\">Plays on {} &middot; {}</p></div>",
        html_escape(&m.instrument),
        html_escape(&m.name),
        html_escape(&m.handle),
        html_escape(&what_it_does(&m.role)),
        html_escape(may),
        html_escape(&plays_on(m, engines)),
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
        "<div class=\"flow\"><h3>{}</h3><p class=\"steps\">{}</p><p>{}</p><p class=\"small\">Runs {} &middot; notes go to <code>{}</code> in your Vault</p><pre>tempo-launch conductor run {} {}</pre></div>",
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

fn app_card(a: &AppState) -> String {
    let state = if a.up { "up" } else { "down" };
    let detail = if a.detail.is_empty() {
        String::new()
    } else {
        format!("<p class=\"small\">{}</p>", html_escape(&a.detail))
    };
    format!(
        "<div class=\"card {state}\"><p class=\"inst {state}\">{}</p><h3>{}</h3>{detail}</div>",
        html_escape(&a.state),
        html_escape(&a.title),
    )
}

fn band_block(v: &BandView, engines: &[Engine]) -> String {
    let b = v.band;
    let cards: String = b.members.iter().map(|m| card(m, engines)).collect();
    let flows: String = b.flows.iter().map(|f| flow_row(b, f)).collect();
    let handles: Vec<String> = b
        .members
        .iter()
        .map(|m| format!("<code>@{}</code>", html_escape(&m.handle)))
        .collect();
    let first = b.members.first().map_or("", |m| m.handle.as_str());
    format!(
        r#"<section class="band">
<h3 class="bandname">{name}</h3>
<p class="lede">{about}</p>
<div class="cards">{cards}</div>
<h3 class="sub">Flows</h3>
{flows}
<div class="room">
<p>Talk to them in Room: each member is a teammate in the room named <code>{name}</code>. Mention one like a colleague: {handles}.</p>
<p>For example: <code>@{first} what would you do first this week?</code></p>
<p class="small">If they are not in Room yet, run <code>tempo-launch conductor room {name}</code> while Room is running.</p>
</div>
</section>"#,
        name = html_escape(&b.name),
        about = html_escape(&b.about),
        handles = handles.join(" "),
        first = html_escape(first),
    )
}

/// The page for `bands` (one, or all), with the engines and apps as they are now.
pub fn render(bands: &[BandView], engines: &[Engine], apps: &[AppState]) -> String {
    let title = match bands {
        [one] => format!("{} - Tempo Conductor", html_escape(&one.band.name)),
        _ => "Tempo Conductor".to_string(),
    };
    let engine_cards: String = engines.iter().map(engine_card).collect();
    let up = engines.iter().filter(|e| e.up).count();
    let bands_html = if bands.is_empty() {
        "<p>No bands yet. Ask the Conductor to set one up: <code>tempo-launch conductor new --template social-media</code></p>".to_string()
    } else {
        bands.iter().map(|v| band_block(v, engines)).collect()
    };
    let runs_html = if bands.iter().all(|v| v.runs.is_empty()) {
        "<p>No runs yet. The Conductor is waiting with the baton up: start one with the command under a flow above.</p>".to_string()
    } else {
        bands
            .iter()
            .filter(|v| !v.runs.is_empty())
            .map(|v| {
                format!(
                    "<h3 class=\"sub\">{}</h3><ul class=\"runs\">{}</ul>",
                    html_escape(&v.band.name),
                    v.runs.iter().map(run_row).collect::<String>()
                )
            })
            .collect()
    };
    let apps_html: String = apps.iter().map(app_card).collect();
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'">
<title>{title}</title>
<style>
:root{{color-scheme:dark}}
*{{box-sizing:border-box}}
body{{margin:0;background:#16181d;color:#ece6d8;font:15px/1.6 "Segoe UI",system-ui,-apple-system,sans-serif}}
main{{width:min(1040px,100%);margin:0 auto;padding:48px 24px 64px}}
.mark{{width:44px;height:44px;margin-bottom:28px}}
h1{{font:500 30px/1.25 "Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;margin:0 0 10px;overflow-wrap:anywhere}}
h2{{font:500 21px/1.3 "Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;margin:44px 0 12px}}
h3{{font:600 16px/1.4 system-ui,sans-serif;margin:0 0 4px;overflow-wrap:anywhere}}
h3.bandname{{font:500 20px/1.3 "Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;margin:22px 0 4px}}
h3.sub{{margin:20px 0 8px;color:#e6d9bd}}
p,li{{color:#cfc9bd}}
.lede{{font-size:16.5px;max-width:62ch;margin:6px 0 12px}}
code{{font:13px ui-monospace,"Cascadia Mono",Consolas,monospace;color:#e6d9bd;overflow-wrap:anywhere}}
pre{{background:#0e1013;border:1px solid #2a2c31;border-radius:10px;padding:10px 14px;margin:8px 0 0;overflow:auto;font:13px/1.5 ui-monospace,"Cascadia Mono",Consolas,monospace;color:#d9dde2;user-select:all;white-space:pre}}
.cards{{display:grid;grid-template-columns:repeat(auto-fill,minmax(min(230px,100%),1fr));gap:16px}}
.card,.flow{{background:#1b1e24;border:1px solid #2a2c31;border-radius:14px;padding:16px 18px;min-width:0}}
.card p{{margin:6px 0 0}}
.card.down{{border-color:#5a3328}}
.inst{{font:600 12px/1.2 system-ui,sans-serif;letter-spacing:.08em;text-transform:uppercase;color:#E8552F;margin:0 0 6px!important}}
.inst.up{{color:#8fcf9a}}
.inst.down{{color:#f08a6a}}
.handle{{font:13px ui-monospace,"Cascadia Mono",Consolas,monospace;color:#e6d9bd;overflow-wrap:anywhere}}
.models{{margin:8px 0 0;padding-left:18px}}
.models li{{overflow-wrap:anywhere}}
.band{{margin-bottom:30px}}
.flow{{margin-bottom:14px}}
.flow p{{margin:6px 0 0}}
.chip{{display:inline-block;background:#23262d;border:1px solid #2a2c31;border-radius:999px;padding:1px 12px;color:#ece6d8}}
.arrow{{color:#8a8b8f}}
.steps{{overflow-wrap:anywhere}}
.small{{font-size:12.5px;color:#8a8b8f}}
.room{{background:#1d2027;border:1px solid #2a2c31;border-left:3px solid #E8552F;border-radius:10px;padding:14px 16px;color:#ece6d8;margin-top:14px}}
.room p{{margin:0 0 6px;color:#ece6d8}}
.runs{{padding-left:0;list-style:none;margin:0}}
.runs li{{background:#1b1e24;border:1px solid #2a2c31;border-radius:12px;padding:10px 14px;margin-bottom:10px;overflow-wrap:anywhere}}
a{{color:#f0b9a6}}
@media (max-width:720px){{main{{padding:32px 16px 48px}}h1{{font-size:25px}}}}
</style></head>
<body><main>
{mark}
<h1>{heading}</h1>
<p class="lede">The Conductor raises the baton. Here is every engine it can cue, every band it leads, what they played last, and which apps are awake.</p>

<h2>Engines</h2>
<p class="small">{up} of {total} up. Heavy parts go to the strongest engine that has their model, light ones to the quickest.</p>
<div class="cards">{engine_cards}</div>

<h2>Bands</h2>
{bands_html}

<h2>Last runs</h2>
{runs_html}

<h2>Apps</h2>
<div class="cards">{apps_html}</div>

<p class="small">Nothing here leaves this computer. Members only talk and write drafts into your Vault: they never post, send, buy or delete anything. This page is static and can be rewritten any time with <code>tempo-launch conductor page</code>.</p>
</main></body></html>
"#,
        mark = MARK_SVG,
        heading = if bands.len() == 1 {
            html_escape(&bands[0].band.name)
        } else {
            "Tempo Conductor".to_string()
        },
        total = engines.len(),
    )
}

/// Write the page and return its path: `<home>/bands/<band>/band.html` for one band,
/// `<home>/conductor.html` for all of them.
pub fn write(
    ctx: &Ctx,
    one: Option<&str>,
    bands: &[Band],
    engines: &[Engine],
    apps: &[AppState],
) -> Result<PathBuf, String> {
    let file = match one {
        Some(n) => super::band_dir(ctx, n).join("band.html"),
        None => ctx.home.join("conductor.html"),
    };
    let views: Vec<BandView> = bands
        .iter()
        .map(|b| BandView {
            band: b,
            runs: recent_runs(ctx, b),
        })
        .collect();
    let html = render(&views, engines, apps);
    write_atomic(&file, html.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", file.display()))?;
    Ok(file)
}
