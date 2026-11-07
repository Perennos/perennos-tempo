//! The vault: a folder of Markdown notes and attachments (an Obsidian vault works as is), indexed
//! for link resolution, backlinks, unresolved links, tags, tasks, unlinked mentions and search.
//! Hidden folders (`.obsidian`, `.trash`, `.tempo`, `.git`) and symlinks are never walked.
use crate::parse::{self, Link, Parsed};
use crate::search::{self, Hit, SearchIndex};
use crate::util::{file_name, fnv64, is_md, join_rel, parent_of, stem, strip_md};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Notes larger than this are listed but not parsed.
pub const MAX_NOTE_BYTES: u64 = 8 << 20;
/// Walking stops after this many files (a vault this large is almost certainly the wrong folder).
pub const MAX_FILES: usize = 200_000;
const MAX_DEPTH: usize = 40;

#[derive(Debug, Clone)]
pub struct Note {
    pub path: String,
    pub title: String,
    pub parsed: Parsed,
    pub mtime: f64,
    pub size: u64,
    pub hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FileStat {
    pub mtime: f64,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlink {
    pub source: String,
    pub line: usize,
    pub embed: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Refresh {
    pub parsed: usize,
    pub removed: usize,
    pub attachments: usize,
    /// Paths that appeared, changed or went away (notes and attachments).
    pub changed: Vec<String>,
}

pub struct Vault {
    pub root: PathBuf,
    notes: BTreeMap<String, Note>,
    files: BTreeMap<String, FileStat>,
    by_stem: HashMap<String, Vec<String>>,
    by_alias: HashMap<String, Vec<String>>,
    by_path: HashMap<String, String>,
    files_by_name: HashMap<String, Vec<String>>,
    files_by_path: HashMap<String, String>,
    resolved: HashMap<String, Vec<Option<String>>>,
    backlinks: HashMap<String, Vec<Backlink>>,
    /// What each `.canvas` links to, kept with the file's stat so it is read again only when it changes.
    canvases: HashMap<String, (FileStat, Vec<Link>)>,
    pub search: SearchIndex,
    pub warnings: Vec<String>,
}

fn stat_of(md: &std::fs::Metadata) -> FileStat {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    FileStat {
        mtime,
        size: md.len(),
    }
}

/// A symlink, or on Windows any reparse point (junctions, mount points).
pub fn is_link(md: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    md.file_type().is_symlink()
}

/// Every visible file under `root` as (vault-relative path, stat). Hidden entries, symlinks and
/// names that are not valid UTF-8 are skipped (the latter with a warning).
pub fn walk(root: &Path) -> (Vec<(String, FileStat)>, Vec<String>) {
    let mut out = Vec::new();
    let mut warn = Vec::new();
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            warn.push(format!(
                "cannot read folder {}",
                if rel.is_empty() { "." } else { &rel }
            ));
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                warn.push(format!(
                    "skipped a name that is not UTF-8 in {}",
                    if rel.is_empty() { "." } else { &rel }
                ));
                continue;
            };
            if name.starts_with('.') || name == "node_modules" || name.ends_with(".tempo-tmp") {
                continue;
            }
            let Ok(md) = e.path().symlink_metadata() else {
                continue;
            };
            let r = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if is_link(&md) {
                continue;
            }
            if md.is_dir() {
                if depth + 1 < MAX_DEPTH {
                    stack.push((e.path(), r, depth + 1));
                }
            } else if md.is_file() {
                if out.len() >= MAX_FILES {
                    warn.push(format!("stopped after {MAX_FILES} files"));
                    return (out, warn);
                }
                out.push((r, stat_of(&md)));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    (out, warn)
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

impl Vault {
    /// Open (and index) an existing folder.
    pub fn open(root: impl Into<PathBuf>) -> Result<Vault, String> {
        let root = root.into();
        if !root.is_dir() {
            return Err(format!("{} is not a folder", root.display()));
        }
        let mut v = Vault {
            root,
            notes: BTreeMap::new(),
            files: BTreeMap::new(),
            by_stem: HashMap::new(),
            by_alias: HashMap::new(),
            by_path: HashMap::new(),
            files_by_name: HashMap::new(),
            files_by_path: HashMap::new(),
            resolved: HashMap::new(),
            backlinks: HashMap::new(),
            canvases: HashMap::new(),
            search: SearchIndex::default(),
            warnings: vec![],
        };
        v.refresh();
        Ok(v)
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        rel.split('/').fold(self.root.clone(), |p, s| p.join(s))
    }

    /// Like `abs`, but `None` when any existing part of the path below the vault root is a
    /// symlink or junction. Reads and writes must never follow one out of the vault.
    pub fn safe_abs(&self, rel: &str) -> Option<PathBuf> {
        let mut p = self.root.clone();
        for seg in rel.split('/') {
            p.push(seg);
            match p.symlink_metadata() {
                Ok(md) if is_link(&md) => return None,
                Ok(_) => {}
                Err(_) => break, // the rest does not exist yet
            }
        }
        Some(self.abs(rel))
    }

    /// Re-read what changed on disk (by mtime and size) and rebuild the derived maps if anything did.
    pub fn refresh(&mut self) -> Refresh {
        let (all, warn) = walk(&self.root);
        self.warnings = warn;
        let mut r = Refresh::default();
        let mut seen_notes = HashSet::new();
        let mut files = BTreeMap::new();
        for (rel, st) in all {
            if !is_md(&rel) {
                files.insert(rel, st);
                continue;
            }
            seen_notes.insert(rel.clone());
            let unchanged = self
                .notes
                .get(&rel)
                .is_some_and(|n| n.mtime == st.mtime && n.size == st.size);
            if unchanged {
                continue;
            }
            self.load_note(&rel, st);
            r.parsed += 1;
            r.changed.push(rel);
        }
        let gone: Vec<String> = self
            .notes
            .keys()
            .filter(|k| !seen_notes.contains(*k))
            .cloned()
            .collect();
        for g in &gone {
            self.notes.remove(g);
            self.search.remove(g);
        }
        r.removed = gone.len();
        r.changed.extend(gone);
        for (p, st) in &files {
            if self.files.get(p) != Some(st) {
                r.changed.push(p.clone());
            }
        }
        r.changed.extend(
            self.files
                .keys()
                .filter(|p| !files.contains_key(*p))
                .cloned(),
        );
        let files_changed = files != self.files;
        r.attachments = files.len();
        self.files = files;
        if r.parsed > 0 || r.removed > 0 || files_changed || self.by_path.len() != self.notes.len()
        {
            self.rebuild();
        }
        r
    }

    /// (Re)load one note from disk, e.g. right after writing it.
    pub fn load_note(&mut self, rel: &str, st: FileStat) {
        let abs = self.abs(rel);
        let (content, parsed) = if st.size > MAX_NOTE_BYTES {
            self.warnings.push(format!(
                "{rel} is larger than {} MB; listed but not indexed",
                MAX_NOTE_BYTES >> 20
            ));
            (String::new(), Parsed::default())
        } else {
            match std::fs::read(&abs) {
                Ok(b) => {
                    let s = String::from_utf8_lossy(&b);
                    let s = s.strip_prefix('\u{feff}').unwrap_or(&s).to_string();
                    let p = parse::parse(&s);
                    (s, p)
                }
                Err(e) => {
                    self.warnings.push(format!("cannot read {rel}: {e}"));
                    (String::new(), Parsed::default())
                }
            }
        };
        // a template's heading like `# {{title}}` is not a title
        let title = parsed
            .title
            .clone()
            .filter(|t| !t.contains("{{"))
            .unwrap_or_else(|| stem(rel).to_string());
        let headings: Vec<&str> = parsed.headings.iter().map(|h| h.text.as_str()).collect();
        let body = &content[parsed.body_start.min(content.len())..];
        self.search.upsert(
            rel,
            &[
                (&title, 3.0),
                (stem(rel), 3.0),
                (&parsed.aliases.join(" "), 3.0),
                (&headings.join(" "), 2.0),
                (&parsed.tags.join(" "), 2.0),
                (body, 1.0),
            ],
        );
        self.notes.insert(
            rel.to_string(),
            Note {
                path: rel.to_string(),
                title,
                mtime: st.mtime,
                size: st.size,
                hash: fnv64(content.as_bytes()),
                parsed,
            },
        );
    }

    /// The vault as a reader of only these notes sees it (every attachment stays): their links,
    /// backlinks, search, embeds and query results know nothing of the other notes. Publishing
    /// renders through this, so a published page cannot show text from an unpublished note.
    pub fn only(&self, keep: &std::collections::BTreeSet<String>) -> Vault {
        let mut v = Vault {
            root: self.root.clone(),
            notes: BTreeMap::new(),
            files: self.files.clone(),
            by_stem: HashMap::new(),
            by_alias: HashMap::new(),
            by_path: HashMap::new(),
            files_by_name: HashMap::new(),
            files_by_path: HashMap::new(),
            resolved: HashMap::new(),
            backlinks: HashMap::new(),
            canvases: HashMap::new(),
            search: SearchIndex::default(),
            warnings: vec![],
        };
        for (p, n) in self.notes.iter().filter(|(p, _)| keep.contains(*p)) {
            v.load_note(
                p,
                FileStat {
                    mtime: n.mtime,
                    size: n.size,
                },
            );
        }
        v.rebuild();
        v
    }

    /// Re-stat and reload one note after a write (cheaper than a full refresh).
    pub fn reload(&mut self, rel: &str) {
        match self.safe_abs(rel).map(std::fs::metadata) {
            Some(Ok(md)) if md.is_file() && is_md(rel) => {
                self.load_note(rel, stat_of(&md));
                self.rebuild();
            }
            Some(Ok(md)) if md.is_file() => {
                self.files.insert(rel.to_string(), stat_of(&md));
                self.rebuild();
            }
            _ => {
                self.notes.remove(rel);
                self.files.remove(rel);
                self.search.remove(rel);
                self.rebuild();
            }
        }
    }

    fn rebuild(&mut self) {
        self.by_stem.clear();
        self.by_alias.clear();
        self.by_path.clear();
        self.files_by_name.clear();
        self.files_by_path.clear();
        for (p, n) in &self.notes {
            self.by_stem
                .entry(lower(stem(p)))
                .or_default()
                .push(p.clone());
            self.by_path.insert(lower(strip_md(p)), p.clone());
            for a in &n.parsed.aliases {
                self.by_alias.entry(lower(a)).or_default().push(p.clone());
            }
        }
        for p in self.files.keys() {
            self.files_by_name
                .entry(lower(file_name(p)))
                .or_default()
                .push(p.clone());
            self.files_by_path.insert(lower(p), p.clone());
        }
        let mut resolved = HashMap::new();
        let mut backlinks: HashMap<String, Vec<Backlink>> = HashMap::new();
        for (p, n) in &self.notes {
            let r: Vec<Option<String>> =
                n.parsed.links.iter().map(|l| self.resolve(p, l)).collect();
            for (l, t) in n.parsed.links.iter().zip(&r) {
                if let Some(t) = t {
                    if t != p {
                        backlinks.entry(t.clone()).or_default().push(Backlink {
                            source: p.clone(),
                            line: l.line,
                            embed: l.embed,
                        });
                    }
                }
            }
            resolved.insert(p.clone(), r);
        }
        // a canvas links to the notes and files on it: they list it among their backlinks
        let mut canvases = std::mem::take(&mut self.canvases);
        canvases.retain(|p, (st, _)| self.files.get(p) == Some(st));
        for (p, st) in self
            .files
            .iter()
            .filter(|(p, _)| crate::render::ext(p) == "canvas")
        {
            if !canvases.contains_key(p) {
                let links = self
                    .read_text(p)
                    .and_then(|t| crate::canvas::parse(&t).ok())
                    .map(|c| crate::canvas::links(&c))
                    .unwrap_or_default();
                canvases.insert(p.clone(), (*st, links));
            }
        }
        for (p, (_, links)) in &canvases {
            for l in links {
                // a file card names a vault path exactly; a [[link]] in a text card resolves as usual
                let exact = (self.notes.contains_key(&l.target)
                    || self.files.contains_key(&l.target))
                .then(|| l.target.clone());
                if let Some(t) = exact.or_else(|| self.resolve(p, l)) {
                    let list = backlinks.entry(t).or_default();
                    if !list.iter().any(|b| b.source == *p) {
                        list.push(Backlink {
                            source: p.clone(),
                            line: 0,
                            embed: l.embed,
                        });
                    }
                }
            }
        }
        self.canvases = canvases;
        self.resolved = resolved;
        self.backlinks = backlinks;
    }

    /// The canvases that show this note or file (on a card or in a card's text).
    pub fn canvases_with(&self, rel: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .backlinks(rel)
            .into_iter()
            .map(|b| b.source)
            .filter(|s| crate::render::ext(s) == "canvas")
            .collect();
        out.dedup();
        out
    }

    fn exact(&self, rel: &str) -> Option<String> {
        let l = lower(rel);
        self.by_path
            .get(&lower(strip_md(rel)))
            .filter(|_| is_md(rel) || !self.files_by_path.contains_key(&l))
            .cloned()
            .or_else(|| self.files_by_path.get(&l).cloned())
    }

    /// Prefer the candidate in the source's folder, then the shortest path, then alphabetical.
    fn pick(from_dir: &str, cands: &[String]) -> Option<String> {
        cands
            .iter()
            .min_by_key(|c| {
                (
                    parent_of(c) != from_dir,
                    c.matches('/').count(),
                    c.to_lowercase(),
                )
            })
            .cloned()
    }

    /// Where a link from note `from` points, Obsidian-style; `None` when unresolved.
    pub fn resolve(&self, from: &str, link: &Link) -> Option<String> {
        let t = link.target.trim().replace('\\', "/");
        if t.is_empty() {
            return self.notes.contains_key(from).then(|| from.to_string());
        }
        let dir = parent_of(from);
        let t = t.trim_start_matches('/').to_string();
        if !link.wiki || t.starts_with("./") || t.starts_with("../") {
            if let Some(p) = join_rel(dir, &t).and_then(|j| self.exact(&j)) {
                return Some(p);
            }
        }
        let lt = lower(strip_md(&t));
        if !t.contains('/') {
            // a bare name: a note in the linking note's own folder wins
            if let Some(p) = self
                .by_stem
                .get(&lt)
                .and_then(|c| c.iter().find(|c| parent_of(c) == dir))
            {
                return Some(p.clone());
            }
        }
        if let Some(p) = self.exact(&t) {
            return Some(p);
        }
        if t.contains('/') {
            let suffix = format!("/{lt}");
            let notes: Vec<String> = self
                .by_path
                .iter()
                .filter(|(k, _)| k.ends_with(&suffix))
                .map(|(_, v)| v.clone())
                .collect();
            if let Some(p) = Self::pick(dir, &notes) {
                return Some(p);
            }
            let lf = lower(&t);
            let fsuf = format!("/{lf}");
            let files: Vec<String> = self
                .files_by_path
                .iter()
                .filter(|(k, _)| k.ends_with(&fsuf))
                .map(|(_, v)| v.clone())
                .collect();
            return Self::pick(dir, &files);
        }
        if let Some(c) = self.by_stem.get(&lt) {
            return Self::pick(dir, c);
        }
        if let Some(c) = self.by_alias.get(&lower(&t)) {
            return Self::pick(dir, c);
        }
        self.files_by_name
            .get(&lower(&t))
            .and_then(|c| Self::pick(dir, c))
    }

    // ---- read access

    pub fn notes(&self) -> impl Iterator<Item = &Note> {
        self.notes.values()
    }

    pub fn note(&self, rel: &str) -> Option<&Note> {
        self.notes.get(rel)
    }

    pub fn files(&self) -> impl Iterator<Item = (&String, &FileStat)> {
        self.files.iter()
    }

    pub fn file_stat(&self, rel: &str) -> Option<&FileStat> {
        self.files.get(rel)
    }

    pub fn has_file(&self, rel: &str) -> bool {
        self.files.contains_key(rel)
    }

    pub fn read(&self, rel: &str) -> Option<String> {
        self.notes.get(rel)?;
        let b = std::fs::read(self.safe_abs(rel)?).ok()?;
        let s = String::from_utf8_lossy(&b);
        Some(s.strip_prefix('\u{feff}').unwrap_or(&s).to_string())
    }

    /// A text file of the vault that is not a note, such as a `.base` or `.canvas` (up to 4 MB).
    pub fn read_text(&self, rel: &str) -> Option<String> {
        let st = self.files.get(rel)?;
        if st.size > 4 << 20 {
            return None;
        }
        let b = std::fs::read(self.safe_abs(rel)?).ok()?;
        let s = String::from_utf8(b).ok()?;
        Some(s.strip_prefix('\u{feff}').unwrap_or(&s).to_string())
    }

    /// Outgoing links of a note with their resolution.
    pub fn links_of(&self, rel: &str) -> Vec<(Link, Option<String>)> {
        let Some(n) = self.notes.get(rel) else {
            return vec![];
        };
        let r = self.resolved.get(rel).cloned().unwrap_or_default();
        n.parsed
            .links
            .iter()
            .cloned()
            .zip(r.into_iter().chain(std::iter::repeat(None)))
            .collect()
    }

    pub fn backlinks(&self, rel: &str) -> Vec<Backlink> {
        self.backlinks.get(rel).cloned().unwrap_or_default()
    }

    /// Notes and attachments nothing links to.
    pub fn orphans(&self) -> Vec<String> {
        self.notes
            .keys()
            .filter(|p| !self.backlinks.contains_key(*p))
            .cloned()
            .collect()
    }

    /// Links that point nowhere: (source, target text, line).
    pub fn unresolved(&self) -> Vec<(String, String, usize)> {
        let mut out = Vec::new();
        for (p, n) in &self.notes {
            let r = self.resolved.get(p);
            for (i, l) in n.parsed.links.iter().enumerate() {
                if r.and_then(|r| r.get(i)).is_some_and(Option::is_none) {
                    out.push((p.clone(), l.target.clone(), l.line));
                }
            }
        }
        out
    }

    /// Tag -> number of notes, with every parent of a nested tag counted too (`a/b` counts for `a`).
    pub fn tags(&self) -> BTreeMap<String, usize> {
        let mut out: BTreeMap<String, usize> = BTreeMap::new();
        for n in self.notes.values() {
            let mut mine: HashSet<String> = HashSet::new();
            for t in &n.parsed.tags {
                let t = t.to_lowercase();
                let mut acc = String::new();
                for seg in t.split('/') {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(seg);
                    mine.insert(acc.clone());
                }
            }
            for t in mine {
                *out.entry(t).or_insert(0) += 1;
            }
        }
        out
    }

    pub fn has_tag(n: &Note, tag: &str) -> bool {
        let tag = tag.trim_start_matches('#').to_lowercase();
        n.parsed.tags.iter().any(|t| {
            let t = t.to_lowercase();
            t == tag || t.starts_with(&format!("{tag}/"))
        })
    }

    /// Mentions of a note's name, title or aliases in other notes that are not links to it.
    pub fn mentions(&self, rel: &str) -> Vec<(String, usize, String)> {
        let Some(n) = self.notes.get(rel) else {
            return vec![];
        };
        let mut names: Vec<String> = vec![stem(rel).to_string(), n.title.clone()];
        names.extend(n.parsed.aliases.iter().cloned());
        let mut names: Vec<String> = names
            .into_iter()
            .map(|s| s.to_lowercase())
            .filter(|s| s.chars().count() >= 3)
            .collect();
        names.sort();
        names.dedup();
        let mut out = Vec::new();
        for (p, other) in &self.notes {
            if p == rel {
                continue;
            }
            let terms_ok = names
                .iter()
                .any(|nm| search::tokenize(nm).all(|t| self.search.doc_has(p, &t)));
            if !terms_ok {
                continue;
            }
            let Some(text) = self.read(p) else { continue };
            let mut m = parse::mask_code(&text);
            for l in &other.parsed.links {
                for b in &mut m[l.span.0.min(text.len())..l.span.1.min(text.len())] {
                    if *b != b'\n' {
                        *b = b' ';
                    }
                }
            }
            for b in &mut m[..other.parsed.body_start.min(text.len())] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
            let masked = String::from_utf8_lossy(&m).to_lowercase();
            for (i, line) in masked.lines().enumerate() {
                if names.iter().any(|nm| word_find(line, nm)) {
                    let orig = text.lines().nth(i).unwrap_or("").trim();
                    out.push((
                        p.clone(),
                        i + 1,
                        crate::util::head_chars(orig, 240).to_string(),
                    ));
                }
            }
        }
        out
    }

    /// Fuzzy quick-switcher match over titles, file names, aliases and paths.
    pub fn suggest(&self, q: &str, limit: usize) -> Vec<(String, String, i64)> {
        let q = q.trim().to_lowercase();
        let mut scored: Vec<(i64, String, String)> = self
            .notes
            .values()
            .filter_map(|n| {
                let cands = std::iter::once(n.title.to_lowercase())
                    .chain(std::iter::once(stem(&n.path).to_lowercase()))
                    .chain(n.parsed.aliases.iter().map(|a| a.to_lowercase()))
                    .chain(std::iter::once(n.path.to_lowercase()));
                let best = cands.filter_map(|c| fuzzy(&q, &c)).max()?;
                Some((
                    best - n.path.len() as i64 / 8,
                    n.path.clone(),
                    n.title.clone(),
                ))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(s, p, t)| (p, t, s))
            .collect()
    }

    pub fn search(&self, q: &str, limit: usize) -> Vec<Hit> {
        search::run(self, q, limit)
    }

    pub fn stats(&self) -> Value {
        let tasks: usize = self
            .notes
            .values()
            .map(|n| n.parsed.tasks.iter().filter(|t| !t.done).count())
            .sum();
        json!({
            "notes": self.notes.len(),
            "attachments": self.files.len(),
            "links": self.notes.values().map(|n| n.parsed.links.len()).sum::<usize>(),
            "unresolved": self.unresolved().len(),
            "orphans": self.orphans().len(),
            "tags": self.tags().len(),
            "open_tasks": tasks,
            "words": self.notes.values().map(|n| n.parsed.words).sum::<usize>(),
            "warnings": self.warnings,
        })
    }
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// `needle` in `hay` on word boundaries (both lower-cased by the caller).
pub fn word_find(hay: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(o) = hay[from..].find(needle) {
        let s = from + o;
        let e = s + needle.len();
        if !is_word(hay[..s].chars().next_back()) && !is_word(hay[e..].chars().next()) {
            return true;
        }
        from = s + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Subsequence score: consecutive and word-start matches score higher; `None` if not all of `q` matches.
pub fn fuzzy(q: &str, cand: &str) -> Option<i64> {
    if q.is_empty() {
        return Some(0);
    }
    if cand.starts_with(q) {
        return Some(1000 - cand.len() as i64);
    }
    if let Some(p) = cand.find(q) {
        return Some(700 - p as i64);
    }
    let mut score = 0i64;
    let mut qi = q.chars().peekable();
    let mut prev_match = false;
    let mut prev: Option<char> = None;
    for c in cand.chars() {
        match qi.peek() {
            Some(&qc) if qc == c => {
                score += if prev_match { 15 } else { 5 };
                if prev.is_none_or(|p| !p.is_alphanumeric()) {
                    score += 10;
                }
                prev_match = true;
                qi.next();
            }
            Some(_) => prev_match = false,
            None => break,
        }
        prev = Some(c);
    }
    qi.peek().is_none().then_some(score)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh scratch folder for one test (left for the OS temp cleaner, never deleted here).
    pub fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "tempo-vault-{tag}-{}-{}-{}",
            std::process::id(),
            crate::util::stamp(crate::util::unix_now()),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub fn put(root: &Path, rel: &str, body: &str) {
        let p = rel.split('/').fold(root.to_path_buf(), |p, s| p.join(s));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    pub fn sample() -> (PathBuf, Vault) {
        let d = tmp("vault");
        put(
            &d,
            "Home.md",
            "# Home\nStart at [[Projects/Tempo]] and [[Ideas]]. Also [[Missing note]].\n#hub\n",
        );
        put(&d, "Projects/Tempo.md", "---\naliases: [Perennos Tempo]\ntags: [project, rust]\n---\n# Tempo\nThe vault links to [[Ideas#Next]] and ![[img/logo.png]].\n- [ ] ship vault 📅 2026-11-10\n");
        put(&d, "Ideas.md", "# Ideas\n## Next\nUse Perennos Tempo daily. Tempo is great. [[Home]]\n- [x] think\n#project/ideas\n");
        put(&d, "Archive/Ideas.md", "# Old ideas\nnothing links here\n");
        put(
            &d,
            "Archive/Index.md",
            "[[Ideas]] (same folder wins) and [Tempo](../Projects/Tempo.md)\n",
        );
        put(&d, "img/logo.png", "PNG");
        put(&d, ".obsidian/app.json", "{}");
        put(&d, ".trash/old.md", "[[Home]]");
        let v = Vault::open(&d).unwrap();
        (d, v)
    }

    #[test]
    fn indexes_and_resolves_like_obsidian() {
        let (_d, v) = sample();
        let paths: Vec<&str> = v.notes().map(|n| n.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Archive/Ideas.md",
                "Archive/Index.md",
                "Home.md",
                "Ideas.md",
                "Projects/Tempo.md"
            ]
        );
        assert!(v.has_file("img/logo.png") && !v.has_file(".obsidian/app.json"));
        let home: Vec<Option<String>> = v.links_of("Home.md").into_iter().map(|(_, r)| r).collect();
        assert_eq!(
            home,
            [
                Some("Projects/Tempo.md".into()),
                Some("Ideas.md".into()),
                None
            ]
        );
        let archive: Vec<Option<String>> = v
            .links_of("Archive/Index.md")
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        assert_eq!(
            archive,
            [
                Some("Archive/Ideas.md".into()),
                Some("Projects/Tempo.md".into())
            ],
            "same folder first; relative md link"
        );
        let tempo: Vec<Option<String>> = v
            .links_of("Projects/Tempo.md")
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        assert_eq!(
            tempo,
            [Some("Ideas.md".into()), Some("img/logo.png".into())]
        );
        let alias = Link {
            target: "perennos tempo".into(),
            heading: None,
            block: None,
            alias: None,
            embed: false,
            wiki: true,
            in_props: false,
            line: 1,
            span: (0, 0),
            target_span: (0, 0),
        };
        assert_eq!(
            v.resolve("Home.md", &alias).as_deref(),
            Some("Projects/Tempo.md"),
            "aliases resolve, case-insensitively"
        );
        let self_link = Link {
            target: String::new(),
            ..alias.clone()
        };
        assert_eq!(
            v.resolve("Ideas.md", &self_link).as_deref(),
            Some("Ideas.md")
        );
        let back: Vec<String> = v
            .backlinks("Ideas.md")
            .into_iter()
            .map(|b| b.source)
            .collect();
        assert_eq!(back, ["Home.md", "Projects/Tempo.md"]);
        assert_eq!(
            v.unresolved(),
            [("Home.md".to_string(), "Missing note".to_string(), 2)]
        );
        assert_eq!(v.orphans(), ["Archive/Index.md"]);
        assert_eq!(v.note("Projects/Tempo.md").unwrap().title, "Tempo");
        assert_eq!(v.note("Archive/Ideas.md").unwrap().title, "Old ideas");
    }

    #[test]
    fn tags_tasks_and_mentions() {
        let (_d, v) = sample();
        let tags = v.tags();
        assert_eq!(
            tags.get("project"),
            Some(&2),
            "nested project/ideas counts for project"
        );
        assert_eq!(tags.get("project/ideas"), Some(&1));
        assert_eq!(tags.get("hub"), Some(&1));
        assert!(Vault::has_tag(v.note("Ideas.md").unwrap(), "#Project"));
        let open: usize = v
            .notes()
            .map(|n| n.parsed.tasks.iter().filter(|t| !t.done).count())
            .sum();
        assert_eq!(open, 1);
        let m = v.mentions("Projects/Tempo.md");
        assert_eq!(
            m.iter()
                .map(|(p, l, _)| (p.as_str(), *l))
                .collect::<Vec<_>>(),
            [("Ideas.md", 3)]
        );
        assert!(m[0].2.contains("Perennos Tempo daily"));
        assert!(v.mentions("Nope.md").is_empty());
    }

    #[test]
    fn refresh_picks_up_changes() {
        let (d, mut v) = sample();
        let r = v.refresh();
        assert_eq!(
            r,
            Refresh {
                parsed: 0,
                removed: 0,
                attachments: 1,
                changed: vec![]
            },
            "nothing changed"
        );
        put(&d, "Missing note.md", "now it exists");
        std::fs::remove_file(d.join("Archive").join("Ideas.md")).unwrap(); // test scratch only
        let r = v.refresh();
        assert_eq!((r.parsed, r.removed), (1, 1));
        assert_eq!(r.changed, ["Missing note.md", "Archive/Ideas.md"]);
        assert!(v.unresolved().is_empty());
        let archive: Vec<Option<String>> = v
            .links_of("Archive/Index.md")
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        assert_eq!(
            archive[0].as_deref(),
            Some("Ideas.md"),
            "falls back to the other Ideas"
        );
    }

    #[test]
    fn suggest_and_fuzzy() {
        let (_d, v) = sample();
        let s = v.suggest("tem", 5);
        assert_eq!(s[0].0, "Projects/Tempo.md");
        let s = v.suggest("prjtmp", 5);
        assert_eq!(s[0].0, "Projects/Tempo.md", "subsequence match on the path");
        assert!(v.suggest("zzzz", 5).is_empty());
        assert!(fuzzy("abc", "a-b-c").is_some() && fuzzy("abd", "abc").is_none());
        assert!(word_find("use perennos tempo daily", "tempo") && !word_find("tempos", "tempo"));
    }
}
