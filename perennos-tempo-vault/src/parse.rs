//! Obsidian-compatible Markdown parsing: frontmatter properties, headings, `[[wikilinks]]` and
//! `![[embeds]]` (with `#heading`, `#^block` and `|alias`), `[markdown](links.md)`, `#tags`, tasks
//! and `^block` ids. Fenced and inline code, HTML comments and `%%comments%%` never produce links,
//! tags or tasks. Every link keeps the byte range of its target so a rename can rewrite it exactly.
use crate::util::pct_decode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prop {
    Text(String),
    List(Vec<String>),
}

impl Prop {
    /// Values as a list (a text value is split on commas).
    pub fn items(&self) -> Vec<String> {
        match self {
            Prop::Text(t) => t
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            Prop::List(l) => l.clone(),
        }
    }
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Prop::Text(t) => serde_json::json!(t),
            Prop::List(l) => serde_json::json!(l),
        }
    }
    pub fn from_json(v: &serde_json::Value) -> Prop {
        match v {
            serde_json::Value::Array(a) => Prop::List(
                a.iter()
                    .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string))
                    .collect(),
            ),
            serde_json::Value::String(s) => Prop::Text(s.clone()),
            serde_json::Value::Null => Prop::Text(String::new()),
            other => Prop::Text(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Target as written, decoded, without `#heading`, `^block` or `|alias`. Empty = this note.
    pub target: String,
    pub heading: Option<String>,
    pub block: Option<String>,
    pub alias: Option<String>,
    pub embed: bool,
    /// `[[wiki]]` (true) or `[md](link)` (false).
    pub wiki: bool,
    /// Inside the frontmatter (a property value).
    pub in_props: bool,
    /// 1-based line.
    pub line: usize,
    /// Byte range of the whole link (including `!` for embeds) in the source.
    pub span: (usize, usize),
    /// Byte range of just the target text in the source (what a rename replaces).
    pub target_span: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub line: usize,
    pub status: char,
    pub done: bool,
    pub text: String,
    pub due: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub line: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub props: Vec<(String, Prop)>,
    /// Byte offset where the body starts (after the frontmatter).
    pub body_start: usize,
    pub title: Option<String>,
    pub aliases: Vec<String>,
    /// Unique (case-insensitive), without `#`, first spelling kept.
    pub tags: Vec<String>,
    pub headings: Vec<Heading>,
    pub links: Vec<Link>,
    pub tasks: Vec<Task>,
    pub blocks: Vec<String>,
    pub words: usize,
}

impl Parsed {
    pub fn prop(&self, key: &str) -> Option<&Prop> {
        self.props
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }
}

/// `(yaml text, body start)` when the source starts with a `---` frontmatter block.
pub fn split_frontmatter(src: &str) -> Option<(usize, usize, usize)> {
    let rest = src.strip_prefix("---")?;
    let after = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let yaml_start = src.len() - after.len();
    let mut pos = yaml_start;
    for line in after.split_inclusive('\n') {
        let t = line.trim_end_matches(['\n', '\r']);
        if t == "---" || t == "..." {
            return Some((yaml_start, pos, pos + line.len()));
        }
        pos += line.len();
    }
    None
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        s[1..s.len() - 1].replace("\\\"", "\"").replace("''", "'")
    } else {
        s.to_string()
    }
}

