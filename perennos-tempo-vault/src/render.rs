//! Safe Markdown to HTML, Obsidian flavour: headings, paragraphs (a single newline is a line
//! break, as in Obsidian), emphasis, `==highlights==`, `~~strike~~`, code, math (kept as TeX),
//! lists and tasks, tables, quotes and callouts, footnotes, tags, wikilinks, Markdown links,
//! embeds (images, audio, video, files, whole notes, sections and blocks, `.base` views),
//! `%%comments%%`, and live `query` (search), `tasks`, `base` and `dataview` blocks.
//!
//! Safety: every character of note text is HTML-escaped and raw HTML shows as text (only `<br>`
//! is kept). Links and images to other sites must be http(s) (links may also be mailto); any
//! other scheme (javascript:, data:, file:, ...) renders as plain text. Internal links carry the
//! vault path in data attributes; the caller decides their href.
use crate::canvas;
use crate::parse::{self, Link};
use crate::query;
use crate::util::{file_name, parse_date, pct_decode};
use crate::vault::Vault;
use std::collections::HashMap;

pub struct Opts<'a> {
    /// href for a note path plus an optional `#heading` / `^block`.
    pub note_href: &'a dyn Fn(&str, Option<&str>) -> String,
    /// src/href for an attachment path.
    pub file_src: &'a dyn Fn(&str) -> String,
    /// href for a tag (without `#`).
    pub tag_href: &'a dyn Fn(&str) -> String,
    /// Task checkboxes the reader may toggle (the app) or not (a published page).
    pub interactive: bool,
    /// How deep note embeds may nest.
    pub max_depth: usize,
}

fn app_note_href(p: &str, anchor: Option<&str>) -> String {
    let mut s = format!("#/n/{}", crate::util::pct_encode_path(p));
    if let Some(a) = anchor {
        s.push_str("?h=");
        s.push_str(&crate::util::pct_encode_path(a));
    }
    s
}
fn app_file_src(p: &str) -> String {
    format!("/vault/file?path={}", crate::util::pct_encode_path(p))
}
fn app_tag_href(t: &str) -> String {
    format!("#/s/{}", crate::util::pct_encode_path(&format!("tag:{t}")))
}

impl Opts<'static> {
    /// What the Tempo Vault app uses.
    pub fn app() -> Opts<'static> {
        Opts {
            note_href: &app_note_href,
            file_src: &app_file_src,
            tag_href: &app_tag_href,
            interactive: true,
            max_depth: 3,
        }
    }
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        esc_char(&mut o, c);
    }
    o
}

fn esc_char(o: &mut String, c: char) {
    match c {
        '&' => o.push_str("&amp;"),
        '<' => o.push_str("&lt;"),
        '>' => o.push_str("&gt;"),
        '"' => o.push_str("&quot;"),
        '\'' => o.push_str("&#39;"),
        c => o.push(c),
    }
}

pub const IMAGE: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif", "ico",
];
pub const AUDIO: &[&str] = &["mp3", "wav", "m4a", "ogg", "flac", "aac", "opus", "3gp"];
pub const VIDEO: &[&str] = &["mp4", "webm", "mov", "ogv", "mkv", "m4v"];

pub fn ext(p: &str) -> String {
    let f = file_name(p);
    f.rfind('.')
        .map(|i| f[i + 1..].to_ascii_lowercase())
        .unwrap_or_default()
}

/// Heading text to a stable id: lower case, runs of other characters become `-`.
pub fn slug(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '_' {
            o.push(c);
        } else if !o.ends_with('-') && !o.is_empty() {
            o.push('-');
        }
    }
    let o = o.trim_end_matches('-').to_string();
    if o.is_empty() {
        "section".into()
    } else {
        o
    }
}

struct Cx<'a> {
    v: Option<&'a Vault>,
    o: &'a Opts<'a>,
    from: String,
    stack: Vec<String>,
    ids: HashMap<String, usize>,
    fn_defs: HashMap<String, String>,
    fn_order: Vec<(String, String)>, // (label, rendered text) in reference order
    in_link: bool,
}

/// The body of a vault note as HTML (`None` if there is no such note).
pub fn note_html(v: &Vault, path: &str, o: &Opts) -> Option<String> {
    let src = v.read(path)?;
    Some(markdown_html(Some(v), path, &src, o))
}

fn new_cx<'a>(v: Option<&'a Vault>, from: &str, o: &'a Opts<'a>) -> Cx<'a> {
    Cx {
        v,
        o,
        from: from.to_string(),
        stack: vec![from.to_string()],
        ids: HashMap::new(),
        fn_defs: HashMap::new(),
        fn_order: vec![],
        in_link: false,
    }
}

/// Any Markdown as HTML. `from` is the note the text belongs to (for resolving its links).
pub fn markdown_html(v: Option<&Vault>, from: &str, src: &str, o: &Opts) -> String {
    let mut cx = new_cx(v, from, o);
    let (body, first_line) = body_of(src);
    doc(&mut cx, body, first_line)
}

/// A `.base` file shown on its own, every view (`None` if there is no such file).
pub fn base_file_html(v: &Vault, path: &str, o: &Opts) -> Option<String> {
    let src = v.read_text(path)?;
    let mut cx = new_cx(Some(v), path, o);
    let mut out = String::new();
    base_html(
        &mut cx,
        v,
        query::base(&src),
        "base",
        None,
        Some(path),
        &mut out,
    );
    Some(out)
}

/// The text after the frontmatter, and the 1-based line it starts on.
fn body_of(src: &str) -> (&str, usize) {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    match parse::split_frontmatter(src) {
        Some((_, _, b)) => {
            let b = b.min(src.len());
            (&src[b..], src[..b].matches('\n').count() + 1)
        }
        None => (src, 1),
    }
}

type Lines = Vec<(usize, String)>;
type Ls = [(usize, String)];

/// Lines with `%%comments%%` removed (outside code fences) and leading tabs as four spaces.
fn prepare(text: &str, first_line: usize) -> Lines {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut in_comment = false;
    for (i, raw) in text.split('\n').enumerate() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let line = expand_tabs(raw);
        let t = line.trim_start();
        if !in_comment {
            if let Some((fc, n)) = fence {
                if fence_of(t).is_some_and(|(c, m, info)| c == fc && m >= n && info.is_empty()) {
                    fence = None;
                }
                out.push((first_line + i, line));
                continue;
            }
            if let Some((c, n, _)) = fence_of(t) {
                fence = Some((c, n));
                out.push((first_line + i, line));
                continue;
            }
        }
        let mut kept = String::new();
        let mut rest = line.as_str();
        loop {
            if in_comment {
                match rest.find("%%") {
                    Some(p) => {
                        rest = &rest[p + 2..];
                        in_comment = false;
                    }
                    None => break,
                }
            } else {
                match rest.find("%%") {
                    Some(p) => {
                        kept.push_str(&rest[..p]);
                        rest = &rest[p + 2..];
                        in_comment = true;
                    }
                    None => {
                        kept.push_str(rest);
                        break;
                    }
                }
            }
        }
        out.push((first_line + i, kept));
    }
    out
}

fn expand_tabs(s: &str) -> String {
    let lead = s.len() - s.trim_start_matches([' ', '\t']).len();
    if !s[..lead].contains('\t') {
        return s.to_string();
    }
    let mut o = String::new();
    for c in s[..lead].chars() {
        if c == '\t' {
            o.push_str(&" ".repeat(4 - o.len() % 4));
        } else {
            o.push(' ');
        }
    }
    o.push_str(&s[lead..]);
    o
}

fn indent_of(s: &str) -> usize {
    s.len() - s.trim_start_matches(' ').len()
}

/// A code fence (three or more backticks or tildes): (char, run length, info string).
fn fence_of(t: &str) -> Option<(char, usize, &str)> {
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|x| *x == c).count();
    if n < 3 {
        return None;
    }
    let info = t[n..].trim();
    if c == '`' && info.contains('`') {
        return None;
    }
    Some((c, n, info))
}

fn heading_of(t: &str) -> Option<(usize, &str)> {
    let n = t.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&n) {
        return None;
    }
    let rest = &t[n..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let mut text = rest.trim();
    // closing #s
    let stripped = text.trim_end_matches('#');
    if stripped.len() != text.len() && (stripped.is_empty() || stripped.ends_with(' ')) {
        text = stripped.trim_end();
    }
    Some((n, text))
}

fn is_hr(t: &str) -> bool {
    let c = match t.chars().next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    t.chars().filter(|x| !x.is_whitespace()).all(|x| x == c)
        && t.chars().filter(|x| *x == c).count() >= 3
}

