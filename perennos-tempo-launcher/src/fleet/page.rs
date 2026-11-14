//! The fleet page: `<home>/fleet.html`, and the same block inside the Conductor page. Static, like
//! every page here: nothing is fetched and there is no script. A switch is shown as the exact
//! command to run; the numbers are the ones each machine's last heartbeat gave.

use super::serve::AWAY_S;
use super::store::{self, now_s, Fleet, Machine, Paths};
use crate::setup::json::J;
use crate::{brand, html_escape, write_atomic, Ctx};
use std::path::PathBuf;

const CSS: &str = "<style>.meter{height:6px;background:var(--line2);border-radius:3px;overflow:hidden;margin:2px 0 8px}.meter i{display:block;height:100%;background:var(--warm)}.fleetline{font:500 11px/1.5 var(--mono);color:var(--silver);margin:0 0 4px}</style>";

fn meter(label: &str, pct: f64) -> String {
    format!(
        "<p class=\"fleetline\">{}</p><div class=\"meter\"><i style=\"width:{:.0}%\"></i></div>",
        html_escape(label),
        pct.clamp(0.0, 100.0)
    )
}

fn health_html(h: &J) -> String {
    let mut s = String::new();
    if let Some(l) = h.f64("load") {
        s.push_str(&meter(&format!("CPU load {:.0}%", l * 100.0), l * 100.0));
    }
    if let (Some(f), Some(t)) = (h.f64("mem_free_gb"), h.f64("mem_total_gb")) {
        if t > 0.0 {
            s.push_str(&meter(
                &format!("memory in use {:.0} of {t:.0} GB", t - f),
                (t - f) / t * 100.0,
            ));
        }
    }
    let models = h.strs("models");
    s.push_str(&format!(
        "<p class=\"small\">{}</p>",
        if models.is_empty() {
            "No model loaded right now.".to_string()
        } else {
            format!("Loaded: {}", html_escape(&models.join(", ")))
        }
    ));
    if let Some(q) = h.f64("queue").filter(|q| *q > 0.0) {
        s.push_str(&format!(
            "<p class=\"small\">{q:.0} message(s) waiting to be sent.</p>"
        ));
    }
    s
}

fn tools_html(t: &J) -> String {
    let mut v = Vec::new();
    if let Some(c) = t.get("comfy").filter(|c| c.bool("up") == Some(true)) {
        v.push(format!(
            "ComfyUI ({} checkpoints)",
            c.arr("checkpoints").len()
        ));
    }
    for (name, tools) in match t.get("mcp") {
        Some(J::Obj(o)) => o.clone(),
        _ => vec![],
    } {
        if let J::Arr(a) = tools {
            v.push(format!("{} ({} tools)", name, a.len()));
        }
    }
    if v.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"small\">Offers: {}</p>",
            html_escape(&v.join(", "))
        )
    }
}

fn card(m: &Machine, now: u64) -> String {
    let up = m.enabled && m.last_seen > 0 && now.saturating_sub(m.last_seen) < AWAY_S;
    let (state, word) = match (m.enabled, up) {
        (false, _) => ("down", "switched off".to_string()),
        (true, true) => ("up", "answering".to_string()),
        (true, false) if m.last_seen == 0 => ("down", "not heard from yet".to_string()),
        (true, false) => (
            "down",
            format!(
                "last heard {} min ago",
                now.saturating_sub(m.last_seen) / 60
            ),
        ),
    };
    let ssh = match (m.ssh.bool("ok"), m.ssh.str("note")) {
        (Some(true), _) => "ssh check: worked".to_string(),
        (Some(false), n) => format!("ssh check: failed ({})", n.unwrap_or("")),
        _ => "ssh not checked yet".into(),
    };
    let toggle = if m.enabled { "disable" } else { "enable" };
    format!(
        "<div class=\"card {state}\"><p class=\"inst {state}\">{}</p><h3>{}</h3><p class=\"handle\">{}</p><p class=\"small\">{} &middot; {}</p>{}{}<p class=\"small\">{}</p><pre>tempo-launch fleet {toggle} {}\ntempo-launch fleet check {}</pre></div>",
        html_escape(&word),
        html_escape(&m.name),
        html_escape(&m.addr),
        html_escape(&m.role),
        if m.enabled { "in the fleet" } else { "out of the fleet" },
        health_html(&m.health),
        tools_html(&m.tools),
        html_escape(&ssh),
        html_escape(&m.name),
        html_escape(&m.name),
    )
}

/// The fleet's heading, the machines with their live health, and what to run to change it.
pub fn section(p: &Paths, with_heading: bool) -> String {
    let f: Fleet = store::load(p);
    let now = now_s();
    let me_health = super::serve::health_with(0, None);
    let me = format!(
        "<div class=\"card up\"><p class=\"inst up\">this computer</p><h3>{}</h3>{}</div>",
        html_escape(&f.me),
        health_html(&me_health)
    );
    let others: String = f.machines.iter().map(|m| card(m, now)).collect();
    let hint = if f.machines.is_empty() {
        "<p>No other computer yet. <code>tempo-launch fleet scan</code> looks for them on your own network; <code>tempo-launch fleet add &lt;address&gt;</code> adds one.</p>"
    } else {
        ""
    };
    let lede = if with_heading {
        format!(
            "<h3 class=\"bandname\">{}</h3>",
            html_escape(&super::heading(f.count()))
        )
    } else {
        String::new()
    };
    let add = "<div class=\"verdict\"><h3>Add a computer</h3><p>On this computer: <code>tempo-launch fleet invite</code>. It shows a short code, good for 10 minutes and once only. On the new computer, once Tempo is installed: <code>tempo-launch fleet join</code>, then type the code. It finds this one by itself, sets itself up, and shows up here with the part its hardware suits.</p></div>";
    format!(
        "{CSS}{lede}<div class=\"cards\">{me}{others}</div>{add}{hint}<p class=\"small\">Health is what each computer's last heartbeat said (every 10 seconds, while <code>tempo-launch fleet serve</code> runs). This page is static: a switch is the command under a machine.</p>",
    )
}

pub fn write(ctx: &Ctx) -> Result<PathBuf, String> {
    let p = Paths::of(ctx);
    let file = ctx.home.join("fleet.html");
    let html = format!(
        "{top}<h1>{heading}</h1>\n<p class=\"lede\">Your own computers, and what each is doing right now.</p>\n{updates}\n{sec}\n<h2>Change the fleet</h2>\n<pre>tempo-launch fleet scan\ntempo-launch fleet add &lt;address or name&gt;\ntempo-launch fleet remove &lt;name&gt;\ntempo-launch fleet ping &lt;name&gt;</pre>\n{end}",
        top = brand::page_top("Tempo Fleet", true, true),
        heading = html_escape(&super::heading(store::load(&p).count())),
        updates = crate::updates::notice_html(&ctx.home),
        sec = section(&p, false),
        end = brand::PAGE_END,
    );
    write_atomic(&file, html.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", file.display()))?;
    Ok(file)
}
