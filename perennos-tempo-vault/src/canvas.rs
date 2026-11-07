//! JSON Canvas 1.0, the format of Obsidian's `.canvas` files (<https://jsoncanvas.org>): cards
//! (text, a file of the vault, a web link, a group) placed on an endless board, and arrows between
//! them. This reads and checks a canvas, lists what it links to (so a note on a canvas shows the
//! canvas among its backlinks), and writes it out as plain text for Tempo.
//!
//! The file itself stays the source of truth and is never rewritten here: fields this code does
//! not know are kept, because the app edits the JSON it was given.
use crate::parse::{self, Link};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    File,
    Link,
    Group,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: String,
    pub kind: Kind,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// `"1"` to `"6"` (red, orange, yellow, green, cyan, purple) or `#rrggbb`.
    pub color: Option<String>,
    /// Text cards: Markdown.
    pub text: String,
    /// File cards: a vault path, and `#Heading` or `#^block` inside it.
    pub file: String,
    pub subpath: Option<String>,
    /// Link cards.
    pub url: String,
    /// Groups: the title, and a picture behind them.
    pub label: String,
    pub background: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub id: String,
    pub from: String,
    pub from_side: Option<Side>,
    pub from_arrow: bool,
    pub to: String,
    pub to_side: Option<Side>,
    pub to_arrow: bool,
    pub color: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Canvas {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// A colour as the format allows it: a preset `1`..`6` or `#rgb`/`#rrggbb` (anything else is dropped,
/// so a colour can never carry markup or CSS into a page).
pub fn color_of(v: &Value) -> Option<String> {
    let c = v.as_str()?.trim();
    let preset = matches!(c, "1" | "2" | "3" | "4" | "5" | "6");
    let hex = c.len() > 1
        && matches!(c.len(), 4 | 7)
        && c.starts_with('#')
        && c[1..].chars().all(|x| x.is_ascii_hexdigit());
    (preset || hex).then(|| c.to_ascii_lowercase())
}

fn side(v: &Value) -> Option<Side> {
    Some(match v.as_str()? {
        "top" => Side::Top,
        "right" => Side::Right,
        "bottom" => Side::Bottom,
        "left" => Side::Left,
        _ => return None,
    })
}

fn num(v: &Value, k: &str, id: &str) -> Result<f64, String> {
    let n = v[k]
        .as_f64()
        .ok_or_else(|| format!("card {id}: {k} must be a number"))?;
    if !n.is_finite() || n.abs() > 1e9 {
        return Err(format!("card {id}: {k} is out of range"));
    }
    Ok(n)
}

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

pub const MAX_NODES: usize = 5_000;

/// Read a canvas; an error names what is wrong (the app shows it, and a write is refused).
pub fn parse(src: &str) -> Result<Canvas, String> {
    let v: Value = serde_json::from_str(src).map_err(|e| format!("not valid JSON ({e})"))?;
    if !v.is_object() {
        return Err("a canvas is a JSON object with nodes and edges".into());
    }
    let mut c = Canvas::default();
    let mut ids = std::collections::HashSet::new();
    let nodes = match &v["nodes"] {
        Value::Null => &[][..],
        Value::Array(a) => a.as_slice(),
        _ => return Err("nodes must be a list".into()),
    };
    if nodes.len() > MAX_NODES {
        return Err(format!("more than {MAX_NODES} cards"));
    }
    for n in nodes {
        let id = n["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("every card needs an id")?
            .to_string();
        if !ids.insert(id.clone()) {
            return Err(format!("two cards have the id {id}"));
        }
        let kind = match n["type"].as_str() {
            Some("text") => Kind::Text,
            Some("file") => Kind::File,
            Some("link") => Kind::Link,
            Some("group") => Kind::Group,
            other => {
                return Err(format!(
                    "card {id}: type {} is not text, file, link or group",
                    other.unwrap_or("(missing)")
                ))
            }
        };
        let (x, y) = (num(n, "x", &id)?, num(n, "y", &id)?);
        let (w, h) = (num(n, "width", &id)?, num(n, "height", &id)?);
        if w < 0.0 || h < 0.0 {
            return Err(format!("card {id}: width and height cannot be negative"));
        }
        let subpath = n["subpath"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        c.nodes.push(Node {
            id,
            kind,
            x,
            y,
            w,
            h,
            color: color_of(&n["color"]),
            text: text(n, "text"),
            file: text(n, "file"),
            subpath,
            url: text(n, "url"),
            label: text(n, "label"),
            background: n["background"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
    }
    let edges = match &v["edges"] {
        Value::Null => &[][..],
        Value::Array(a) => a.as_slice(),
        _ => return Err("edges must be a list".into()),
    };
    for e in edges {
        let id = e["id"].as_str().unwrap_or("").to_string();
        let (from, to) = (text(e, "fromNode"), text(e, "toNode"));
        if !ids.contains(&from) || !ids.contains(&to) {
            return Err(format!(
                "the arrow {} joins a card that is not on the canvas",
                if id.is_empty() { "(no id)" } else { &id }
            ));
        }
        c.edges.push(Edge {
            id,
            from,
            from_side: side(&e["fromSide"]),
            from_arrow: e["fromEnd"].as_str() == Some("arrow"),
            to,
            to_side: side(&e["toSide"]),
            // the default end of an arrow is an arrowhead
            to_arrow: e["toEnd"].as_str() != Some("none"),
            color: color_of(&e["color"]),
            label: text(e, "label"),
        });
    }
    Ok(c)
}

/// What a canvas links to: each file card (as an embed) and every `[[link]]` in its text cards.
pub fn links(c: &Canvas) -> Vec<Link> {
    let mut out = Vec::new();
    for n in &c.nodes {
        match n.kind {
            Kind::File if !n.file.is_empty() => {
                let (heading, block) = match n.subpath.as_deref().map(|s| s.trim_start_matches('#'))
                {
                    Some(s) if s.starts_with('^') => (None, Some(s[1..].to_string())),
                    Some(s) if !s.is_empty() => (Some(s.to_string()), None),
                    _ => (None, None),
                };
                out.push(Link {
                    target: n.file.clone(),
                    heading,
                    block,
                    alias: None,
                    embed: true,
                    wiki: true,
                    in_props: false,
                    line: 0,
                    span: (0, 0),
                    target_span: (0, 0),
                });
            }
            Kind::Group => {
                if let Some(b) = &n.background {
                    out.extend(parse::wiki_inner(b, true));
                }
            }
            Kind::Text => out.extend(parse::parse(&n.text).links.into_iter().map(|l| Link {
                line: 0,
                span: (0, 0),
                target_span: (0, 0),
                ..l
            })),
            _ => {}
        }
    }
    out
}

/// The box around every card (x, y, width, height), or a small empty box.
pub fn bounds(c: &Canvas) -> (f64, f64, f64, f64) {
    if c.nodes.is_empty() {
        return (0.0, 0.0, 400.0, 240.0);
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for n in &c.nodes {
        x0 = x0.min(n.x);
        y0 = y0.min(n.y);
        x1 = x1.max(n.x + n.w);
        y1 = y1.max(n.y + n.h);
    }
    (x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0))
}

/// Where an arrow leaves or reaches a card: the middle of the side it names, or else the side
/// that faces the other card.
pub fn anchor(n: &Node, side: Option<Side>, toward: &Node) -> (f64, f64, Side) {
    let s = side.unwrap_or_else(|| {
        let (dx, dy) = (
            (toward.x + toward.w / 2.0) - (n.x + n.w / 2.0),
            (toward.y + toward.h / 2.0) - (n.y + n.h / 2.0),
        );
        if dx.abs() * n.h >= dy.abs() * n.w {
            if dx >= 0.0 {
                Side::Right
            } else {
                Side::Left
            }
        } else if dy >= 0.0 {
            Side::Bottom
        } else {
            Side::Top
        }
    });
    let (x, y) = match s {
        Side::Top => (n.x + n.w / 2.0, n.y),
        Side::Bottom => (n.x + n.w / 2.0, n.y + n.h),
        Side::Left => (n.x, n.y + n.h / 2.0),
        Side::Right => (n.x + n.w, n.y + n.h / 2.0),
    };
    (x, y, s)
}

/// The four points of the curve from one card to another (start, two pulls, end).
pub fn edge_curve(a: (f64, f64, Side), b: (f64, f64, Side)) -> [(f64, f64); 4] {
    let len = (((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt() / 2.5).clamp(30.0, 160.0);
    let pull = |(x, y, s): (f64, f64, Side)| match s {
        Side::Top => (x, y - len),
        Side::Bottom => (x, y + len),
        Side::Left => (x - len, y),
        Side::Right => (x + len, y),
    };
    [(a.0, a.1), pull(a), pull(b), (b.0, b.1)]
}

/// The curve as an SVG path.
pub fn edge_path(a: (f64, f64, Side), b: (f64, f64, Side)) -> String {
    let [p0, c1, c2, p3] = edge_curve(a, b);
    format!(
        "M{:.1} {:.1} C{:.1} {:.1} {:.1} {:.1} {:.1} {:.1}",
        p0.0, p0.1, c1.0, c1.1, c2.0, c2.1, p3.0, p3.1
    )
}

/// An arrowhead at `tip`, pointing away from `from`, as an SVG polygon's points.
pub fn arrowhead(from: (f64, f64), tip: (f64, f64)) -> String {
    let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
    let l = (dx * dx + dy * dy).sqrt().max(1e-6);
    let (ux, uy) = (dx / l, dy / l);
    let (bx, by) = (tip.0 - ux * 14.0, tip.1 - uy * 14.0);
    format!(
        "{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}",
        tip.0,
        tip.1,
        bx - uy * 7.0,
        by + ux * 7.0,
        bx + uy * 7.0,
        by - ux * 7.0
    )
}

/// The canvas as text, for Tempo: groups with what is inside them, then the loose cards, then
/// the arrows ("A -> B: label").
pub fn outline(c: &Canvas) -> String {
    let name = |n: &Node| -> String {
        match n.kind {
            Kind::Text => {
                let first = n
                    .text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                format!(
                    "\"{}\"",
                    crate::util::head_chars(first.trim_start_matches('#').trim(), 80)
                )
            }
            Kind::File => format!("[[{}{}]]", n.file, n.subpath.as_deref().unwrap_or("")),
            Kind::Link => format!("<{}>", n.url),
            Kind::Group => format!("group \"{}\"", n.label),
        }
    };
    let inside = |g: &Node, n: &Node| {
        n.id != g.id && n.x >= g.x && n.y >= g.y && n.x + n.w <= g.x + g.w && n.y + n.h <= g.y + g.h
    };
    let card = |n: &Node| -> String {
        match n.kind {
            Kind::Text => format!(
                "- text card:\n{}",
                n.text
                    .lines()
                    .map(|l| format!("    {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            _ => format!("- {}", name(n)),
        }
    };
    let groups: Vec<&Node> = c.nodes.iter().filter(|n| n.kind == Kind::Group).collect();
    let mut s = format!(
        "canvas: {} cards, {} arrows\n",
        c.nodes.len(),
        c.edges.len()
    );
    let mut placed = std::collections::HashSet::new();
    for g in &groups {
        s.push_str(&format!(
            "\n## {}\n",
            if g.label.is_empty() {
                "(group)"
            } else {
                &g.label
            }
        ));
        for n in c
            .nodes
            .iter()
            .filter(|n| n.kind != Kind::Group && inside(g, n))
        {
            if placed.insert(n.id.clone()) {
                s.push_str(&card(n));
                s.push('\n');
            }
        }
    }
    let loose: Vec<&Node> = c
        .nodes
        .iter()
        .filter(|n| n.kind != Kind::Group && !placed.contains(&n.id))
        .collect();
    if !loose.is_empty() {
        if !groups.is_empty() {
            s.push_str("\n## (not in a group)\n");
        }
        for n in loose {
            s.push_str(&card(n));
            s.push('\n');
        }
    }
    if !c.edges.is_empty() {
        s.push_str("\n## arrows\n");
        let by_id = |id: &str| c.nodes.iter().find(|n| n.id == id);
        for e in &c.edges {
            if let (Some(a), Some(b)) = (by_id(&e.from), by_id(&e.to)) {
                let arrow = match (e.from_arrow, e.to_arrow) {
                    (true, true) => "<->",
                    (false, true) => "->",
                    (true, false) => "<-",
                    (false, false) => "--",
                };
                s.push_str(&format!(
                    "- {} {arrow} {}{}\n",
                    name(a),
                    name(b),
                    if e.label.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", e.label)
                    }
                ));
            }
        }
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const SAMPLE: &str = r##"{
      "nodes": [
        {"id": "g1", "type": "group", "x": -40, "y": -60, "width": 700, "height": 400, "label": "Launch", "color": "4"},
        {"id": "t1", "type": "text", "x": 0, "y": 0, "width": 260, "height": 120, "text": "# Plan\nShip [[Tempo]] and see [[Ideas#Next]]", "color": "1"},
        {"id": "f1", "type": "file", "x": 340, "y": 0, "width": 260, "height": 200, "file": "Projects/Tempo.md", "subpath": "#Tempo"},
        {"id": "l1", "type": "link", "x": 0, "y": 500, "width": 300, "height": 80, "url": "https://perennos.com"},
        {"id": "t2", "type": "text", "x": 700, "y": 500, "width": 200, "height": 80, "text": "evil", "color": "red;background:url(x)", "unknownField": 1}
      ],
      "edges": [
        {"id": "e1", "fromNode": "t1", "fromSide": "right", "toNode": "f1", "toSide": "left", "label": "builds"},
        {"id": "e2", "fromNode": "f1", "toNode": "l1", "toEnd": "none", "color": "#FF8800"}
      ]
    }"##;

    #[test]
    fn reads_the_format_and_keeps_colours_safe() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.nodes.len(), 5);
        assert_eq!(c.nodes[0].kind, Kind::Group);
        assert_eq!(c.nodes[1].color.as_deref(), Some("1"));
        assert_eq!(
            c.nodes[4].color, None,
            "a colour that is not a preset or a hex is dropped"
        );
        assert_eq!(c.edges[1].color.as_deref(), Some("#ff8800"));
        assert!(c.edges[0].to_arrow && !c.edges[1].to_arrow && !c.edges[0].from_arrow);
        assert_eq!(c.edges[0].from_side, Some(Side::Right));
        assert_eq!(bounds(&c), (-40.0, -60.0, 940.0, 640.0));
        assert!(parse("{}").unwrap().nodes.is_empty());
    }

    #[test]
    fn refuses_broken_canvases() {
        for (src, want) in [
            ("[1]", "JSON object"),
            ("{\"nodes\": 3}", "must be a list"),
            ("{\"nodes\": [{\"type\": \"text\", \"x\": 0, \"y\": 0, \"width\": 1, \"height\": 1}]}", "needs an id"),
            ("{\"nodes\": [{\"id\": \"a\", \"type\": \"blob\", \"x\": 0, \"y\": 0, \"width\": 1, \"height\": 1}]}", "not text, file"),
            ("{\"nodes\": [{\"id\": \"a\", \"type\": \"text\", \"x\": \"0\", \"y\": 0, \"width\": 1, \"height\": 1}]}", "x must be a number"),
            ("{\"nodes\": [{\"id\": \"a\", \"type\": \"text\", \"x\": 0, \"y\": 0, \"width\": 1, \"height\": 1}, {\"id\": \"a\", \"type\": \"text\", \"x\": 0, \"y\": 0, \"width\": 1, \"height\": 1}]}", "two cards"),
            ("{\"nodes\": [], \"edges\": [{\"id\": \"e\", \"fromNode\": \"x\", \"toNode\": \"y\"}]}", "not on the canvas"),
            ("not json", "not valid JSON"),
        ] {
            let e = parse(src).unwrap_err();
            assert!(e.contains(want), "{src}: {e}");
        }
    }

    #[test]
    fn links_and_outline() {
        let c = parse(SAMPLE).unwrap();
        let l = links(&c);
        let targets: Vec<&str> = l.iter().map(|x| x.target.as_str()).collect();
        assert_eq!(targets, ["Tempo", "Ideas", "Projects/Tempo.md"]);
        assert_eq!(l[1].heading.as_deref(), Some("Next"));
        assert!(l[2].embed && l[2].heading.as_deref() == Some("Tempo"));
        let o = outline(&c);
        assert!(o.starts_with("canvas: 5 cards, 2 arrows"), "{o}");
        assert!(o.contains("## Launch\n- text card:\n    # Plan"), "{o}");
        assert!(o.contains("- [[Projects/Tempo.md#Tempo]]"), "{o}");
        assert!(
            o.contains("## (not in a group)\n- <https://perennos.com>"),
            "{o}"
        );
        assert!(
            o.contains("- \"Plan\" -> [[Projects/Tempo.md#Tempo]]: builds"),
            "{o}"
        );
        assert!(
            o.contains("[[Projects/Tempo.md#Tempo]] -- <https://perennos.com>"),
            "{o}"
        );
    }

    #[test]
    fn arrows_leave_from_the_facing_side() {
        let c = parse(SAMPLE).unwrap();
        let (t1, f1, l1) = (&c.nodes[1], &c.nodes[2], &c.nodes[3]);
        assert_eq!(anchor(t1, None, f1).2, Side::Right);
        assert_eq!(anchor(f1, None, t1).2, Side::Left);
        assert_eq!(anchor(f1, None, l1).2, Side::Bottom);
        assert_eq!(anchor(t1, Some(Side::Top), f1), (130.0, 0.0, Side::Top));
        assert!(edge_path(anchor(t1, None, f1), anchor(f1, None, t1)).starts_with("M260.0 60.0 C"));
    }
}