/// A list marker: (ordered start number or None, width of marker plus following space).
fn list_marker(t: &str) -> Option<(Option<u64>, usize)> {
    let b = t.as_bytes();
    match b.first()? {
        b'-' | b'*' | b'+' => {
            if b.len() == 1 || b[1] == b' ' {
                Some((None, if b.len() == 1 { 1 } else { 2 }))
            } else {
                None
            }
        }
        b'0'..=b'9' => {
            let d = b.iter().take_while(|c| c.is_ascii_digit()).count();
            if d > 9 || !matches!(b.get(d), Some(b'.') | Some(b')')) {
                return None;
            }
            if b.len() == d + 1 || b[d + 1] == b' ' {
                Some((
                    t[..d].parse().ok(),
                    if b.len() == d + 1 { d + 1 } else { d + 2 },
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn is_list_item(line: &str) -> bool {
    let t = line.trim_start();
    !is_hr(t) && list_marker(t).is_some()
}

fn split_cells(row: &str) -> Vec<String> {
    let t = row.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !t.ends_with("\\|") {
        &t[..t.len() - 1]
    } else {
        t
    };
    let mut cells = vec![];
    let mut cur = String::new();
    let mut code = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cur.push_str("\\|");
            }
            '`' => {
                code = !code;
                cur.push(c);
            }
            '|' if !code => cells.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    cells.push(cur);
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

fn delim_row(row: &str) -> Option<Vec<&'static str>> {
    if !row.contains('-') {
        return None;
    }
    let cells = split_cells(row);
    let mut al = vec![];
    for c in &cells {
        let c = c.trim();
        let core = c.trim_start_matches(':').trim_end_matches(':');
        if core.is_empty() || !core.chars().all(|x| x == '-') {
            return None;
        }
        al.push(match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => "center",
            (false, true) => "right",
            (true, false) => "left",
            _ => "",
        });
    }
    Some(al)
}

fn table_start(lines: &Ls, i: usize) -> bool {
    lines[i].1.contains('|')
        && lines
            .get(i + 1)
            .is_some_and(|(_, l)| l.contains('|') && delim_row(l).is_some())
}

fn footnote_def(t: &str) -> Option<(String, String)> {
    let r = t.strip_prefix("[^")?;
    let close = r.find("]:")?;
    let label = &r[..close];
    if label.is_empty() || label.contains([' ', ']']) {
        return None;
    }
    Some((label.to_string(), r[close + 2..].trim().to_string()))
}

/// A whole document (or embedded note): blocks plus its footnotes.
fn doc(cx: &mut Cx, text: &str, first_line: usize) -> String {
    let lines = prepare(text, first_line);
    let saved_defs = std::mem::take(&mut cx.fn_defs);
    let saved_order = std::mem::take(&mut cx.fn_order);
    let mut kept = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut fence: Option<(char, usize)> = None;
    while i < lines.len() {
        let t = lines[i].1.trim_start();
        if let Some((fc, n)) = fence {
            if fence_of(t).is_some_and(|(c, m, info)| c == fc && m >= n && info.is_empty()) {
                fence = None;
            }
        } else if let Some((c, n, _)) = fence_of(t) {
            fence = Some((c, n));
        } else if let Some((label, first)) = footnote_def(t) {
            let mut body = first;
            while i + 1 < lines.len()
                && indent_of(&lines[i + 1].1) >= 4
                && !lines[i + 1].1.trim().is_empty()
            {
                i += 1;
                body.push('\n');
                body.push_str(lines[i].1.trim());
            }
            cx.fn_defs.entry(label).or_insert(body);
            i += 1;
            continue;
        }
        kept.push(lines[i].clone());
        i += 1;
    }
    let mut out = String::new();
    blocks(cx, &kept, &mut out);
    if !cx.fn_order.is_empty() {
        out.push_str("<section class=\"footnotes\"><ol>");
        for (n, (_, html)) in cx.fn_order.iter().enumerate() {
            let n = n + 1;
            out.push_str(&format!("<li id=\"fn-{n}\">{html} <a class=\"fn-back\" href=\"#fnref-{n}\" aria-label=\"back\">&#8617;</a></li>"));
        }
        out.push_str("</ol></section>");
    }
    cx.fn_defs = saved_defs;
    cx.fn_order = saved_order;
    out
}

fn block_id(text: &str) -> (&str, Option<&str>) {
    let t = text.trim_end();
    if let Some(p) = t.rfind(" ^") {
        let id = &t[p + 2..];
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return (t[..p].trim_end(), Some(id));
        }
    }
    if let Some(id) = t.strip_prefix('^') {
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return ("", Some(id));
        }
    }
    (text, None)
}

fn starts_block(lines: &Ls, i: usize) -> bool {
    let t = lines[i].1.trim_start();
    t.is_empty()
        || fence_of(t).is_some()
        || heading_of(t).is_some()
        || t.starts_with('>')
        || is_hr(t)
        || is_list_item(&lines[i].1)
        || t.starts_with("$$")
        || table_start(lines, i)
}

fn blocks(cx: &mut Cx, lines: &Ls, out: &mut String) {
    let mut i = 0;
    while i < lines.len() {
        let (ln, line) = (&lines[i].0, &lines[i].1);
        let t = line.trim_start();
        if t.is_empty() {
            i += 1;
            continue;
        }
        if let Some((c, n, info)) = fence_of(t) {
            let ind = indent_of(line);
            let mut body = vec![];
            i += 1;
            while i < lines.len() {
                let tt = lines[i].1.trim_start();
                if fence_of(tt).is_some_and(|(c2, m, inf)| c2 == c && m >= n && inf.is_empty()) {
                    i += 1;
                    break;
                }
                let l = &lines[i].1;
                let cut = indent_of(l).min(ind);
                body.push(l[cut..].to_string());
                i += 1;
            }
            code_block(cx, info, &body.join("\n"), out);
            continue;
        }
        if let Some(rest) = t.strip_prefix("$$") {
            let mut tex = String::new();
            if let Some(inner) = rest.trim_end().strip_suffix("$$") {
                tex.push_str(inner);
                i += 1;
            } else {
                tex.push_str(rest);
                i += 1;
                while i < lines.len() {
                    let l = lines[i].1.trim_end();
                    i += 1;
                    if let Some(before) = l.strip_suffix("$$") {
                        tex.push('\n');
                        tex.push_str(before);
                        break;
                    }
                    tex.push('\n');
                    tex.push_str(l);
                }
            }
            out.push_str(&crate::math::mathml(tex.trim(), true));
            continue;
        }
        if let Some((level, text)) = heading_of(t) {
            heading(cx, level, text, *ln, out);
            i += 1;
            continue;
        }
        if is_hr(t) {
            out.push_str("<hr>");
            i += 1;
            continue;
        }
        if t.starts_with('>') {
            let mut inner: Lines = vec![];
            while i < lines.len() {
                let tt = lines[i].1.trim_start();
                if let Some(r) = tt.strip_prefix('>') {
                    inner.push((lines[i].0, r.strip_prefix(' ').unwrap_or(r).to_string()));
                    i += 1;
                } else {
                    break;
                }
            }
            quote(cx, inner, out);
            continue;
        }
        if table_start(lines, i) {
            let header = split_cells(&lines[i].1);
            let al = delim_row(&lines[i + 1].1).unwrap_or_default();
            i += 2;
            out.push_str("<div class=\"table-wrap\"><table><thead><tr>");
            for (k, h) in header.iter().enumerate() {
                let a = al.get(k).copied().unwrap_or("");
                let st = if a.is_empty() {
                    String::new()
                } else {
                    format!(" class=\"al-{a}\"")
                };
                out.push_str(&format!(
                    "<th{st}>{}</th>",
                    inline(cx, &h.replace("\\|", "|"))
                ));
            }
            out.push_str("</tr></thead><tbody>");
            while i < lines.len() && lines[i].1.contains('|') && !lines[i].1.trim().is_empty() {
                let cells = split_cells(&lines[i].1);
                out.push_str("<tr>");
                for k in 0..header.len() {
                    let a = al.get(k).copied().unwrap_or("");
                    let st = if a.is_empty() {
                        String::new()
                    } else {
                        format!(" class=\"al-{a}\"")
                    };
                    let c = cells
                        .get(k)
                        .map(|c| c.replace("\\|", "|"))
                        .unwrap_or_default();
                    out.push_str(&format!("<td{st}>{}</td>", inline(cx, &c)));
                }
                out.push_str("</tr>");
                i += 1;
            }
            out.push_str("</tbody></table></div>");
            continue;
        }
        if is_list_item(line) {
            i = list(cx, lines, i, out);
            continue;
        }
        // paragraph (or a setext heading: text underlined with === or ---)
        let start = i;
        let mut para: Vec<&str> = vec![];
        let mut setext = 0;
        while i < lines.len()
            && (i == start || !starts_block(lines, i) || setext_level(&lines[i].1).is_some())
        {
            if i > start {
                if let Some(l) = setext_level(&lines[i].1) {
                    setext = l;
                    i += 1;
                    break;
                }
            }
            para.push(lines[i].1.trim());
            i += 1;
        }
        if setext > 0 {
            let text = para.join(" ");
            heading(cx, setext, &text, lines[start].0, out);
            continue;
        }
        paragraph(cx, lines[start].0, &para, out);
    }
}

fn setext_level(line: &str) -> Option<usize> {
    let t = line.trim();
    if indent_of(line) > 3 || t.is_empty() {
        return None;
    }
    if t.chars().all(|c| c == '=') {
        Some(1)
    } else if t.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

fn heading(cx: &mut Cx, level: usize, text: &str, ln: usize, out: &mut String) {
    let (text, bid) = block_id(text);
    let mut id = slug(text);
    let n = cx.ids.entry(id.clone()).or_insert(0);
    *n += 1;
    if *n > 1 {
        id = format!("{id}-{}", *n - 1);
    }
    let inner = inline(cx, text);
    let extra = bid
        .map(|b| format!(" data-block=\"{}\"", esc(b)))
        .unwrap_or_default();
    out.push_str(&format!(
        "<h{level} id=\"{}\" data-heading=\"{}\" data-line=\"{ln}\"{extra}>{inner}</h{level}>",
        esc(&id),
        esc(text)
    ));
}

fn paragraph(cx: &mut Cx, ln: usize, para: &[&str], out: &mut String) {
    let last = para.len() - 1;
    let (last_text, bid) = block_id(para[last]);
    let mut texts: Vec<&str> = para[..last].to_vec();
    if !last_text.is_empty() {
        texts.push(last_text);
    }
    if texts.is_empty() {
        if let Some(b) = bid {
            out.push_str(&format!(
                "<span class=\"block-anchor\" id=\"^{}\"></span>",
                esc(b)
            ));
        }
        return;
    }
    let id = bid
        .map(|b| format!(" id=\"^{}\" data-block=\"{}\"", esc(b), esc(b)))
        .unwrap_or_default();
    // A paragraph that is just one `![[embed]]` is a block embed (whole notes are transcluded).
    if texts.len() == 1 {
        let t = texts[0];
        if t.starts_with("![[") && t.ends_with("]]") && t[3..t.len() - 2].find("]]").is_none() {
            if let Some(l) = parse::wiki_inner(&t[3..t.len() - 2], true) {
                let e = embed(cx, &l, true);
                out.push_str(&format!(
                    "<div class=\"embed-block\" data-line=\"{ln}\"{id}>{e}</div>"
                ));
                return;
            }
        }
    }
    let parts: Vec<String> = texts.iter().map(|t| inline(cx, t)).collect();
    out.push_str(&format!(
        "<p data-line=\"{ln}\"{id}>{}</p>",
        parts.join("<br>\n")
    ));
}

fn quote(cx: &mut Cx, inner: Lines, out: &mut String) {
    let first = inner
        .first()
        .map(|l| l.1.trim().to_string())
        .unwrap_or_default();
    if let Some(rest) = first.strip_prefix("[!") {
        if let Some(close) = rest.find(']') {
            let kind = rest[..close].trim().to_lowercase();
            let after = &rest[close + 1..];
            let (fold, title) = match after.chars().next() {
                Some('+') => (Some(true), after[1..].trim()),
                Some('-') => (Some(false), after[1..].trim()),
                _ => (None, after.trim()),
            };
            let kind: String = kind
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-')
                .collect();
            let kind = if kind.is_empty() {
                "note".to_string()
            } else {
                kind
            };
            let title_html = if title.is_empty() {
                let mut c = kind.chars();
                esc(&c
                    .next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default())
            } else {
                inline(cx, title)
            };
            let mut body = String::new();
            blocks(cx, &inner[1..], &mut body);
            match fold {
                Some(open) => out.push_str(&format!(
                    "<details class=\"callout\" data-callout=\"{}\"{}><summary class=\"callout-title\">{title_html}</summary><div class=\"callout-content\">{body}</div></details>",
                    esc(&kind),
                    if open { " open" } else { "" }
                )),
                None => out.push_str(&format!("<div class=\"callout\" data-callout=\"{}\"><div class=\"callout-title\">{title_html}</div><div class=\"callout-content\">{body}</div></div>", esc(&kind))),
            }
            return;
        }
    }
    let mut body = String::new();
    blocks(cx, &inner, &mut body);
    out.push_str(&format!("<blockquote>{body}</blockquote>"));
}

struct Item {
    ln: usize,
    first: String,
    children: Lines,
}

/// A list starting at line `i`; returns the index after it.
fn list(cx: &mut Cx, lines: &Ls, mut i: usize, out: &mut String) -> usize {
    let base = indent_of(&lines[i].1);
    let (start_num, _) = list_marker(lines[i].1.trim_start()).unwrap_or((None, 2));
    let ordered = start_num.is_some();
    let mut items: Vec<Item> = vec![];
    let mut content_indent = base + 2;
    let mut blank_run = false;
    while i < lines.len() {
        let (ln, l) = (&lines[i].0, &lines[i].1);
        if l.trim().is_empty() {
            // a blank line continues the list only if more of it follows
            let next = lines[i + 1..].iter().find(|x| !x.1.trim().is_empty());
            let cont = next.is_some_and(|x| {
                let ind = indent_of(&x.1);
                ind >= content_indent
                    || (ind >= base
                        && ind < content_indent
                        && is_list_item(&x.1)
                        && list_marker(x.1.trim_start()).is_some_and(|m| m.0.is_some() == ordered))
            });
            if !cont {
                break;
            }
            if let Some(it) = items.last_mut() {
                it.children.push((*ln, String::new()));
            }
            blank_run = true;
            i += 1;
            continue;
        }
        let ind = indent_of(l);
        if is_list_item(l) && ind < content_indent {
            let t = l.trim_start();
            let (num, w) = list_marker(t).unwrap_or((None, 2));
            if ind < base || num.is_some() != ordered {
                break;
            }
            content_indent = ind + w;
            items.push(Item {
                ln: *ln,
                first: t[w.min(t.len())..].to_string(),
                children: vec![],
            });
            blank_run = false;
            i += 1;
            continue;
        }
        if ind >= content_indent || (ind > base && !items.is_empty()) {
            let cut = ind.min(content_indent);
            if let Some(it) = items.last_mut() {
                it.children.push((*ln, l[cut..].to_string()));
            }
            i += 1;
            continue;
        }
        // lazy continuation of the item's first paragraph
        if !blank_run && !items.is_empty() && !starts_block(lines, i) && ind >= base {
            if let Some(it) = items.last_mut() {
                if it.children.is_empty() {
                    it.first.push('\n');
                    it.first.push_str(l.trim());
                } else {
                    it.children.push((*ln, l.trim().to_string()));
                }
            }
            i += 1;
            continue;
        }
        break;
    }
    let tag = if ordered { "ol" } else { "ul" };
    match start_num {
        Some(n) if n != 1 => out.push_str(&format!("<ol start=\"{n}\">")),
        _ => out.push_str(&format!("<{tag}>")),
    }
    for it in items {
        let (text, bid) = block_id(&it.first);
        let id = bid
            .map(|b| format!(" id=\"^{}\" data-block=\"{}\"", esc(b), esc(b)))
            .unwrap_or_default();
        let mut task: Option<char> = None;
        let mut text = text.to_string();
        let b = text.as_bytes();
        if b.len() >= 3
            && b[0] == b'['
            && b[1].is_ascii()
            && b[2] == b']'
            && (b.len() == 3 || b[3] == b' ')
        {
            task = Some(char::from(b[1]));
            text = text[3..].trim_start().to_string();
        }
        let body: String = text
            .split('\n')
            .map(|t| inline(cx, t))
            .collect::<Vec<_>>()
            .join("<br>\n");
        let mut kids = String::new();
        if !it.children.is_empty() {
            blocks(cx, &it.children, &mut kids);
        }
        match task {
            Some(c) => {
                let done = c == 'x' || c == 'X';
                let status = if c == ' ' {
                    "todo".to_string()
                } else {
                    esc(&c.to_string())
                };
                let dis = if cx.o.interactive { "" } else { " disabled" };
                out.push_str(&format!(
                    "<li class=\"task{}\" data-task=\"{status}\" data-line=\"{}\"{id}><input type=\"checkbox\" class=\"task-box\" data-path=\"{}\" data-line=\"{}\"{}{dis}> <span class=\"task-text\">{body}</span>{kids}</li>",
                    if done { " done" } else { "" },
                    it.ln,
                    esc(&cx.from),
                    it.ln,
                    if done { " checked" } else { "" },
                ));
            }
            None => out.push_str(&format!(
                "<li data-line=\"{}\"{id}>{body}{kids}</li>",
                it.ln
            )),
        }
    }
    out.push_str(&format!("</{tag}>"));
    i
}

fn code_block(cx: &mut Cx, info: &str, body: &str, out: &mut String) {
    let lang: String = info
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
        .collect();
    let lang = lang.to_lowercase();
    if lang == "mermaid" {
        out.push_str(&crate::diagram::mermaid_html(body));
        return;
    }
    if let Some(v) = cx.v {
        if lang == "query" {
            return query_block(cx, v, body.trim(), out);
        }
        if lang == "tasks" {
            return tasks_block(cx, v, body, out);
        }
        if lang == "base" {
            return base_html(cx, v, query::base(body), "base", None, None, out);
        }
        if lang == "dataview" {
            return base_html(cx, v, query::dataview(body), "dataview", None, None, out);
        }
    }
    let cls = if lang.is_empty() {
        String::new()
    } else {
        format!(" class=\"language-{lang}\"")
    };
    let label = if lang.is_empty() {
        String::new()
    } else {
        format!(" data-lang=\"{lang}\"")
    };
    out.push_str(&format!(
        "<pre{label}><code{cls}>{}</code></pre>",
        esc(body)
    ));
}

fn query_block(cx: &mut Cx, v: &Vault, q: &str, out: &mut String) {
    let hits = v.search(q, 50);
    out.push_str(&format!("<div class=\"query\"><div class=\"query-head\"><span class=\"query-label\">query</span> <code>{}</code> <span class=\"query-count\">{} found</span></div><ul>", esc(q), hits.len()));
    for h in hits {
        out.push_str(&format!(
            "<li><a class=\"internal\" href=\"{}\" data-path=\"{}\">{}</a> <span class=\"query-snippet\">{}</span></li>",
            esc(&(cx.o.note_href)(&h.path, None)),
            esc(&h.path),
            esc(&h.title),
            esc(&h.snippet)
        ));
    }
    out.push_str("</ul></div>");
}

/// A `tasks` block: open tasks across the vault, filtered by simple rules, one per line:
/// `done`, `not done`, `due before|after|on YYYY-MM-DD`, `due today`, `has due date`, `no due date`,
/// `path includes x`, `tag includes x`, `text includes x`, `limit n`.
type TaskPred = Box<dyn Fn(&str, &crate::vault::Note, &parse::Task) -> bool>;

/// `today`, `tomorrow`, `yesterday` or a date as written.
fn rel_day(d: &str, today: &str) -> String {
    let shift = match d {
        "today" => 0,
        "tomorrow" => 1,
        "yesterday" => -1,
        _ => return d.to_string(),
    };
    match parse_date(today) {
        Some((y, m, dd)) => {
            let (y, m, dd) =
                crate::util::civil_from_days(crate::util::days_from_civil(y, m, dd) + shift);
            format!("{y:04}-{m:02}-{dd:02}")
        }
        None => today.to_string(),
    }
}

pub fn task_query(v: &Vault, rules: &str, today: &str) -> Vec<(String, parse::Task)> {
    let mut done: Option<bool> = Some(false);
    let mut limit = 200usize;
    let mut preds: Vec<TaskPred> = vec![];
    for r in rules
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty() && !r.starts_with('#'))
    {
        let low = r.to_lowercase();
        let arg = |p: &str| low.strip_prefix(p).map(|a| a.trim().to_string());
        if low == "done" {
            done = Some(true);
        } else if low == "not done" {
            done = Some(false);
        } else if low == "all" {
            done = None;
        } else if low == "has due date" {
            preds.push(Box::new(|_, _, t| t.due.is_some()));
        } else if low == "no due date" {
            preds.push(Box::new(|_, _, t| t.due.is_none()));
        } else if let Some(d) = arg("due before") {
            let d = rel_day(&d, today);
            preds.push(Box::new(move |_, _, t| {
                t.due.as_deref().is_some_and(|x| x < d.as_str())
            }));
        } else if let Some(d) = arg("due after") {
            let d = rel_day(&d, today);
            preds.push(Box::new(move |_, _, t| {
                t.due.as_deref().is_some_and(|x| x > d.as_str())
            }));
        } else if let Some(d) =
            arg("due on").or_else(|| (low == "due today").then(|| today.to_string()))
        {
            let d = rel_day(&d, today);
            preds.push(Box::new(move |_, _, t| {
                t.due.as_deref() == Some(d.as_str())
            }));
        } else if let Some(s) = arg("path includes") {
            preds.push(Box::new(move |p, _, _| p.to_lowercase().contains(&s)));
        } else if let Some(s) = arg("tag includes") {
            let s = s.trim_start_matches('#').to_string();
            preds.push(Box::new(move |_, n, _| Vault::has_tag(n, &s)));
        } else if let Some(s) = arg("text includes") {
            preds.push(Box::new(move |_, _, t| t.text.to_lowercase().contains(&s)));
        } else if let Some(n) = arg("limit") {
            limit = n.parse().unwrap_or(limit);
        }
    }
    let mut out = vec![];
    for n in v.notes() {
        for t in &n.parsed.tasks {
            if done.is_some_and(|d| t.done != d) {
                continue;
            }
            if !t.text.trim().is_empty() && preds.iter().all(|p| p(&n.path, n, t)) {
                out.push((n.path.clone(), t.clone()));
            }
        }
    }
    out.sort_by(|a, b| {
        let da = a.1.due.as_deref().unwrap_or("9999");
        let db = b.1.due.as_deref().unwrap_or("9999");
        da.cmp(db)
            .then_with(|| a.0.cmp(&b.0))
            .then(a.1.line.cmp(&b.1.line))
    });
    out.truncate(limit);
    out
}

fn tasks_block(cx: &mut Cx, v: &Vault, rules: &str, out: &mut String) {
    let today = crate::util::date_of(
        crate::util::unix_now(),
        crate::util::local_offset_minutes(None),
    );
    let found = task_query(v, rules, &today);
    out.push_str(&format!("<div class=\"query tasks-query\"><div class=\"query-head\"><span class=\"query-label\">tasks</span> <span class=\"query-count\">{} found</span></div><ul>", found.len()));
    let dis = if cx.o.interactive { "" } else { " disabled" };
    for (p, t) in found {
        let saved = std::mem::replace(&mut cx.from, p.clone());
        let text = inline(cx, &t.text);
        cx.from = saved;
        let due = t.due.as_deref().map(|d| {
            let late = parse_date(d).is_some() && d < today.as_str() && !t.done;
            format!(
                " <span class=\"due{}\">{}</span>",
                if late { " late" } else { "" },
                esc(d)
            )
        });
        out.push_str(&format!(
            "<li class=\"task{}\"><input type=\"checkbox\" class=\"task-box\" data-path=\"{}\" data-line=\"{}\"{}{dis}> <span class=\"task-text\">{text}</span>{} <a class=\"internal task-src\" href=\"{}\" data-path=\"{}\">{}</a></li>",
            if t.done { " done" } else { "" },
            esc(&p),
            t.line,
            if t.done { " checked" } else { "" },
            due.unwrap_or_default(),
            esc(&(cx.o.note_href)(&p, None)),
            esc(&p),
            esc(crate::util::stem(&p)),
        ));
    }
    out.push_str("</ul></div>");
}

// ---- bases and Dataview views

/// One value in a cell: links and Markdown in text render as they would in the note.
fn cell_html(cx: &mut Cx, val: &query::Val) -> String {
    use query::Val;
    match val {
        Val::Null => String::new(),
        Val::Bool(b) => format!(
            "<span class=\"base-bool{}\" aria-label=\"{b}\">{}</span>",
            if *b { " on" } else { "" },
            if *b { "\u{2713}" } else { "\u{2013}" }
        ),
        Val::Num(_) => format!("<span class=\"base-num\">{}</span>", esc(&val.text())),
        Val::Str(s) => inline(cx, s),
        Val::List(l) => l
            .iter()
            .map(|x| cell_html(cx, x))
            .filter(|s| !s.is_empty())
            .map(|s| format!("<span class=\"base-item\">{s}</span>"))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// A card's picture: a vault file (`[[cover.png]]` or its path) or an https address.
fn card_image(cx: &Cx, v: &Vault, from: &str, val: &query::Val) -> Option<String> {
    let t = val.text();
    let t = t.trim().trim_start_matches('!');
    if t.starts_with("https://") && !t.contains(['"', '<', '>', ' ']) {
        return Some(t.to_string());
    }
    let inner = t
        .strip_prefix("[[")
        .and_then(|x| x.strip_suffix("]]"))
        .unwrap_or(t);
    let link = parse::wiki_inner(inner, true)?;
    let p = v.resolve(from, &link)?;
    (v.note(&p).is_none() && IMAGE.contains(&ext(&p).as_str())).then(|| (cx.o.file_src)(&p))
}

fn note_link(cx: &Cx, v: &Vault, p: &str, class: &str) -> String {
    let title = v
        .note(p)
        .map(|n| n.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| crate::util::stem(p).to_string());
    format!(
        "<a class=\"internal {class}\" href=\"{}\" data-path=\"{}\">{}</a>",
        esc(&(cx.o.note_href)(p, None)),
        esc(p),
        esc(&title)
    )
}

/// A ```` ```base ```` or ```` ```dataview ```` block, or an embedded `.base` file. `pick` keeps
/// one view by name; `file` is the `.base` file shown, if any.
fn base_html(
    cx: &mut Cx,
    v: &Vault,
    parsed: Result<query::Base, String>,
    label: &str,
    pick: Option<&str>,
    file: Option<&str>,
    out: &mut String,
) {
    let title = file.map(|f| {
        let name = file_name(f);
        format!(
            " <a class=\"internal base-file\" href=\"{}\" data-path=\"{}\">{}</a>",
            esc(&(cx.o.note_href)(f, None)),
            esc(f),
            esc(name.rsplit_once('.').map_or(name, |(s, _)| s))
        )
    });
    let head = format!(
        "<div class=\"base-head\"><span class=\"query-label\">{}</span>{}</div>",
        esc(label),
        title.unwrap_or_default()
    );
    let mut b = match parsed {
        Ok(b) => b,
        Err(e) => {
            out.push_str(&format!("<div class=\"base base-error\">{head}<p class=\"base-problem\">This {} cannot be read: {}</p></div>", esc(label), esc(&e)));
            return;
        }
    };
    if let Some(name) = pick.map(|p| p.trim().to_lowercase()) {
        if let Some(i) = b.views.iter().position(|x| x.name.to_lowercase() == name) {
            b.views = vec![b.views.remove(i)];
        }
    }
    let outs = query::run(v, &b);
    out.push_str(&format!(
        "<div class=\"base\"{}>{head}",
        file.map(|f| format!(" data-file=\"{}\"", esc(f)))
            .unwrap_or_default()
    ));
    for (i, o) in outs.iter().enumerate() {
        let kind = match o.kind {
            query::Kind::Table => "table",
            query::Kind::Cards => "cards",
            query::Kind::List => "list",
            query::Kind::Task => "task",
            query::Kind::Board => "board",
        };
        out.push_str(&format!(
            "<section class=\"base-view\" data-view=\"{i}\" data-kind=\"{kind}\" data-name=\"{}\">",
            esc(&o.name)
        ));
        if outs.len() > 1 || !o.name.is_empty() {
            out.push_str(&format!(
                "<h4 class=\"base-view-name\">{}</h4>",
                esc(&o.name)
            ));
        }
        if let Some(e) = &o.error {
            out.push_str(&format!(
                "<p class=\"base-problem\">{}</p></section>",
                esc(e)
            ));
            continue;
        }
        let shown: usize = o.groups.iter().map(|g| g.1.len()).sum();
        let what = if o.kind == query::Kind::Task {
            "task"
        } else {
            "note"
        };
        let count = if shown < o.total {
            format!("{shown} of {} {what}s", o.total)
        } else {
            format!("{shown} {what}{}", if shown == 1 { "" } else { "s" })
        };
        out.push_str(&format!("<div class=\"base-meta\">{count}</div>"));
        match o.kind {
            query::Kind::Table => base_table(cx, v, o, out),
            query::Kind::Cards => {
                for (key, rows) in &o.groups {
                    group_head(key.as_deref(), rows.len(), out);
                    out.push_str("<div class=\"base-cards\">");
                    for r in rows {
                        base_card(cx, v, o, r, None, out);
                    }
                    out.push_str("</div>");
                }
            }
            query::Kind::Board => {
                out.push_str(&format!(
                    "<div class=\"base-board\"{}>",
                    o.group_prop
                        .as_deref()
                        .map(|p| format!(" data-prop=\"{}\"", esc(p)))
                        .unwrap_or_default()
                ));
                for (key, rows) in &o.groups {
                    let key = key.clone().unwrap_or_default();
                    out.push_str(&format!(
                        "<div class=\"base-lane\" data-lane=\"{}\"><div class=\"base-lane-head\">{}<span class=\"count\">{}</span></div><div class=\"base-lane-body\">",
                        esc(&key),
                        if key.is_empty() { "<em>No value</em>".to_string() } else { esc(&key) },
                        rows.len()
                    ));
                    for r in rows {
                        base_card(cx, v, o, r, o.group_prop.as_deref(), out);
                    }
                    out.push_str("</div></div>");
                }
                out.push_str("</div>");
            }
            query::Kind::List => {
                for (key, rows) in &o.groups {
                    group_head(key.as_deref(), rows.len(), out);
                    out.push_str("<ul class=\"base-list\">");
                    for r in rows {
                        let saved = std::mem::replace(&mut cx.from, r.path.clone());
                        let extra: Vec<String> = r
                            .cells
                            .iter()
                            .map(|c| cell_html(cx, c))
                            .filter(|s| !s.is_empty())
                            .collect();
                        cx.from = saved;
                        out.push_str(&format!(
                            "<li>{}{}</li>",
                            if o.with_id {
                                note_link(cx, v, &r.path, "")
                            } else {
                                String::new()
                            },
                            if extra.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    "<span class=\"base-extra\">{}</span>",
                                    extra.join(" \u{b7} ")
                                )
                            }
                        ));
                    }
                    out.push_str("</ul>");
                }
            }
            query::Kind::Task => {
                let today = crate::util::date_of(
                    crate::util::unix_now(),
                    crate::util::local_offset_minutes(None),
                );
                let dis = if cx.o.interactive { "" } else { " disabled" };
                for (key, rows) in &o.groups {
                    group_head(key.as_deref(), rows.len(), out);
                    out.push_str("<ul class=\"base-tasks\">");
                    for r in rows {
                        let Some(t) = &r.task else { continue };
                        let saved = std::mem::replace(&mut cx.from, r.path.clone());
                        let text = inline(cx, &t.text);
                        cx.from = saved;
                        let due = t.due.as_deref().map(|d| {
                            let late = parse_date(d).is_some() && d < today.as_str() && !t.done;
                            format!(
                                " <span class=\"due{}\">{}</span>",
                                if late { " late" } else { "" },
                                esc(d)
                            )
                        });
                        out.push_str(&format!(
                            "<li class=\"task{}\"><input type=\"checkbox\" class=\"task-box\" data-path=\"{}\" data-line=\"{}\"{}{dis}> <span class=\"task-text\">{text}</span>{} {}</li>",
                            if t.done { " done" } else { "" },
                            esc(&r.path),
                            t.line,
                            if t.done { " checked" } else { "" },
                            due.unwrap_or_default(),
                            note_link(cx, v, &r.path, "task-src"),
                        ));
                    }
                    out.push_str("</ul>");
                }
            }
        }
        out.push_str("</section>");
    }
    out.push_str("</div>");
}

fn group_head(key: Option<&str>, n: usize, out: &mut String) {
    if let Some(k) = key {
        out.push_str(&format!(
            "<div class=\"base-group-head\">{}<span class=\"count\">{n}</span></div>",
            if k.is_empty() {
                "<em>No value</em>".to_string()
            } else {
                esc(k)
            }
        ));
    }
}

fn base_table(cx: &mut Cx, v: &Vault, o: &query::ViewOut, out: &mut String) {
    let span = o.cols.len() + usize::from(o.with_id);
    out.push_str("<div class=\"base-scroll\"><table class=\"base-table\"><thead><tr>");
    if o.with_id {
        out.push_str("<th>Name</th>");
    }
    for c in &o.cols {
        out.push_str(&format!("<th>{}</th>", esc(c)));
    }
    out.push_str("</tr></thead><tbody>");
    for (key, rows) in &o.groups {
        if let Some(k) = key {
            out.push_str(&format!(
                "<tr class=\"base-group\"><th colspan=\"{span}\">{}<span class=\"count\">{}</span></th></tr>",
                if k.is_empty() { "<em>No value</em>".to_string() } else { esc(k) },
                rows.len()
            ));
        }
        for r in rows {
            out.push_str(&format!("<tr data-path=\"{}\">", esc(&r.path)));
            if o.with_id {
                out.push_str(&format!(
                    "<td class=\"base-name\">{}</td>",
                    note_link(cx, v, &r.path, "")
                ));
            }
            let saved = std::mem::replace(&mut cx.from, r.path.clone());
            let note = v.note(&r.path);
            for (i, c) in r.cells.iter().enumerate() {
                if o.name_cols.get(i).copied().unwrap_or(false) {
                    out.push_str(&format!(
                        "<td class=\"base-name\">{}</td>",
                        note_link(cx, v, &r.path, "")
                    ));
                    continue;
                }
                let prop = o
                    .col_props
                    .get(i)
                    .cloned()
                    .flatten()
                    .filter(|_| cx.o.interactive);
                let num = matches!(c, query::Val::Num(_));
                // a property cell carries the value as written (JSON), so editing it keeps links
                let edit = prop.map(|p| {
                    let raw = note
                        .and_then(|n| n.parsed.prop(&p))
                        .map_or(serde_json::Value::Null, parse::Prop::to_json);
                    format!(
                        " data-prop=\"{}\" data-raw=\"{}\"",
                        esc(&p),
                        esc(&raw.to_string())
                    )
                });
                out.push_str(&format!(
                    "<td{}{}>{}</td>",
                    if num { " class=\"num\"" } else { "" },
                    edit.unwrap_or_default(),
                    cell_html(cx, c)
                ));
            }
            cx.from = saved;
            out.push_str("</tr>");
        }
    }
    out.push_str("</tbody></table></div>");
}

/// A card: its picture, the note, then each column as a label and value. On a board the lane
/// property is left out (the lane says it) and the card can be dragged to another lane.
fn base_card(
    cx: &mut Cx,
    v: &Vault,
    o: &query::ViewOut,
    r: &query::Row,
    lane_prop: Option<&str>,
    out: &mut String,
) {
    let drag = lane_prop.is_some() && cx.o.interactive;
    out.push_str(&format!(
        "<article class=\"base-card\" data-path=\"{}\"{}>",
        esc(&r.path),
        if drag { " draggable=\"true\"" } else { "" }
    ));
    if let Some(src) = card_image(cx, v, &r.path, &r.image) {
        out.push_str(&format!(
            "<div class=\"base-card-img\"><img src=\"{}\" alt=\"\" loading=\"lazy\"></div>",
            esc(&src)
        ));
    }
    out.push_str(&note_link(cx, v, &r.path, "base-card-title"));
    let saved = std::mem::replace(&mut cx.from, r.path.clone());
    let mut dl = String::new();
    for (i, c) in r.cells.iter().enumerate() {
        let prop = o.col_props.get(i).cloned().flatten();
        if (lane_prop.is_some() && prop.as_deref() == lane_prop)
            || o.name_cols.get(i).copied().unwrap_or(false)
        {
            continue; // the lane, or the title above, already says it
        }
        let h = cell_html(cx, c);
        if !h.is_empty() {
            dl.push_str(&format!(
                "<dt>{}</dt><dd>{h}</dd>",
                esc(o.cols.get(i).map_or("", String::as_str))
            ));
        }
    }
    cx.from = saved;
    if !dl.is_empty() {
        out.push_str(&format!("<dl>{dl}</dl>"));
    }
    out.push_str("</article>");
}

// ---- canvases

/// Class and attributes for a card or arrow colour: presets by class, a hex by attribute
/// (`canvas::color_of` has already made sure it is `#` and hex digits).
fn cv_color(c: Option<&str>, attr: &str) -> (String, String) {
    match c {
        Some(p) if !p.starts_with('#') => (format!(" cv-c{p}"), String::new()),
        // the class tells the stylesheet to leave the colour attribute alone
        Some(h) => (" cv-hex".to_string(), format!(" {attr}=\"{h}\"")),
        None => (String::new(), String::new()),
    }
}

/// A canvas drawn as one SVG: groups, cards with their text rendered, arrows with labels. Only
/// attributes position things (no inline styles), so it works under the app's and published
/// pages' Content Security Policy.
fn canvas_view(
    cx: &mut Cx,
    v: &Vault,
    path: &str,
    parsed: Result<canvas::Canvas, String>,
    out: &mut String,
) {
    use canvas::Kind;
    let name = {
        let f = file_name(path);
        f.rsplit_once('.').map_or(f, |(s, _)| s).to_string()
    };
    let head = format!(
        "<div class=\"base-head\"><span class=\"query-label\">canvas</span> <a class=\"internal base-file\" href=\"{}\" data-path=\"{}\">{}</a></div>",
        esc(&(cx.o.note_href)(path, None)),
        esc(path),
        esc(&name)
    );
    let c = match parsed {
        Ok(c) => c,
        Err(e) => {
            out.push_str(&format!("<div class=\"canvas-view base-error\">{head}<p class=\"base-problem\">This canvas cannot be read: {}</p></div>", esc(&e)));
            return;
        }
    };
    let (bx, by, bw, bh) = canvas::bounds(&c);
    let pad = 40.0;
    out.push_str(&format!(
        "<figure class=\"canvas-view\" data-file=\"{}\">{head}<svg class=\"cv\" viewBox=\"{:.0} {:.0} {:.0} {:.0}\" role=\"img\" aria-label=\"{}\" xmlns=\"http://www.w3.org/2000/svg\">",
        esc(path),
        bx - pad,
        by - pad,
        bw + 2.0 * pad,
        bh + 2.0 * pad,
        esc(&name)
    ));
    let saved_from = std::mem::replace(&mut cx.from, path.to_string());
    let saved_ids = std::mem::take(&mut cx.ids);
    cx.stack.push(path.to_string());
    // groups first, behind everything
    for n in c.nodes.iter().filter(|n| n.kind == Kind::Group) {
        let (cls, attr) = cv_color(n.color.as_deref(), "stroke");
        out.push_str(&format!(
            "<g class=\"cv-group{cls}\"><rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"8\"{attr}/>",
            n.x, n.y, n.w, n.h
        ));
        if let Some(src) = n
            .background
            .as_deref()
            .and_then(|b| {
                parse::wiki_inner(b.trim_start_matches("[[").trim_end_matches("]]"), true)
            })
            .and_then(|l| v.resolve(path, &l))
            .filter(|p| IMAGE.contains(&ext(p).as_str()))
        {
            out.push_str(&format!(
                "<image class=\"cv-bg\" href=\"{}\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" preserveAspectRatio=\"xMidYMid slice\"/>",
                esc(&(cx.o.file_src)(&src)),
                n.x, n.y, n.w, n.h
            ));
        }
        if !n.label.is_empty() {
            out.push_str(&format!(
                "<text class=\"cv-glabel\" x=\"{:.1}\" y=\"{:.1}\">{}</text>",
                n.x + 4.0,
                n.y - 10.0,
                esc(&n.label)
            ));
        }
        out.push_str("</g>");
    }
    // arrows
    let by_id = |id: &str| c.nodes.iter().find(|n| n.id == id);
    for e in &c.edges {
        let (Some(a), Some(b)) = (by_id(&e.from), by_id(&e.to)) else {
            continue;
        };
        let (pa, pb) = (
            canvas::anchor(a, e.from_side, b),
            canvas::anchor(b, e.to_side, a),
        );
        let pts = canvas::edge_curve(pa, pb);
        let (cls, stroke) = cv_color(e.color.as_deref(), "stroke");
        let (_, fill) = cv_color(e.color.as_deref(), "fill");
        out.push_str(&format!(
            "<g class=\"cv-edge{cls}\"><path d=\"{}\"{stroke}/>",
            canvas::edge_path(pa, pb)
        ));
        if e.to_arrow {
            out.push_str(&format!(
                "<polygon points=\"{}\"{fill}/>",
                canvas::arrowhead(pts[2], pts[3])
            ));
        }
        if e.from_arrow {
            out.push_str(&format!(
                "<polygon points=\"{}\"{fill}/>",
                canvas::arrowhead(pts[1], pts[0])
            ));
        }
        if !e.label.is_empty() {
            // the middle of the curve (t = 0.5)
            let mx = (pts[0].0 + 3.0 * pts[1].0 + 3.0 * pts[2].0 + pts[3].0) / 8.0;
            let my = (pts[0].1 + 3.0 * pts[1].1 + 3.0 * pts[2].1 + pts[3].1) / 8.0;
            out.push_str(&format!("<text class=\"cv-elabel\" x=\"{mx:.1}\" y=\"{my:.1}\" text-anchor=\"middle\">{}</text>", esc(&e.label)));
        }
        out.push_str("</g>");
    }
    // cards
    for n in c.nodes.iter().filter(|n| n.kind != Kind::Group) {
        let (cls, attr) = cv_color(n.color.as_deref(), "stroke");
        let kind = match n.kind {
            Kind::Text => "text",
            Kind::File => "file",
            Kind::Link => "link",
            Kind::Group => "group",
        };
        out.push_str(&format!(
            "<g class=\"cv-node cv-{kind}{cls}\"><rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"6\"{attr}/>",
            n.x, n.y, n.w, n.h
        ));
        let target = (n.kind == Kind::File)
            .then(|| {
                let l = canvas::links(&canvas::Canvas {
                    nodes: vec![n.clone()],
                    edges: vec![],
                })
                .pop()?;
                (v.note(&n.file).is_some() || v.has_file(&n.file))
                    .then(|| n.file.clone())
                    .or_else(|| v.resolve(path, &l))
            })
            .flatten();
        let body = match (n.kind, &target) {
            (Kind::Text, _) => doc(cx, &n.text, 1),
            (Kind::File, Some(t)) if IMAGE.contains(&ext(t).as_str()) => {
                out.push_str(&format!(
                    "<image href=\"{}\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" preserveAspectRatio=\"xMidYMid meet\"/></g>",
                    esc(&(cx.o.file_src)(t)),
                    n.x, n.y, n.w, n.h
                ));
                continue;
            }
            (Kind::File, Some(t)) if v.note(t).is_some() => {
                let title = v.note(t).map(|x| x.title.clone()).unwrap_or_default();
                let link = format!(
                    "<a class=\"internal cv-title\" href=\"{}\" data-path=\"{}\">{}</a>",
                    esc(&(cx.o.note_href)(
                        t,
                        n.subpath.as_deref().map(|s| s.trim_start_matches('#'))
                    )),
                    esc(t),
                    esc(&title)
                );
                let inner = if cx.stack.len() > cx.o.max_depth || cx.stack.contains(t) {
                    String::new()
                } else {
                    let l = parse::wiki_inner(
                        &format!("{t}{}", n.subpath.as_deref().unwrap_or("")),
                        true,
                    );
                    let sec = v.read(t).and_then(|src| section(&src, l.as_ref()?));
                    match sec {
                        Some((text, first)) => {
                            let keep = std::mem::replace(&mut cx.from, t.clone());
                            cx.stack.push(t.clone());
                            let h = doc(cx, &text, first);
                            cx.stack.pop();
                            cx.from = keep;
                            h
                        }
                        None => String::new(),
                    }
                };
                format!("{link}{inner}")
            }
            (Kind::File, Some(t)) => format!(
                "<a class=\"embed-file\" href=\"{}\" data-file=\"{}\">{}</a>",
                esc(&(cx.o.file_src)(t)),
                esc(t),
                esc(file_name(t))
            ),
            (Kind::File, None) => format!(
                "<span class=\"embed unresolved\">{}</span>",
                esc(if n.file.is_empty() {
                    "(no file)"
                } else {
                    &n.file
                })
            ),
            (Kind::Link, _) => {
                let u = n.url.trim();
                if (u.starts_with("https://") || u.starts_with("http://"))
                    && !u.contains(['"', '<', '>', ' '])
                {
                    format!("<a class=\"external cv-url\" href=\"{}\" rel=\"noopener noreferrer\" target=\"_blank\">{}</a>", esc(u), esc(u))
                } else {
                    format!("<span class=\"cv-url\">{}</span>", esc(u))
                }
            }
            (Kind::Group, _) => String::new(),
        };
        out.push_str(&format!(
            "<foreignObject x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\"><div xmlns=\"http://www.w3.org/1999/xhtml\" class=\"cv-body\">{body}</div></foreignObject></g>",
            n.x, n.y, n.w, n.h
        ));
    }
    cx.stack.pop();
    cx.ids = saved_ids;
    cx.from = saved_from;
    out.push_str("</svg></figure>");
}

/// A `.canvas` file drawn on its own (`None` if there is no such file).
pub fn canvas_file_html(v: &Vault, path: &str, o: &Opts) -> Option<String> {
    let src = v.read_text(path)?;
    let mut cx = new_cx(Some(v), path, o);
    let mut out = String::new();
    canvas_view(&mut cx, v, path, canvas::parse(&src), &mut out);
    Some(out)
}

/// Every text card and note card of a canvas rendered (by card id), for the canvas editor.
pub fn canvas_cards_html(
    v: &Vault,
    path: &str,
    c: &canvas::Canvas,
    o: &Opts,
) -> Vec<(String, String)> {
    let mut cx = new_cx(Some(v), path, o);
    let mut out = Vec::new();
    for n in &c.nodes {
        let html = match n.kind {
            canvas::Kind::Text => doc(&mut cx, &n.text, 1),
            canvas::Kind::File => {
                let t = (v.note(&n.file).is_some())
                    .then(|| n.file.clone())
                    .or_else(|| {
                        let l = parse::wiki_inner(&n.file, true)?;
                        v.resolve(path, &l).filter(|p| v.note(p).is_some())
                    });
                match t {
                    Some(t) => {
                        let l = parse::wiki_inner(
                            &format!("{t}{}", n.subpath.as_deref().unwrap_or("")),
                            true,
                        );
                        match v.read(&t).and_then(|src| section(&src, l.as_ref()?)) {
                            Some((text, first)) => {
                                cx.from = t.clone();
                                cx.stack = vec![path.to_string(), t.clone()];
                                let h = doc(&mut cx, &text, first);
                                cx.from = path.to_string();
                                cx.stack = vec![path.to_string()];
                                h
                            }
                            None => String::new(),
                        }
                    }
                    None => continue,
                }
            }
            _ => continue,
        };
        out.push((n.id.clone(), html));
    }
    out
}

// ---- embeds

fn size_attrs(alias: Option<&str>) -> (String, Option<String>) {
    // `|300` or `|300x200` sets the size; anything else is alt text
    if let Some(a) = alias {
        let (w, h) = match a.split_once('x') {
            Some((w, h)) => (w.trim(), Some(h.trim())),
            None => (a.trim(), None),
        };
        if !w.is_empty()
            && w.chars().all(|c| c.is_ascii_digit())
            && h.is_none_or(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_digit()))
        {
            let mut s = format!(" width=\"{w}\"");
            if let Some(h) = h {
                s.push_str(&format!(" height=\"{h}\""));
            }
            return (s, None);
        }
        return (String::new(), Some(a.to_string()));
    }
    (String::new(), None)
}

fn file_embed(cx: &Cx, p: &str, alias: Option<&str>) -> String {
    let src = esc(&(cx.o.file_src)(p));
    let e = ext(p);
    let (size, alt) = size_attrs(alias);
    let name = esc(file_name(p));
    if IMAGE.contains(&e.as_str()) {
        let alt = alt.map(|a| esc(&a)).unwrap_or_else(|| name.clone());
        format!("<img class=\"embed-img\" src=\"{src}\" alt=\"{alt}\"{size} loading=\"lazy\" data-file=\"{}\">", esc(p))
    } else if AUDIO.contains(&e.as_str()) {
        format!("<audio class=\"embed-audio\" controls preload=\"metadata\" src=\"{src}\" data-file=\"{}\"></audio>", esc(p))
    } else if VIDEO.contains(&e.as_str()) {
        format!("<video class=\"embed-video\" controls preload=\"metadata\" src=\"{src}\"{size} data-file=\"{}\"></video>", esc(p))
    } else {
        format!("<a class=\"embed-file\" href=\"{src}\" data-file=\"{}\" target=\"_blank\" rel=\"noopener\"><span class=\"embed-ext\">{}</span> {name}</a>", esc(p), esc(&e))
    }
}

/// The part of a note an embed shows: a heading's section, a block, or the whole body.
fn section(src: &str, link: &Link) -> Option<(String, usize)> {
    let (body, first) = body_of(src);
    let lines: Vec<&str> = body.split('\n').collect();
    if let Some(h) = &link.heading {
        let want = h.trim().to_lowercase();
        let mut fence = false;
        let mut start: Option<(usize, usize)> = None;
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim_start();
            if fence_of(t).is_some() {
                fence = !fence;
                continue;
            }
            if fence {
                continue;
            }
            if let Some((lvl, text)) = heading_of(t) {
                match start {
                    None if text.trim().to_lowercase() == want || slug(text) == slug(&want) => {
                        start = Some((i, lvl))
                    }
                    Some((s, sl)) if lvl <= sl => return Some((lines[s..i].join("\n"), first + s)),
                    _ => {}
                }
            }
        }
        return start.map(|(s, _)| (lines[s..].join("\n"), first + s));
    }
    if let Some(b) = &link.block {
        let suffix = format!("^{b}");
        let at = lines.iter().position(|l| {
            let t = l.trim_end();
            t.ends_with(&format!(" {suffix}")) || t == suffix
        })?;
        let t = lines[at].trim_end();
        if t == suffix {
            // an id on its own line names the block above it
            let end = at;
            let mut s = end;
            while s > 0 && !lines[s - 1].trim().is_empty() {
                s -= 1;
            }
            return Some((lines[s..end].join("\n"), first + s));
        }
        if is_list_item(lines[at]) {
            let ind = indent_of(&expand_tabs(lines[at]));
            let mut e = at + 1;
            while e < lines.len()
                && !lines[e].trim().is_empty()
                && indent_of(&expand_tabs(lines[e])) > ind
            {
                e += 1;
            }
            return Some((lines[at..e].join("\n"), first + at));
        }
        let mut s = at;
        while s > 0 && !lines[s - 1].trim().is_empty() {
            s -= 1;
        }
        let mut e = at + 1;
        while e < lines.len() && !lines[e].trim().is_empty() {
            e += 1;
        }
        return Some((lines[s..e].join("\n"), first + s));
    }
    Some((body.to_string(), first))
}

