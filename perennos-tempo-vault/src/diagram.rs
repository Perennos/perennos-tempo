//! ```` ```mermaid ```` diagrams drawn on the server as SVG: flowcharts (`graph` / `flowchart`,
//! every common node shape, edge style and label, `subgraph`), state diagrams, sequence diagrams
//! and pie charts. No script runs in the page, so they work in the app and on published pages
//! under a strict Content Security Policy. Everything is laid out here (layers, then a few
//! ordering sweeps to untangle edges); styling comes from the stylesheet.
use crate::render::esc;
use std::collections::HashMap;

const CHAR_W: f64 = 7.4;
const LINE_H: f64 = 18.0;

fn text_w(s: &str) -> f64 {
    s.split('\n').map(|l| l.chars().count()).max().unwrap_or(0) as f64 * CHAR_W
}

/// SVG `<text>` with one line per `\n` (or `<br>`), centred on (x, y).
fn label(x: f64, y: f64, s: &str, class: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let top = y - (lines.len() as f64 - 1.0) * LINE_H / 2.0;
    let mut o = format!("<text class=\"{class}\" x=\"{x:.1}\" y=\"{top:.1}\" text-anchor=\"middle\" dominant-baseline=\"central\">");
    for (i, l) in lines.iter().enumerate() {
        if i == 0 {
            o.push_str(&format!("<tspan x=\"{x:.1}\">{}</tspan>", esc(l)));
        } else {
            o.push_str(&format!(
                "<tspan x=\"{x:.1}\" dy=\"{LINE_H}\">{}</tspan>",
                esc(l)
            ));
        }
    }
    o.push_str("</text>");
    o
}

fn clean_text(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s);
    s.replace("<br/>", "\n")
        .replace("<br>", "\n")
        .replace("<br />", "\n")
        .replace("\\n", "\n")
        .replace("&quot;", "\"")
        .replace("#quot;", "\"")
}

fn arrowhead(from: (f64, f64), tip: (f64, f64), class: &str) -> String {
    let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
    let l = (dx * dx + dy * dy).sqrt().max(1e-6);
    let (ux, uy) = (dx / l, dy / l);
    let (bx, by) = (tip.0 - ux * 10.0, tip.1 - uy * 10.0);
    format!(
        "<polygon class=\"{class}\" points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\"/>",
        tip.0,
        tip.1,
        bx - uy * 5.0,
        by + ux * 5.0,
        bx + uy * 5.0,
        by - ux * 5.0
    )
}

// ------------------------------------------------------------------ flowcharts and state diagrams

#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    Rect,
    Round,
    Stadium,
    Sub,
    Cyl,
    Circle,
    Flag,
    Diamond,
    Hex,
    Para,
    Start,
    End,
}

#[derive(Debug, Clone)]
struct FNode {
    id: String,
    text: String,
    shape: Shape,
    w: f64,
    h: f64,
    x: f64,
    y: f64,
}

#[derive(Debug, Clone)]
struct FEdge {
    a: usize,
    b: usize,
    text: String,
    dotted: bool,
    thick: bool,
    head_b: bool,
    head_a: bool,
}

#[derive(Default)]
struct Flow {
    nodes: Vec<FNode>,
    edges: Vec<FEdge>,
    index: HashMap<String, usize>,
    groups: Vec<(String, Vec<usize>)>,
    dir: String,
}

impl Flow {
    fn node(&mut self, id: &str, text: Option<String>, shape: Option<Shape>) -> usize {
        let i = *self.index.entry(id.to_string()).or_insert_with(|| {
            self.nodes.push(FNode {
                id: id.to_string(),
                text: id.to_string(),
                shape: Shape::Rect,
                w: 0.0,
                h: 0.0,
                x: 0.0,
                y: 0.0,
            });
            self.nodes.len() - 1
        });
        if let Some(t) = text {
            self.nodes[i].text = t;
        }
        if let Some(s) = shape {
            self.nodes[i].shape = s;
        }
        i
    }
}

/// One node reference with its optional shape: `A`, `A[text]`, `B((text))`, `C{text}` ...
fn node_ref(s: &str) -> Option<(String, Option<String>, Option<Shape>, usize)> {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < c.len() && c[i].is_whitespace() {
        i += 1;
    }
    let start = i;
    while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '-') {
        // a hyphen only inside an id, never as the start of an arrow
        if c[i] == '-' && matches!(c.get(i + 1), Some('-' | '.' | '>' | '=')) {
            break;
        }
        i += 1;
    }
    if i == start {
        // the state diagram's [*]
        if c[start..].starts_with(&['[', '*', ']']) {
            return Some(("[*]".into(), None, None, start + 3));
        }
        return None;
    }
    let id: String = c[start..i].iter().collect();
    let pairs: [(&str, &str, Shape); 11] = [
        ("([", "])", Shape::Stadium),
        ("[[", "]]", Shape::Sub),
        ("[(", ")]", Shape::Cyl),
        ("((", "))", Shape::Circle),
        ("{{", "}}", Shape::Hex),
        ("[/", "/]", Shape::Para),
        ("[\\", "\\]", Shape::Para),
        ("[", "]", Shape::Rect),
        ("(", ")", Shape::Round),
        ("{", "}", Shape::Diamond),
        (">", "]", Shape::Flag),
    ];
    let rest: String = c[i..].iter().collect();
    for (open, close, shape) in pairs {
        if let Some(after) = rest.strip_prefix(open) {
            // the closing mark outside quotes
            let mut q = false;
            let ac: Vec<char> = after.chars().collect();
            let cc: Vec<char> = close.chars().collect();
            let mut k = 0;
            while k < ac.len() {
                if ac[k] == '"' {
                    q = !q;
                } else if !q && ac[k..].starts_with(&cc) {
                    let text: String = ac[..k].iter().collect();
                    let used = i + open.chars().count() + k + cc.len();
                    return Some((id, Some(clean_text(&text)), Some(shape), used));
                }
                k += 1;
            }
            return Some((id, Some(clean_text(after)), Some(shape), c.len()));
        }
    }
    Some((id, None, None, i))
}

