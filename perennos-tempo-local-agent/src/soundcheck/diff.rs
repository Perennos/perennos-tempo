//! A change as the soundcheck sees it: files, hunks and numbered lines.
//!
//! The same shapes come from `git diff` output, from a patch file, and from whole files
//! (an audit, or a new file git has not seen yet), so every later step reads one structure.

use crate::util::{self, head_chars};
use regex::Regex;
use std::sync::OnceLock;

/// One line of a hunk. `no` is the line number in the NEW file; for a removed line it is the
/// place in the new file where the removal happened.
#[derive(Clone, Debug, PartialEq)]
pub struct DLine {
    /// '+', '-' or ' '
    pub kind: char,
    pub text: String,
    pub no: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Hunk {
    pub new_start: usize,
    /// What git printed after the second `@@`, usually the enclosing function.
    pub context: String,
    pub lines: Vec<DLine>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug)]
pub struct FileDiff {
    pub path: String,
    pub status: FileStatus,
    pub binary: bool,
    /// The file is part of the change but its content was deliberately not read (its name
    /// says it holds secrets). Scanners still see the name.
    pub unread: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn new(path: &str, status: FileStatus) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            status,
            binary: false,
            unread: false,
            hunks: vec![],
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &DLine> {
        self.hunks.iter().flat_map(|h| h.lines.iter())
    }

    pub fn added(&self) -> impl Iterator<Item = &DLine> {
        self.lines().filter(|l| l.kind == '+')
    }

    pub fn removed(&self) -> impl Iterator<Item = &DLine> {
        self.lines().filter(|l| l.kind == '-')
    }

    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    pub fn extension(&self) -> String {
        match self.file_name().rsplit_once('.') {
            Some((_, e)) => e.to_ascii_lowercase(),
            None => String::new(),
        }
    }

    /// The text at `line` of the new file, when this change shows it.
    pub fn text_at(&self, line: usize) -> Option<&str> {
        self.lines()
            .find(|l| l.kind != '-' && l.no == line)
            .map(|l| l.text.as_str())
    }

    /// A short number that changes whenever anything shown for this file changes.
    pub fn fingerprint(&self) -> u64 {
        let mut h = Fnv::new();
        h.feed(self.path.as_bytes());
        for l in self.lines() {
            h.feed(&[l.kind as u8]);
            h.feed(l.text.as_bytes());
            h.feed(b"\n");
        }
        h.0
    }
}

/// FNV-1a: tiny, stable across runs and platforms, and enough to tell two diffs apart.
pub struct Fnv(pub u64);

impl Fnv {
    pub fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Fnv::new()
    }
}

fn hunk_header() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)$").expect("hunk header")
    })
}

/// `a/src/x.rs`, `"b/odd name"` or `src/x.rs\t2024-01-01` become `src/x.rs`.
fn clean_path(raw: &str) -> String {
    let p = raw
        .split('\t')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    for prefix in ["a/", "b/"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    p.to_string()
}

fn path_from_git_header(line: &str) -> String {
    let rest = line.trim_start_matches("diff --git ");
    match rest.rfind(" b/") {
        Some(i) => clean_path(&rest[i + 1..]),
        None => clean_path(rest.split_whitespace().last().unwrap_or("")),
    }
}

/// Read a unified diff (git's or a plain `diff -u`) into files and hunks.
pub fn parse(text: &str) -> Vec<FileDiff> {
    let lines: Vec<&str> = text.lines().collect();
    let mut files: Vec<FileDiff> = vec![];
    let mut in_headers = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("diff --git ") {
            files.push(FileDiff::new(
                &path_from_git_header(line),
                FileStatus::Modified,
            ));
            in_headers = true;
        } else if line.starts_with("--- ")
            && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "))
        {
            if !in_headers {
                files.push(FileDiff::new("", FileStatus::Modified));
                in_headers = true;
            }
            let (old, new) = (clean_path(&line[4..]), clean_path(&lines[i + 1][4..]));
            if let Some(f) = files.last_mut() {
                if old == "/dev/null" {
                    f.status = FileStatus::Added;
                }
                if new != "/dev/null" {
                    f.path = new;
                } else if old != "/dev/null" {
                    f.path = old;
                    f.status = FileStatus::Deleted;
                }
            }
            i += 1;
        } else if let Some(f) = files.last_mut() {
            if line.starts_with("new file mode") {
                f.status = FileStatus::Added;
            } else if line.starts_with("deleted file mode") {
                f.status = FileStatus::Deleted;
            } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
                f.binary = true;
            } else if let Some(c) = hunk_header().captures(line) {
                in_headers = false;
                i = read_hunk(&lines, i, &c, f);
                continue;
            }
        }
        i += 1;
    }
    files.retain(|f| !f.path.is_empty());
    files
}