fn embed(cx: &mut Cx, link: &Link, block: bool) -> String {
    let Some(v) = cx.v else {
        return format!(
            "<span class=\"embed unresolved\">{}</span>",
            esc(&link.target)
        );
    };
    let Some(p) = v.resolve(&cx.from, link) else {
        return format!(
            "<span class=\"embed unresolved\" data-new=\"{}\">{}</span>",
            esc(&link.target),
            esc(&link.target)
        );
    };
    if v.note(&p).is_none() {
        if block
            && ext(&p) == "canvas"
            && !cx.stack.contains(&p)
            && cx.stack.len() <= cx.o.max_depth
        {
            let parsed = v
                .read_text(&p)
                .ok_or_else(|| "the file cannot be read as text".to_string())
                .and_then(|s| canvas::parse(&s));
            let mut out = String::new();
            canvas_view(cx, v, &p, parsed, &mut out);
            return out;
        }
        if block && ext(&p) == "base" && !cx.stack.contains(&p) {
            // `![[Projects.base]]` shows the base; `![[Projects.base#Board]]` one of its views
            let parsed = v
                .read_text(&p)
                .ok_or_else(|| "the file cannot be read as text".to_string())
                .and_then(|s| query::base(&s));
            let mut out = String::new();
            cx.stack.push(p.clone());
            base_html(
                cx,
                v,
                parsed,
                "base",
                link.heading.as_deref(),
                Some(&p),
                &mut out,
            );
            cx.stack.pop();
            return out;
        }
        return file_embed(cx, &p, link.alias.as_deref());
    }
    let anchor = link
        .heading
        .clone()
        .or_else(|| link.block.as_ref().map(|b| format!("^{b}")));
    let title = v.note(&p).map(|n| n.title.clone()).unwrap_or_default();
    let label = match &anchor {
        Some(a) => format!("{} &gt; {}", esc(&title), esc(a.trim_start_matches('^'))),
        None => esc(&title),
    };
    let href = esc(&(cx.o.note_href)(&p, anchor.as_deref()));
    let head = format!(
        "<a class=\"internal embed-title\" href=\"{href}\" data-path=\"{}\">{label}</a>",
        esc(&p)
    );
    if !block || cx.stack.len() > cx.o.max_depth || cx.stack.contains(&p) {
        return format!("<span class=\"embed-link\">{head}</span>");
    }
    let Some(src) = v.read(&p) else { return head };
    let Some((text, first)) = section(&src, link) else {
        return format!("<span class=\"embed unresolved\">{label}</span>");
    };
    let saved_from = std::mem::replace(&mut cx.from, p.clone());
    let saved_ids = std::mem::take(&mut cx.ids);
    cx.stack.push(p.clone());
    let inner = doc(cx, &text, first);
    cx.stack.pop();
    cx.ids = saved_ids;
    cx.from = saved_from;
    format!("<div class=\"embed\" data-path=\"{}\"><div class=\"embed-head\">{head}</div><div class=\"embed-body\">{inner}</div></div>", esc(&p))
}

