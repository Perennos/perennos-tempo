//! Every change to a vault: create, write (with a conflict check), append, insert under a
//! heading, set a property, toggle a task, daily notes, templates, rename/move with link rewriting
//! across the vault, trash and restore. Nothing is ever deleted: a changed file is first copied to
//! `.tempo/history/<path>/<stamp>.md`, trash moves to `.trash/<date>/`, writes are atomic
//! (temp file, then rename) and every change is appended to `.tempo/changes.jsonl`.
use crate::parse::{self, Prop};
use crate::util::{
    self, clean_rel, file_name, is_md, iso_utc, parent_of, pct_encode_path, relative_path, stamp,
    stem, strip_md, unix_now,
};
use crate::vault::Vault;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum WriteError {
    BadPath(String),
    NotFound(String),
    Exists(String),
    /// The file changed on disk since the caller read it: (current mtime, current content).
    Conflict(f64, String),
    Io(String),
    Invalid(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::BadPath(p) => write!(f, "not a safe vault path: {p}"),
            WriteError::NotFound(p) => write!(f, "no such note or file: {p}"),
            WriteError::Exists(p) => write!(f, "already exists: {p}"),
            WriteError::Conflict(..) => write!(f, "the note changed on disk since it was read"),
            WriteError::Io(e) => write!(f, "{e}"),
            WriteError::Invalid(e) => write!(f, "{e}"),
        }
    }
}

type R<T> = Result<T, WriteError>;

fn io(e: std::io::Error) -> WriteError {
    WriteError::Io(e.to_string())
}

/// Vault settings in `.tempo/config.json` (all optional). An Obsidian vault's own settings
/// (`.obsidian/daily-notes.json`, `.obsidian/templates.json`) are used when Tempo has none.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultConfig {
    pub daily_folder: String,
    pub templates_folder: String,
    /// The daily note template (a vault path); default `<templates>/Daily.md`.
    pub daily_template: Option<String>,
    pub utc_offset: Option<String>,
}

impl Default for VaultConfig {
    fn default() -> Self {
        VaultConfig {
            daily_folder: "Daily".into(),
            templates_folder: "Templates".into(),
            daily_template: None,
            utc_offset: None,
        }
    }
}

impl VaultConfig {
    pub fn load(root: &Path) -> VaultConfig {
        let mut c = VaultConfig::default();
        let read = |p: std::path::PathBuf| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .unwrap_or(Value::Null)
        };
        let obs_daily = read(root.join(".obsidian").join("daily-notes.json"));
        let obs_tpl = read(root.join(".obsidian").join("templates.json"));
        if let Some(f) = obs_daily["folder"].as_str() {
            // Obsidian's "" means the vault root
            c.daily_folder = clean_rel(f).unwrap_or_default();
        }
        if let Some(t) = obs_daily["template"].as_str().and_then(clean_rel) {
            c.daily_template = Some(if is_md(&t) { t } else { format!("{t}.md") });
        }
        if let Some(f) = obs_tpl["folder"].as_str().and_then(clean_rel) {
            c.templates_folder = f;
        }
        let v = read(root.join(".tempo").join("config.json"));
        let folder = |k: &str| v[k].as_str().map(|s| clean_rel(s).unwrap_or_default());
        if let Some(d) = folder("daily_folder") {
            c.daily_folder = d;
        }
        if let Some(d) = folder("templates_folder").filter(|d| !d.is_empty()) {
            c.templates_folder = d;
        }
        if let Some(t) = v["daily_template"].as_str().and_then(clean_rel) {
            c.daily_template = Some(if is_md(&t) { t } else { format!("{t}.md") });
        }
        c.utc_offset = v["utc_offset"].as_str().map(str::to_string);
        c
    }
}

const DAILY_TEMPLATE: &str = "# {{date}}\n\n## Plan\n- [ ] \n\n## Notes\n\n";

/// `{{date}}`, `{{time}}`, `{{title}}`, any extra variables, and Obsidian's
/// `{{date:FORMAT}}` / `{{time:FORMAT}}` (moment.js tokens) for the time `at` (secs, offset).
pub fn fill_template(t: &str, vars: &BTreeMap<String, String>, at: (f64, i64)) -> String {
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    while let Some(p) = rest.find("{{") {
        out.push_str(&rest[..p]);
        let after = &rest[p + 2..];
        if let Some(e) = after.find("}}") {
            let inner = after[..e].trim();
            if let Some(fmt) = inner
                .strip_prefix("date:")
                .or_else(|| inner.strip_prefix("time:"))
            {
                out.push_str(&util::moment(fmt.trim(), at.0, at.1));
                rest = &after[e + 2..];
                continue;
            }
        }
        out.push_str("{{");
        rest = after;
    }
    out.push_str(rest);
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

fn note_path(p: &str) -> R<String> {
    let c = clean_rel(p).ok_or_else(|| WriteError::BadPath(p.to_string()))?;
    Ok(if is_md(&c) { c } else { format!("{c}.md") })
}

/// A snapshot id is a time stamp (`20261105T120000123Z.md`, `...-2.md`): digits, `T`, `Z`, `-`.
pub(crate) fn snapshot_id_ok(id: &str) -> bool {
    let stem = id.strip_suffix(".md").unwrap_or("");
    stem.starts_with(|c: char| c.is_ascii_digit())
        && stem
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'T' | 'Z' | '-'))
}