/// An arrow at the start of `s`: its style, ends, inline label, and how many bytes it takes.
fn arrow(s: &str) -> Option<(FEdge, usize)> {
    let t = s.trim_start();
    let lead = s.len() - t.len();
    let b = t.as_bytes();
    let mut e = FEdge {
        a: 0,
        b: 0,
        text: String::new(),
        dotted: false,
        thick: false,
        head_b: false,
        head_a: false,
    };
    let mut i = 0;
    // a head at the start: <-->, x--, o--
    if b.len() > 2 && matches!(b[0], b'<' | b'x' | b'o') && matches!(b[1], b'-' | b'=') {
        e.head_a = b[0] == b'<';
        i = 1;
    }
    let body_start = i;
    while i < b.len() && matches!(b[i], b'-' | b'=' | b'.') {
        i += 1;
    }
    let body = &t[body_start..i];
    if !(body.starts_with("--") || body.starts_with("==") || body.starts_with("-.")) {
        return None;
    }
    e.dotted = body.contains('.');
    e.thick = body.starts_with("==");
    match b.get(i) {
        Some(b'>') => {
            e.head_b = true;
            i += 1;
        }
        // a cross or circle end, drawn as a plain end
        Some(b'x' | b'o')
            if b.get(i + 1)
                .is_none_or(|c| c.is_ascii_whitespace() || *c == b'|') =>
        {
            i += 1
        }
        _ => {}
    }
    // `-- text -->`: a label between two halves of the arrow
    if !e.head_b && (body == "--" || body == "==") {
        let rest = &t[i..];
        let (close, plain) = if e.thick {
            ("==>", "===")
        } else {
            ("-->", "---")
        };
        if let Some(k) = rest.find(close).or_else(|| rest.find(plain)) {
            let text = rest[..k].trim();
            if !text.is_empty() {
                e.text = clean_text(text);
                e.head_b = rest[k..].starts_with(close);
                return Some((e, lead + i + k + 3));
            }
        }
    }
    // `-->|text|`
    let rest = &t[i..];
    let rt = rest.trim_start();
    if let Some(after) = rt.strip_prefix('|') {
        if let Some(k) = after.find('|') {
            e.text = clean_text(&after[..k]);
            return Some((e, lead + i + (rest.len() - rt.len()) + 1 + k + 1));
        }
    }
    Some((e, lead + i))
}

fn parse_flow(src: &str, state: bool) -> Flow {
    let mut f = Flow::default();
    let mut open: Vec<usize> = vec![];
    let mut lines = src.lines();
    let first = lines.next().unwrap_or("");
    f.dir = first
        .split_whitespace()
        .nth(1)
        .unwrap_or(if state { "TB" } else { "TD" })
        .to_uppercase();
    if state {
        f.dir = "TB".into();
    }
    let mut starts = 0usize;
    for raw in lines {
        for stmt in raw.split(';') {
            let s = stmt.trim();
            if s.is_empty() || s.starts_with("%%") {
                continue;
            }
            let word = s.split_whitespace().next().unwrap_or("");
            if matches!(
                word,
                "classDef"
                    | "class"
                    | "style"
                    | "linkStyle"
                    | "click"
                    | "direction"
                    | "note"
                    | "accTitle"
                    | "accDescr"
            ) {
                if word == "direction" {
                    f.dir = s.split_whitespace().nth(1).unwrap_or("TD").to_uppercase();
                }
                continue;
            }
            if word == "subgraph" || (state && word == "state" && s.ends_with('{')) {
                let title = s[word.len()..].trim().trim_end_matches('{').trim();
                let (id, text) = match title.find('[') {
                    Some(k) => (
                        title[..k].trim().to_string(),
                        clean_text(title[k + 1..].trim_end_matches(']')),
                    ),
                    None => (title.to_string(), clean_text(title)),
                };
                let _ = id; // the id names the group for styling, which is not drawn
                f.groups.push((text, vec![]));
                open.push(f.groups.len() - 1);
                continue;
            }
            if word == "end" || s == "}" {
                open.pop();
                continue;
            }
            if state && word == "state" {
                // state "Long name" as id
                let rest = s[5..].trim();
                if let Some((name, id)) = rest.rsplit_once(" as ") {
                    let i = f.node(id.trim(), Some(clean_text(name)), Some(Shape::Round));
                    if let Some(g) = open.last() {
                        if !f.groups[*g].1.contains(&i) {
                            f.groups[*g].1.push(i);
                        }
                    }
                }
                continue;
            }
            // a chain: A --> B & C -->|x| D
            let mut rest = s;
            let mut prev: Vec<usize> = vec![];
            let mut pending: Option<FEdge> = None;
            let mut guard = 0;
            while !rest.trim().is_empty() && guard < 64 {
                guard += 1;
                let mut here = vec![];
                while let Some((id, text, shape, used)) = node_ref(rest) {
                    let (id, text, shape) = if state && id == "[*]" {
                        // a start when it has no arrow before it, an end when it has
                        starts += 1;
                        if pending.is_some() {
                            (
                                format!("[*]end{starts}"),
                                Some(String::new()),
                                Some(Shape::End),
                            )
                        } else {
                            (
                                format!("[*]start{starts}"),
                                Some(String::new()),
                                Some(Shape::Start),
                            )
                        }
                    } else {
                        let shape = shape.or(if state { Some(Shape::Round) } else { None });
                        (id, text, shape)
                    };
                    let i = f.node(&id, text, shape);
                    if let Some(g) = open.last() {
                        if !f.groups[*g].1.contains(&i) {
                            f.groups[*g].1.push(i);
                        }
                    }
                    here.push(i);
                    rest = &rest[rest.char_indices().nth(used).map_or(rest.len(), |(b, _)| b)..];
                    let t = rest.trim_start();
                    if let Some(after) = t.strip_prefix('&') {
                        rest = after;
                        continue;
                    }
                    break;
                }
                if here.is_empty() {
                    break;
                }
                let mut made = false;
                if let Some(e) = pending.take() {
                    for &a in &prev {
                        for &b in &here {
                            f.edges.push(FEdge { a, b, ..e.clone() });
                            made = true;
                        }
                    }
                }
                // a state transition's label (`A --> B : event`), or a state's description (`A : text`)
                if state {
                    if let Some(lbl) = rest.trim_start().strip_prefix(':') {
                        if made {
                            if let Some(last) = f.edges.last_mut() {
                                last.text = clean_text(lbl);
                            }
                        } else if let Some(&n) = here.first() {
                            f.nodes[n].text = format!("{}\n{}", f.nodes[n].text, clean_text(lbl));
                        }
                        rest = "";
                    }
                }
                prev = here;
                match arrow(rest) {
                    Some((e, used)) => {
                        pending = Some(e);
                        rest = &rest[used.min(rest.len())..];
                    }
                    None => break,
                }
            }
        }
    }
    f
}