// ---- inline

fn starts(c: &[char], i: usize, pat: &str) -> bool {
    (i..).zip(pat.chars()).all(|(k, p)| c.get(k) == Some(&p))
}

fn find_from(c: &[char], from: usize, pat: &str) -> Option<usize> {
    (from..c.len()).find(|&k| starts(c, k, pat))
}

fn code_span(c: &[char], i: usize) -> Option<(String, usize)> {
    let n = c[i..].iter().take_while(|x| **x == '`').count();
    let mut k = i + n;
    while k < c.len() {
        if c[k] == '`' {
            let m = c[k..].iter().take_while(|x| **x == '`').count();
            if m == n {
                let mut s: String = c[i + n..k].iter().collect();
                if s.len() > 2 && s.starts_with(' ') && s.ends_with(' ') && !s.trim().is_empty() {
                    s = s[1..s.len() - 1].to_string();
                }
                return Some((s, k + m));
            }
            k += m;
        } else {
            k += 1;
        }
    }
    None
}

/// The closing position of an emphasis run `m` opened at `i`, if any.
fn emph_close(c: &[char], i: usize, m: &str) -> Option<usize> {
    let l = m.chars().count();
    let first = m.chars().next()?;
    let after = *c.get(i + l)?;
    if after.is_whitespace() || (l == 1 && after == first) {
        return None;
    }
    if first == '_' && i > 0 && c[i - 1].is_alphanumeric() {
        return None;
    }
    let mut j = i + l + 1;
    while j + l <= c.len() {
        if c[j] == '`' {
            if let Some((_, e)) = code_span(c, j) {
                j = e;
                continue;
            }
        }
        if l == 1 && c[j] == first && c.get(j + 1) == Some(&first) {
            // a `**` run inside `*...*` belongs to a nested strong: skip the whole run
            j += c[j..].iter().take_while(|x| **x == first).count();
            continue;
        }
        if starts(c, j, m) && !c[j - 1].is_whitespace() {
            if first == '_' && c.get(j + l).is_some_and(|x| x.is_alphanumeric()) {
                j += 1;
                continue;
            }
            return Some(j);
        }
        j += 1;
    }
    None
}

