//! The YAML that Obsidian's `.base` files use: nested maps and lists by indentation, `- item`
//! lists (also of maps), inline `[a, b]` lists and `{k: v}` maps, plain, 'single' and "double"
//! quoted scalars, `#` comments, and `|` / `>` text blocks. Everything else is an error with its
//! line, never a guess. Scalars stay text; the reader decides what a value means.

#[derive(Debug, Clone, PartialEq)]
pub enum Y {
    Null,
    Str(String),
    List(Vec<Y>),
    Map(Vec<(String, Y)>),
}

impl Y {
    pub fn get(&self, key: &str) -> Option<&Y> {
        match self {
            Y::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            Y::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn list(&self) -> &[Y] {
        match self {
            Y::List(l) => l,
            _ => &[],
        }
    }
    pub fn entries(&self) -> &[(String, Y)] {
        match self {
            Y::Map(m) => m,
            _ => &[],
        }
    }
}

struct Line {
    indent: usize,
    text: String,
    no: usize,
}

/// Remove a `#` comment (one that starts the line or follows a space, outside quotes).
fn strip_comment(s: &str) -> &str {
    let (mut q, mut prev) = (None::<char>, ' ');
    for (i, c) in s.char_indices() {
        match q {
            Some(open) if c == open => q = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => q = Some(c),
            None if c == '#' && (prev == ' ' || prev == '\t') => return s[..i].trim_end(),
            None => {}
        }
        prev = c;
    }
    s.trim_end()
}

fn lines_of(src: &str) -> Result<Vec<Line>, String> {
    let mut out = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let raw = raw.trim_end_matches('\r');
        if raw.contains('\t') && raw.trim_start_matches(' ').starts_with('\t') {
            return Err(format!("line {}: indent with spaces, not tabs", i + 1));
        }
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let body = raw.trim_start_matches(' ');
        if body.is_empty() || body.starts_with('#') || body == "---" || body == "..." {
            continue;
        }
        out.push(Line {
            indent,
            text: strip_comment(body).to_string(),
            no: i + 1,
        });
    }
    Ok(out)
}

/// Where `key: value` splits (a colon followed by a space or the end, outside quotes and brackets).
fn key_split(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let (mut q, mut depth) = (None::<u8>, 0i32);
    for (i, &c) in b.iter().enumerate() {
        match q {
            Some(open) if c == open => q = None,
            Some(_) => {}
            None => match c {
                b'"' | b'\'' if i == 0 || matches!(b[i - 1], b' ' | b'[' | b'{' | b',') => {
                    q = Some(c)
                }
                b'[' | b'{' => depth += 1,
                b']' | b'}' => depth -= 1,
                b':' if depth == 0 && (i + 1 == b.len() || b[i + 1] == b' ') => return Some(i),
                _ => {}
            },
        }
    }
    None
}

fn unquote(s: &str, no: usize) -> Result<String, String> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('\'') {
        let inner = inner
            .strip_suffix('\'')
            .ok_or_else(|| format!("line {no}: a 'quoted' value is not closed"))?;
        return Ok(inner.replace("''", "'"));
    }
    if let Some(inner) = s.strip_prefix('"') {
        let inner = inner
            .strip_suffix('"')
            .ok_or_else(|| format!("line {no}: a \"quoted\" value is not closed"))?;
        let mut out = String::new();
        let mut it = inner.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => out.push('\\'),
            }
        }
        return Ok(out);
    }
    Ok(s.to_string())
}