fn size(n: &mut FNode) {
    let (tw, lines) = (text_w(&n.text), n.text.split('\n').count() as f64);
    let (w, h) = (tw + 28.0, lines * LINE_H + 18.0);
    (n.w, n.h) = match n.shape {
        Shape::Start | Shape::End => (22.0, 22.0),
        Shape::Circle => {
            let d = w.max(h) + 4.0;
            (d, d)
        }
        Shape::Diamond => (w * 1.5 + 10.0, h * 1.7),
        Shape::Hex | Shape::Para => (w + 30.0, h),
        Shape::Cyl => (w, h + 12.0),
        _ => (w.max(48.0), h),
    };
}

/// Layers by longest path (cycles broken where they close), then barycentre sweeps to order each
/// layer, then positions.
fn layout(f: &mut Flow) -> (f64, f64) {
    let n = f.nodes.len();
    for node in &mut f.nodes {
        size(node);
    }
    // break cycles: drop the edges that close one (by visiting order)
    let mut out: Vec<Vec<usize>> = vec![vec![]; n];
    for e in &f.edges {
        if e.a != e.b {
            out[e.a].push(e.b);
        }
    }
    let mut state = vec![0u8; n];
    let mut keep: Vec<(usize, usize)> = vec![];
    fn dfs(
        v: usize,
        out: &[Vec<usize>],
        state: &mut [u8],
        keep: &mut Vec<(usize, usize)>,
        depth: usize,
    ) {
        state[v] = 1;
        for &w in &out[v] {
            if state[w] == 0 && depth < 2000 {
                keep.push((v, w));
                dfs(w, out, state, keep, depth + 1);
            } else if state[w] == 2 {
                keep.push((v, w));
            }
        }
        state[v] = 2;
    }
    for v in 0..n {
        if state[v] == 0 {
            dfs(v, &out, &mut state, &mut keep, 0);
        }
    }
    let mut layer = vec![0usize; n];
    for _ in 0..n.min(200) {
        let mut changed = false;
        for &(a, b) in &keep {
            if layer[b] < layer[a] + 1 {
                layer[b] = layer[a] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let depth = layer.iter().copied().max().unwrap_or(0) + 1;
    let mut rows: Vec<Vec<usize>> = vec![vec![]; depth];
    for v in 0..n {
        rows[layer[v]].push(v);
    }
    let mut pos = vec![0.0f64; n];
    let set_pos = |rows: &Vec<Vec<usize>>, pos: &mut Vec<f64>| {
        for r in rows {
            for (k, &v) in r.iter().enumerate() {
                pos[v] = k as f64;
            }
        }
    };
    set_pos(&rows, &mut pos);
    for sweep in 0..6 {
        let down = sweep % 2 == 0;
        let order: Vec<usize> = if down {
            (1..depth).collect()
        } else {
            (0..depth.saturating_sub(1)).rev().collect()
        };
        for li in order {
            let mut scored: Vec<(f64, usize)> = rows[li]
                .iter()
                .map(|&v| {
                    let nb: Vec<f64> = keep
                        .iter()
                        .filter_map(|&(a, b)| {
                            if down && b == v && layer[a] + 1 == layer[v] {
                                Some(pos[a])
                            } else if !down && a == v && layer[b] == layer[v] + 1 {
                                Some(pos[b])
                            } else {
                                None
                            }
                        })
                        .collect();
                    let bc = if nb.is_empty() {
                        pos[v]
                    } else {
                        nb.iter().sum::<f64>() / nb.len() as f64
                    };
                    (bc, v)
                })
                .collect();
            scored.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
            rows[li] = scored.into_iter().map(|(_, v)| v).collect();
            set_pos(&rows, &mut pos);
        }
    }
    let horizontal = matches!(f.dir.as_str(), "LR" | "RL");
    let (gap_main, gap_cross) = (56.0, 28.0);
    // each layer's thickness along the main axis, and the widest layer across
    let thick: Vec<f64> = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|&v| {
                    if horizontal {
                        f.nodes[v].w
                    } else {
                        f.nodes[v].h
                    }
                })
                .fold(0.0, f64::max)
        })
        .collect();
    let spans: Vec<f64> = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|&v| {
                    if horizontal {
                        f.nodes[v].h
                    } else {
                        f.nodes[v].w
                    }
                })
                .sum::<f64>()
                + gap_cross * (r.len().max(1) - 1) as f64
        })
        .collect();
    let widest = spans.iter().copied().fold(0.0, f64::max);
    let mut main = 0.0;
    for (li, r) in rows.iter().enumerate() {
        let mut cross = (widest - spans[li]) / 2.0;
        for &v in r {
            let nd = &mut f.nodes[v];
            let (along, across) = if horizontal {
                (nd.w, nd.h)
            } else {
                (nd.h, nd.w)
            };
            let c_main = main + thick[li] / 2.0;
            let c_cross = cross + across / 2.0;
            if horizontal {
                (nd.x, nd.y) = (c_main, c_cross);
            } else {
                (nd.x, nd.y) = (c_cross, c_main);
            }
            let _ = along;
            cross += across + gap_cross;
        }
        main += thick[li] + gap_main;
    }
    let total_main = main - gap_main;
    // bottom-to-top and right-to-left are the same drawing, mirrored
    if f.dir == "BT" || f.dir == "RL" {
        for nd in &mut f.nodes {
            if horizontal {
                nd.x = total_main - nd.x;
            } else {
                nd.y = total_main - nd.y;
            }
        }
    }
    if horizontal {
        (total_main.max(1.0), widest.max(1.0))
    } else {
        (widest.max(1.0), total_main.max(1.0))
    }
}