/// A text document the app writes whole: a note, a `.base` or a `.canvas` (anything else gets `.md`).
pub fn doc_path(p: &str) -> R<String> {
    let c = clean_rel(p).ok_or_else(|| WriteError::BadPath(p.to_string()))?;
    Ok(if is_md(&c) || is_doc(&c) {
        c
    } else {
        format!("{c}.md")
    })
}

/// A `.base` or `.canvas` file: text the app opens and edits like a note.
pub fn is_doc(rel: &str) -> bool {
    matches!(crate::render::ext(rel).as_str(), "base" | "canvas")
}

/// A canvas must stay JSON (JSON Canvas 1.0) so Obsidian and the canvas view can read it.
fn check_doc(rel: &str, content: &str) -> R<()> {
    if crate::render::ext(rel) == "canvas" {
        crate::canvas::parse(content)
            .map_err(|e| WriteError::Invalid(format!("the canvas is not saved: {e}")))?;
    }
    Ok(())
}

impl Vault {
    /// The real path of a vault-relative path, refused when it runs through a symlink or junction.
    fn path(&self, rel: &str) -> R<PathBuf> {
        self.safe_abs(rel)
            .ok_or_else(|| WriteError::BadPath(format!("{rel} (goes through a link)")))
    }

    /// Another file or note whose path differs from `rel` only by letter case (Windows and macOS
    /// would treat them as one file).
    fn collides(&self, rel: &str) -> bool {
        self.notes()
            .map(|n| n.path.as_str())
            .chain(self.files().map(|(p, _)| p.as_str()))
            .any(|p| p != rel && p.eq_ignore_ascii_case(rel))
    }