/// `[a, "b, c", [d]]` or `{k: v}`.
fn flow(s: &str, no: usize) -> Result<Y, String> {
    let s = s.trim();
    let (open, close) = match s.as_bytes().first() {
        Some(b'[') => ('[', ']'),
        Some(b'{') => ('{', '}'),
        _ => return scalar(s, no),
    };
    let inner = s
        .strip_prefix(open)
        .and_then(|x| x.strip_suffix(close))
        .ok_or_else(|| format!("line {no}: {open} is not closed with {close}"))?;
    let mut parts = Vec::new();
    let (mut q, mut depth, mut start) = (None::<char>, 0i32, 0usize);
    for (i, c) in inner.char_indices() {
        match q {
            Some(o) if c == o => q = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => q = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(&inner[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    if !inner[start..].trim().is_empty() {
        parts.push(&inner[start..]);
    }
    if open == '[' {
        return Ok(Y::List(
            parts
                .iter()
                .map(|p| flow(p, no))
                .collect::<Result<_, _>>()?,
        ));
    }
    let mut m = Vec::new();
    for p in parts {
        let at = key_split(p.trim()).ok_or_else(|| format!("line {no}: {{k: v}} needs a key"))?;
        let p = p.trim();
        m.push((unquote(&p[..at], no)?, flow(&p[at + 1..], no)?));
    }
    Ok(Y::Map(m))
}

fn scalar(s: &str, no: usize) -> Result<Y, String> {
    let s = s.trim();
    if s.is_empty() || s == "~" || s == "null" {
        return Ok(Y::Null);
    }
    Ok(Y::Str(unquote(s, no)?))
}

fn value(text: &str, ls: &[Line], i: &mut usize, indent: usize, no: usize) -> Result<Y, String> {
    let t = text.trim();
    if t == "|" || t == ">" || t.starts_with("|-") || t.starts_with(">-") || t == "|+" || t == ">+"
    {
        // a text block: the lines indented deeper than the key
        let fold = t.starts_with('>');
        let mut out: Vec<String> = Vec::new();
        let base = ls.get(*i).map(|l| l.indent).unwrap_or(0);
        while let Some(l) = ls.get(*i) {
            if l.indent <= indent {
                break;
            }
            out.push(format!(
                "{}{}",
                " ".repeat(l.indent.saturating_sub(base)),
                l.text
            ));
            *i += 1;
        }
        return Ok(Y::Str(out.join(if fold { " " } else { "\n" })));
    }
    if t.is_empty() {
        return match ls.get(*i) {
            Some(l) if l.indent > indent => block(ls, i, l.indent),
            // a list may sit at the same indent as its key
            Some(l) if l.indent == indent && (l.text == "-" || l.text.starts_with("- ")) => {
                block(ls, i, indent)
            }
            _ => Ok(Y::Null),
        };
    }
    flow(t, no)
}

fn block(ls: &[Line], i: &mut usize, indent: usize) -> Result<Y, String> {
    let first = &ls[*i];
    if first.text == "-" || first.text.starts_with("- ") {
        let mut items = Vec::new();
        while let Some(l) = ls.get(*i) {
            if l.indent != indent || !(l.text == "-" || l.text.starts_with("- ")) {
                break;
            }
            let rest = l.text[1..].trim_start().to_string();
            let no = l.no;
            *i += 1;
            if rest.is_empty() {
                items.push(value("", ls, i, indent, no)?);
            } else if key_split(&rest).is_some() && !rest.starts_with(['[', '{', '"', '\'']) {
                // `- key: value` starts a map whose keys line up after the dash
                let col = indent + (l.text.len() - rest.len());
                let mut sub = vec![Line {
                    indent: col,
                    text: rest,
                    no,
                }];
                while let Some(n) = ls.get(*i) {
                    if n.indent < col {
                        break;
                    }
                    sub.push(Line {
                        indent: n.indent,
                        text: n.text.clone(),
                        no: n.no,
                    });
                    *i += 1;
                }
                let mut j = 0;
                items.push(block(&sub, &mut j, col)?);
                if j < sub.len() {
                    return Err(format!(
                        "line {}: this does not line up with the list item",
                        sub[j].no
                    ));
                }
            } else {
                items.push(flow(&rest, no)?);
            }
        }
        return Ok(Y::List(items));
    }
    let mut map = Vec::new();
    while let Some(l) = ls.get(*i) {
        if l.indent < indent {
            break;
        }
        if l.indent > indent {
            return Err(format!("line {}: indented more than the line before", l.no));
        }
        if l.text.starts_with("- ") || l.text == "-" {
            break;
        }
        let at =
            key_split(&l.text).ok_or_else(|| format!("line {}: expected \"key: value\"", l.no))?;
        let key = unquote(&l.text[..at], l.no)?;
        let rest = l.text[at + 1..].to_string();
        let no = l.no;
        *i += 1;
        map.push((key, value(&rest, ls, i, indent, no)?));
    }
    Ok(Y::Map(map))
}

pub fn parse(src: &str) -> Result<Y, String> {
    let ls = lines_of(src)?;
    if ls.is_empty() {
        return Ok(Y::Map(vec![]));
    }
    let mut i = 0;
    let y = block(&ls, &mut i, ls[0].indent)?;
    if i < ls.len() {
        return Err(format!("line {}: this does not fit where it is", ls[i].no));
    }
    Ok(y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Y {
        Y::Str(x.into())
    }

    #[test]
    fn reads_an_obsidian_base() {
        let src = r#"
# books I have read
filters:
  and:
    - file.hasTag("book")
    - 'status != "done"'
    - or:
        - "price > 2.1"
        - file.inFolder("Library")
formulas:
  ppu: "(price / age).toFixed(2)"
properties:
  formula.ppu:
    displayName: Price per unit
views:
  - type: table
    name: "My table"
    limit: 10
    order:
      - file.name
      - status
    sort:
      - property: file.mtime
        direction: DESC
  - type: cards
    name: Gallery
    image: note.cover
    order: [file.name, "author"]
"#;
        let y = parse(src).unwrap();
        let and = y.get("filters").unwrap().get("and").unwrap().list();
        assert_eq!(and[0], s("file.hasTag(\"book\")"));
        assert_eq!(and[1], s("status != \"done\""));
        assert_eq!(
            and[2].get("or").unwrap().list(),
            &[s("price > 2.1"), s("file.inFolder(\"Library\")")]
        );
        assert_eq!(
            y.get("formulas").unwrap().get("ppu"),
            Some(&s("(price / age).toFixed(2)"))
        );
        assert_eq!(
            y.get("properties")
                .unwrap()
                .get("formula.ppu")
                .unwrap()
                .get("displayName"),
            Some(&s("Price per unit"))
        );
        let views = y.get("views").unwrap().list();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].get("name"), Some(&s("My table")));
        assert_eq!(views[0].get("limit"), Some(&s("10")));
        assert_eq!(
            views[0].get("order").unwrap().list(),
            &[s("file.name"), s("status")]
        );
        let sort = views[0].get("sort").unwrap().list();
        assert_eq!(sort[0].get("property"), Some(&s("file.mtime")));
        assert_eq!(sort[0].get("direction"), Some(&s("DESC")));
        assert_eq!(
            views[1].get("order").unwrap().list(),
            &[s("file.name"), s("author")]
        );
        assert_eq!(views[1].get("image"), Some(&s("note.cover")));
    }

    #[test]
    fn scalars_lists_and_errors() {
        let y = parse("a: 'it''s'\nb: \"x\\\"y\"\nc:\n- 1\n- two # comment\nd: [ ]\ne: {k: v, n: [1, 2]}\nf: ~\ng: a#b\n").unwrap();
        assert_eq!(y.get("a"), Some(&s("it's")));
        assert_eq!(y.get("b"), Some(&s("x\"y")));
        assert_eq!(y.get("c").unwrap().list(), &[s("1"), s("two")]);
        assert_eq!(y.get("d"), Some(&Y::List(vec![])));
        assert_eq!(
            y.get("e").unwrap().get("n").unwrap().list(),
            &[s("1"), s("2")]
        );
        assert_eq!(y.get("f"), Some(&Y::Null));
        assert_eq!(
            y.get("g"),
            Some(&s("a#b")),
            "a # without a space before it is text"
        );
        let block = parse("text: |\n  line one\n    two\nnext: x\n").unwrap();
        assert_eq!(block.get("text"), Some(&s("line one\n  two")));
        assert_eq!(block.get("next"), Some(&s("x")));
        assert!(parse("a: 1\n   b: 2\n").unwrap_err().contains("line 2"));
        assert!(parse("a: \"open\n").unwrap_err().contains("not closed"));
        assert!(parse("just words\n").unwrap_err().contains("key: value"));
        assert!(parse("a:\n\t- x\n").unwrap_err().contains("tabs"));
        assert_eq!(parse("").unwrap(), Y::Map(vec![]));
    }
}