fn shape_svg(n: &FNode) -> String {
    let (x, y, w, h) = (n.x - n.w / 2.0, n.y - n.h / 2.0, n.w, n.h);
    let body = match n.shape {
        Shape::Rect | Shape::Sub => {
            let mut s = format!("<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"2\"/>");
            if n.shape == Shape::Sub {
                s.push_str(&format!(
                    "<path d=\"M{:.1} {y:.1}v{h:.1}M{:.1} {y:.1}v{h:.1}\"/>",
                    x + 8.0,
                    x + w - 8.0
                ));
            }
            s
        }
        Shape::Round => format!("<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"10\"/>"),
        Shape::Stadium => format!("<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"{:.1}\"/>", h / 2.0),
        Shape::Circle => format!("<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"{:.1}\"/>", n.x, n.y, w / 2.0),
        Shape::Start => format!("<circle class=\"mm-dot\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"8\"/>", n.x, n.y),
        Shape::End => format!(
            "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"10\"/><circle class=\"mm-dot\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"6\"/>",
            n.x, n.y, n.x, n.y
        ),
        Shape::Diamond => format!(
            "<polygon points=\"{:.1},{y:.1} {:.1},{:.1} {:.1},{:.1} {x:.1},{:.1}\"/>",
            n.x,
            x + w,
            n.y,
            n.x,
            y + h,
            n.y
        ),
        Shape::Hex => format!(
            "<polygon points=\"{:.1},{y:.1} {:.1},{y:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {x:.1},{:.1}\"/>",
            x + 14.0,
            x + w - 14.0,
            x + w,
            n.y,
            x + w - 14.0,
            y + h,
            x + 14.0,
            y + h,
            n.y
        ),
        Shape::Para => format!(
            "<polygon points=\"{:.1},{y:.1} {:.1},{y:.1} {:.1},{:.1} {x:.1},{:.1}\"/>",
            x + 14.0,
            x + w,
            x + w - 14.0,
            y + h,
            y + h
        ),
        Shape::Flag => format!(
            "<polygon points=\"{x:.1},{y:.1} {:.1},{y:.1} {:.1},{:.1} {x:.1},{:.1} {:.1},{:.1}\"/>",
            x + w,
            x + w,
            y + h,
            y + h,
            x + 12.0,
            n.y
        ),
        Shape::Cyl => format!(
            // the top ellipse, down the left side, the bottom curve, up the right side
            "<path d=\"M{x:.1} {:.1}a{:.1} 6 0 0 0 {w:.1} 0a{:.1} 6 0 0 0 {:.1} 0v{:.1}a{:.1} 6 0 0 0 {w:.1} 0v{:.1}\"/><path class=\"mm-line\" d=\"M{x:.1} {:.1}a{:.1} 6 0 0 0 {w:.1} 0\"/>",
            y + 6.0,
            w / 2.0,
            w / 2.0,
            -w,
            h - 12.0,
            w / 2.0,
            -(h - 12.0),
            y + 6.0,
            w / 2.0
        ),
    };
    let text = if n.text.is_empty() {
        String::new()
    } else {
        label(n.x, n.y, &n.text, "mm-text")
    };
    format!(
        "<g class=\"mm-node\" data-id=\"{}\">{body}{text}</g>",
        esc(&n.id)
    )
}

/// Where the line from a node toward a point leaves its box.
fn border(n: &FNode, toward: (f64, f64)) -> (f64, f64) {
    let (dx, dy) = (toward.0 - n.x, toward.1 - n.y);
    if dx == 0.0 && dy == 0.0 {
        return (n.x, n.y);
    }
    let (hw, hh) = (n.w / 2.0, n.h / 2.0);
    let t = if matches!(n.shape, Shape::Circle | Shape::Start | Shape::End) {
        hw / (dx * dx + dy * dy).sqrt()
    } else if matches!(n.shape, Shape::Diamond) {
        1.0 / (dx.abs() / hw + dy.abs() / hh)
    } else {
        (hw / dx.abs().max(1e-9)).min(hh / dy.abs().max(1e-9))
    };
    (n.x + dx * t, n.y + dy * t)
}