    /// Read a note's text straight from disk, never through a link.
    fn text_of(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.safe_abs(rel)?).ok()
    }

    fn log_change(&self, origin: &str, op: &str, extra: Value) {
        let Some(dir) = self.safe_abs(".tempo") else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let mut row = json!({"t": iso_utc(unix_now()), "origin": origin, "op": op});
        if let (Some(r), Some(e)) = (row.as_object_mut(), extra.as_object()) {
            r.extend(e.clone());
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("changes.jsonl"))
        {
            let _ = writeln!(f, "{row}");
        }
    }

    /// Copy the current file to `.tempo/history/<rel>/<stamp>.md`; returns the snapshot id.
    pub fn snapshot(&self, rel: &str) -> R<Option<String>> {
        let src = self.path(rel)?;
        if !src.is_file() {
            return Ok(None);
        }
        let dir = self.path(&format!(".tempo/history/{rel}"))?;
        std::fs::create_dir_all(&dir).map_err(io)?;
        let base = stamp(unix_now());
        let mut id = format!("{base}.md");
        let mut n = 1;
        while dir.join(&id).exists() {
            id = format!("{base}-{n}.md");
            n += 1;
        }
        std::fs::copy(&src, dir.join(&id)).map_err(io)?;
        Ok(Some(id))
    }

    /// Snapshots of a note, newest first.
    pub fn history(&self, rel: &str) -> Vec<String> {
        let Some(c) = clean_rel(rel) else {
            return vec![];
        };
        let Ok(dir) = self.path(&format!(".tempo/history/{c}")) else {
            return vec![];
        };
        let mut ids: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.ends_with(".md"))
            .collect();
        // `<stamp>.md` then `<stamp>-1.md`, `-2`... for snapshots taken in the same millisecond.
        let key = |id: &String| {
            let s = id.trim_end_matches(".md");
            match s.split_once('-') {
                Some((t, n)) => (t.to_string(), n.parse::<u32>().unwrap_or(0)),
                None => (s.to_string(), 0),
            }
        };
        ids.sort_by_key(|id| std::cmp::Reverse(key(id)));
        ids
    }

    pub fn history_text(&self, rel: &str, id: &str) -> Option<String> {
        let c = clean_rel(rel)?;
        if !snapshot_id_ok(id) {
            return None;
        }
        let p = self.safe_abs(&format!(".tempo/history/{c}/{id}"))?;
        std::fs::read_to_string(p).ok()
    }

    /// Where new attachments go for `note`: Obsidian's `attachmentFolderPath` (`/` the vault root,
    /// `./` the note's folder, `./x` a folder beside the note, else that folder), or `Attachments`.
    pub fn attachment_folder(&self, note: Option<&str>) -> String {
        let app: Value = std::fs::read_to_string(self.root.join(".obsidian").join("app.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let here = note.map(parent_of).unwrap_or("");
        match app["attachmentFolderPath"].as_str().map(str::trim) {
            None => "Attachments".into(),
            Some("/") | Some("") => String::new(),
            Some(p) if p == "./" || p == "." => here.to_string(),
            Some(p) if p.starts_with("./") => {
                let sub = p[2..].trim_matches('/');
                clean_rel(&if here.is_empty() {
                    sub.to_string()
                } else {
                    format!("{here}/{sub}")
                })
                .unwrap_or_default()
            }
            Some(p) => clean_rel(p.trim_matches('/')).unwrap_or_else(|| "Attachments".into()),
        }
    }

    /// Save an attachment (pasted or dropped) under a free name in `folder`; never overwrites.
    pub fn add_file(&mut self, folder: &str, name: &str, bytes: &[u8], origin: &str) -> R<String> {
        let mut clean: String = file_name(&name.replace('\\', "/"))
            .chars()
            .map(|c| {
                if "\\/:*?\"<>|#^[]".contains(c) || c.is_control() {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
        let clean = clean.trim_matches(['.', ' ']);
        let clean = if clean.is_empty() {
            "Pasted file"
        } else {
            clean
        };
        let clean: String = clean.chars().take(120).collect();
        let (base, ext) = match clean.rfind('.') {
            Some(i) if i > 0 => (clean[..i].to_string(), clean[i..].to_string()),
            _ => (clean.clone(), String::new()),
        };
        let join = |n: &str| {
            if folder.is_empty() {
                n.to_string()
            } else {
                format!("{folder}/{n}")
            }
        };
        let mut rel = clean_rel(&join(&clean)).ok_or_else(|| WriteError::BadPath(join(&clean)))?;
        let mut k = 1;
        while self.path(&rel)?.exists() || self.collides(&rel) {
            rel = clean_rel(&join(&format!("{base} {k}{ext}")))
                .ok_or_else(|| WriteError::BadPath(rel.clone()))?;
            k += 1;
        }
        let abs = self.path(&rel)?;
        if let Some(d) = abs.parent() {
            std::fs::create_dir_all(d).map_err(io)?;
        }
        let tmp = self.path(&format!("{rel}.tempo-tmp"))?;
        std::fs::write(&tmp, bytes).map_err(io)?;
        std::fs::rename(&tmp, &abs).map_err(io)?;
        self.log_change(origin, "upload", json!({"path": rel, "bytes": bytes.len()}));
        self.reload(&rel);
        Ok(rel)
    }

    fn atomic_write(&self, rel: &str, content: &str) -> R<()> {
        let abs = self.path(rel)?;
        let tmp = self.path(&format!("{rel}.tempo-tmp"))?;
        if let Some(d) = abs.parent() {
            std::fs::create_dir_all(d).map_err(io)?;
        }
        std::fs::write(&tmp, content).map_err(io)?;
        std::fs::rename(&tmp, &abs).map_err(io)
    }

    fn mtime_of(&self, rel: &str) -> Option<f64> {
        let md = std::fs::metadata(self.safe_abs(rel)?).ok()?;
        md.modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs_f64())
    }

    /// Create a new note; refuses to overwrite.
    pub fn create(&mut self, path: &str, content: &str, origin: &str) -> R<String> {
        let rel = doc_path(path)?;
        check_doc(&rel, content)?;
        if self.path(&rel)?.exists() || self.collides(&rel) {
            return Err(WriteError::Exists(rel));
        }
        self.atomic_write(&rel, content)?;
        self.log_change(
            origin,
            "create",
            json!({"path": rel, "bytes": content.len()}),
        );
        self.reload(&rel);
        Ok(rel)
    }

    /// Replace a note's content (creating it if missing). With `expected_mtime`, refuse when the
    /// file changed since the caller read it.
    pub fn write(
        &mut self,
        path: &str,
        content: &str,
        expected_mtime: Option<f64>,
        origin: &str,
    ) -> R<f64> {
        let rel = doc_path(path)?;
        check_doc(&rel, content)?;
        if let (Some(exp), Some(now)) = (expected_mtime, self.mtime_of(&rel)) {
            if (now - exp).abs() > 0.000_5 {
                let cur = self.text_of(&rel).unwrap_or_default();
                return Err(WriteError::Conflict(now, cur));
            }
        }
        self.path(&rel)?;
        let current = self.text_of(&rel);
        if current.as_deref() == Some(content) {
            return Ok(self.mtime_of(&rel).unwrap_or(0.0));
        }
        let snap = self.snapshot(&rel)?;
        self.atomic_write(&rel, content)?;
        self.log_change(
            origin,
            "write",
            json!({"path": rel, "bytes": content.len(), "history": snap}),
        );
        self.reload(&rel);
        Ok(self.mtime_of(&rel).unwrap_or(0.0))
    }

    /// Append text (on its own line) to a note, creating it if missing. With `heading`, the text
    /// goes at the end of that section (the heading is added at the end if missing).
    pub fn append(
        &mut self,
        path: &str,
        text: &str,
        heading: Option<&str>,
        origin: &str,
    ) -> R<String> {
        let rel = note_path(path)?;
        let cur = self
            .path(&rel)
            .map(|_| self.text_of(&rel).unwrap_or_default())?;
        let new = match heading.map(str::trim).filter(|h| !h.is_empty()) {
            Some(h) => insert_under_heading(&cur, h, text),
            None => {
                let mut s = cur.clone();
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(text.trim_end_matches('\n'));
                s.push('\n');
                s
            }
        };
        self.write(&rel, &new, None, origin)?;
        Ok(rel)
    }

    /// Set (or with an empty list, clear) one frontmatter property, keeping the others as written.
    pub fn set_prop(&mut self, path: &str, key: &str, value: &Prop, origin: &str) -> R<()> {
        let rel = note_path(path)?;
        let key = key.trim();
        if key.is_empty() || key.contains([':', '\n']) {
            return Err(WriteError::Invalid(
                "a property name cannot be empty or contain ':'".into(),
            ));
        }
        let cur = self
            .text_of(&rel)
            .ok_or_else(|| WriteError::NotFound(rel.clone()))?;
        let new = set_prop_text(&cur, key, value);
        self.write(&rel, &new, None, origin)?;
        Ok(())
    }

    /// Tick or untick the task on a 1-based line.
    pub fn set_task(&mut self, path: &str, line: usize, done: bool, origin: &str) -> R<()> {
        let rel = note_path(path)?;
        let cur = self
            .text_of(&rel)
            .ok_or_else(|| WriteError::NotFound(rel.clone()))?;
        let p = parse::parse(&cur);
        if !p.tasks.iter().any(|t| t.line == line) {
            return Err(WriteError::Invalid(format!("line {line} is not a task")));
        }
        let mut lines: Vec<String> = cur.split('\n').map(str::to_string).collect();
        let l = &mut lines[line - 1];
        if let Some(i) = l.find('[') {
            let mark = if done { "x" } else { " " };
            l.replace_range(i + 1..i + 2, mark);
        }
        self.write(&rel, &lines.join("\n"), None, origin)?;
        Ok(())
    }

    /// The daily note for `date` (`YYYY-MM-DD`, default today in local time), created from
    /// `<templates>/Daily.md` or the built-in template if missing.
    pub fn daily(&mut self, date: Option<&str>, origin: &str) -> R<String> {
        let cfg = VaultConfig::load(&self.root);
        let offset = util::local_offset_minutes(cfg.utc_offset.as_deref());
        let now = unix_now();
        let date = match date {
            Some(d) => {
                util::parse_date(d)
                    .ok_or_else(|| WriteError::Invalid(format!("not a date (YYYY-MM-DD): {d}")))?;
                d.to_string()
            }
            None => util::date_of(now, offset),
        };
        let folder = cfg.daily_folder.trim_matches('/');
        let rel = if folder.is_empty() {
            format!("{date}.md")
        } else {
            format!("{folder}/{date}.md")
        };
        if self.safe_abs(&rel).is_some_and(|p| p.is_file()) {
            return Ok(rel);
        }
        let tpl_path = cfg
            .daily_template
            .clone()
            .unwrap_or_else(|| format!("{}/Daily.md", cfg.templates_folder));
        let tpl = self
            .text_of(&tpl_path)
            .unwrap_or_else(|| DAILY_TEMPLATE.to_string());
        let vars: BTreeMap<String, String> = [
            ("date", date.clone()),
            ("title", date.clone()),
            ("time", util::time_of(now, offset)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        // date formats refer to the note's own day (noon, so no offset can move it)
        let (y, m, d) = util::parse_date(&date).unwrap_or((1970, 1, 1));
        let noon = util::days_from_civil(y, m, d) as f64 * 86_400.0 + 43_200.0;
        self.create(&rel, &fill_template(&tpl, &vars, (noon, 0)), origin)
    }

    /// A new note from `<templates>/<name>.md`.
    pub fn from_template(&mut self, path: &str, template: &str, origin: &str) -> R<String> {
        let rel = note_path(path)?;
        let cfg = VaultConfig::load(&self.root);
        let tname = note_path(template)?;
        let tpl = self
            .text_of(&format!("{}/{}", cfg.templates_folder, tname))
            .ok_or_else(|| WriteError::NotFound(format!("{}/{}", cfg.templates_folder, tname)))?;
        let offset = util::local_offset_minutes(cfg.utc_offset.as_deref());
        let now = unix_now();
        let vars: BTreeMap<String, String> = [
            ("date", util::date_of(now, offset)),
            ("time", util::time_of(now, offset)),
            ("title", stem(&rel).to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        self.create(&rel, &fill_template(&tpl, &vars, (now, offset)), origin)
    }

    /// Move to `.trash/<YYYYMMDD>/<path>` (never deleted). Returns the trash location.
    pub fn trash(&mut self, path: &str, origin: &str) -> R<String> {
        let rel = clean_rel(path).ok_or_else(|| WriteError::BadPath(path.to_string()))?;
        let abs = self.path(&rel)?;
        if !abs.exists() {
            return Err(WriteError::NotFound(rel));
        }
        let day = util::date_of(unix_now(), 0).replace('-', "");
        let mut dest = format!(".trash/{day}/{rel}");
        let mut n = 1;
        while self.path(&dest)?.exists() {
            dest = format!(".trash/{day}/{}-{n}{}", strip_ext(&rel), ext_of(&rel));
            n += 1;
        }
        let dabs = self.path(&dest)?;
        if let Some(p) = dabs.parent() {
            std::fs::create_dir_all(p).map_err(io)?;
        }
        std::fs::rename(&abs, &dabs).map_err(io)?;
        self.log_change(origin, "trash", json!({"path": rel, "to": dest}));
        self.reload(&rel);
        Ok(dest)
    }

    /// Put a snapshot back (the current text is snapshotted first).
    pub fn restore(&mut self, path: &str, id: &str, origin: &str) -> R<()> {
        let rel = note_path(path)?;
        let text = self
            .history_text(&rel, id)
            .ok_or_else(|| WriteError::NotFound(format!("{rel} @ {id}")))?;
        self.write(&rel, &text, None, origin)?;
        self.log_change(origin, "restore", json!({"path": rel, "from": id}));
        Ok(())
    }

    /// Rename or move a note or attachment and rewrite every link to it across the vault. Returns
    /// the notes whose text changed (each snapshotted first).
    pub fn rename(&mut self, from: &str, to: &str, origin: &str) -> R<Vec<String>> {
        let old = clean_rel(from).ok_or_else(|| WriteError::BadPath(from.to_string()))?;
        let is_note = self.note(&old).is_some();
        let new = if is_note {
            note_path(to)?
        } else {
            clean_rel(to).ok_or_else(|| WriteError::BadPath(to.to_string()))?
        };
        if !is_note && !self.has_file(&old) {
            return Err(WriteError::NotFound(old));
        }
        if old == new {
            return Ok(vec![]);
        }
        let case_only = old.eq_ignore_ascii_case(&new);
        if !case_only && (self.path(&new)?.exists() || self.collides(&new)) {
            return Err(WriteError::Exists(new));
        }
        // Every link that points at `old` now, grouped by note, before anything moves.
        let mut edits: BTreeMap<String, Vec<(usize, usize, String)>> = BTreeMap::new();
        let sources: Vec<String> = self.notes().map(|n| n.path.clone()).collect();
        for src in &sources {
            for (l, r) in self.links_of(src) {
                if r.as_deref() != Some(old.as_str()) || l.target.is_empty() {
                    continue;
                }
                let src_after = if *src == old {
                    new.clone()
                } else {
                    src.clone()
                };
                let text = &l.target;
                let replacement = if l.wiki {
                    let keep_md = is_md(text);
                    let base = if text.contains('/') || self.stem_is_ambiguous_after(&old, &new) {
                        strip_md(&new).to_string()
                    } else {
                        strip_md(file_name(&new)).to_string()
                    };
                    if keep_md || !is_note {
                        if is_note {
                            format!("{base}.md")
                        } else if text.contains('/') {
                            new.clone()
                        } else {
                            file_name(&new).to_string()
                        }
                    } else {
                        base
                    }
                } else {
                    let rel_link = relative_path(parent_of(&src_after), &new);
                    let shown = if text.contains('/') || parent_of(&src_after) != parent_of(&new) {
                        rel_link
                    } else {
                        file_name(&new).to_string()
                    };
                    let shown = if is_md(text) || !is_note {
                        shown
                    } else {
                        strip_md(&shown).to_string()
                    };
                    pct_encode_path(&shown)
                };
                edits.entry(src.clone()).or_default().push((
                    l.target_span.0,
                    l.target_span.1,
                    replacement,
                ));
            }
        }
        // The moved note's own relative Markdown links, if its folder changes.
        if is_note && parent_of(&old) != parent_of(&new) {
            for (l, r) in self.links_of(&old) {
                if l.wiki
                    || l.target.is_empty()
                    || !l.target.contains('/') && !l.target.starts_with('.')
                {
                    continue;
                }
                if let Some(t) = r.filter(|t| *t != old) {
                    let shown = relative_path(parent_of(&new), &t);
                    let shown = if is_md(&l.target) || !is_md(&t) {
                        shown
                    } else {
                        strip_md(&shown).to_string()
                    };
                    let e = edits.entry(old.clone()).or_default();
                    if !e.iter().any(|x| x.0 == l.target_span.0) {
                        e.push((l.target_span.0, l.target_span.1, pct_encode_path(&shown)));
                    }
                }
            }
        }
        // Move first (snapshotting nothing: the bytes are unchanged), then rewrite.
        let (oabs, nabs) = (self.path(&old)?, self.path(&new)?);
        if let Some(p) = nabs.parent() {
            std::fs::create_dir_all(p).map_err(io)?;
        }
        if case_only {
            let tmp = self.path(&format!("{old}.tempo-tmp"))?;
            std::fs::rename(&oabs, &tmp).map_err(io)?;
            std::fs::rename(&tmp, &nabs).map_err(io)?;
        } else {
            std::fs::rename(&oabs, &nabs).map_err(io)?;
        }
        // History follows the note.
        let hist = |r: &str| self.path(&format!(".tempo/history/{r}"));
        if let (Ok(oh), Ok(nh)) = (hist(&old), hist(&new)) {
            if oh.is_dir() && !nh.exists() {
                if let Some(p) = nh.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                let _ = std::fs::rename(&oh, &nh);
            }
        }
        let mut changed = Vec::new();
        for (src, mut list) in edits {
            let target = if src == old { new.clone() } else { src.clone() };
            let Some(text) = self.text_of(&target) else {
                continue;
            };
            list.sort_by_key(|x| std::cmp::Reverse(x.0));
            let mut t = text.clone();
            for (a, b, rep) in list {
                if b <= t.len() && t.is_char_boundary(a) && t.is_char_boundary(b) {
                    t.replace_range(a..b, &rep);
                }
            }
            if t != text {
                let snap = self.snapshot(&target)?;
                self.atomic_write(&target, &t)?;
                self.log_change(origin, "relink", json!({"path": target, "history": snap}));
                changed.push(target);
            }
        }
        self.log_change(
            origin,
            "rename",
            json!({"path": old, "to": new, "relinked": changed.len()}),
        );
        self.refresh();
        Ok(changed)
    }

    /// Whether, after the rename, the new bare file name would also match another note.
    fn stem_is_ambiguous_after(&self, old: &str, new: &str) -> bool {
        let s = stem(new).to_lowercase();
        self.notes()
            .any(|n| n.path != old && stem(&n.path).to_lowercase() == s)
    }
}

fn strip_ext(rel: &str) -> &str {
    match file_name(rel).rfind('.') {
        Some(i) if i > 0 => &rel[..rel.len() - (file_name(rel).len() - i)],
        _ => rel,
    }
}

fn ext_of(rel: &str) -> &str {
    &rel[strip_ext(rel).len()..]
}

/// Insert `text` at the end of the section under the first heading named `heading` (any level).
pub fn insert_under_heading(cur: &str, heading: &str, text: &str) -> String {
    let p = parse::parse(cur);
    let want = heading.trim_start_matches('#').trim().to_lowercase();
    let body = text.trim_end_matches('\n');
    let Some(h) = p.headings.iter().find(|h| h.text.to_lowercase() == want) else {
        let mut s = cur.to_string();
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        if !s.is_empty() && !s.ends_with("\n\n") {
            s.push('\n');
        }
        s.push_str(&format!(
            "## {}\n{body}\n",
            heading.trim_start_matches('#').trim()
        ));
        return s;
    };
    let next = p
        .headings
        .iter()
        .find(|x| x.line > h.line && x.level <= h.level)
        .map(|x| x.line);
    let mut lines: Vec<&str> = cur.split('\n').collect();
    let trailing_newline = cur.ends_with('\n');
    if trailing_newline {
        lines.pop();
    }
    let end = next.map_or(lines.len(), |n| n - 1);
    // the section ends at the last non-blank line before `end`
    let mut at = end;
    while at > h.line && lines[at - 1].trim().is_empty() {
        at -= 1;
    }
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    out.insert(at, body.to_string());
    let mut s = out.join("\n");
    if trailing_newline || next.is_none() {
        s.push('\n');
    }
    s
}

fn yaml_value(v: &Prop) -> String {
    let q = |s: &str| {
        if s.is_empty()
            || s.contains([':', '#', '[', ']', '{', '}', ',', '"', '\''])
            || s.trim() != s
        {
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            s.to_string()
        }
    };
    match v {
        Prop::Text(t) => q(t),
        Prop::List(l) => format!(
            "[{}]",
            l.iter().map(|s| q(s)).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Rewrite or add one property in the frontmatter, leaving everything else byte-for-byte.
pub fn set_prop_text(cur: &str, key: &str, value: &Prop) -> String {
    let clear = matches!(value, Prop::List(l) if l.is_empty());
    let line = format!("{key}: {}", yaml_value(value));
    let Some((ys, ye, _)) = parse::split_frontmatter(cur) else {
        return if clear {
            cur.to_string()
        } else {
            format!("---\n{line}\n---\n{cur}")
        };
    };
    let yaml = &cur[ys..ye];
    let mut out: Vec<String> = Vec::new();
    let mut lines = yaml.lines().peekable();
    let mut done = false;
    while let Some(l) = lines.next() {
        let is_key = !l.starts_with([' ', '\t', '-'])
            && l.split_once(':')
                .is_some_and(|(k, _)| k.trim().trim_matches('"').eq_ignore_ascii_case(key));
        if is_key {
            // drop the old value, including an indented block list
            while lines
                .peek()
                .is_some_and(|n| n.starts_with([' ', '\t']) || n.trim_start().starts_with("- "))
            {
                lines.next();
            }
            if !clear && !done {
                out.push(line.clone());
            }
            done = true;
        } else {
            out.push(l.to_string());
        }
    }
    if !done && !clear {
        out.push(line);
    }
    let mut y = out.join("\n");
    if !y.is_empty() {
        y.push('\n');
    }
    format!("{}{}{}", &cur[..ys], y, &cur[ye..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::{put, sample, tmp};

    #[test]
    fn create_write_conflict_history_restore() {
        let d = tmp("write");
        let mut v = Vault::open(&d).unwrap();
        assert_eq!(
            v.create("Notes/First", "one", "test").unwrap(),
            "Notes/First.md"
        );
        assert!(matches!(
            v.create("Notes/First.md", "x", "test"),
            Err(WriteError::Exists(_))
        ));
        for bad in ["../x.md", "/abs.md", ".tempo/x.md", "a/../../b"] {
            assert!(
                matches!(v.create(bad, "x", "test"), Err(WriteError::BadPath(_))),
                "{bad}"
            );
        }
        let m1 = v.write("Notes/First.md", "two", None, "test").unwrap();
        assert!(
            matches!(v.write("Notes/First.md", "stale", Some(m1 - 100.0), "test"), Err(WriteError::Conflict(_, ref c)) if c == "two")
        );
        v.write("Notes/First.md", "three", Some(m1), "test")
            .unwrap();
        let h = v.history("Notes/First.md");
        assert_eq!(h.len(), 2, "one snapshot per change: {h:?}");
        assert_eq!(
            v.history_text("Notes/First.md", &h[1]).as_deref(),
            Some("one")
        );
        assert_eq!(v.history_text("Notes/First.md", "../../x.md"), None);
        v.restore("Notes/First.md", &h[1], "test").unwrap();
        assert_eq!(v.read("Notes/First.md").as_deref(), Some("one"));
        assert_eq!(
            v.history("Notes/First.md").len(),
            3,
            "restoring snapshots the current text first"
        );
        assert!(
            v.write("Notes/First.md", "one", None, "test").unwrap() > 0.0,
            "an identical write is a no-op"
        );
        assert_eq!(v.history("Notes/First.md").len(), 3);
        let log = std::fs::read_to_string(d.join(".tempo/changes.jsonl")).unwrap();
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("\"op\":\"write\""))
                .count(),
            3
        );
        assert!(!d.join("Notes/First.md.tempo-tmp").exists());
    }

    #[test]
    fn append_heading_prop_task() {
        let d = tmp("edit");
        let mut v = Vault::open(&d).unwrap();
        v.append("Log", "first", None, "t").unwrap();
        v.append("Log", "second", None, "t").unwrap();
        assert_eq!(v.read("Log.md").as_deref(), Some("first\nsecond\n"));
        put(&d, "S.md", "# Top\nintro\n## A\na1\n\n## B\nb1\n");
        v.refresh();
        v.append("S", "a2", Some("A"), "t").unwrap();
        v.append("S", "c1", Some("C"), "t").unwrap();
        assert_eq!(
            v.read("S.md").unwrap(),
            "# Top\nintro\n## A\na1\na2\n\n## B\nb1\n\n## C\nc1\n"
        );
        assert_eq!(
            insert_under_heading("# Top\nx", "Top", "y"),
            "# Top\nx\ny\n"
        );
        v.set_prop("S", "status", &Prop::Text("active".into()), "t")
            .unwrap();
        assert!(v
            .read("S.md")
            .unwrap()
            .starts_with("---\nstatus: active\n---\n# Top"));
        v.set_prop(
            "S",
            "tags",
            &Prop::List(vec!["a".into(), "b c".into()]),
            "t",
        )
        .unwrap();
        v.set_prop("S", "status", &Prop::Text("done: yes".into()), "t")
            .unwrap();
        let t = v.read("S.md").unwrap();
        assert!(
            t.starts_with("---\nstatus: \"done: yes\"\ntags: [a, b c]\n---\n"),
            "{t}"
        );
        assert_eq!(
            v.note("S.md").unwrap().parsed.tags,
            ["a", "b", "c"],
            "tags cannot hold spaces, as in Obsidian"
        );
        v.set_prop("S", "tags", &Prop::List(vec![]), "t").unwrap();
        assert!(!v.read("S.md").unwrap().contains("tags"));
        assert_eq!(
            set_prop_text(
                "---\nlist:\n  - a\n  - b\nkeep: 1\n---\nbody",
                "list",
                &Prop::Text("x".into())
            ),
            "---\nlist: x\nkeep: 1\n---\nbody"
        );
        put(&d, "T.md", "- [ ] one\n- [x] two\n");
        v.refresh();
        v.set_task("T", 1, true, "t").unwrap();
        v.set_task("T", 2, false, "t").unwrap();
        assert_eq!(v.read("T.md").unwrap(), "- [x] one\n- [ ] two\n");
        assert!(matches!(
            v.set_task("T", 3, true, "t"),
            Err(WriteError::Invalid(_))
        ));
    }

    #[test]
    fn daily_and_templates() {
        let d = tmp("daily");
        let mut v = Vault::open(&d).unwrap();
        let p = v.daily(Some("2026-11-06"), "t").unwrap();
        assert_eq!(p, "Daily/2026-11-06.md");
        assert!(v.read(&p).unwrap().starts_with("# 2026-11-06\n"));
        assert_eq!(
            v.daily(Some("2026-11-06"), "t").unwrap(),
            p,
            "an existing daily note is reused"
        );
        assert!(matches!(
            v.daily(Some("2026-02-30"), "t"),
            Err(WriteError::Invalid(_))
        ));
        put(&d, "Templates/Meeting.md", "# {{title}}\nDate: {{date}}\n");
        put(
            &d,
            ".tempo/config.json",
            r#"{"daily_folder": "Journal", "utc_offset": "-05:00"}"#,
        );
        v.refresh();
        let m = v.from_template("Meetings/Standup", "Meeting", "t").unwrap();
        assert!(v.read(&m).unwrap().starts_with("# Standup\nDate: 20"));
        assert!(v.daily(None, "t").unwrap().starts_with("Journal/"));
        // an Obsidian vault's own daily-note settings and moment formats
        put(&d, ".tempo/config.json", "{}");
        put(
            &d,
            ".obsidian/daily-notes.json",
            r#"{"folder": "Journal/Days", "template": "Templates/Day"}"#,
        );
        put(&d, "Templates/Day.md", "# {{date:dddd D MMMM YYYY}}\n");
        v.refresh();
        let p = v.daily(Some("2026-11-06"), "t").unwrap();
        assert_eq!(p, "Journal/Days/2026-11-06.md");
        assert_eq!(v.read(&p).unwrap(), "# Friday 6 November 2026\n");
        assert!(matches!(
            v.from_template("X", "Nope", "t"),
            Err(WriteError::NotFound(_))
        ));
    }

    #[test]
    fn attachments_land_in_obsidians_folder_under_free_names() {
        let d = tmp("attach");
        let mut v = Vault::open(&d).unwrap();
        assert_eq!(
            v.attachment_folder(Some("Notes/A.md")),
            "Attachments",
            "Tempo's default"
        );
        let p = v
            .add_file("Attachments", "photo.png", b"PNG1", "t")
            .unwrap();
        assert_eq!(p, "Attachments/photo.png");
        let p2 = v
            .add_file("Attachments", "photo.png", b"PNG2", "t")
            .unwrap();
        assert_eq!(p2, "Attachments/photo 1.png", "never overwrites");
        assert_eq!(
            std::fs::read(d.join("Attachments/photo.png")).unwrap(),
            b"PNG1"
        );
        assert!(v.has_file(&p2));
        assert_eq!(
            v.add_file("", "../../etc/x?.png", b"x", "t").unwrap(),
            "x .png",
            "names are cleaned to one file name"
        );
        for (setting, want) in [
            ("/", ""),
            ("./", "Notes"),
            ("./img", "Notes/img"),
            ("Assets/Media", "Assets/Media"),
        ] {
            put(
                &d,
                ".obsidian/app.json",
                &format!("{{\"attachmentFolderPath\": \"{setting}\"}}"),
            );
            assert_eq!(v.attachment_folder(Some("Notes/A.md")), want, "{setting}");
        }
    }

    #[test]
    fn trash_never_deletes() {
        let (d, mut v) = sample();
        let to = v.trash("Ideas.md", "t").unwrap();
        assert!(to.starts_with(".trash/") && to.ends_with("/Ideas.md"));
        assert!(d.join(&to).is_file() && !d.join("Ideas.md").exists());
        let home: Vec<Option<String>> = v.links_of("Home.md").into_iter().map(|(_, r)| r).collect();
        assert_eq!(
            home[1].as_deref(),
            Some("Archive/Ideas.md"),
            "[[Ideas]] now finds the other Ideas note"
        );
        v.trash("Archive/Ideas.md", "t").unwrap();
        assert!(
            v.unresolved().iter().any(|(_, t, _)| t == "Ideas"),
            "with both gone the link is unresolved"
        );
        put(&d, "Ideas.md", "again");
        v.refresh();
        let to2 = v.trash("Ideas.md", "t").unwrap();
        assert_ne!(to, to2, "a second trash of the same name gets a new file");
        assert!(to2.ends_with("Ideas-1.md"));
        assert!(matches!(
            v.trash("Nope.md", "t"),
            Err(WriteError::NotFound(_))
        ));
        assert!(matches!(v.trash("../x", "t"), Err(WriteError::BadPath(_))));
    }

    #[test]
    fn rename_rewrites_every_link() {
        let (d, mut v) = sample();
        let changed = v.rename("Ideas.md", "Thinking/Big Ideas.md", "t").unwrap();
        assert_eq!(changed, ["Home.md", "Projects/Tempo.md"]);
        assert_eq!(
            v.read("Home.md").unwrap(),
            "# Home\nStart at [[Projects/Tempo]] and [[Big Ideas]]. Also [[Missing note]].\n#hub\n"
        );
        assert!(
            v.read("Projects/Tempo.md")
                .unwrap()
                .contains("[[Big Ideas#Next]]"),
            "headings are kept"
        );
        assert_eq!(v.backlinks("Thinking/Big Ideas.md").len(), 2);
        assert!(v.note("Ideas.md").is_none());
        assert!(
            v.unresolved().iter().all(|(_, t, _)| t == "Missing note"),
            "{:?}",
            v.unresolved()
        );
        // Archive/Index.md links to Archive/Ideas.md, not to the renamed note: untouched.
        assert!(v.read("Archive/Index.md").unwrap().starts_with("[[Ideas]]"));
        // A relative Markdown link from a note that moves folders is recomputed.
        let changed = v
            .rename("Archive/Index.md", "Deep/Er/Index.md", "t")
            .unwrap();
        assert_eq!(changed, ["Deep/Er/Index.md"]);
        assert_eq!(
            v.read("Deep/Er/Index.md").unwrap(),
            "[[Ideas]] (same folder wins) and [Tempo](../../Projects/Tempo.md)\n"
        );
        // Markdown links pointing at a renamed note get a correct relative path.
        let changed = v
            .rename("Projects/Tempo.md", "Projects/Tempo Core.md", "t")
            .unwrap();
        assert!(
            changed.contains(&"Deep/Er/Index.md".to_string())
                && changed.contains(&"Home.md".to_string())
        );
        assert!(v
            .read("Deep/Er/Index.md")
            .unwrap()
            .contains("[Tempo](../../Projects/Tempo%20Core.md)"));
        assert!(
            v.read("Home.md")
                .unwrap()
                .contains("[[Projects/Tempo Core]]"),
            "a path-style wikilink keeps its path style"
        );
        // Attachments too.
        let changed = v.rename("img/logo.png", "img/brand.png", "t").unwrap();
        assert_eq!(changed, ["Projects/Tempo Core.md"]);
        assert!(v
            .read("Projects/Tempo Core.md")
            .unwrap()
            .contains("![[img/brand.png]]"));
        assert!(v.unresolved().iter().all(|(_, t, _)| t == "Missing note"));
        // Refusals.
        assert!(matches!(
            v.rename("Home.md", "Thinking/Big Ideas.md", "t"),
            Err(WriteError::Exists(_))
        ));
        assert!(matches!(
            v.rename("Nope.md", "X.md", "t"),
            Err(WriteError::NotFound(_))
        ));
        assert!(
            !v.history("Home.md").is_empty(),
            "relinked notes were snapshotted"
        );
        let _ = d;
    }

    #[test]
    fn rename_keeps_names_unique() {
        let d = tmp("ambig");
        put(&d, "A.md", "[[Target]]");
        put(&d, "Target.md", "t");
        put(&d, "Other/Thing.md", "other");
        let mut v = Vault::open(&d).unwrap();
        v.rename("Target.md", "New/Thing.md", "t").unwrap();
        assert_eq!(
            v.read("A.md").unwrap(),
            "[[New/Thing]]",
            "Thing alone would be ambiguous"
        );
        assert_eq!(v.links_of("A.md")[0].1.as_deref(), Some("New/Thing.md"));
        v.rename("New/Thing.md", "New/thing.md", "t").unwrap();
        assert!(v.note("New/thing.md").is_some(), "a case-only rename works");
    }
}