fn scheme_ok(dest: &str, mailto: bool) -> bool {
    let d = dest.trim().to_ascii_lowercase();
    d.starts_with("http://") || d.starts_with("https://") || (mailto && d.starts_with("mailto:"))
}

/// `(destination, title, index after ')')` for a link destination starting at `(`.
fn link_dest(c: &[char], open: usize) -> Option<(String, usize)> {
    let mut k = open + 1;
    while k < c.len() && c[k] == ' ' {
        k += 1;
    }
    let mut dest = String::new();
    if c.get(k) == Some(&'<') {
        k += 1;
        while k < c.len() && c[k] != '>' {
            if c[k] == '\n' {
                return None;
            }
            dest.push(c[k]);
            k += 1;
        }
        k += 1;
    } else {
        let mut depth = 0;
        while k < c.len() {
            let ch = c[k];
            if ch == ' ' {
                break;
            }
            if ch == '(' {
                depth += 1;
            }
            if ch == ')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            if ch == '\\' && k + 1 < c.len() {
                dest.push(c[k + 1]);
                k += 2;
                continue;
            }
            dest.push(ch);
            k += 1;
        }
    }
    while k < c.len() && c[k] == ' ' {
        k += 1;
    }
    if matches!(c.get(k), Some('"') | Some('\'')) {
        let q = c[k];
        k += 1;
        while k < c.len() && c[k] != q {
            k += 1;
        }
        k += 1;
        while k < c.len() && c[k] == ' ' {
            k += 1;
        }
    }
    (c.get(k) == Some(&')')).then_some((dest, k + 1))
}