fn flow_svg(src: &str, state: bool) -> Result<String, String> {
    let mut f = parse_flow(src, state);
    if f.nodes.is_empty() {
        return Err("no nodes".into());
    }
    if f.nodes.len() > 400 || f.edges.len() > 1200 {
        return Err("too big to draw here (over 400 nodes)".into());
    }
    let (w, h) = layout(&mut f);
    let horizontal = matches!(f.dir.as_str(), "LR" | "RL");
    let pad = 24.0;
    let mut g = String::new();
    // subgraphs behind
    for (title, members) in &f.groups {
        if members.is_empty() {
            continue;
        }
        let x0 = members
            .iter()
            .map(|&i| f.nodes[i].x - f.nodes[i].w / 2.0)
            .fold(f64::MAX, f64::min)
            - 14.0;
        let y0 = members
            .iter()
            .map(|&i| f.nodes[i].y - f.nodes[i].h / 2.0)
            .fold(f64::MAX, f64::min)
            - 30.0;
        let x1 = members
            .iter()
            .map(|&i| f.nodes[i].x + f.nodes[i].w / 2.0)
            .fold(f64::MIN, f64::max)
            + 14.0;
        let y1 = members
            .iter()
            .map(|&i| f.nodes[i].y + f.nodes[i].h / 2.0)
            .fold(f64::MIN, f64::max)
            + 14.0;
        g.push_str(&format!(
            "<g class=\"mm-group\"><rect x=\"{x0:.1}\" y=\"{y0:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"4\"/><text class=\"mm-gtitle\" x=\"{:.1}\" y=\"{:.1}\">{}</text></g>",
            x1 - x0,
            y1 - y0,
            x0 + 8.0,
            y0 + 18.0,
            esc(title)
        ));
    }
    for e in &f.edges {
        let (a, b) = (&f.nodes[e.a], &f.nodes[e.b]);
        let cls = format!(
            "mm-edge{}{}",
            if e.dotted { " dotted" } else { "" },
            if e.thick { " thick" } else { "" }
        );
        let (p0, p3) = if e.a == e.b {
            ((a.x + a.w / 2.0, a.y - 6.0), (a.x + a.w / 2.0, a.y + 6.0))
        } else {
            (border(a, (b.x, b.y)), border(b, (a.x, a.y)))
        };
        let d = if e.a == e.b {
            format!(
                "M{:.1} {:.1}C{:.1} {:.1} {:.1} {:.1} {:.1} {:.1}",
                p0.0,
                p0.1,
                p0.0 + 40.0,
                p0.1 - 30.0,
                p3.0 + 40.0,
                p3.1 + 30.0,
                p3.0,
                p3.1
            )
        } else {
            // a gentle curve along the flow
            let (c1, c2) = if horizontal {
                let m = (p0.0 + p3.0) / 2.0;
                ((m, p0.1), (m, p3.1))
            } else {
                let m = (p0.1 + p3.1) / 2.0;
                ((p0.0, m), (p3.0, m))
            };
            format!(
                "M{:.1} {:.1}C{:.1} {:.1} {:.1} {:.1} {:.1} {:.1}",
                p0.0, p0.1, c1.0, c1.1, c2.0, c2.1, p3.0, p3.1
            )
        };
        g.push_str(&format!("<g class=\"{cls}\"><path d=\"{d}\"/>"));
        if e.head_b {
            let from = if e.a == e.b {
                (p3.0 + 12.0, p3.1 + 8.0)
            } else if horizontal {
                ((p0.0 + p3.0) / 2.0, p3.1)
            } else {
                (p3.0, (p0.1 + p3.1) / 2.0)
            };
            g.push_str(&arrowhead(from, p3, "mm-head"));
        }
        if e.head_a && e.a != e.b {
            let from = if horizontal {
                ((p0.0 + p3.0) / 2.0, p0.1)
            } else {
                (p0.0, (p0.1 + p3.1) / 2.0)
            };
            g.push_str(&arrowhead(from, p0, "mm-head"));
        }
        if !e.text.is_empty() {
            let (mx, my) = if e.a == e.b {
                (p0.0 + 46.0, a.y)
            } else {
                ((p0.0 + p3.0) / 2.0, (p0.1 + p3.1) / 2.0)
            };
            let tw = text_w(&e.text) + 10.0;
            let th = e.text.split('\n').count() as f64 * LINE_H + 4.0;
            g.push_str(&format!(
                "<rect class=\"mm-lbg\" x=\"{:.1}\" y=\"{:.1}\" width=\"{tw:.1}\" height=\"{th:.1}\" rx=\"3\"/>{}",
                mx - tw / 2.0,
                my - th / 2.0,
                label(mx, my, &e.text, "mm-ltext")
            ));
        }
        g.push_str("</g>");
    }
    for n in &f.nodes {
        g.push_str(&shape_svg(n));
    }
    let (vx, vy, vw, vh) = (
        -pad - 16.0,
        -pad - 34.0,
        w + 2.0 * pad + 32.0,
        h + 2.0 * pad + 50.0,
    );
    Ok(format!(
        "<svg class=\"mm mm-flow\" viewBox=\"{vx:.0} {vy:.0} {vw:.0} {vh:.0}\" width=\"{vw:.0}\" role=\"img\" xmlns=\"http://www.w3.org/2000/svg\">{g}</svg>"
    ))
}

// ------------------------------------------------------------------ sequence diagrams

