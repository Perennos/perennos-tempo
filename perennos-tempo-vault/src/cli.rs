//! The `tempo-vault` command line.
use crate::graph::{self, GraphOpts};
use crate::learn::{self, LearnOpts};
use crate::render;
use crate::server::{self, ServeOpts};
use crate::util::{clean_rel, expand_home, unix_now};
use crate::vault::Vault;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

pub const USAGE: &str = "Perennos Tempo Vault: Obsidian-compatible Markdown notes in Rust.

usage: tempo-vault <command> [--vault DIR] [options]      (DIR defaults to TEMPO_VAULT)

  init [DIR]                      start a vault (or add Tempo's settings to an Obsidian vault)
  index                           index the vault and show its numbers
  search QUERY [--limit N]        full-text search (\"phrase\", -not, a OR b, tag:x, path:x, [prop:value])
  read PATH [--html]              print a note (or its rendered HTML)
  new PATH [--text T | --template NAME]
  daily [--date YYYY-MM-DD] [--append TEXT] [--heading H]
  append PATH TEXT [--heading H]
  rename FROM TO                  move a note or attachment; every link to it is rewritten
  trash PATH                      move to the vault's .trash folder (never deletes)
  history PATH | restore PATH ID  versions kept before every change
  links PATH | backlinks PATH | mentions PATH
  tags | tasks [--rules R] | orphans | unresolved
  query QUERY | query --base FILE   Dataview (TABLE x FROM \"folder\" WHERE ... SORT ...) or a .base
  graph [--center PATH] [--depth N] [--tags] [--unresolved] [--no-attachments] [--filter Q] [--out FILE]
  check                           vault health: broken links, orphans, clashing names, unsafe names
  publish --out DIR [--all]       a static site of the notes marked publish: true
  serve [--port 8766] [--bind 127.0.0.1] [--read-only] [--allow-host H] [--watch SECS]
        [--allow-writes]            off loopback the vault is read-only unless this is given
  mcp [--write]                   the vault as MCP tools on stdio (read-only unless --write)
  learn [--agent-data DIR] [--model NAME] [--ollama URL] [--dry-run] [--since DAYS] [--max N]
        [--with-model-cycle]        then Tempo Learn's daily cycle (tempo-learn run --dry-run)
  schedule [--every hourly|daily] [--print | --install | --remove]
  version

Add --json to most commands for machine-readable output.";

const VALUE_FLAGS: &[&str] = &[
    "--vault",
    "--limit",
    "--text",
    "--template",
    "--date",
    "--append",
    "--heading",
    "--rules",
    "--center",
    "--depth",
    "--filter",
    "--out",
    "--port",
    "--bind",
    "--allow-host",
    "--watch",
    "--agent-data",
    "--model",
    "--ollama",
    "--since",
    "--max",
    "--every",
    "--id",
    "--base",
];
const BOOL_FLAGS: &[&str] = &[
    "--json",
    "--with-model-cycle",
    "--html",
    "--tags",
    "--unresolved",
    "--no-attachments",
    "--all",
    "--read-only",
    "--allow-writes",
    "--write",
    "--dry-run",
    "--print",
    "--install",
    "--remove",
    "--help",
    "--no-orphans",
];

struct Args {
    pos: Vec<String>,
    val: HashMap<String, Vec<String>>,
    on: HashSet<String>,
}

impl Args {
    fn get(&self, k: &str) -> Option<&str> {
        self.val.get(k).and_then(|v| v.last()).map(String::as_str)
    }
    fn all(&self, k: &str) -> Vec<String> {
        self.val.get(k).cloned().unwrap_or_default()
    }
    fn has(&self, k: &str) -> bool {
        self.on.contains(k)
    }
    fn pos(&self, i: usize, what: &str) -> Result<&str, String> {
        self.pos
            .get(i)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {what} (see tempo-vault --help)"))
    }
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        pos: vec![],
        val: HashMap::new(),
        on: HashSet::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let s = &args[i];
        if let Some((k, v)) = s.split_once('=').filter(|(k, _)| VALUE_FLAGS.contains(k)) {
            a.val.entry(k.to_string()).or_default().push(v.to_string());
        } else if VALUE_FLAGS.contains(&s.as_str()) {
            let v = args
                .get(i + 1)
                .ok_or_else(|| format!("{s} needs a value"))?;
            a.val.entry(s.clone()).or_default().push(v.clone());
            i += 1;
        } else if BOOL_FLAGS.contains(&s.as_str()) || s == "-h" {
            a.on.insert(if s == "-h" {
                "--help".into()
            } else {
                s.clone()
            });
        } else if s.starts_with("--") {
            return Err(format!("unknown option {s} (see tempo-vault --help)"));
        } else {
            a.pos.push(s.clone());
        }
        i += 1;
    }
    Ok(a)
}

fn vault_dir(a: &Args) -> Result<PathBuf, String> {
    let d = a
        .get("--vault")
        .map(str::to_string)
        .or_else(|| std::env::var("TEMPO_VAULT").ok())
        .filter(|s| !s.trim().is_empty())
        .ok_or("say which vault: --vault DIR or the TEMPO_VAULT environment variable")?;
    Ok(expand_home(&d))
}

fn open(a: &Args) -> Result<Vault, String> {
    Vault::open(vault_dir(a)?)
}

fn rel(p: &str) -> Result<String, String> {
    clean_rel(p).ok_or_else(|| format!("not a usable vault path: {p}"))
}

fn note_rel(v: &Vault, p: &str) -> Result<String, String> {
    let r = rel(p)?;
    Ok(
        if v.note(&r).is_some() || v.has_file(&r) || r.to_lowercase().ends_with(".md") {
            r
        } else {
            format!("{r}.md")
        },
    )
}

fn emit(
    out: &mut dyn Write,
    a: &Args,
    j: Value,
    text: impl FnOnce() -> String,
) -> Result<(), String> {
    let s = if a.has("--json") {
        serde_json::to_string_pretty(&j).unwrap_or_default()
    } else {
        text()
    };
    writeln!(out, "{}", s.trim_end()).map_err(|e| e.to_string())
}

const WELCOME: &str = "---\ntags: [tempo]\n---\n# Welcome to Perennos Tempo Vault\n\nPlain Markdown files in a folder: yours to keep, open in any editor, and readable by Obsidian.\nTempo adds a fast Rust index, a 3D graph, history for every change and a daily learning loop.\n\n> [!tip] Try it\n> - Link notes with `[[double brackets]]`; a link to a note that does not exist yet creates it.\n> - Embed one note in another with `![[Note]]`, or just one section with `![[Note#Heading]]`.\n> - Tag with `#tags`, add properties at the top, and tasks with `- [ ]`.\n> - Press Ctrl+K to jump anywhere, Ctrl+Shift+F to search, Ctrl+G for the graph.\n\n## Today\n```tasks\nnot done\ndue before tomorrow\n```\n\n## Start here\n- [[Daily notes]] keep a log of each day.\n- Tempo writes what it learned from your conversations into [[Tempo Learned]].\n";

const DAILY_NOTES: &str = "# Daily notes\nOne note per day in the `Daily` folder, from `Templates/Daily.md`.\nOpen today's from the toolbar, with `tempo-vault daily`, or let Tempo add to it while it learns.\n";

const DAILY_TPL: &str =
    "---\ntags: [daily]\n---\n# {{date:dddd D MMMM YYYY}}\n\n## Plan\n- [ ] \n\n## Notes\n\n";

const NOTE_TPL: &str = "---\ncreated: {{date}} {{time}}\ntags: []\n---\n# {{title}}\n\n";

fn init(a: &Args, out: &mut dyn Write) -> Result<(), String> {
    let dir = match a.pos.get(1) {
        Some(d) => expand_home(d),
        None => vault_dir(a)?,
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let obsidian = dir.join(".obsidian").is_dir();
    let has_notes = crate::vault::walk(&dir)
        .0
        .iter()
        .any(|(p, _)| crate::util::is_md(p));
    let mut made = vec![];
    let mut put = |rel: &str, text: &str| -> Result<(), String> {
        let p = rel.split('/').fold(dir.clone(), |p, s| p.join(s));
        if p.exists() {
            return Ok(()); // never overwrite
        }
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, text).map_err(|e| e.to_string())?;
        made.push(rel.to_string());
        Ok(())
    };
    put(
        ".tempo/config.json",
        "{\n  \"daily_folder\": \"Daily\",\n  \"templates_folder\": \"Templates\"\n}\n",
    )?;
    if !has_notes {
        put("Welcome.md", WELCOME)?;
        put("Daily notes.md", DAILY_NOTES)?;
        put("Templates/Daily.md", DAILY_TPL)?;
        put("Templates/Note.md", NOTE_TPL)?;
    }
    let v = Vault::open(&dir)?;
    let st = v.stats();
    emit(
        out,
        a,
        json!({"vault": dir, "made": made, "obsidian": obsidian, "stats": st}),
        || {
            format!(
            "vault ready at {}{}\n{} notes, {} attachments{}\nnext: tempo-vault serve --vault \"{}\"",
            dir.display(),
            if obsidian { " (an Obsidian vault: its notes and settings are used as they are)" } else { "" },
            st["notes"],
            st["attachments"],
            if made.is_empty() { String::new() } else { format!("\nadded: {}", made.join(", ")) },
            dir.display()
        )
        },
    )
}

/// Serve options. Off loopback the vault is read-only unless `--allow-writes` says otherwise
/// (anyone who can reach the address could change notes); the note says which it is.
fn serve_opts(a: &Args) -> Result<(ServeOpts, Option<String>), String> {
    let bind: std::net::IpAddr = a
        .get("--bind")
        .unwrap_or("127.0.0.1")
        .parse()
        .map_err(|_| "--bind takes an IP address")?;
    let off_loopback = !bind.is_loopback();
    let read_only = a.has("--read-only") || (off_loopback && !a.has("--allow-writes"));
    let note = off_loopback.then(|| {
        if read_only {
            format!("serving on {bind}, read-only: anyone who can reach this address can read the vault. Add --allow-writes to let them change notes, and --allow-host for the name they use")
        } else {
            format!("warning: serving on {bind} with --allow-writes: anyone who can reach this address can read and change the vault")
        }
    });
    let o = ServeOpts {
        bind,
        port: a
            .get("--port")
            .map(|p| p.parse().map_err(|_| "--port takes a number"))
            .transpose()?
            .unwrap_or(8766),
        allow_hosts: a.all("--allow-host"),
        read_only,
        agent_url: None,
        watch: Duration::from_secs_f64(
            a.get("--watch")
                .and_then(|w| w.parse().ok())
                .unwrap_or(2.0f64)
                .max(0.0),
        ),
    };
    Ok((o, note))
}

fn check(v: &Vault) -> Value {
    let unresolved = v.unresolved();
    let mut wanted: HashMap<String, usize> = HashMap::new();
    for (_, t, _) in &unresolved {
        *wanted.entry(t.to_lowercase()).or_insert(0) += 1;
    }
    let mut wanted: Vec<(String, usize)> = wanted.into_iter().collect();
    wanted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut by_stem: HashMap<String, Vec<String>> = HashMap::new();
    for n in v.notes() {
        by_stem
            .entry(crate::util::stem(&n.path).to_lowercase())
            .or_default()
            .push(n.path.clone());
    }
    let mut clashes: Vec<Vec<String>> = by_stem.into_values().filter(|l| l.len() > 1).collect();
    clashes.sort();
    let empty: Vec<String> = v
        .notes()
        .filter(|n| n.parsed.words == 0 && n.parsed.links.is_empty())
        .map(|n| n.path.clone())
        .collect();
    let broken_embeds: Vec<Value> = v
        .notes()
        .flat_map(|n| {
            v.links_of(&n.path)
                .into_iter()
                .filter(|(l, r)| l.embed && r.is_none())
                .map(|(l, _)| json!({"source": n.path, "line": l.line, "target": l.target}))
                .collect::<Vec<_>>()
        })
        .collect();
    let unsafe_names: Vec<String> = crate::vault::walk(&v.root)
        .0
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| clean_rel(p).is_none())
        .collect();
    let big: Vec<String> = v
        .notes()
        .filter(|n| n.size > 1 << 20)
        .map(|n| n.path.clone())
        .collect();
    json!({
        "notes": v.notes().count(),
        "unresolved_links": unresolved.len(),
        "most_wanted": wanted.iter().take(20).map(|(t, n)| json!({"target": t, "count": n})).collect::<Vec<_>>(),
        "broken_embeds": broken_embeds,
        "orphans": v.orphans().len(),
        "name_clashes": clashes,
        "empty_notes": empty,
        "names_windows_cannot_store": unsafe_names,
        "large_notes": big,
        "warnings": v.warnings,
    })
}

pub fn run(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let a = parse(args)?;
    let cmd = a.pos.first().map(String::as_str).unwrap_or("");
    if cmd.is_empty() || cmd == "help" || a.has("--help") {
        writeln!(out, "{USAGE}").map_err(|e| e.to_string())?;
        return Ok(());
    }
    let origin = "cli";
    match cmd {
        "version" => emit(
            out,
            &a,
            json!({"version": env!("CARGO_PKG_VERSION")}),
            || format!("tempo-vault {}", env!("CARGO_PKG_VERSION")),
        ),
        "init" => init(&a, out),
        "index" | "stats" => {
            let v = open(&a)?;
            let s = v.stats();
            emit(out, &a, s.clone(), || {
                format!("{} notes, {} attachments, {} links ({} unresolved), {} tags, {} open tasks, {} words{}", s["notes"], s["attachments"], s["links"], s["unresolved"], s["tags"], s["open_tasks"], s["words"], v.warnings.iter().map(|w| format!("\nwarning: {w}")).collect::<String>())
            })
        }
        "search" => {
            let v = open(&a)?;
            let q = a.pos[1..].join(" ");
            if q.trim().is_empty() {
                return Err("search for what? tempo-vault search WORDS".into());
            }
            let limit = a.get("--limit").and_then(|l| l.parse().ok()).unwrap_or(20);
            let hits = v.search(&q, limit);
            let j = json!(hits.iter().map(|h| json!({"path": h.path, "title": h.title, "score": h.score, "line": h.line, "snippet": h.snippet})).collect::<Vec<_>>());
            emit(out, &a, j, || {
                if hits.is_empty() {
                    "no notes match".into()
                } else {
                    hits.iter()
                        .map(|h| format!("{}:{}  {}\n    {}", h.path, h.line, h.title, h.snippet))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
        }
        "read" => {
            let v = open(&a)?;
            let p = note_rel(&v, a.pos(1, "PATH")?)?;
            if a.has("--html") {
                let h = render::note_html(&v, &p, &render::Opts::app())
                    .ok_or_else(|| format!("no note {p}"))?;
                return writeln!(out, "{h}").map_err(|e| e.to_string());
            }
            let t = v.read(&p).ok_or_else(|| format!("no note {p}"))?;
            write!(out, "{t}").map_err(|e| e.to_string())
        }
        "new" => {
            let mut v = open(&a)?;
            let p = a.pos(1, "PATH")?;
            let made = match a.get("--template") {
                Some(t) => v.from_template(p, t, origin),
                None => v.create(p, a.get("--text").unwrap_or(""), origin),
            }
            .map_err(|e| e.to_string())?;
            emit(out, &a, json!({"path": made}), || made.clone())
        }
        "daily" => {
            let mut v = open(&a)?;
            let p = v
                .daily(a.get("--date"), origin)
                .map_err(|e| e.to_string())?;
            if let Some(t) = a.get("--append") {
                v.append(&p, t, a.get("--heading"), origin)
                    .map_err(|e| e.to_string())?;
            }
            emit(out, &a, json!({"path": p}), || p.clone())
        }
        "append" => {
            let mut v = open(&a)?;
            let p = a.pos(1, "PATH")?;
            let text = a.pos[2..].join(" ");
            if text.is_empty() {
                return Err("append what? tempo-vault append PATH TEXT".into());
            }
            let p = v
                .append(p, &text, a.get("--heading"), origin)
                .map_err(|e| e.to_string())?;
            emit(out, &a, json!({"path": p}), || p.clone())
        }
        "rename" | "move" | "mv" => {
            let mut v = open(&a)?;
            let changed = v
                .rename(a.pos(1, "FROM")?, a.pos(2, "TO")?, origin)
                .map_err(|e| e.to_string())?;
            emit(out, &a, json!({"changed": changed}), || {
                if changed.is_empty() {
                    "renamed; no links needed changing".into()
                } else {
                    format!("renamed; links rewritten in:\n{}", changed.join("\n"))
                }
            })
        }
        "trash" | "rm" => {
            let mut v = open(&a)?;
            let to = v
                .trash(a.pos(1, "PATH")?, origin)
                .map_err(|e| e.to_string())?;
            emit(out, &a, json!({"to": to}), || format!("moved to {to}"))
        }
        "history" => {
            let v = open(&a)?;
            let p = note_rel(&v, a.pos(1, "PATH")?)?;
            let h = v.history(&p);
            emit(out, &a, json!({"path": p, "versions": h}), || {
                if h.is_empty() {
                    format!("no earlier versions of {p}")
                } else {
                    h.join("\n")
                }
            })
        }
        "restore" => {
            let mut v = open(&a)?;
            let p = note_rel(&v, a.pos(1, "PATH")?)?;
            let id = a.get("--id").or(a.pos.get(2).map(String::as_str)).ok_or(
                "which version? tempo-vault restore PATH ID (see tempo-vault history PATH)",
            )?;
            v.restore(&p, id, origin).map_err(|e| e.to_string())?;
            emit(out, &a, json!({"path": p, "restored": id}), || {
                format!("restored {p} to {id} (the text before is kept in history)")
            })
        }
        "links" => {
            let v = open(&a)?;
            let p = note_rel(&v, a.pos(1, "PATH")?)?;
            let l = v.links_of(&p);
            emit(out, &a, json!(l.iter().map(|(k, r)| json!({"line": k.line, "target": k.target, "to": r, "embed": k.embed})).collect::<Vec<_>>()), || l.iter().map(|(k, r)| format!("{}: {} -> {}", k.line, k.target, r.as_deref().unwrap_or("(does not exist yet)"))).collect::<Vec<_>>().join("\n"))
        }
        "backlinks" => {
            let v = open(&a)?;
            let p = note_rel(&v, a.pos(1, "PATH")?)?;
            let b = v.backlinks(&p);
            emit(
                out,
                &a,
                json!(b
                    .iter()
                    .map(|x| json!({"source": x.source, "line": x.line, "embed": x.embed}))
                    .collect::<Vec<_>>()),
                || {
                    if b.is_empty() {
                        format!("nothing links to {p}")
                    } else {
                        b.iter()
                            .map(|x| format!("{}:{}", x.source, x.line))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                },
            )
        }
        "mentions" => {
            let v = open(&a)?;
            let p = note_rel(&v, a.pos(1, "PATH")?)?;
            let m = v.mentions(&p);
            emit(
                out,
                &a,
                json!(m
                    .iter()
                    .map(|(s, l, t)| json!({"source": s, "line": l, "text": t}))
                    .collect::<Vec<_>>()),
                || {
                    if m.is_empty() {
                        "no unlinked mentions".into()
                    } else {
                        m.iter()
                            .map(|(s, l, t)| format!("{s}:{l}  {t}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                },
            )
        }
        "tags" => {
            let v = open(&a)?;
            let t = v.tags();
            emit(out, &a, json!(t), || {
                t.iter()
                    .map(|(k, n)| format!("#{k} {n}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        "tasks" => {
            let v = open(&a)?;
            let today = crate::util::date_of(
                unix_now(),
                crate::util::local_offset_minutes(
                    crate::write::VaultConfig::load(&v.root)
                        .utc_offset
                        .as_deref(),
                ),
            );
            let rules = a.all("--rules").join("\n");
            let found = render::task_query(
                &v,
                if rules.is_empty() { "not done" } else { &rules },
                &today,
            );
            emit(out, &a, json!(found.iter().map(|(p, t)| json!({"path": p, "line": t.line, "done": t.done, "text": t.text, "due": t.due})).collect::<Vec<_>>()), || {
                found.iter().map(|(p, t)| format!("[{}] {}{}  ({p}:{})", if t.done { "x" } else { " " }, t.text, t.due.as_deref().map(|d| format!(" due {d}")).unwrap_or_default(), t.line)).collect::<Vec<_>>().join("\n")
            })
        }
        "query" => {
            let v = open(&a)?;
            let b = match a.get("--base") {
                Some(f) => {
                    let p =
                        crate::util::clean_rel(f).ok_or("--base takes a vault-relative path")?;
                    crate::query::base(&v.read_text(&p).ok_or(format!("no base file {p}"))?)
                }
                None => {
                    let q = a.pos.get(1..).map(|x| x.join(" ")).unwrap_or_default();
                    if q.trim().is_empty() {
                        return Err("usage: tempo-vault query \"TABLE status FROM #project\" (or --base FILE)".into());
                    }
                    crate::query::any(&q)
                }
            }?;
            let outs = crate::query::run(&v, &b);
            emit(out, &a, crate::query::to_json(&outs), || {
                crate::query::markdown(&outs)
            })
        }
        "orphans" => {
            let v = open(&a)?;
            let o = v.orphans();
            emit(out, &a, json!(o), || o.join("\n"))
        }
        "unresolved" => {
            let v = open(&a)?;
            let u = v.unresolved();
            emit(
                out,
                &a,
                json!(u
                    .iter()
                    .map(|(s, t, l)| json!({"source": s, "target": t, "line": l}))
                    .collect::<Vec<_>>()),
                || {
                    u.iter()
                        .map(|(s, t, l)| format!("{s}:{l} -> [[{t}]]"))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            )
        }
        "graph" => {
            let v = open(&a)?;
            let o = GraphOpts {
                center: a.get("--center").map(|c| note_rel(&v, c)).transpose()?,
                depth: a.get("--depth").and_then(|d| d.parse().ok()).unwrap_or(2),
                tags: a.has("--tags"),
                attachments: !a.has("--no-attachments"),
                unresolved: a.has("--unresolved"),
                orphans: !a.has("--no-orphans"),
                filter: a.get("--filter").map(str::to_string),
                ..GraphOpts::default()
            };
            let g = graph::vault_graph(&v, &o);
            let s = g.to_string();
            match a.get("--out") {
                Some(f) => {
                    std::fs::write(f, &s).map_err(|e| format!("cannot write {f}: {e}"))?;
                    writeln!(
                        out,
                        "wrote {} nodes and {} edges to {f}",
                        g["nodes"].as_array().map_or(0, Vec::len),
                        g["edges"].as_array().map_or(0, Vec::len)
                    )
                    .map_err(|e| e.to_string())
                }
                None => writeln!(out, "{s}").map_err(|e| e.to_string()),
            }
        }
        "check" => {
            let v = open(&a)?;
            let c = check(&v);
            emit(out, &a, c.clone(), || {
                let mut s = format!(
                    "{} notes · {} unresolved links · {} orphans · {} broken embeds\n",
                    c["notes"],
                    c["unresolved_links"],
                    c["orphans"],
                    c["broken_embeds"].as_array().map_or(0, Vec::len)
                );
                for w in c["most_wanted"].as_array().into_iter().flatten().take(10) {
                    s.push_str(&format!(
                        "wanted: [[{}]] x{}\n",
                        w["target"].as_str().unwrap_or(""),
                        w["count"]
                    ));
                }
                for cl in c["name_clashes"].as_array().into_iter().flatten() {
                    s.push_str(&format!(
                        "same name, bare links are ambiguous: {}\n",
                        cl.as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                for n in c["names_windows_cannot_store"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    s.push_str(&format!(
                        "name Windows cannot store: {}\n",
                        n.as_str().unwrap_or("")
                    ));
                }
                for n in c["empty_notes"].as_array().into_iter().flatten() {
                    s.push_str(&format!("empty: {}\n", n.as_str().unwrap_or("")));
                }
                s
            })
        }
        "publish" => {
            let v = open(&a)?;
            let dir = expand_home(a.get("--out").ok_or("publish where? --out DIR")?);
            let r = crate::publish::publish(&v, &dir, a.has("--all"))?;
            emit(
                out,
                &a,
                json!({"pages": r.pages, "files": r.files, "out": dir}),
                || {
                    format!(
                        "published {} pages and {} attachments to {}{}",
                        r.pages,
                        r.files,
                        dir.display(),
                        if r.pages == 0 && !a.has("--all") {
                            " (mark notes with `publish: true`, or pass --all)"
                        } else {
                            ""
                        }
                    )
                },
            )
        }
        "serve" => {
            let v = open(&a)?;
            let (o, note) = serve_opts(&a)?;
            if let Some(n) = note {
                eprintln!("[{n}]");
            }
            server::serve(v, o)
        }
        "mcp" => {
            let v = open(&a)?;
            let stdin = std::io::stdin();
            crate::mcp::run(v, a.has("--write"), stdin.lock(), std::io::stdout())
                .map_err(|e| e.to_string())
        }
        "learn" => {
            let mut v = open(&a)?;
            let mut o = LearnOpts::resolve(
                &v.root,
                a.get("--agent-data").map(str::to_string),
                a.get("--model").map(str::to_string),
                a.get("--ollama").map(str::to_string),
            )?;
            o.dry_run = a.has("--dry-run");
            if a.has("--with-model-cycle") {
                o.model_cycle = Some(learn::find_tempo_learn());
            }
            o.since_days = a.get("--since").and_then(|d| d.parse().ok());
            if let Some(m) = a.get("--max").and_then(|m| m.parse().ok()) {
                o.max = m;
            }
            let r = learn::learn(&mut v, &o)?;
            let j = r.to_json();
            emit(out, &a, j, || {
                let mut s = format!(
                    "{} conversation(s) looked at, {} learned, {} unchanged{}",
                    r.scanned,
                    r.learned,
                    r.unchanged,
                    if o.dry_run {
                        " (dry run: nothing written)"
                    } else {
                        ""
                    }
                );
                if r.model_used > 0 {
                    s.push_str(&format!(
                        ", {} summarized by {}",
                        r.model_used,
                        o.model.as_deref().unwrap_or("")
                    ));
                }
                for n in &r.notes {
                    s.push_str(&format!("\n  {n}"));
                }
                if r.taught > 0 {
                    s.push_str(&format!(
                        "\n{} reviewed answer(s) added to {}",
                        r.taught,
                        learn::TAUGHT_FOLDER
                    ));
                }
                if let Some(m) = &r.model_cycle {
                    s.push_str(&format!("\n{m}"));
                }
                for w in &r.warnings {
                    s.push_str(&format!("\nwarning: {w}"));
                }
                s
            })
        }
        "schedule" => {
            let dir = vault_dir(&a)?;
            let vault = dir
                .canonicalize()
                .map_err(|e| format!("no vault at {}: {e}", dir.display()))?;
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let every = a.get("--every").unwrap_or("hourly");
            if a.has("--install") || a.has("--remove") {
                let msg = learn::schedule_apply(&exe, &vault, every, a.has("--remove"))?;
                return writeln!(out, "{msg}").map_err(|e| e.to_string());
            }
            let (line, args) = learn::schedule_plan(&exe, &vault, every)?;
            let how = if cfg!(windows) {
                format!(
                    "schtasks {}",
                    args.iter()
                        .map(|x| if x.contains(' ') {
                            format!("\"{}\"", x.replace('"', "\\\""))
                        } else {
                            x.clone()
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            } else {
                format!("crontab line:\n{line}")
            };
            writeln!(out, "{how}\n(run again with --install to set it up; learn needs agent_data in {}/.tempo/config.json or TEMPO_AGENT_DATA)", vault.display()).map_err(|e| e.to_string())
        }
        other => Err(format!("unknown command {other} (see tempo-vault --help)")),
    }
}

/// For tests: run with arguments and capture what it prints.
pub fn run_capture(args: &[&str]) -> Result<String, String> {
    let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    run(&a, &mut out)?;
    Ok(String::from_utf8_lossy(&out).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::tmp;

    #[test]
    fn off_loopback_is_read_only_unless_writes_are_allowed() {
        let args =
            |v: &[&str]| parse(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        let (o, note) = serve_opts(&args(&["serve"])).unwrap();
        assert!(
            !o.read_only && note.is_none(),
            "loopback: writable, nothing to say"
        );
        let (o, note) = serve_opts(&args(&["serve", "--bind", "0.0.0.0"])).unwrap();
        assert!(
            o.read_only && note.unwrap().contains("read-only"),
            "a LAN bind is read-only by default"
        );
        let (o, note) =
            serve_opts(&args(&["serve", "--bind", "10.0.0.5", "--allow-writes"])).unwrap();
        assert!(!o.read_only && note.unwrap().contains("read and change"));
        let (o, _) = serve_opts(&args(&[
            "serve",
            "--bind",
            "10.0.0.5",
            "--allow-writes",
            "--read-only",
        ]))
        .unwrap();
        assert!(o.read_only, "--read-only always wins");
        assert!(serve_opts(&args(&["serve", "--bind", "nope"])).is_err());
    }

    #[test]
    fn init_new_daily_search_rename_check() {
        let d = tmp("cli");
        let vd = d.join("Vault");
        let v = vd.to_str().unwrap();
        let out = run_capture(&["init", v]).unwrap();
        assert!(
            out.contains("vault ready") && out.contains("Welcome.md"),
            "{out}"
        );
        assert!(
            run_capture(&["init", v]).unwrap().contains("vault ready"),
            "init twice is harmless"
        );
        assert_eq!(
            std::fs::read_to_string(vd.join("Welcome.md")).unwrap(),
            WELCOME,
            "never overwritten"
        );
        assert_eq!(
            run_capture(&[
                "new",
                "Ideas/Rust",
                "--text",
                "Use [[Welcome]] #rust",
                "--vault",
                v
            ])
            .unwrap()
            .trim(),
            "Ideas/Rust.md"
        );
        let day = run_capture(&[
            "daily",
            "--date",
            "2026-11-06",
            "--append",
            "met Riley",
            "--heading",
            "Log",
            "--vault",
            v,
        ])
        .unwrap();
        assert_eq!(day.trim(), "Daily/2026-11-06.md");
        let t = std::fs::read_to_string(vd.join("Daily/2026-11-06.md")).unwrap();
        assert!(
            t.starts_with("---\ntags: [daily]\n---\n# Friday 6 November 2026\n")
                && t.contains("## Log\nmet Riley"),
            "{t}"
        );
        let s = run_capture(&["search", "rust", "--vault", v, "--json"]).unwrap();
        let j: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(j[0]["path"], "Ideas/Rust.md");
        let r = run_capture(&[
            "rename",
            "Welcome.md",
            "Start/Welcome Home.md",
            "--vault",
            v,
        ])
        .unwrap();
        assert!(r.contains("Ideas/Rust.md"), "{r}");
        assert!(std::fs::read_to_string(vd.join("Ideas/Rust.md"))
            .unwrap()
            .contains("[[Welcome Home]]"));
        let c: Value =
            serde_json::from_str(&run_capture(&["check", "--vault", v, "--json"]).unwrap())
                .unwrap();
        assert!(
            c["most_wanted"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["target"] == "tempo learned"),
            "{c}"
        );
        let g: Value =
            serde_json::from_str(&run_capture(&["graph", "--vault", v, "--tags"]).unwrap())
                .unwrap();
        assert!(g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["src"] == "#rust"));
        assert!(run_capture(&["trash", "Ideas/Rust.md", "--vault", v])
            .unwrap()
            .starts_with("moved to .trash/"));
        assert!(run_capture(&["bogus", "--vault", v])
            .unwrap_err()
            .contains("unknown command"));
        assert!(run_capture(&["search", "--nope"])
            .unwrap_err()
            .contains("unknown option"));
        assert!(run_capture(&["--help"])
            .unwrap()
            .contains("usage: tempo-vault"));
    }
}
