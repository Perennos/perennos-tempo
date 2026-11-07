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
use crate::{brand, html_escape, write_atomic, Ctx};
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
    render_with(bands, engines, apps, "")
}

/// `render`, with the fleet section (`fleet::page::section`) when there is one.
pub fn render_with(
    bands: &[BandView],
    engines: &[Engine],
    apps: &[AppState],
    fleet: &str,
) -> String {
    let fleet = if fleet.is_empty() {
        String::new()
    } else {
        format!("<h2>Fleet</h2>\n{fleet}\n")
    };
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
        r#"{top}<h1>{heading}</h1>
<p class="lede">The Conductor raises the baton. Here is every engine it can cue, every band it leads, what they played last, and which apps are awake.</p>

{fleet}<h2>Engines</h2>
<p class="small">{up} of {total} up. Heavy parts go to the strongest engine that has their model, light ones to the quickest.</p>
<div class="cards">{engine_cards}</div>

<h2>Bands</h2>
{bands_html}

<h2>Last runs</h2>
{runs_html}

<h2>Apps</h2>
<div class="cards">{apps_html}</div>

<p class="small">Nothing here leaves this computer. Members only talk and write drafts into your Vault: they never post, send, buy or delete anything. This page is static and can be rewritten any time with <code>tempo-launch conductor page</code>.</p>
{end}"#,
        top = brand::page_top(&title, true, true),
        end = brand::PAGE_END,
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
    let html = render_with(
        &views,
        engines,
        apps,
        &crate::fleet::page::section(&crate::fleet::store::Paths::of(ctx), true),
    );
    write_atomic(&file, html.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", file.display()))?;
    Ok(file)
}