fn sequence_svg(src: &str) -> Result<String, String> {
    struct Msg {
        a: usize,
        b: usize,
        text: String,
        dashed: bool,
        head: char,
    }
    enum Item {
        Msg(Msg),
        Note(usize, usize, String),
        Open(String, String),
        Else(String),
        Close,
    }
    let mut parts: Vec<(String, String, bool)> = vec![]; // id, label, actor
    let mut items: Vec<Item> = vec![];
    let mut number = false;
    let part = |parts: &mut Vec<(String, String, bool)>, id: &str| -> usize {
        let id = id.trim();
        if let Some(i) = parts.iter().position(|p| p.0 == id) {
            return i;
        }
        parts.push((id.to_string(), id.to_string(), false));
        parts.len() - 1
    };
    for raw in src.lines().skip(1) {
        let s = raw.trim();
        if s.is_empty() || s.starts_with("%%") {
            continue;
        }
        let word = s.split_whitespace().next().unwrap_or("");
        match word {
            "participant" | "actor" => {
                let rest = s[word.len()..].trim();
                let (id, name) = rest
                    .split_once(" as ")
                    .map_or((rest, rest), |(a, b)| (a.trim(), b.trim()));
                let i = part(&mut parts, id);
                parts[i].1 = clean_text(name);
                parts[i].2 = word == "actor";
                continue;
            }
            "autonumber" => {
                number = true;
                continue;
            }
            "loop" | "alt" | "opt" | "par" | "critical" | "break" | "rect" => {
                items.push(Item::Open(
                    word.to_string(),
                    clean_text(s[word.len()..].trim()),
                ));
                continue;
            }
            "else" | "and" | "option" => {
                items.push(Item::Else(clean_text(s[word.len()..].trim())));
                continue;
            }
            "end" => {
                items.push(Item::Close);
                continue;
            }
            "activate" | "deactivate" | "title" | "box" => continue,
            _ => {}
        }
        if let Some(rest) = s.strip_prefix("Note ").or_else(|| s.strip_prefix("note ")) {
            let (place, text) = rest.split_once(':').unwrap_or((rest, ""));
            let place = place.trim();
            let who = place
                .trim_start_matches("right of")
                .trim_start_matches("left of")
                .trim_start_matches("over")
                .trim();
            let (a, b) = match who.split_once(',') {
                Some((x, y)) => (part(&mut parts, x), part(&mut parts, y)),
                None => {
                    let i = part(&mut parts, who);
                    (i, i)
                }
            };
            items.push(Item::Note(a.min(b), a.max(b), clean_text(text)));
            continue;
        }
        // A->>B: text, A-->>B, A->B, A-->B, A-xB, A-)B
        let arrows = ["-->>", "->>", "--x", "-x", "--)", "-)", "-->", "->"];
        let Some((k, arr)) = arrows
            .iter()
            .filter_map(|a| s.find(a).map(|k| (k, *a)))
            .min_by_key(|(k, a)| (*k, std::cmp::Reverse(a.len())))
        else {
            continue;
        };
        let from = &s[..k];
        let after = &s[k + arr.len()..];
        let (to, text) = after.split_once(':').unwrap_or((after, ""));
        let to = to.trim_start_matches(['+', '-']);
        let a = part(&mut parts, from);
        let b = part(&mut parts, to);
        items.push(Item::Msg(Msg {
            a,
            b,
            text: clean_text(text),
            dashed: arr.starts_with("--"),
            head: if arr.ends_with(">>") {
                '>'
            } else if arr.ends_with('x') {
                'x'
            } else if arr.ends_with(')') {
                ')'
            } else {
                '-'
            },
        }));
    }
    if parts.is_empty() {
        return Err("no participants".into());
    }
    if parts.len() > 40 || items.len() > 800 {
        return Err("too big to draw here".into());
    }
    let gap = 40.0;
    let widths: Vec<f64> = parts
        .iter()
        .map(|p| (text_w(&p.1) + 30.0).max(90.0))
        .collect();
    // room between lifelines for the longest message between neighbours
    let mut xs = vec![0.0f64; parts.len()];
    let mut x = 0.0;
    for (i, w) in widths.iter().enumerate() {
        let need = items
            .iter()
            .filter_map(|it| match it {
                Item::Msg(m) if m.a.min(m.b) + 1 == i && m.a.max(m.b) == i => {
                    Some(text_w(&m.text) + 40.0)
                }
                _ => None,
            })
            .fold(0.0, f64::max);
        if i > 0 {
            x += (widths[i - 1] / 2.0 + gap + w / 2.0).max(need);
        }
        xs[i] = x;
    }
    let total_w = x + widths.last().copied().unwrap_or(0.0);
    let head_h = 40.0;
    let mut y = head_h + 30.0;
    let mut g = String::new();
    let mut frames: Vec<(String, String, f64)> = vec![];
    let mut n = 0;
    let left = -widths[0] / 2.0 - 10.0;
    let right =
        xs.last().copied().unwrap_or(0.0) + widths.last().copied().unwrap_or(0.0) / 2.0 + 10.0;
    let mut body = String::new();
    for it in &items {
        match it {
            Item::Msg(m) => {
                n += 1;
                let text = if number {
                    format!("{n}. {}", m.text)
                } else {
                    m.text.clone()
                };
                let cls = if m.dashed { "mm-msg dashed" } else { "mm-msg" };
                if m.a == m.b {
                    let x0 = xs[m.a];
                    body.push_str(&format!(
                        "<g class=\"{cls}\"><path d=\"M{x0:.1} {:.1}h36v22h-36\"/>{}<text class=\"mm-mtext\" x=\"{:.1}\" y=\"{:.1}\">{}</text></g>",
                        y,
                        arrowhead((x0 + 20.0, y + 22.0), (x0 + 2.0, y + 22.0), "mm-head"),
                        x0 + 44.0,
                        y + 14.0,
                        esc(&text)
                    ));
                    y += 46.0;
                } else {
                    let (x0, x1) = (xs[m.a], xs[m.b]);
                    let dir = if x1 > x0 { 1.0 } else { -1.0 };
                    let tip = x1 - dir * 2.0;
                    body.push_str(&format!(
                        "<g class=\"{cls}\"><path d=\"M{x0:.1} {y:.1}H{tip:.1}\"/>"
                    ));
                    match m.head {
                        '>' => {
                            body.push_str(&arrowhead((tip - dir * 10.0, y), (tip, y), "mm-head"))
                        }
                        'x' => body.push_str(&format!(
                            "<path class=\"mm-x\" d=\"M{:.1} {:.1}l8 8M{:.1} {:.1}l-8 8\"/>",
                            tip - dir * 10.0 - 4.0,
                            y - 4.0,
                            tip - dir * 10.0 + 4.0,
                            y - 4.0
                        )),
                        ')' => body.push_str(&format!(
                            "<path d=\"M{:.1} {:.1}l{:.1} 6l{:.1} 6\"/>",
                            tip - dir * 8.0,
                            y - 6.0,
                            dir * 8.0,
                            -dir * 8.0
                        )),
                        _ => {}
                    }
                    body.push_str(&label((x0 + x1) / 2.0, y - 12.0, &text, "mm-mtext"));
                    body.push_str("</g>");
                    y += 40.0 + (text.split('\n').count() as f64 - 1.0) * LINE_H;
                }
            }
            Item::Note(a, b, text) => {
                let (x0, x1) = (
                    xs[*a] - widths[*a] / 2.0 + 10.0,
                    xs[*b] + widths[*b] / 2.0 - 10.0,
                );
                let w = (x1 - x0).max(text_w(text) + 20.0);
                let cx = (x0 + x1) / 2.0;
                let h = text.split('\n').count() as f64 * LINE_H + 14.0;
                body.push_str(&format!(
                    "<g class=\"mm-note\"><rect x=\"{:.1}\" y=\"{:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"2\"/>{}</g>",
                    cx - w / 2.0,
                    y - 10.0,
                    label(cx, y - 10.0 + h / 2.0, text, "mm-mtext")
                ));
                y += h + 16.0;
            }
            Item::Open(kind, text) => {
                frames.push((kind.clone(), text.clone(), y - 8.0));
                y += 26.0;
            }
            Item::Else(text) => {
                body.push_str(&format!(
                    "<g class=\"mm-frame\"><path class=\"dashed\" d=\"M{left:.1} {:.1}H{right:.1}\"/><text class=\"mm-ftext\" x=\"{:.1}\" y=\"{:.1}\">[{}]</text></g>",
                    y - 8.0,
                    left + 8.0,
                    y + 8.0,
                    esc(text)
                ));
                y += 26.0;
            }
            Item::Close => {
                if let Some((kind, text, top)) = frames.pop() {
                    let inset = frames.len() as f64 * 6.0;
                    g.push_str(&format!(
                        "<g class=\"mm-frame\"><rect x=\"{:.1}\" y=\"{top:.1}\" width=\"{:.1}\" height=\"{:.1}\"/><text class=\"mm-ftag\" x=\"{:.1}\" y=\"{:.1}\">{}</text><text class=\"mm-ftext\" x=\"{:.1}\" y=\"{:.1}\">[{}]</text></g>",
                        left + inset,
                        right - left - 2.0 * inset,
                        y - top - 4.0,
                        left + inset + 8.0,
                        top + 15.0,
                        esc(&kind),
                        left + inset + 14.0 + text_w(&kind),
                        top + 15.0,
                        esc(&text)
                    ));
                    y += 14.0;
                }
            }
        }
    }
    let bottom = y + 10.0;
    let mut heads = String::new();
    for (i, p) in parts.iter().enumerate() {
        let (cx, w) = (xs[i], widths[i]);
        heads.push_str(&format!(
            "<path class=\"mm-life\" d=\"M{cx:.1} {head_h:.1}V{bottom:.1}\"/>"
        ));
        for top in [0.0, bottom] {
            if p.2 {
                heads.push_str(&format!(
                    "<g class=\"mm-actor\"><circle cx=\"{cx:.1}\" cy=\"{:.1}\" r=\"7\"/><path d=\"M{cx:.1} {:.1}v12M{:.1} {:.1}h20M{cx:.1} {:.1}l-9 10M{cx:.1} {:.1}l9 10\"/>{}</g>",
                    top + 8.0,
                    top + 15.0,
                    cx - 10.0,
                    top + 19.0,
                    top + 27.0,
                    top + 27.0,
                    label(cx, top + 46.0, &p.1, "mm-text")
                ));
            } else {
                heads.push_str(&format!(
                    "<g class=\"mm-node\"><rect x=\"{:.1}\" y=\"{top:.1}\" width=\"{w:.1}\" height=\"{head_h:.1}\" rx=\"3\"/>{}</g>",
                    cx - w / 2.0,
                    label(cx, top + head_h / 2.0, &p.1, "mm-text")
                ));
            }
        }
    }
    let _ = total_w;
    let (vx, vw, vh) = (left - 20.0, right - left + 40.0, bottom + head_h + 30.0);
    Ok(format!(
        "<svg class=\"mm mm-seq\" viewBox=\"{vx:.0} -10 {vw:.0} {vh:.0}\" width=\"{vw:.0}\" role=\"img\" xmlns=\"http://www.w3.org/2000/svg\">{heads}{g}{body}</svg>"
    ))
}