/// Read one hunk body. Its header says how many old and new lines to expect, which is the only
/// reliable way to tell a removed line that starts with `--` from the next file's header.
fn read_hunk(lines: &[&str], header_at: usize, c: &regex::Captures, file: &mut FileDiff) -> usize {
    let num = |k: usize, default: usize| {
        c.get(k)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(default)
    };
    let (mut old_left, mut new_left) = (num(2, 1), num(4, 1));
    let mut next_new = num(3, 1);
    let mut hunk = Hunk {
        new_start: next_new,
        context: c[5].trim().to_string(),
        lines: vec![],
    };
    let mut i = header_at + 1;
    while i < lines.len() && (old_left > 0 || new_left > 0) {
        let line = lines[i];
        let (kind, text) = match line.chars().next() {
            Some('+') => ('+', &line[1..]),
            Some('-') => ('-', &line[1..]),
            Some(' ') => (' ', &line[1..]),
            None => (' ', ""), // an editor stripped the space off an empty context line
            Some('\\') => {
                i += 1; // "\ No newline at end of file"
                continue;
            }
            _ => break,
        };
        match kind {
            '+' => new_left = new_left.saturating_sub(1),
            '-' => old_left = old_left.saturating_sub(1),
            _ => {
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
            }
        }
        hunk.lines.push(DLine {
            kind,
            text: text.to_string(),
            no: next_new,
        });
        if kind != '-' {
            next_new += 1;
        }
        i += 1;
    }
    file.hunks.push(hunk);
    i
}

/// A file shown whole, as if every line had just been added.
pub fn whole_file(path: &str, content: &str, status: FileStatus) -> FileDiff {
    let mut f = FileDiff::new(path, status);
    let lines: Vec<DLine> = content
        .lines()
        .enumerate()
        .map(|(i, t)| DLine {
            kind: '+',
            text: t.to_string(),
            no: i + 1,
        })
        .collect();
    if !lines.is_empty() {
        f.hunks.push(Hunk {
            new_start: 1,
            context: String::new(),
            lines,
        });
    }
    f
}

/// What the model gets to see of a line: tokens and key blocks are blanked, and a file whose
/// name says "secrets" is blanked by the agent's usual redaction. The scanners read the raw
/// line, so a leak is still reported.
pub fn model_view(path: &str, text: &str) -> String {
    if util::is_secret_path(path) {
        return util::redact(text);
    }
    let t = util::secret_pem_rx().replace_all(text, "[REDACTED PRIVATE KEY]");
    util::secret_tok_rx()
        .replace_all(&t, "[REDACTED]")
        .into_owned()
}

fn render_line(path: &str, l: &DLine) -> String {
    let text = model_view(path, &l.text);
    match l.kind {
        '-' => format!("      - {text}"),
        k => format!("{:>5} {k} {text}", l.no),
    }
}

fn status_word(s: FileStatus) -> &'static str {
    match s {
        FileStatus::Added => "new file",
        FileStatus::Modified => "modified",
        FileStatus::Deleted => "deleted",
    }
}

pub fn file_header(f: &FileDiff) -> String {
    format!("=== FILE {} ({}) ===\n", f.path, status_word(f.status))
}

fn render_hunk(path: &str, h: &Hunk) -> String {
    let mut s = format!("@@ line {} @@ {}\n", h.new_start, h.context);
    for l in &h.lines {
        s.push_str(&render_line(path, l));
        s.push('\n');
    }
    s
}

/// The hunks of `f` nearest `line`, as text, at most `cap` characters. Used to show a second
/// listener the code around a finding.
pub fn view_around(f: &FileDiff, line: usize, cap: usize) -> String {
    let mut hunks: Vec<&Hunk> = f.hunks.iter().collect();
    let dist = |h: &Hunk| {
        let end = h.new_start + h.lines.len();
        if line < h.new_start {
            h.new_start - line
        } else {
            line.saturating_sub(end)
        }
    };
    hunks.sort_by_key(|h| dist(h));
    let mut out = file_header(f);
    for h in hunks {
        let piece = render_hunk(&f.path, h);
        if out.chars().count() + piece.chars().count() > cap {
            break;
        }
        out.push_str(&piece);
    }
    out
}

/// A slice of the change small enough for one model request.
#[derive(Clone, Debug)]
pub struct Chunk {
    pub text: String,
    pub files: Vec<String>,
}