fn matching_bracket(c: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut k = open;
    while k < c.len() {
        match c[k] {
            '\\' => k += 1,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            '`' => {
                if let Some((_, e)) = code_span(c, k) {
                    k = e;
                    continue;
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
}

fn wikilink(cx: &mut Cx, l: &Link) -> String {
    let display = match (&l.alias, l.target.is_empty()) {
        (Some(a), _) => inline_plain(cx, a),
        (None, true) => esc(l.heading.as_deref().or(l.block.as_deref()).unwrap_or("")),
        (None, false) => {
            let mut d = esc(&l.target);
            if let Some(h) = &l.heading {
                d.push_str(&format!(" &gt; {}", esc(h)));
            } else if let Some(b) = &l.block {
                d.push_str(&format!(" &gt; ^{}", esc(b)));
            }
            d
        }
    };
    let anchor = l
        .heading
        .clone()
        .or_else(|| l.block.as_ref().map(|b| format!("^{b}")));
    let data_anchor = anchor
        .as_ref()
        .map(|a| format!(" data-anchor=\"{}\"", esc(a)))
        .unwrap_or_default();
    match cx.v.and_then(|v| v.resolve(&cx.from, l)) {
        Some(p) if cx.v.is_some_and(|v| v.note(&p).is_some()) => {
            format!("<a class=\"internal\" href=\"{}\" data-path=\"{}\"{data_anchor}>{display}</a>", esc(&(cx.o.note_href)(&p, anchor.as_deref())), esc(&p))
        }
        Some(p) => format!("<a class=\"internal file\" href=\"{}\" data-file=\"{}\" target=\"_blank\" rel=\"noopener\">{display}</a>", esc(&(cx.o.file_src)(&p)), esc(&p)),
        None => format!("<a class=\"internal unresolved\" href=\"{}\" data-new=\"{}\">{display}</a>", esc(&(cx.o.note_href)(&format!("{}.md", l.target), None)), esc(&l.target)),
    }
}

/// Inline text inside a link (no nested links).
fn inline_plain(cx: &mut Cx, s: &str) -> String {
    let was = cx.in_link;
    cx.in_link = true;
    let h = inline(cx, s);
    cx.in_link = was;
    h
}

fn md_link(cx: &mut Cx, text: &str, dest: &str) -> String {
    let label = inline_plain(cx, text);
    let d = dest.trim();
    if d.is_empty() {
        return label;
    }
    if let Some(anchor) = d.strip_prefix('#') {
        let a = pct_decode(anchor);
        return format!(
            "<a class=\"internal\" href=\"{}\" data-path=\"{}\" data-anchor=\"{}\">{label}</a>",
            esc(&(cx.o.note_href)(&cx.from, Some(&a))),
            esc(&cx.from),
            esc(&a)
        );
    }
    if parse::has_scheme(d) {
        if scheme_ok(d, true) {
            return format!("<a class=\"external\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{label}</a>", esc(d));
        }
        return format!(
            "<span class=\"link-blocked\" title=\"{}\">{label}</span>",
            esc(d)
        );
    }
    let (path, anchor) = match d.split_once('#') {
        Some((p, a)) => (pct_decode(p), Some(pct_decode(a))),
        None => (pct_decode(d), None),
    };
    let link = Link {
        target: path.clone(),
        heading: anchor.clone(),
        block: None,
        alias: None,
        embed: false,
        wiki: false,
        in_props: false,
        line: 0,
        span: (0, 0),
        target_span: (0, 0),
    };
    match cx.v.and_then(|v| v.resolve(&cx.from, &link)) {
        Some(p) if cx.v.is_some_and(|v| v.note(&p).is_some()) => {
            let da = anchor.as_ref().map(|a| format!(" data-anchor=\"{}\"", esc(a))).unwrap_or_default();
            format!("<a class=\"internal\" href=\"{}\" data-path=\"{}\"{da}>{label}</a>", esc(&(cx.o.note_href)(&p, anchor.as_deref())), esc(&p))
        }
        Some(p) => format!("<a class=\"internal file\" href=\"{}\" data-file=\"{}\" target=\"_blank\" rel=\"noopener\">{label}</a>", esc(&(cx.o.file_src)(&p)), esc(&p)),
        None => format!("<a class=\"internal unresolved\" href=\"{}\" data-new=\"{}\">{label}</a>", esc(&(cx.o.note_href)(&path, None)), esc(crate::util::strip_md(&path))),
    }
}

fn md_image(cx: &mut Cx, alt: &str, dest: &str) -> String {
    let (alt, size) = match alt.rsplit_once('|') {
        Some((a, s)) if size_attrs(Some(s)).1.is_none() => (a, size_attrs(Some(s)).0),
        _ => (alt, String::new()),
    };
    let d = dest.trim();
    if parse::has_scheme(d) {
        if scheme_ok(d, false) {
            return format!("<img class=\"embed-img\" src=\"{}\" alt=\"{}\"{size} loading=\"lazy\" referrerpolicy=\"no-referrer\">", esc(d), esc(alt));
        }
        return format!(
            "<span class=\"link-blocked\" title=\"{}\">{}</span>",
            esc(d),
            esc(alt)
        );
    }
    let link = Link {
        target: pct_decode(d),
        heading: None,
        block: None,
        alias: None,
        embed: true,
        wiki: false,
        in_props: false,
        line: 0,
        span: (0, 0),
        target_span: (0, 0),
    };
    match cx.v.and_then(|v| v.resolve(&cx.from, &link)) {
        Some(p) => format!("<img class=\"embed-img\" src=\"{}\" alt=\"{}\"{size} loading=\"lazy\" data-file=\"{}\">", esc(&(cx.o.file_src)(&p)), esc(alt), esc(&p)),
        None => format!("<span class=\"embed unresolved\">{}</span>", esc(alt)),
    }
}

fn footnote_ref(cx: &mut Cx, label: &str, inline_text: Option<&str>) -> String {
    let key = match inline_text {
        Some(t) => format!("^inline:{}:{t}", cx.fn_order.len()),
        None => label.to_string(),
    };
    if let Some(pos) = cx.fn_order.iter().position(|(k, _)| *k == key) {
        let n = pos + 1;
        return format!("<sup class=\"fn-ref\"><a href=\"#fn-{n}\">{n}</a></sup>");
    }
    let text = match inline_text {
        Some(t) => t.to_string(),
        None => match cx.fn_defs.get(label) {
            Some(t) => t.clone(),
            None => return format!("[^{}]", esc(label)),
        },
    };
    cx.fn_order.push((key.clone(), String::new()));
    let n = cx.fn_order.len();
    let html = text
        .split('\n')
        .map(|t| inline(cx, t))
        .collect::<Vec<_>>()
        .join("<br>");
    if let Some(e) = cx.fn_order.iter_mut().find(|(k, _)| *k == key) {
        e.1 = html;
    }
    format!("<sup class=\"fn-ref\" id=\"fnref-{n}\"><a href=\"#fn-{n}\">{n}</a></sup>")
}

fn inline(cx: &mut Cx, s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut o = String::with_capacity(s.len() + 16);
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        let prev = if i > 0 { Some(c[i - 1]) } else { None };
        match ch {
            '\\' if c.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                esc_char(&mut o, c[i + 1]);
                i += 2;
                continue;
            }
            '`' => {
                if let Some((code, e)) = code_span(&c, i) {
                    o.push_str(&format!("<code>{}</code>", esc(&code)));
                    i = e;
                } else {
                    let n = c[i..].iter().take_while(|x| **x == '`').count();
                    o.push_str(&"`".repeat(n));
                    i += n;
                }
                continue;
            }
            '!' if starts(&c, i, "![[") => {
                if let Some(end) = find_from(&c, i + 3, "]]") {
                    let inner: String = c[i + 3..end].iter().collect();
                    if let Some(l) = parse::wiki_inner(&inner, true) {
                        o.push_str(&embed(cx, &l, false));
                        i = end + 2;
                        continue;
                    }
                }
            }
            '[' if starts(&c, i, "[[") && !cx.in_link => {
                if let Some(end) = find_from(&c, i + 2, "]]") {
                    let inner: String = c[i + 2..end].iter().collect();
                    if !inner.contains("[[") {
                        if let Some(l) = parse::wiki_inner(&inner, false) {
                            o.push_str(&wikilink(cx, &l));
                            i = end + 2;
                            continue;
                        }
                    }
                }
            }
            '!' if c.get(i + 1) == Some(&'[') => {
                if let Some(close) = matching_bracket(&c, i + 1) {
                    if c.get(close + 1) == Some(&'(') {
                        if let Some((dest, e)) = link_dest(&c, close + 1) {
                            let alt: String = c[i + 2..close].iter().collect();
                            o.push_str(&md_image(cx, &alt, &dest));
                            i = e;
                            continue;
                        }
                    }
                }
            }
            '^' if c.get(i + 1) == Some(&'[') => {
                if let Some(close) = matching_bracket(&c, i + 1) {
                    let t: String = c[i + 2..close].iter().collect();
                    if !t.trim().is_empty() {
                        o.push_str(&footnote_ref(cx, "", Some(&t)));
                        i = close + 1;
                        continue;
                    }
                }
            }
            '[' => {
                if c.get(i + 1) == Some(&'^') {
                    if let Some(close) = find_from(&c, i + 2, "]") {
                        let label: String = c[i + 2..close].iter().collect();
                        if !label.is_empty()
                            && !label.contains(' ')
                            && cx.fn_defs.contains_key(&label)
                        {
                            o.push_str(&footnote_ref(cx, &label, None));
                            i = close + 1;
                            continue;
                        }
                    }
                }
                if !cx.in_link {
                    if let Some(close) = matching_bracket(&c, i) {
                        if c.get(close + 1) == Some(&'(') {
                            if let Some((dest, e)) = link_dest(&c, close + 1) {
                                let text: String = c[i + 1..close].iter().collect();
                                o.push_str(&md_link(cx, &text, &dest));
                                i = e;
                                continue;
                            }
                        }
                    }
                }
            }
            '<' => {
                let rest: String = c[i..c.len().min(i + 8)]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase();
                if rest.starts_with("<br>")
                    || rest.starts_with("<br/>")
                    || rest.starts_with("<br />")
                {
                    o.push_str("<br>");
                    i += if rest.starts_with("<br>") {
                        4
                    } else if rest.starts_with("<br/>") {
                        5
                    } else {
                        6
                    };
                    continue;
                }
                if starts(&c, i, "<!--") {
                    if let Some(e) = find_from(&c, i + 4, "-->") {
                        i = e + 3;
                        continue;
                    }
                }
                if let Some(close) =
                    (i + 1..c.len()).find(|&k| c[k] == '>' || c[k] == ' ' || c[k] == '<')
                {
                    if c[close] == '>' {
                        let url: String = c[i + 1..close].iter().collect();
                        if scheme_ok(&url, true) && !cx.in_link {
                            o.push_str(&format!("<a class=\"external\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>", esc(&url), esc(url.trim_start_matches("mailto:"))));
                            i = close + 1;
                            continue;
                        }
                    }
                }
            }
            '$' if !starts(&c, i, "$$")
                && c.get(i + 1).is_some_and(|n| !n.is_whitespace())
                && prev != Some('\\') =>
            {
                // Pandoc's rule: no space inside the dollars, no digit right after the closing one,
                // and the next `$` must close (so "$5 or $10" stays text).
                if let Some(e) = (i + 2..c.len()).find(|&k| c[k] == '$') {
                    if !c[e - 1].is_whitespace()
                        && !c.get(e + 1).is_some_and(|n| n.is_ascii_digit())
                    {
                        let tex: String = c[i + 1..e].iter().collect();
                        o.push_str(&crate::math::mathml(&tex, false));
                        i = e + 1;
                        continue;
                    }
                }
            }
            '#' if prev.is_none_or(char::is_whitespace) && !cx.in_link => {
                let tag: String = c[i + 1..]
                    .iter()
                    .take_while(|x| parse::is_tag_char(**x))
                    .collect();
                let tag = tag.trim_end_matches('/').to_string();
                if !tag.is_empty() && !tag.chars().all(|x| x.is_ascii_digit()) {
                    o.push_str(&format!(
                        "<a class=\"tag\" href=\"{}\" data-tag=\"{}\">#{}</a>",
                        esc(&(cx.o.tag_href)(&tag)),
                        esc(&tag),
                        esc(&tag)
                    ));
                    i += 1 + tag.chars().count();
                    continue;
                }
            }
            '*' | '_' | '~' | '=' => {
                let pairs: &[(&str, &str)] = &[
                    ("**", "strong"),
                    ("__", "strong"),
                    ("~~", "del"),
                    ("==", "mark"),
                    ("*", "em"),
                    ("_", "em"),
                ];
                let mut done = false;
                for (m, tag) in pairs {
                    if starts(&c, i, m) {
                        if let Some(j) = emph_close(&c, i, m) {
                            let inner: String = c[i + m.len()..j].iter().collect();
                            let h = inline(cx, &inner);
                            o.push_str(&format!("<{tag}>{h}</{tag}>"));
                            i = j + m.len();
                            done = true;
                        }
                        break;
                    }
                }
                if done {
                    continue;
                }
            }
            'h' | 'H'
                if !cx.in_link
                    && prev.is_none_or(|p| {
                        !p.is_alphanumeric() && p != '/' && p != '"' && p != '='
                    }) =>
            {
                let rest: String = c[i..c.len().min(i + 8)]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase();
                if rest.starts_with("http://") || rest.starts_with("https://") {
                    let mut e = i;
                    while e < c.len() && !c[e].is_whitespace() && c[e] != '<' {
                        e += 1;
                    }
                    let mut url: String = c[i..e].iter().collect();
                    while url.ends_with(['.', ',', ';', ':', '!', '?', '\'', '"', '*', '_'])
                        || (url.ends_with(')')
                            && url.matches(')').count() > url.matches('(').count())
                    {
                        url.pop();
                    }
                    if url.len() > 8 {
                        o.push_str(&format!("<a class=\"external\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>", esc(&url), esc(&url)));
                        i += url.chars().count();
                        continue;
                    }
                }
            }
            _ => {}
        }
        esc_char(&mut o, ch);
        i += 1;
    }
    o
}

/// A note's properties as a small table (used by published pages; the app draws its own).
pub fn props_html(
    v: Option<&Vault>,
    from: &str,
    props: &[(String, parse::Prop)],
    o: &Opts,
) -> String {
    if props.is_empty() {
        return String::new();
    }
    let mut cx = Cx {
        v,
        o,
        from: from.to_string(),
        stack: vec![from.to_string()],
        ids: HashMap::new(),
        fn_defs: HashMap::new(),
        fn_order: vec![],
        in_link: false,
    };
    let mut out = String::from("<table class=\"props\"><tbody>");
    for (k, val) in props {
        let items: Vec<String> = val
            .items()
            .iter()
            .map(|it| {
                if k.eq_ignore_ascii_case("tags") {
                    let t = it.trim_start_matches('#');
                    format!(
                        "<a class=\"tag\" href=\"{}\" data-tag=\"{}\">#{}</a>",
                        esc(&(o.tag_href)(t)),
                        esc(t),
                        esc(t)
                    )
                } else {
                    inline(&mut cx, it)
                }
            })
            .collect();
        out.push_str(&format!(
            "<tr><th>{}</th><td>{}</td></tr>",
            esc(k),
            items.join(" ")
        ));
    }
    out.push_str("</tbody></table>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::{put, sample, tmp};

    fn md(s: &str) -> String {
        markdown_html(None, "x.md", s, &Opts::app())
    }

    #[test]
    fn blocks_and_inline() {
        assert_eq!(md("# Title\n\nHello *you* and **me**."), "<h1 id=\"title\" data-heading=\"Title\" data-line=\"1\">Title</h1><p data-line=\"3\">Hello <em>you</em> and <strong>me</strong>.</p>");
        assert_eq!(
            md("a\nb"),
            "<p data-line=\"1\">a<br>\nb</p>",
            "single newlines are line breaks"
        );
        assert_eq!(md("==hi== ~~no~~ `a<b>` snake_case_name"), "<p data-line=\"1\"><mark>hi</mark> <del>no</del> <code>a&lt;b&gt;</code> snake_case_name</p>");
        assert_eq!(
            md("*a **b** c*"),
            "<p data-line=\"1\"><em>a <strong>b</strong> c</em></p>"
        );
        assert_eq!(md("2 * 3 * 4"), "<p data-line=\"1\">2 * 3 * 4</p>");
        assert_eq!(md("---"), "<hr>");
        assert_eq!(md("```rust\nfn main() {}\n<x>\n```"), "<pre data-lang=\"rust\"><code class=\"language-rust\">fn main() {}\n&lt;x&gt;</code></pre>");
        assert_eq!(
            md("$$\na^2\n$$ "),
            "<math class=\"math math-block\" display=\"block\"><semantics><mrow><msup><mi>a</mi><mn>2</mn></msup></mrow><annotation encoding=\"application/x-tex\">a^2</annotation></semantics></math>"
        );
        assert_eq!(
            md("cost $5 or $10, and $x^2$"),
            "<p data-line=\"1\">cost $5 or $10, and <math class=\"math\"><semantics><mrow><msup><mi>x</mi><mn>2</mn></msup></mrow><annotation encoding=\"application/x-tex\">x^2</annotation></semantics></math></p>"
        );
        assert_eq!(md("#tag and #123 and a#b"), "<p data-line=\"1\"><a class=\"tag\" href=\"#/s/tag%3Atag\" data-tag=\"tag\">#tag</a> and #123 and a#b</p>");
        assert_eq!(
            md("a %%hidden%% b\n%%\nall\nhidden\n%%\nc"),
            "<p data-line=\"1\">a  b</p><p data-line=\"6\">c</p>"
        );
        assert_eq!(md("Title\n===\nSub\n---\ntext"), "<h1 id=\"title\" data-heading=\"Title\" data-line=\"1\">Title</h1><h2 id=\"sub\" data-heading=\"Sub\" data-line=\"3\">Sub</h2><p data-line=\"5\">text</p>");
    }

    #[test]
    fn nothing_executes() {
        let h = md("<script>alert(1)</script> <img src=x onerror=alert(1)> [x](javascript:alert(1)) ![i](data:text/html,x) <javascript:alert(1)>");
        assert!(
            !h.contains("<script")
                && !h.contains("<img src=x")
                && !h.contains("href=\"javascript")
                && !h.contains("src=\"data:"),
            "{h}"
        );
        assert!(h.contains("&lt;script&gt;"));
        assert!(h.contains("<span class=\"link-blocked\" title=\"javascript:alert(1)\">x</span>"));
        let h = md("[ok](https://example.com/a?b=1&c=2 \"t\") <https://x.io> https://y.io/p). line<br>break");
        assert!(h.contains("<a class=\"external\" href=\"https://example.com/a?b=1&amp;c=2\" target=\"_blank\" rel=\"noopener noreferrer\">ok</a>"), "{h}");
        assert!(h.contains("href=\"https://x.io\""));
        assert!(
            h.contains("href=\"https://y.io/p\"") && h.contains(">https://y.io/p</a>)."),
            "{h}"
        );
        assert!(h.contains("line<br>break"));
        let h = md("[[x\" onmouseover=\"alert(1)]]");
        assert!(!h.contains("\" onmouseover"), "{h}");
    }

    #[test]
    fn lists_tasks_tables_quotes() {
        let h = md("- a\n- [ ] todo\n- [x] done ^blk\n  - nested\n\n1. one\n2. two");
        assert!(h.starts_with("<ul><li data-line=\"1\">a</li><li class=\"task\" data-task=\"todo\" data-line=\"2\"><input type=\"checkbox\" class=\"task-box\" data-path=\"x.md\" data-line=\"2\"> <span class=\"task-text\">todo</span></li>"), "{h}");
        assert!(h.contains("<li class=\"task done\" data-task=\"x\" data-line=\"3\" id=\"^blk\" data-block=\"blk\"><input type=\"checkbox\" class=\"task-box\" data-path=\"x.md\" data-line=\"3\" checked> <span class=\"task-text\">done</span><ul><li data-line=\"4\">nested</li></ul></li></ul>"), "{h}");
        assert!(
            h.ends_with("<ol><li data-line=\"6\">one</li><li data-line=\"7\">two</li></ol>"),
            "{h}"
        );
        assert_eq!(
            md("3. three\n4. four"),
            "<ol start=\"3\"><li data-line=\"1\">three</li><li data-line=\"2\">four</li></ol>"
        );
        assert!(md("\t- tab\n\t\t- deeper").contains(
            "<ul><li data-line=\"1\">tab<ul><li data-line=\"2\">deeper</li></ul></li></ul>"
        ));
        let h = md("| a | b |\n|:--|--:|\n| 1 | x \\| y |\n");
        assert_eq!(h, "<div class=\"table-wrap\"><table><thead><tr><th class=\"al-left\">a</th><th class=\"al-right\">b</th></tr></thead><tbody><tr><td class=\"al-left\">1</td><td class=\"al-right\">x | y</td></tr></tbody></table></div>");
        assert_eq!(
            md("> quote\n> more"),
            "<blockquote><p data-line=\"1\">quote<br>\nmore</p></blockquote>"
        );
        assert_eq!(md("> [!warning]- Careful\n> body"), "<details class=\"callout\" data-callout=\"warning\"><summary class=\"callout-title\">Careful</summary><div class=\"callout-content\"><p data-line=\"2\">body</p></div></details>");
        assert!(md("> [!tip]\n> x").starts_with(
            "<div class=\"callout\" data-callout=\"tip\"><div class=\"callout-title\">Tip</div>"
        ));
        let h = md("A[^1] and B^[inline note].\n\n[^1]: The *first*.");
        assert!(h.contains("A<sup class=\"fn-ref\" id=\"fnref-1\"><a href=\"#fn-1\">1</a></sup> and B<sup class=\"fn-ref\" id=\"fnref-2\"><a href=\"#fn-2\">2</a></sup>."), "{h}");
        assert!(
            h.contains("<li id=\"fn-1\">The <em>first</em>. <a class=\"fn-back\""),
            "{h}"
        );
        assert!(h.contains("<li id=\"fn-2\">inline note"), "{h}");
        assert!(
            md("# A\n## A").contains("id=\"a-1\""),
            "duplicate headings get unique ids"
        );
    }

    #[test]
    fn links_and_embeds_resolve() {
        let (d, v) = sample();
        let o = Opts::app();
        let h = note_html(&v, "Home.md", &o).unwrap();
        assert!(h.contains("<a class=\"internal\" href=\"#/n/Projects/Tempo.md\" data-path=\"Projects/Tempo.md\">Projects/Tempo</a>"), "{h}");
        assert!(h.contains("<a class=\"internal unresolved\" href=\"#/n/Missing%20note.md\" data-new=\"Missing note\">Missing note</a>"), "{h}");
        let h = note_html(&v, "Projects/Tempo.md", &o).unwrap();
        assert!(!h.contains("aliases"), "frontmatter is not body text");
        assert!(
            h.contains("data-path=\"Ideas.md\" data-anchor=\"Next\">Ideas &gt; Next</a>"),
            "{h}"
        );
        assert!(h.contains("<img class=\"embed-img\" src=\"/vault/file?path=img/logo.png\" alt=\"logo.png\" loading=\"lazy\" data-file=\"img/logo.png\">"), "{h}");
        put(&d, "Embeds.md", "![[Ideas#Next]]\n\n![[Home]]\n\n![[Embeds]]\n\nSee ![[img/logo.png|120]] and [rel](Projects/Tempo.md#Tempo).");
        put(&d, "Loop.md", "![[Loop]]");
        let mut v = v;
        v.refresh();
        let h = note_html(&v, "Embeds.md", &o).unwrap();
        assert!(h.contains("<div class=\"embed\" data-path=\"Ideas.md\"><div class=\"embed-head\"><a class=\"internal embed-title\" href=\"#/n/Ideas.md?h=Next\" data-path=\"Ideas.md\">Ideas &gt; Next</a></div><div class=\"embed-body\"><h2 id=\"next\" data-heading=\"Next\" data-line=\"2\">Next</h2>"), "{h}");
        assert!(
            h.contains("data-path=\"Home.md\"><div class=\"embed-head\">"),
            "whole notes transclude"
        );
        assert!(h.contains("<span class=\"embed-link\"><a class=\"internal embed-title\" href=\"#/n/Embeds.md\""), "a note never embeds itself");
        assert!(h.contains("width=\"120\""));
        assert!(h.contains("<a class=\"internal\" href=\"#/n/Projects/Tempo.md?h=Tempo\" data-path=\"Projects/Tempo.md\" data-anchor=\"Tempo\">rel</a>"), "{h}");
        assert!(note_html(&v, "Loop.md", &o).unwrap().contains("embed-link"));
        let _ = tmp;
    }

    #[test]
    fn query_and_tasks_blocks() {
        let (d, mut v) = sample();
        put(
            &d,
            "Q.md",
            "```query\nperennos\n```\n\n```tasks\nnot done\npath includes projects\n```\n",
        );
        put(
            &d,
            "Later.md",
            "- [ ] late thing 📅 2020-01-01\n- [ ] someday\n",
        );
        v.refresh();
        let h = note_html(&v, "Q.md", &Opts::app()).unwrap();
        assert!(
            h.contains("<span class=\"query-label\">query</span> <code>perennos</code>"),
            "{h}"
        );
        assert!(h.contains("data-path=\"Ideas.md\">Ideas</a>"), "{h}");
        assert!(h.contains("ship vault") && !h.contains("late thing"), "{h}");
        let due = task_query(&v, "has due date\nlimit 5", "2026-11-06");
        assert_eq!(
            due.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["Later.md", "Projects/Tempo.md"],
            "earliest due first"
        );
        assert_eq!(task_query(&v, "due before today", "2026-11-06").len(), 1);
        assert_eq!(
            task_query(&v, "due before tomorrow", "2026-11-10").len(),
            2,
            "due today or earlier"
        );
        assert_eq!(task_query(&v, "due on tomorrow", "2026-11-09").len(), 1);
        assert_eq!(task_query(&v, "done", "2026-11-06").len(), 1);
    }

    fn base_vault() -> Vault {
        let d = tmp("render-base");
        put(&d, "Projects/Tempo.md", "---\nstatus: Doing\nrank: 2\ncover: \"[[cover.png]]\"\nowner: \"[[Riley]]\"\n---\n# Tempo\n- [ ] ship the vault\n");
        put(
            &d,
            "Projects/Room.md",
            "---\nstatus: Todo\nrank: 1\nnote: \"<script>alert(1)</script>\"\n---\n# Room\n",
        );
        put(
            &d,
            "Projects/Studio.md",
            "---\nstatus: Done\nrank: 3\n---\n# Studio\n",
        );
        put(&d, "Riley.md", "# Riley\n");
        put(&d, "cover.png", "png");
        put(
            &d,
            "Projects.base",
            "filters:\n  and:\n    - file.inFolder(\"Projects\")\nviews:\n  - type: table\n    name: All\n    order: [file.name, status, rank, note]\n    sort:\n      - property: rank\n        direction: DESC\n  - type: board\n    name: Board\n    groupBy: status\n    lanes: [Todo, Doing, Done, Blocked]\n    image: cover\n",
        );
        put(&d, "Home.md", "# Home\n![[Projects.base#Board]]\n\n```dataview\nTABLE status AS \"State\" FROM \"Projects\" SORT rank ASC\n```\n\n```base\nviews: [\n```\n");
        Vault::open(&d).unwrap()
    }

    #[test]
    fn bases_render_as_tables_boards_and_dataview() {
        let v = base_vault();
        let html = base_file_html(&v, "Projects.base", &Opts::app()).unwrap();
        // the table: sorted by rank, high first; links to the notes; the property cells editable
        let (a, b, c) = (
            html.find(">Studio<").unwrap(),
            html.find(">Tempo<").unwrap(),
            html.find(">Room<").unwrap(),
        );
        assert!(a < b && b < c, "{html}");
        assert!(html.contains("<td class=\"num\" data-prop=\"rank\" data-raw=\"&quot;3&quot;\"><span class=\"base-num\">3</span></td>"), "{html}");
        assert!(html.contains("3 notes"));
        // values are escaped: the property text is never markup
        assert!(
            !html.contains("<script>") && html.contains("&lt;script&gt;"),
            "{html}"
        );
        // the board: lanes in the given order, an empty one too, cards draggable, the picture from the vault
        let lanes: Vec<usize> = [
            "data-lane=\"Todo\"",
            "data-lane=\"Doing\"",
            "data-lane=\"Done\"",
            "data-lane=\"Blocked\"",
        ]
        .iter()
        .map(|l| html.find(l).unwrap())
        .collect();
        assert!(lanes.windows(2).all(|w| w[0] < w[1]), "{html}");
        assert!(html.contains("<div class=\"base-board\" data-prop=\"status\">"));
        assert!(html.contains("draggable=\"true\""));
        assert!(
            html.contains("src=\"/vault/file?path=cover.png\""),
            "{html}"
        );
        // a note embeds one view of the base, runs a Dataview table, and shows a broken base kindly
        let home = note_html(&v, "Home.md", &Opts::app()).unwrap();
        assert!(
            home.contains("data-kind=\"board\"")
                && !home.contains("data-kind=\"table\" data-name=\"All\""),
            "{home}"
        );
        assert!(
            home.contains("<th>State</th>") && home.contains("data-kind=\"table\""),
            "{home}"
        );
        assert!(
            home.contains("base-error") && home.contains("cannot be read"),
            "{home}"
        );
        // a published page (not interactive): nothing to drag
        let still = Opts {
            interactive: false,
            ..Opts::app()
        };
        let page = base_file_html(&v, "Projects.base", &still).unwrap();
        assert!(!page.contains("draggable") && page.contains("base-card"));
    }

    #[test]
    fn canvases_draw_link_and_embed() {
        let d = tmp("render-canvas");
        put(
            &d,
            "Projects/Tempo.md",
            "# Tempo\nThe harness.\n## Next\nShip it.\n",
        );
        put(&d, "Ideas.md", "# Ideas\n");
        put(&d, "cover.png", "png");
        put(
            &d,
            "Board.canvas",
            r##"{"nodes":[
            {"id":"g","type":"group","x":-20,"y":-40,"width":700,"height":300,"label":"Launch <now>","color":"4"},
            {"id":"t","type":"text","x":0,"y":0,"width":240,"height":120,"text":"**Plan** with [[Ideas]] <script>x()</script>","color":"#ff8800"},
            {"id":"f","type":"file","x":300,"y":0,"width":240,"height":160,"file":"Projects/Tempo.md","subpath":"#Next"},
            {"id":"i","type":"file","x":0,"y":400,"width":100,"height":100,"file":"cover.png"},
            {"id":"l","type":"link","x":200,"y":400,"width":200,"height":60,"url":"javascript:alert(1)"}],
          "edges":[{"id":"e","fromNode":"t","toNode":"f","label":"then","fromEnd":"arrow"}]}"##,
        );
        put(&d, "Home.md", "# Home\n![[Board.canvas]]\n");
        let v = Vault::open(&d).unwrap();
        let html = canvas_file_html(&v, "Board.canvas", &Opts::app()).unwrap();
        assert!(
            html.starts_with("<figure class=\"canvas-view\"")
                && html.contains("viewBox=\"-60 -80 780 620\""),
            "{html}"
        );
        assert!(
            html.contains("<strong>Plan</strong>") && html.contains("data-path=\"Ideas.md\""),
            "{html}"
        );
        assert!(
            !html.contains("<script>") && html.contains("&lt;script&gt;"),
            "{html}"
        );
        assert!(
            html.contains("cv-group cv-c4") && html.contains("stroke=\"#ff8800\""),
            "{html}"
        );
        assert!(html.contains("Launch &lt;now&gt;"));
        assert!(
            html.contains("Ship it.") && !html.contains("The harness."),
            "the card shows the #Next section: {html}"
        );
        assert!(
            html.contains("<image href=\"/vault/file?path=cover.png\""),
            "{html}"
        );
        assert!(!html.contains("href=\"javascript"), "{html}");
        assert_eq!(
            html.matches("<polygon").count(),
            2,
            "arrowheads at both ends: {html}"
        );
        assert!(html.contains(">then</text>"));
        // the notes on it list the canvas among their backlinks; nothing calls them orphans
        assert!(v
            .backlinks("Projects/Tempo.md")
            .iter()
            .any(|b| b.source == "Board.canvas"));
        assert!(v
            .backlinks("Ideas.md")
            .iter()
            .any(|b| b.source == "Board.canvas"));
        assert_eq!(v.canvases_with("Ideas.md"), ["Board.canvas"]);
        assert!(!v.orphans().contains(&"Ideas.md".to_string()));
        // a note embeds the canvas
        let home = note_html(&v, "Home.md", &Opts::app()).unwrap();
        assert!(
            home.contains("<figure class=\"canvas-view\" data-file=\"Board.canvas\">"),
            "{home}"
        );
        // the editor's cards
        let c = canvas::parse(&v.read_text("Board.canvas").unwrap()).unwrap();
        let cards = canvas_cards_html(&v, "Board.canvas", &c, &Opts::app());
        let ids: Vec<&str> = cards.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(ids, ["t", "f"]);
        assert!(cards[1].1.contains("Ship it."));
    }
}