// ------------------------------------------------------------------ pie charts

fn pie_svg(src: &str) -> Result<String, String> {
    let mut title = String::new();
    let mut slices: Vec<(String, f64)> = vec![];
    let first = src.lines().next().unwrap_or("");
    if let Some(t) = first.split_once("title") {
        title = clean_text(t.1);
    }
    for raw in src.lines().skip(1) {
        let s = raw.trim();
        if let Some(t) = s.strip_prefix("title") {
            title = clean_text(t);
            continue;
        }
        if let Some((k, v)) = s.rsplit_once(':') {
            if let Ok(n) = v.trim().parse::<f64>() {
                if n > 0.0 && n.is_finite() {
                    slices.push((clean_text(k), n));
                }
            }
        }
    }
    if slices.is_empty() {
        return Err("no slices (write \"Label\" : 42)".into());
    }
    let total: f64 = slices.iter().map(|s| s.1).sum();
    let (cx, cy, r) = (130.0, 140.0, 110.0);
    let mut g = String::new();
    let mut a0 = -std::f64::consts::FRAC_PI_2;
    for (i, (name, v)) in slices.iter().enumerate() {
        let frac = v / total;
        let a1 = a0 + frac * std::f64::consts::TAU;
        let cls = format!("mm-slice s{}", i % 8);
        if frac >= 0.9999 {
            g.push_str(&format!(
                "<circle class=\"{cls}\" cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\"/>"
            ));
        } else {
            let (x0, y0) = (cx + r * a0.cos(), cy + r * a0.sin());
            let (x1, y1) = (cx + r * a1.cos(), cy + r * a1.sin());
            g.push_str(&format!(
                "<path class=\"{cls}\" d=\"M{cx} {cy}L{x0:.2} {y0:.2}A{r} {r} 0 {} 1 {x1:.2} {y1:.2}z\"/>",
                u8::from(frac > 0.5)
            ));
        }
        if frac > 0.05 {
            let am = (a0 + a1) / 2.0;
            g.push_str(&format!(
                "<text class=\"mm-pct\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" dominant-baseline=\"central\">{:.0}%</text>",
                cx + r * 0.62 * am.cos(),
                cy + r * 0.62 * am.sin(),
                frac * 100.0
            ));
        }
        g.push_str(&format!(
            "<rect class=\"{cls}\" x=\"270\" y=\"{:.1}\" width=\"12\" height=\"12\" rx=\"2\"/><text class=\"mm-text\" x=\"290\" y=\"{:.1}\">{} ({})</text>",
            40.0 + i as f64 * 22.0,
            51.0 + i as f64 * 22.0,
            esc(name),
            crate::query::Val::Num(*v).text()
        ));
        a0 = a1;
    }
    let legend_w = slices
        .iter()
        .map(|s| text_w(&s.0) + 60.0)
        .fold(0.0, f64::max);
    let w = 290.0 + legend_w;
    let h = (290.0f64).max(60.0 + slices.len() as f64 * 22.0);
    let t = if title.is_empty() {
        String::new()
    } else {
        format!(
            "<text class=\"mm-title\" x=\"{:.1}\" y=\"16\" text-anchor=\"middle\">{}</text>",
            w / 2.0,
            esc(&title)
        )
    };
    Ok(format!(
        "<svg class=\"mm mm-pie\" viewBox=\"0 0 {w:.0} {h:.0}\" width=\"{w:.0}\" role=\"img\" xmlns=\"http://www.w3.org/2000/svg\">{t}{g}</svg>"
    ))
}