fn split_flow(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    for c in s.chars() {
        match (c, quote) {
            ('"' | '\'', None) => {
                quote = Some(c);
                cur.push(c);
            }
            (q, Some(open)) if q == open => {
                quote = None;
                cur.push(c);
            }
            ('[', None) => {
                depth += 1;
                cur.push(c);
            }
            (']', None) => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            (',', None) if depth == 0 => {
                out.push(unquote(&cur));
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(unquote(&cur));
    }
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

/// The YAML subset Obsidian properties use: `key: value`, `key: [a, b]` and block lists.
pub fn parse_props(yaml: &str) -> Vec<(String, Prop)> {
    let mut out: Vec<(String, Prop)> = Vec::new();
    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || line.starts_with([' ', '\t', '-']) {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = unquote(k);
        if key.is_empty() {
            continue;
        }
        let v = v.trim();
        let prop = if v.is_empty() {
            let mut items = Vec::new();
            while i < lines.len() {
                let n = lines[i];
                let nt = n.trim_start();
                if let Some(item) = nt.strip_prefix("- ").or_else(|| (nt == "-").then_some("")) {
                    let item = unquote(item);
                    if !item.is_empty() {
                        items.push(item);
                    }
                } else if !(nt.is_empty() || n.starts_with([' ', '\t'])) {
                    break;
                }
                i += 1;
            }
            if items.is_empty() {
                Prop::Text(String::new())
            } else {
                Prop::List(items)
            }
        } else if v.starts_with('[') && v.ends_with(']') && !v.starts_with("[[") {
            Prop::List(split_flow(&v[1..v.len() - 1]))
        } else {
            Prop::Text(unquote(v))
        };
        out.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
        out.push((key, prop));
    }
    out
}

fn blank(out: &mut [u8], from: usize, to: usize) {
    let end = to.min(out.len());
    for b in &mut out[from.min(end)..end] {
        if *b != b'\n' {
            *b = b' ';
        }
    }
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// A same-length copy with fenced code, inline code, HTML comments and `%%comments%%` blanked to
/// spaces (newlines kept), so byte offsets still match the source.
pub fn mask_code(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = b.to_vec();
    // fenced blocks, line by line
    let mut i = 0;
    let mut fence: Option<(u8, usize)> = None;
    while i < b.len() {
        let end = find(b, i, b"\n").unwrap_or(b.len());
        let line = &s[i..end];
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let first = trimmed.bytes().next();
        let run = first.map_or(0, |c| trimmed.bytes().take_while(|x| *x == c).count());
        match fence {
            Some((c, n)) => {
                let closes =
                    indent <= 3 && first == Some(c) && run >= n && trimmed[run..].trim().is_empty();
                blank(&mut out, i, end);
                if closes {
                    fence = None;
                }
            }
            None => {
                if indent <= 3
                    && matches!(first, Some(b'`') | Some(b'~'))
                    && run >= 3
                    && !(first == Some(b'`') && trimmed[run..].contains('`'))
                {
                    fence = Some((first.unwrap_or(b'`'), run));
                    blank(&mut out, i, end);
                }
            }
        }
        i = end + 1;
    }
    // inline code and comments, left to right
    let n = out.len();
    let mut i = 0;
    while i < n {
        if out[i] == b'`' {
            let run = out[i..].iter().take_while(|x| **x == b'`').count();
            let mut j = i + run;
            let mut close = None;
            while j < n {
                if out[j] == b'`' {
                    let r = out[j..].iter().take_while(|x| **x == b'`').count();
                    if r == run {
                        close = Some(j + r);
                        break;
                    }
                    j += r;
                } else {
                    j += 1;
                }
            }
            match close {
                Some(c) => {
                    blank(&mut out, i, c);
                    i = c;
                }
                None => i += run,
            }
            continue;
        }
        if out[i..].starts_with(b"<!--") {
            if let Some(j) = find(&out, i + 4, b"-->") {
                blank(&mut out, i, j + 3);
                i = j + 3;
                continue;
            }
        }
        if out[i..].starts_with(b"%%") {
            if let Some(j) = find(&out, i + 2, b"%%") {
                blank(&mut out, i, j + 2);
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    fn new(s: &str) -> Lines {
        let mut starts = vec![0];
        starts.extend(
            s.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Lines { starts }
    }
    fn line_of(&self, off: usize) -> usize {
        match self.starts.binary_search(&off) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }
}

pub(crate) fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

/// Parse the wikilink inner text `target#heading|alias` found at byte `inner` in `src`.
fn wiki(
    src: &str,
    start: usize,
    inner: usize,
    inner_end: usize,
    embed: bool,
    in_props: bool,
    lines: &Lines,
) -> Option<Link> {
    let raw = &src[inner..inner_end];
    if raw.trim().is_empty() {
        return None;
    }
    // `\|` is the alias separator inside tables
    let (tgt_part, alias) = match raw
        .find("\\|")
        .map(|p| (p, 2))
        .or_else(|| raw.find('|').map(|p| (p, 1)))
    {
        Some((p, w)) => (
            &raw[..p],
            Some(raw[p + w..].trim().to_string()).filter(|a| !a.is_empty()),
        ),
        None => (raw, None),
    };
    let (target, frag) = match tgt_part.find('#') {
        Some(p) => (&tgt_part[..p], Some(&tgt_part[p + 1..])),
        None => (tgt_part, None),
    };
    let (heading, block) = match frag {
        Some(f) if f.starts_with('^') => (None, Some(f[1..].trim().to_string())),
        Some(f) if !f.trim().is_empty() => (Some(f.trim().to_string()), None),
        _ => (None, None),
    };
    let lead = target.len() - target.trim_start().len();
    let tgt = target.trim();
    let ts = inner + lead;
    Some(Link {
        target: tgt.to_string(),
        heading,
        block,
        alias,
        embed,
        wiki: true,
        in_props,
        line: lines.line_of(start),
        span: (start, inner_end + 2),
        target_span: (ts, ts + tgt.len()),
    })
}

/// A wikilink from its inner text (`target#heading|alias`), for renderers; positions are zero.
pub fn wiki_inner(inner: &str, embed: bool) -> Option<Link> {
    let src = format!("[[{inner}]]");
    let lines = Lines::new(&src);
    wiki(&src, 0, 2, 2 + inner.len(), embed, false, &lines).map(|l| Link {
        line: 0,
        span: (0, 0),
        target_span: (0, 0),
        ..l
    })
}

pub(crate) fn has_scheme(dest: &str) -> bool {
    let d = dest.trim_start();
    if d.starts_with('#') || d.starts_with("//") {
        return true;
    }
    match d.find(':') {
        Some(p) => {
            p > 1
                && d[..p]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

pub fn parse(src: &str) -> Parsed {
    let lines = Lines::new(src);
    let mut p = Parsed::default();
    let mut body_start = 0;
    if let Some((ys, ye, bs)) = split_frontmatter(src) {
        p.props = parse_props(&src[ys..ye]);
        body_start = bs;
        // wikilinks inside property values count as links
        let fm = &src[ys..ye];
        let mut from = 0;
        while let Some(o) = fm[from..].find("[[") {
            let s = ys + from + o;
            let Some(close) = src[s + 2..ye].find("]]") else {
                break;
            };
            if let Some(l) = wiki(src, s, s + 2, s + 2 + close, false, true, &lines) {
                p.links.push(l);
            }
            from = s + 2 + close + 2 - ys;
        }
    }
    p.body_start = body_start;
    let mut m = mask_code(src);
    blank(&mut m, 0, body_start);
    let ms = |m: &[u8]| String::from_utf8_lossy(m).into_owned();

    // headings, tasks, block ids (masked, line by line)
    let masked = ms(&m);
    let mut off = 0;
    for line in masked.split_inclusive('\n') {
        let l = line.trim_end_matches(['\n', '\r']);
        let t = l.trim_start();
        let indent = l.len() - t.len();
        let lineno = lines.line_of(off);
        if indent <= 3 && t.starts_with('#') {
            let level = t.bytes().take_while(|b| *b == b'#').count();
            let rest = &t[level..];
            if (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with([' ', '\t'])) {
                let text = rest.trim().trim_end_matches('#').trim().to_string();
                p.headings.push(Heading {
                    level: level as u8,
                    text,
                    line: lineno,
                });
            }
        }
        // - [ ] task / 1. [x] task
        let after_marker = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("+ "))
            .or_else(|| {
                let d = t.bytes().take_while(u8::is_ascii_digit).count();
                (d > 0 && d < 10)
                    .then(|| &t[d..])
                    .and_then(|r| r.strip_prefix(". ").or_else(|| r.strip_prefix(") ")))
            });
        if let Some(r) = after_marker {
            let rb = r.as_bytes();
            if rb.len() >= 3
                && rb[0] == b'['
                && rb[2] == b']'
                && (rb.len() == 3 || rb[3] == b' ')
                && rb[1].is_ascii()
            {
                let status = rb[1] as char;
                let text_off = off + (l.len() - r.len()) + 3;
                let text = src[text_off.min(src.len())..(off + l.len()).min(src.len())]
                    .trim()
                    .to_string();
                let due = text
                    .find("📅")
                    .map(|i| text[i + "📅".len()..].trim_start())
                    .or_else(|| text.find("due::").map(|i| text[i + 5..].trim_start()))
                    .and_then(|d| {
                        crate::util::parse_date(d.get(..10)?).map(|_| d[..10].to_string())
                    });
                p.tasks.push(Task {
                    line: lineno,
                    status,
                    done: matches!(status, 'x' | 'X'),
                    text,
                    due,
                });
            }
        }
        if let Some(i) = l.rfind(" ^") {
            let id = &l[i + 2..];
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                p.blocks.push(id.to_string());
            }
        }
        off += line.len();
    }

    // [[wikilinks]] and ![[embeds]] (single line)
    let mut i = body_start;
    while let Some(s) = find(&m, i, b"[[") {
        let line_end = find(&m, s, b"\n").unwrap_or(m.len());
        let Some(close) = find(&m[..line_end], s + 2, b"]]") else {
            i = s + 2;
            continue;
        };
        if let Some(inner_open) = find(&m[..close], s + 2, b"[[") {
            i = inner_open; // `[[a [[b]]`: restart at the inner one
            continue;
        }
        let embed = s > 0 && m[s - 1] == b'!';
        let start = if embed { s - 1 } else { s };
        if let Some(l) = wiki(src, start, s + 2, close, embed, false, &lines) {
            p.links.push(l);
        }
        blank(&mut m, start, close + 2);
        i = close + 2;
    }

    // [markdown](links) and ![images](x.png)
    let mut i = body_start;
    while let Some(s) = find(&m, i, b"[") {
        i = s + 1;
        if s > 0 && m[s - 1] == b'\\' {
            continue;
        }
        let line_end = find(&m, s, b"\n").unwrap_or(m.len());
        let Some(close) = find(&m[..line_end], s + 1, b"](") else {
            continue;
        };
        if find(&m[..close], s + 1, b"[").is_some() {
            continue;
        }
        let d0 = close + 2;
        let (dest_start, dest_end, after) = if m.get(d0) == Some(&b'<') {
            match find(&m[..line_end], d0 + 1, b">") {
                Some(e) => (d0 + 1, e, e + 1),
                None => continue,
            }
        } else {
            let mut depth = 0usize;
            let mut e = d0;
            while e < line_end {
                match m[e] {
                    b'(' => depth += 1,
                    b')' if depth == 0 => break,
                    b')' => depth -= 1,
                    b' ' | b'\t' => break,
                    _ => {}
                }
                e += 1;
            }
            (d0, e, e)
        };
        let Some(paren) = find(&m[..line_end], after, b")") else {
            continue;
        };
        let dest_raw = &src[dest_start..dest_end];
        let embed = s > 0 && m[s - 1] == b'!';
        let start = if embed { s - 1 } else { s };
        if !dest_raw.is_empty() && !has_scheme(dest_raw) {
            let (path_part, frag) = match dest_raw.find('#') {
                Some(h) => (&dest_raw[..h], Some(pct_decode(&dest_raw[h + 1..]))),
                None => (dest_raw, None),
            };
            let (heading, block) = match frag {
                Some(f) if f.starts_with('^') => (None, Some(f[1..].to_string())),
                Some(f) if !f.is_empty() => (Some(f), None),
                _ => (None, None),
            };
            let text = src[s + 1..close].trim();
            p.links.push(Link {
                target: pct_decode(path_part),
                heading,
                block,
                alias: (!text.is_empty()).then(|| text.to_string()),
                embed,
                wiki: false,
                in_props: false,
                line: lines.line_of(start),
                span: (start, paren + 1),
                target_span: (dest_start, dest_start + path_part.len()),
            });
        }
        blank(&mut m, start, paren + 1);
        i = paren + 1;
    }

    // #tags (after links are blanked, so URLs and link text cannot produce tags)
    let masked = ms(&m);
    let mut seen: Vec<String> = Vec::new();
    let push_tag = |t: &str, seen: &mut Vec<String>, tags: &mut Vec<String>| {
        let t = t.trim_matches('/');
        if t.is_empty() || t.chars().all(|c| c.is_ascii_digit() || c == '/') {
            return;
        }
        let low = t.to_lowercase();
        if !seen.contains(&low) {
            seen.push(low);
            tags.push(t.to_string());
        }
    };
    if let Some(v) = p.prop("tags").or_else(|| p.prop("tag")).cloned() {
        for t in v
            .items()
            .iter()
            .flat_map(|x| x.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        {
            push_tag(t.trim_start_matches('#'), &mut seen, &mut p.tags);
        }
    }
    let chars: Vec<(usize, char)> = masked.char_indices().collect();
    for (k, &(_, c)) in chars.iter().enumerate() {
        if c != '#' || (k > 0 && !chars[k - 1].1.is_whitespace()) {
            continue;
        }
        let tag: String = chars[k + 1..]
            .iter()
            .map(|(_, c)| *c)
            .take_while(|c| is_tag_char(*c))
            .collect();
        if !tag.is_empty() {
            push_tag(&tag, &mut seen, &mut p.tags);
        }
    }
    p.words = masked
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .count();

    p.aliases = p
        .prop("aliases")
        .or_else(|| p.prop("alias"))
        .map(Prop::items)
        .unwrap_or_default();
    p.title = match p.prop("title") {
        Some(Prop::Text(t)) if !t.is_empty() => Some(t.clone()),
        _ => p
            .headings
            .iter()
            .find(|h| h.level == 1)
            .map(|h| h.text.clone()),
    };
    p.links.sort_by_key(|l| l.span.0);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\ntitle: Tempo plan\naliases: [Plan, \"Road map\"]\ntags:\n  - project\n  - rust/core\nstatus: active\nrelated: \"[[Vectis]]\"\n---\n# Heading one\nSee [[Vault Design]] and [[notes/Ideas#Next steps|the ideas]] or ![[diagram.png]].\nAlso [this](sub/Other%20Note.md#Part) and [site](https://example.com/#x) #tagged #2026 #a/b/.\n\n```rust\nlet x = \"[[Not a link]]\"; // #nottag\n```\nInline `[[code link]]` and <!-- [[hidden]] --> %%[[secret]]%% stay out.\n- [ ] write the parser 📅 2026-11-10\n- [x] done item\n1. [ ] numbered task due:: 2026-12-01\nA paragraph with a block id ^abc-1\n## Second ##\n| a | [[Table Note\\|alias]] |\n";

    #[test]
    fn frontmatter_properties() {
        let p = parse(NOTE);
        assert_eq!(p.prop("title"), Some(&Prop::Text("Tempo plan".into())));
        assert_eq!(p.aliases, ["Plan", "Road map"]);
        assert_eq!(p.prop("status"), Some(&Prop::Text("active".into())));
        assert_eq!(p.title.as_deref(), Some("Tempo plan"));
        assert!(NOTE[p.body_start..].starts_with("# Heading one"));
        let props = parse_props("a: 1\nb: [x, 'y, z', \"w\"]\nc:\n  - one\n  -\n  - two\nd:\ne: \"quoted: colon\"\n# comment\nA: override\n");
        assert_eq!(
            props.iter().find(|(k, _)| k == "b").unwrap().1,
            Prop::List(vec!["x".into(), "y, z".into(), "w".into()])
        );
        assert_eq!(
            props.iter().find(|(k, _)| k == "c").unwrap().1,
            Prop::List(vec!["one".into(), "two".into()])
        );
        assert_eq!(
            props.iter().find(|(k, _)| k == "d").unwrap().1,
            Prop::Text(String::new())
        );
        assert_eq!(
            props.iter().find(|(k, _)| k == "e").unwrap().1,
            Prop::Text("quoted: colon".into())
        );
        assert_eq!(
            props
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("a"))
                .count(),
            1,
            "later keys win"
        );
        assert!(split_frontmatter("--- not frontmatter\n").is_none());
        assert!(split_frontmatter("---\nnever closed\n").is_none());
        assert_eq!(
            split_frontmatter("---\na: 1\n---").map(|x| x.2),
            Some(12),
            "closing line at end of file"
        );
    }

    #[test]
    fn links_tags_tasks_headings_blocks() {
        let p = parse(NOTE);
        let t: Vec<(&str, bool, bool)> = p
            .links
            .iter()
            .map(|l| (l.target.as_str(), l.wiki, l.embed))
            .collect();
        assert_eq!(
            t,
            [
                ("Vectis", true, false),
                ("Vault Design", true, false),
                ("notes/Ideas", true, false),
                ("diagram.png", true, true),
                ("sub/Other Note.md", false, false),
                ("Table Note", true, false),
            ]
        );
        assert!(p.links[0].in_props);
        let ideas = &p.links[2];
        assert_eq!(
            (ideas.heading.as_deref(), ideas.alias.as_deref()),
            (Some("Next steps"), Some("the ideas"))
        );
        assert_eq!(
            &NOTE[ideas.target_span.0..ideas.target_span.1],
            "notes/Ideas"
        );
        assert_eq!(
            &NOTE[ideas.span.0..ideas.span.1],
            "[[notes/Ideas#Next steps|the ideas]]"
        );
        let md = &p.links[4];
        assert_eq!(md.heading.as_deref(), Some("Part"));
        assert_eq!(
            &NOTE[md.target_span.0..md.target_span.1],
            "sub/Other%20Note.md"
        );
        assert_eq!(p.links[5].alias.as_deref(), Some("alias"));
        assert_eq!(p.links[3].line, 11);
        assert_eq!(p.tags, ["project", "rust/core", "tagged", "a/b"]);
        assert_eq!(
            p.headings,
            [
                Heading {
                    level: 1,
                    text: "Heading one".into(),
                    line: 10
                },
                Heading {
                    level: 2,
                    text: "Second".into(),
                    line: 22
                }
            ]
        );
        assert_eq!(p.tasks.len(), 3);
        assert_eq!(
            (p.tasks[0].done, p.tasks[0].due.as_deref()),
            (false, Some("2026-11-10"))
        );
        assert!(p.tasks[1].done);
        assert_eq!(p.tasks[2].due.as_deref(), Some("2026-12-01"));
        assert_eq!(p.tasks[0].text, "write the parser 📅 2026-11-10");
        assert_eq!(p.blocks, ["abc-1"]);
    }

    #[test]
    fn code_and_comments_are_inert() {
        let p = parse("```\n[[a]] #b\n- [ ] c\n```\n~~~~\n[[d]]\n~~~\nstill fenced [[e]]\n~~~~\nafter `x` [[f]] ``code `with` tick [[g]]`` %%\n[[h]]\n%%\n");
        let targets: Vec<&str> = p.links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, ["f"]);
        assert!(p.tags.is_empty() && p.tasks.is_empty());
        let unclosed = parse("text `not closed [[x]]");
        assert_eq!(unclosed.links.len(), 1, "an unmatched backtick is literal");
    }

    #[test]
    fn edge_cases() {
        assert!(parse("[[]] [[ ]] [[a").links.is_empty());
        let p = parse("[[a [[b]] c]] [x](<My File.md>) [y](http://e.com) [z](#local) [w](mailto:a@b.c) \\[n](o.md)");
        let t: Vec<&str> = p.links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(t, ["b", "My File.md"]);
        let p = parse("[[#Heading]] [[^blk]] [[Note#^blk]]");
        assert_eq!(p.links[0].target, "");
        assert_eq!(p.links[0].heading.as_deref(), Some("Heading"));
        assert_eq!(
            p.links[1].target, "^blk",
            "Obsidian treats [[^x]] as a name"
        );
        assert_eq!(p.links[2].block.as_deref(), Some("blk"));
        let tags =
            parse("#123 #a1 x#notatag (#nope) #ünïcode #with-dash_and/sub #Project #project").tags;
        assert_eq!(tags, ["a1", "ünïcode", "with-dash_and/sub", "Project"]);
        assert_eq!(
            parse("# Only heading").title.as_deref(),
            Some("Only heading")
        );
        assert_eq!(parse("plain").title, None);
        assert_eq!(parse("a b  c\nd").words, 4);
        assert_eq!(parse("- [/] in progress").tasks[0].status, '/');
        assert!(parse("- [ ]no space").tasks.is_empty());
        let crlf = parse("---\r\ntitle: x\r\n---\r\n# H\r\n[[L]]\r\n");
        assert_eq!((crlf.title.as_deref(), crlf.links.len()), (Some("x"), 1));
    }
}