/// Pack the change into chunks of about `budget` characters. A file is split between chunks at
/// hunk boundaries; a single hunk longer than the budget is cut (and counted in the second
/// value, so the report can say part of the change was not shown to the model).
pub fn chunk(files: &[FileDiff], budget: usize) -> (Vec<Chunk>, usize) {
    let mut chunks: Vec<Chunk> = vec![];
    let mut cut = 0;
    let mut cur = Chunk {
        text: String::new(),
        files: vec![],
    };
    for f in files.iter().filter(|f| !f.binary && !f.unread) {
        let mut header_in_cur = false;
        for h in &f.hunks {
            let mut piece = render_hunk(&f.path, h);
            if piece.chars().count() > budget {
                piece = format!(
                    "{}\n[hunk cut: too long to show whole]\n",
                    head_chars(&piece, budget)
                );
                cut += 1;
            }
            let header = file_header(f);
            let needed = piece.chars().count() + if header_in_cur { 0 } else { header.len() };
            if !cur.text.is_empty() && cur.text.chars().count() + needed > budget {
                chunks.push(std::mem::replace(
                    &mut cur,
                    Chunk {
                        text: String::new(),
                        files: vec![],
                    },
                ));
                header_in_cur = false;
            }
            if !header_in_cur {
                cur.text.push_str(&header);
                if !cur.files.contains(&f.path) {
                    cur.files.push(f.path.clone());
                }
                header_in_cur = true;
            }
            cur.text.push_str(&piece);
        }
    }
    if !cur.text.is_empty() {
        chunks.push(cur);
    }
    (chunks, cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@ fn main() {
 one
-two
+two changed
+three
 four
 five
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+-- looks like a header
diff --git a/gone.py b/gone.py
deleted file mode 100644
--- a/gone.py
+++ /dev/null
@@ -1 +0,0 @@
-print(1)
diff --git a/logo.png b/logo.png
Binary files a/logo.png and b/logo.png differ
";

    #[test]
    fn parses_files_hunks_and_line_numbers() {
        let files = parse(SAMPLE);
        assert_eq!(files.len(), 4);
        let a = &files[0];
        assert_eq!(
            (a.path.as_str(), a.status),
            ("src/a.rs", FileStatus::Modified)
        );
        assert_eq!(a.hunks[0].context, "fn main() {");
        let added: Vec<_> = a.added().map(|l| (l.no, l.text.as_str())).collect();
        assert_eq!(added, vec![(2, "two changed"), (3, "three")]);
        // the removed line sits where the new file continues
        assert_eq!(a.removed().next().unwrap().no, 2);
        assert_eq!(a.text_at(4), Some("four"));
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_stays_in_its_hunk() {
        let files = parse(SAMPLE);
        let new = &files[1];
        assert_eq!(new.status, FileStatus::Added);
        assert_eq!(new.added().count(), 2);
        assert_eq!(files[2].status, FileStatus::Deleted);
        assert_eq!(files[2].path, "gone.py");
        assert!(files[3].binary);
    }

    #[test]
    fn reads_a_plain_unified_patch() {
        let p = "--- x/old.txt\t2024-01-01\n+++ x/new.txt\t2024-01-02\n@@ -1 +1 @@\n-a\n+b\n";
        let f = parse(p);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].path, "x/new.txt");
        assert_eq!(f[0].added().count(), 1);
    }

    #[test]
    fn chunks_split_at_hunks_and_cut_giant_hunks() {
        let big = "x".repeat(300);
        let content: String = (0..40).map(|i| format!("line {i} {big}\n")).collect();
        let f = whole_file("big.txt", &content, FileStatus::Added);
        let (chunks, cut) = chunk(std::slice::from_ref(&f), 500);
        assert_eq!(chunks.len(), 1);
        assert_eq!(cut, 1);
        assert!(chunks[0].text.contains("hunk cut"));

        let mut many = FileDiff::new("m.txt", FileStatus::Modified);
        for n in 0..6 {
            many.hunks.push(Hunk {
                new_start: n * 10 + 1,
                context: String::new(),
                lines: vec![DLine {
                    kind: '+',
                    text: "y".repeat(100),
                    no: n * 10 + 1,
                }],
            });
        }
        let (chunks, cut) = chunk(&[many], 300);
        assert!(chunks.len() > 1 && cut == 0);
        // the file header is repeated in each chunk it appears in
        assert!(chunks.iter().all(|c| c.text.starts_with("=== FILE m.txt")));
    }

    #[test]
    fn the_model_never_sees_a_token_or_key_block() {
        let token = format!("ghp_{}", "a1B2".repeat(6));
        let shown = model_view("src/x.rs", &format!("let t = \"{token}\";"));
        assert!(!shown.contains(&token) && shown.contains("[REDACTED]"));
        let file = whole_file("src/x.rs", &format!("t = {token}\n"), FileStatus::Added);
        let (c, _) = chunk(&[file], 4000);
        assert!(!c[0].text.contains(&token));
    }

    #[test]
    fn fingerprints_change_with_content() {
        let a = whole_file("a", "one\n", FileStatus::Added);
        let b = whole_file("a", "two\n", FileStatus::Added);
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(
            a.fingerprint(),
            whole_file("a", "one\n", FileStatus::Added).fingerprint()
        );
    }
}