/// A mermaid block as HTML: the drawing, or the code with a short note when it cannot be drawn.
pub fn mermaid_html(src: &str) -> String {
    let body = src.trim();
    // front matter (---\ntitle: x\n---) and %%{init}%% lines carry settings, not the diagram
    let body = match body.strip_prefix("---") {
        Some(rest) => rest
            .split_once("\n---")
            .map_or(body, |(_, after)| after.trim_start()),
        None => body,
    };
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("%%"))
        .collect();
    let body = lines.join("\n");
    let kind = body.split_whitespace().next().unwrap_or("");
    let drawn = match kind {
        "graph" | "flowchart" => flow_svg(&body, false),
        "stateDiagram" | "stateDiagram-v2" => flow_svg(&body, true),
        "sequenceDiagram" => sequence_svg(&body),
        "pie" => pie_svg(&body),
        "" => Err("empty".into()),
        other => Err(format!("{other} diagrams are not drawn yet")),
    };
    match drawn {
        Ok(svg) => format!("<figure class=\"mermaid\" data-kind=\"{}\">{svg}</figure>", esc(kind)),
        Err(why) => format!(
            "<figure class=\"mermaid not-drawn\"><figcaption>{}</figcaption><pre data-lang=\"mermaid\"><code class=\"language-mermaid\">{}</code></pre></figure>",
            esc(&why),
            esc(src)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flowcharts() {
        let f = parse_flow("graph LR\n  A[Start here] --> B{Ready?}\n  B -->|yes| C((Ship))\n  B -- no --> D[(Store)]\n  C & D --> E>Done]\n  E -.-> A\n  F ==> G\n  %% a comment\n  classDef x fill:#f00", false);
        let ids: Vec<&str> = f.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["A", "B", "C", "D", "E", "F", "G"]);
        assert_eq!(f.nodes[0].text, "Start here");
        assert_eq!(f.nodes[1].shape, Shape::Diamond);
        assert_eq!(f.nodes[2].shape, Shape::Circle);
        assert_eq!(f.nodes[3].shape, Shape::Cyl);
        assert_eq!(f.nodes[4].shape, Shape::Flag);
        assert_eq!(f.edges.len(), 7, "{:?}", f.edges);
        assert_eq!(f.edges[1].text, "yes");
        assert_eq!(f.edges[2].text, "no");
        assert!(f.edges[2].head_b);
        assert!(f.edges[5].dotted && f.edges[5].head_b);
        assert!(f.edges[6].thick);
        assert_eq!(f.dir, "LR");
        let html = mermaid_html("graph TD\nA[\"Quote (with brackets)\"] --> B\nsubgraph One [The group]\nB --> C\nend\nC --> A");
        assert!(
            html.starts_with("<figure class=\"mermaid\" data-kind=\"graph\"><svg"),
            "{html}"
        );
        assert!(
            html.contains("Quote (with brackets)") && html.contains("The group"),
            "{html}"
        );
        assert_eq!(html.matches("class=\"mm-node\"").count(), 3);
        assert!(html.contains("mm-group"));
    }

    #[test]
    fn layers_follow_the_arrows() {
        let mut f = parse_flow("graph TD\nA --> B\nB --> C\nA --> C\nC --> A", false);
        layout(&mut f);
        let y = |id: &str| f.nodes.iter().find(|n| n.id == id).unwrap().y;
        assert!(
            y("A") < y("B") && y("B") < y("C"),
            "a cycle is broken, the rest flows down"
        );
        let mut g = parse_flow("graph LR\nA --> B", false);
        layout(&mut g);
        assert!(g.nodes[0].x < g.nodes[1].x && (g.nodes[0].y - g.nodes[1].y).abs() < 1e-6);
        let mut h = parse_flow("graph BT\nA --> B", false);
        layout(&mut h);
        assert!(h.nodes[0].y > h.nodes[1].y);
    }

    #[test]
    fn state_sequence_and_pie() {
        let s = mermaid_html("stateDiagram-v2\n[*] --> Idle\nIdle --> Busy : start\nBusy --> Idle : done\nBusy --> [*]");
        assert!(
            s.contains("data-kind=\"stateDiagram-v2\"")
                && s.contains(">start<")
                && s.matches("mm-dot").count() == 2,
            "{s}"
        );
        let q = mermaid_html("sequenceDiagram\nautonumber\nparticipant A as Riley\nactor T as Tempo\nA->>T: Mix the song\nT-->>A: Done\nNote over A,T: in the studio\nloop Every day\nT->>T: learn\nend\nalt ok\nA->>T: thanks\nelse not ok\nA-xT: retry\nend");
        assert!(
            q.contains("data-kind=\"sequenceDiagram\"")
                && q.contains("1. Mix the song")
                && q.contains("2. Done"),
            "{q}"
        );
        assert!(
            q.contains("mm-actor")
                && q.contains("[Every day]")
                && q.contains("[not ok]")
                && q.contains("in the studio"),
            "{q}"
        );
        let p =
            mermaid_html("pie title Time\n\"Mixing\" : 40\n\"Mastering\" : 25\n\"Learning\" : 35");
        assert!(
            p.contains("mm-pie")
                && p.contains("40%")
                && p.contains("Mixing (40)")
                && p.contains(">Time<"),
            "{p}"
        );
    }

    #[test]
    fn what_cannot_be_drawn_shows_as_code_and_stays_safe() {
        let g = mermaid_html("gantt\ntitle A plan");
        assert!(
            g.contains("not-drawn")
                && g.contains("gantt diagrams are not drawn yet")
                && g.contains("language-mermaid")
        );
        let x =
            mermaid_html("graph TD\nA[<script>alert(1)</script>] --> B[\"<img src=x onerror=y>\"]");
        assert!(!x.contains("<script>") && !x.contains("<img"), "{x}");
        let _ = mermaid_html("graph TD\n-->\n[[[\n((");
        let _ = mermaid_html("sequenceDiagram\n->>: x\nNote over : y\nend\nend");
        let _ = mermaid_html("pie\n\"a\" : -1\n\"b\" : nan");
        let big = (0..500)
            .map(|i| format!("N{i} --> N{}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(mermaid_html(&format!("graph TD\n{big}")).contains("too big"));
    }
}
