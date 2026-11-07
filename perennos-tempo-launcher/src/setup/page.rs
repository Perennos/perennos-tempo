//! `<home>/setup.html`: the plan in plain words, written at the first start. Nothing is fetched
//! by it; it shows the exact command that would do it.

use super::plan::{Plan, Status};
use crate::{brand, html_escape};

pub const D1_LICENCE: &str = "d1-2B is Liquid AI's model, under the LFM Open License v1.0. It is free to use only while your yearly revenue is under USD 10 million. Read the licence before you fetch it.";

pub fn setup_page(p: &Plan, apply_command: &str) -> String {
    let li = |items: Vec<String>| -> String {
        items
            .into_iter()
            .map(|t| format!("<li>{}</li>", html_escape(&t)))
            .collect()
    };
    let machine = li(p.summary.clone());
    let picks = li(p.picks.iter().map(|k| k.why.clone()).collect());
    let fetch: Vec<String> = p
        .fetch
        .iter()
        .map(|f| {
            format!(
                "{} ({}, licence: {})",
                f.name,
                f.gb.map_or("size unknown".into(), |g| format!("{g:.1} GB")),
                f.licence
            )
        })
        .collect();
    let fetch_html = if fetch.is_empty() {
        "<p>Nothing needs to be fetched.</p>".to_string()
    } else {
        format!(
            "<ul>{}</ul><p>Total {:.1} GB{}; free disk {}.</p>",
            li(fetch),
            p.fetch_gb,
            if p.unknown_sizes.is_empty() {
                String::new()
            } else {
                format!(
                    " plus {} of unknown size",
                    html_escape(&p.unknown_sizes.join(", "))
                )
            },
            p.disk_free_gb
                .map_or("unknown".into(), |d| format!("{d:.0} GB"))
        )
    };
    let installed = p
        .picks
        .iter()
        .filter(|k| k.status == Status::Installed)
        .count();
    let refused = p.refused.as_ref().map_or(String::new(), |r| {
        format!("<p><strong>{}</strong></p>", html_escape(r))
    });
    // the same facts the terminal plan shows, when this computer has no Ollama
    let ollama = p.runtime.as_ref().map_or(String::new(), |r| {
        format!("<h2>Ollama</h2><ul>{}</ul>", li(r.lines()))
    });
    format!(
        r#"{top}<h1>Set up Perennos Tempo for this computer</h1>
<p>Tempo looked at this computer and your goals ({goals}) and chose the models below. Nothing has been fetched. {installed} of the chosen models are already installed.</p>
<h2>This computer</h2><ul>{machine}</ul>
<h2>What Tempo chose, and why</h2><ul>{picks}</ul>
{ollama}<h2>What would be fetched</h2>{fetch}{refused}
<h2>To go ahead</h2>
<p>Open a terminal and run this. It asks before it fetches anything:</p>
<pre>{command}</pre>
<p>Run it again whenever you like; it only changes what is missing. <code>tempo-launch setup</code> on its own shows this plan again.</p>
<h2>Optional: d1</h2>
<p>d1 can route and judge work on this computer. It is not included unless you add <code>--with-d1</code>. {d1}</p>
<p class="small">Perennos's own UMBRA models are never downloaded: Tempo only uses them where they are already on this computer.</p>
{end}"#,
        top = brand::page_top("Set up Perennos Tempo", true, true),
        end = brand::PAGE_END,
        goals = html_escape(&p.goals.join(", ")),
        installed = installed,
        machine = machine,
        picks = picks,
        fetch = fetch_html,
        refused = refused,
        ollama = ollama,
        command = html_escape(apply_command),
        d1 = html_escape(D1_LICENCE),
    )
}
