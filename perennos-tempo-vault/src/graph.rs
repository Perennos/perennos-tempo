//! The vault as a graph, in the memory-graph shape the Perennos Tempo GUI already draws
//! (`{nodes:[{id,src,name,kind,chunks,used}],edges:[{a,b,w}]}`), plus positions computed here in
//! Rust (`pos3`, `pos2`) so even a large vault never lays itself out in the browser.
//!
//! Kinds: `note`, `daily`, `current` (the centre of a local graph), `tag`, `attachment` and
//! `unresolved` (a link to a note that does not exist yet). `chunks` sizes a node (its degree,
//! or how many notes carry a tag); `used` marks notes changed in the last day.
use crate::util::{file_name, fnv64, parse_date, stem, unix_now};
use crate::vault::Vault;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

#[derive(Debug, Clone)]
pub struct GraphOpts {
    /// A local graph around this path; `None` is the whole vault.
    pub center: Option<String>,
    /// Hops from the centre for a local graph.
    pub depth: usize,
    pub tags: bool,
    pub attachments: bool,
    pub unresolved: bool,
    /// Keep notes with no connections (whole-vault graph only).
    pub orphans: bool,
    /// Only notes that match this search.
    pub filter: Option<String>,
    /// Notes changed within this many seconds are `used` (highlighted and labelled).
    pub recent_secs: f64,
    pub max_nodes: usize,
    pub layout: bool,
}

impl Default for GraphOpts {
    fn default() -> Self {
        GraphOpts {
            center: None,
            depth: 2,
            tags: false,
            attachments: true,
            unresolved: false,
            orphans: true,
            filter: None,
            recent_secs: 86_400.0,
            max_nodes: 5_000,
            layout: true,
        }
    }
}

struct G {
    ids: HashMap<String, usize>,
    nodes: Vec<(String, String, &'static str, usize, bool)>, // src, name, kind, chunks, used
    edges: BTreeMap<(usize, usize), f32>,
}

impl G {
    fn node(&mut self, src: &str, name: &str, kind: &'static str, used: bool) -> usize {
        if let Some(&i) = self.ids.get(src) {
            return i;
        }
        let i = self.nodes.len();
        self.ids.insert(src.to_string(), i);
        self.nodes
            .push((src.to_string(), name.to_string(), kind, 0, used));
        i
    }
    fn edge(&mut self, a: usize, b: usize, w: f32) {
        if a == b {
            return;
        }
        let k = (a.min(b), a.max(b));
        let e = self.edges.entry(k).or_insert(0.0);
        *e = (*e + w).min(1.0);
    }
}

fn is_daily(path: &str) -> bool {
    parse_date(stem(path)).is_some()
}

/// Undirected neighbours over notes, attachments and unresolved targets.
fn adjacency(v: &Vault, o: &GraphOpts) -> HashMap<String, Vec<String>> {
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for n in v.notes() {
        for (l, r) in v.links_of(&n.path) {
            let t = match r {
                Some(t) if v.note(&t).is_some() || o.attachments => t,
                Some(_) => continue,
                None if o.unresolved && !l.target.is_empty() => {
                    format!("?{}", l.target.to_lowercase())
                }
                None => continue,
            };
            if t == n.path {
                continue;
            }
            adj.entry(n.path.clone()).or_default().push(t.clone());
            adj.entry(t).or_default().push(n.path.clone());
        }
    }
    adj
}

/// Build the graph. `now` is passed in so tests are stable.
pub fn build(v: &Vault, o: &GraphOpts, now: f64) -> Value {
    let adj = adjacency(v, o);
    let filter: Option<HashSet<String>> = o
        .filter
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(|q| {
            v.search(q, usize::MAX)
                .into_iter()
                .map(|h| h.path)
                .collect()
        });
    let mut keep: Option<HashSet<String>> = None;
    if let Some(c) = &o.center {
        let mut seen: HashSet<String> = HashSet::new();
        let mut q = VecDeque::new();
        seen.insert(c.clone());
        q.push_back((c.clone(), 0usize));
        while let Some((p, d)) = q.pop_front() {
            if d >= o.depth {
                continue;
            }
            for nb in adj.get(&p).into_iter().flatten() {
                if seen.insert(nb.clone()) {
                    q.push_back((nb.clone(), d + 1));
                }
            }
        }
        keep = Some(seen);
    }
    let allowed = |p: &str| {
        keep.as_ref().is_none_or(|k| k.contains(p))
            && (filter.as_ref().is_none_or(|f| f.contains(p)) || o.center.as_deref() == Some(p))
    };
    let mut g = G {
        ids: HashMap::new(),
        nodes: vec![],
        edges: BTreeMap::new(),
    };
    // highlight (and label) only the few notes touched most recently, so the picture stays calm
    let mut fresh: Vec<(f64, &str)> = v
        .notes()
        .filter(|n| now - n.mtime < o.recent_secs)
        .map(|n| (n.mtime, n.path.as_str()))
        .collect();
    fresh.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let recent: HashSet<&str> = fresh.into_iter().take(12).map(|(_, p)| p).collect();
    let templates = format!(
        "{}/",
        crate::write::VaultConfig::load(&v.root)
            .templates_folder
            .trim_matches('/')
    );
    for n in v.notes() {
        if !allowed(&n.path)
            || (n.path.starts_with(&templates) && o.center.as_deref() != Some(n.path.as_str()))
        {
            continue;
        }
        let kind = if o.center.as_deref() == Some(n.path.as_str()) {
            "current"
        } else if is_daily(&n.path) {
            "daily"
        } else {
            "note"
        };
        g.node(&n.path, &n.title, kind, recent.contains(n.path.as_str()));
    }
    for n in v.notes() {
        let Some(&a) = g.ids.get(&n.path) else {
            continue;
        };
        for (l, r) in v.links_of(&n.path) {
            let b = match r {
                Some(t) if v.note(&t).is_some() => match g.ids.get(&t) {
                    Some(&b) => b,
                    None => continue,
                },
                Some(t) if o.attachments && allowed(&t) => {
                    g.node(&t, file_name(&t), "attachment", false)
                }
                None if o.unresolved && !l.target.is_empty() => {
                    let src = format!("?{}", l.target.to_lowercase());
                    if !allowed(&src) && keep.is_some() {
                        continue;
                    }
                    g.node(&src, &l.target, "unresolved", false)
                }
                _ => continue,
            };
            g.edge(a, b, if l.embed { 0.3 } else { 0.25 });
        }
        if o.tags {
            for t in &n.parsed.tags {
                let src = format!("#{}", t.to_lowercase());
                let b = g.node(&src, &format!("#{t}"), "tag", false);
                g.edge(a, b, 0.08);
            }
        }
    }
    // degree (or, for tags, how many notes carry them)
    let mut deg = vec![0usize; g.nodes.len()];
    for (a, b) in g.edges.keys() {
        deg[*a] += 1;
        deg[*b] += 1;
    }
    // drop unconnected notes when asked, then cap the size by keeping the best connected
    let mut order: Vec<usize> = (0..g.nodes.len())
        .filter(|&i| o.orphans || o.center.is_some() || deg[i] > 0)
        .collect();
    if order.len() > o.max_nodes {
        order.sort_by_key(|&i| {
            (
                std::cmp::Reverse(deg[i] + usize::from(g.nodes[i].2 == "current") * 1_000_000),
                i,
            )
        });
        order.truncate(o.max_nodes);
        order.sort_unstable();
    }
    let remap: HashMap<usize, usize> = order
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new))
        .collect();
    let nodes: Vec<Value> = order
        .iter()
        .enumerate()
        .map(|(new, &old)| {
            let (src, name, kind, _, used) = &g.nodes[old];
            json!({"id": new, "src": src, "name": name, "kind": kind, "chunks": deg[old].max(1), "used": used})
        })
        .collect();
    let edges: Vec<(usize, usize, f32)> = g
        .edges
        .iter()
        .filter_map(|((a, b), w)| {
            Some((*remap.get(a)?, *remap.get(b)?, (*w * 100.0).round() / 100.0))
        })
        .collect();
    let mut out = json!({
        "nodes": nodes,
        "edges": edges.iter().map(|(a, b, w)| json!({"a": a, "b": b, "w": w})).collect::<Vec<_>>(),
        "total_notes": v.notes().count(),
    });
    if o.layout {
        add_layout(&mut out);
    }
    out
}

/// Positions (`pos3`, `pos2`) for a graph built with `layout: false`, e.g. outside a lock.
pub fn add_layout(g: &mut Value) {
    let n = g["nodes"].as_array().map_or(0, Vec::len);
    let edges: Vec<(usize, usize, f32)> = g["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some((
                e["a"].as_u64()? as usize,
                e["b"].as_u64()? as usize,
                e["w"].as_f64()? as f32,
            ))
        })
        .filter(|(a, b, _)| *a < n && *b < n)
        .collect();
    g["pos3"] = json!(round(&layout(n, &edges, 3)));
    // the flat (no WebGL) view: its own layout for small vaults; for big ones the 3D picture
    // seen from the front, which costs nothing
    g["pos2"] = if n <= 1_500 {
        json!(round(&layout(n, &edges, 2)))
    } else {
        let mut p = g["pos3"].as_array().cloned().unwrap_or_default();
        for z in p.iter_mut().skip(2).step_by(3) {
            *z = json!(0.0);
        }
        Value::Array(p)
    };
}

fn round(p: &[f32]) -> Vec<f32> {
    p.iter()
        .map(|x| (x * 10_000.0).round() / 10_000.0)
        .collect()
}

/// A stable fingerprint of a graph's nodes and edges (for caching its layout).
pub fn signature(g: &Value) -> u64 {
    let mut s = String::new();
    for n in g["nodes"].as_array().into_iter().flatten() {
        s.push_str(n["src"].as_str().unwrap_or(""));
        s.push('\n');
    }
    s.push_str(&g["edges"].to_string());
    fnv64(s.as_bytes())
}

// ---- layout: Fruchterman-Reingold, seeded so the same vault always draws the same picture.
// Small graphs use exact repulsion (the same as the GUI's own layout); larger ones use a
// Barnes-Hut octree, so 10,000 notes lay out in about a second on a laptop CPU.

struct Cell {
    center: [f32; 3],
    half: f32,
    mass: f32,
    com: [f32; 3],
    kids: [u32; 8],
    body: u32,
}

const NONE: u32 = u32::MAX;

struct Tree {
    cells: Vec<Cell>,
}

impl Tree {
    fn new(pos: &[f32], n: usize) -> Tree {
        let mut lo = [f32::MAX; 3];
        let mut hi = [f32::MIN; 3];
        for i in 0..n {
            for d in 0..3 {
                lo[d] = lo[d].min(pos[i * 3 + d]);
                hi[d] = hi[d].max(pos[i * 3 + d]);
            }
        }
        let center = [
            (lo[0] + hi[0]) / 2.0,
            (lo[1] + hi[1]) / 2.0,
            (lo[2] + hi[2]) / 2.0,
        ];
        let half = (0..3).map(|d| hi[d] - lo[d]).fold(1e-3f32, f32::max) / 2.0 + 1e-3;
        let mut t = Tree {
            cells: Vec::with_capacity(n * 2),
        };
        t.cells.push(Cell {
            center,
            half,
            mass: 0.0,
            com: [0.0; 3],
            kids: [NONE; 8],
            body: NONE,
        });
        for i in 0..n {
            t.insert(0, i as u32, pos, 0);
        }
        t.finish(0);
        t
    }

    fn octant(c: &Cell, p: &[f32]) -> usize {
        usize::from(p[0] > c.center[0])
            | usize::from(p[1] > c.center[1]) << 1
            | usize::from(p[2] > c.center[2]) << 2
    }

    fn child(&mut self, at: usize, oct: usize) -> usize {
        let k = self.cells[at].kids[oct];
        if k != NONE {
            return k as usize;
        }
        let (c, h) = (self.cells[at].center, self.cells[at].half / 2.0);
        let center = [
            c[0] + if oct & 1 != 0 { h } else { -h },
            c[1] + if oct & 2 != 0 { h } else { -h },
            c[2] + if oct & 4 != 0 { h } else { -h },
        ];
        let id = self.cells.len();
        self.cells.push(Cell {
            center,
            half: h,
            mass: 0.0,
            com: [0.0; 3],
            kids: [NONE; 8],
            body: NONE,
        });
        self.cells[at].kids[oct] = id as u32;
        id
    }

    fn insert(&mut self, at: usize, i: u32, pos: &[f32], depth: usize) {
        let p = &pos[i as usize * 3..i as usize * 3 + 3];
        let c = &mut self.cells[at];
        c.mass += 1.0;
        for (acc, x) in c.com.iter_mut().zip(p) {
            *acc += x;
        }
        let leaf = c.kids.iter().all(|k| *k == NONE);
        if leaf && c.body == NONE && c.mass == 1.0 {
            c.body = i;
            return;
        }
        if depth > 24 {
            return; // (nearly) the same point twice: keep the mass, stop splitting
        }
        if c.body != NONE {
            let old = c.body;
            c.body = NONE;
            let oc = Self::octant(
                &self.cells[at],
                &pos[old as usize * 3..old as usize * 3 + 3],
            );
            let ch = self.child(at, oc);
            self.insert(ch, old, pos, depth + 1);
        }
        let oc = Self::octant(&self.cells[at], p);
        let ch = self.child(at, oc);
        self.insert(ch, i, pos, depth + 1);
    }

    fn finish(&mut self, at: usize) {
        let m = self.cells[at].mass;
        if m > 0.0 {
            for d in 0..3 {
                self.cells[at].com[d] /= m;
            }
        }
        for k in self.cells[at].kids {
            if k != NONE {
                self.finish(k as usize);
            }
        }
    }

    fn repulse(&self, i: usize, pos: &[f32], k2: f32, theta: f32, out: &mut [f32; 3]) {
        let p = [pos[i * 3], pos[i * 3 + 1], pos[i * 3 + 2]];
        let mut stack = vec![0usize];
        while let Some(at) = stack.pop() {
            let c = &self.cells[at];
            if c.mass == 0.0 || c.body == i as u32 {
                continue;
            }
            let dx = p[0] - c.com[0];
            let dy = p[1] - c.com[1];
            let dz = p[2] - c.com[2];
            let d2 = dx * dx + dy * dy + dz * dz + 1e-6;
            let leaf = c.kids.iter().all(|k| *k == NONE);
            if leaf || (2.0 * c.half) * (2.0 * c.half) < theta * theta * d2 {
                // a leaf holding this node among coincident others still pushes the others
                let m = if leaf && c.body == NONE && c.mass > 1.0 && d2 < 1e-5 {
                    c.mass - 1.0
                } else {
                    c.mass
                };
                let f = k2 / d2 * m;
                out[0] += dx * f;
                out[1] += dy * f;
                out[2] += dz * f;
            } else {
                stack.extend(c.kids.iter().filter(|k| **k != NONE).map(|k| *k as usize));
            }
        }
    }
}

/// Positions for `n` nodes (x, y, z per node, scaled into the unit sphere). `dims` is 2 or 3.
pub fn layout(n: usize, edges: &[(usize, usize, f32)], dims: usize) -> Vec<f32> {
    let mut pos = vec![0f32; n * 3];
    if n == 0 {
        return pos;
    }
    let mut seed: u64 = 7;
    let mut rnd = || {
        seed = seed * 16_807 % 2_147_483_647;
        seed as f32 / 2_147_483_647.0 - 0.5
    };
    for (i, p) in pos.iter_mut().enumerate() {
        if dims == 3 || i % 3 != 2 {
            *p = rnd() * 2.0;
        }
    }
    let iterations = if n <= 2_000 {
        240
    } else {
        (240 * 2_000 / n).max(80)
    };
    let k = (8.0 / n as f32).cbrt();
    let k2 = k * k;
    let mut t = 0.25f32;
    let mut disp = vec![0f32; n * 3];
    for _ in 0..iterations {
        disp.fill(0.0);
        if n <= 600 {
            for i in 0..n {
                for j in i + 1..n {
                    let dx = pos[i * 3] - pos[j * 3];
                    let dy = pos[i * 3 + 1] - pos[j * 3 + 1];
                    let dz = pos[i * 3 + 2] - pos[j * 3 + 2];
                    let f = k2 / (dx * dx + dy * dy + dz * dz + 1e-6);
                    disp[i * 3] += dx * f;
                    disp[i * 3 + 1] += dy * f;
                    disp[i * 3 + 2] += dz * f;
                    disp[j * 3] -= dx * f;
                    disp[j * 3 + 1] -= dy * f;
                    disp[j * 3 + 2] -= dz * f;
                }
            }
        } else {
            let tree = Tree::new(&pos, n);
            for i in 0..n {
                let mut f = [0f32; 3];
                tree.repulse(i, &pos, k2, 0.9, &mut f);
                disp[i * 3] += f[0];
                disp[i * 3 + 1] += f[1];
                disp[i * 3 + 2] += f[2];
            }
        }
        for &(a, b, w) in edges {
            let (a, b) = (a * 3, b * 3);
            let dx = pos[a] - pos[b];
            let dy = pos[a + 1] - pos[b + 1];
            let dz = pos[a + 2] - pos[b + 2];
            let f = ((dx * dx + dy * dy + dz * dz).sqrt() / k) * (1.0 + 3.0 * (w - 0.17).max(0.0));
            disp[a] -= dx * f;
            disp[a + 1] -= dy * f;
            disp[a + 2] -= dz * f;
            disp[b] += dx * f;
            disp[b + 1] += dy * f;
            disp[b + 2] += dz * f;
        }
        let pull = 0.08 * n as f32 * k;
        for i in 0..n * 3 {
            disp[i] -= pos[i] * pull;
        }
        for i in 0..n {
            let (dx, dy, dz) = (disp[i * 3], disp[i * 3 + 1], disp[i * 3 + 2]);
            let len = (dx * dx + dy * dy + dz * dz).sqrt() + 1e-9;
            let s = len.min(t) / len;
            pos[i * 3] += dx * s;
            pos[i * 3 + 1] += dy * s;
            if dims == 3 {
                pos[i * 3 + 2] += dz * s;
            }
        }
        t *= 0.985;
    }
    let r = (0..n)
        .map(|i| (pos[i * 3].powi(2) + pos[i * 3 + 1].powi(2) + pos[i * 3 + 2].powi(2)).sqrt())
        .fold(1e-9f32, f32::max);
    for p in &mut pos {
        *p /= r;
    }
    pos
}

/// The whole-vault graph with the defaults (for the CLI and MCP).
pub fn vault_graph(v: &Vault, o: &GraphOpts) -> Value {
    build(v, o, unix_now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::{put, sample};

    fn names(g: &Value) -> Vec<(String, String)> {
        g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                (
                    n["src"].as_str().unwrap().to_string(),
                    n["kind"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn whole_vault_and_local_graphs() {
        let (d, mut v) = sample();
        put(&d, "2026-11-06.md", "[[Home]]");
        v.refresh();
        let now = 4_000_000_000.0; // nothing counts as recent
        let g = build(
            &v,
            &GraphOpts {
                layout: false,
                ..GraphOpts::default()
            },
            now,
        );
        let n = names(&g);
        assert!(n.contains(&("Home.md".into(), "note".into())));
        assert!(n.contains(&("2026-11-06.md".into(), "daily".into())));
        assert!(n.contains(&("img/logo.png".into(), "attachment".into())));
        assert!(!n.iter().any(|(_, k)| k == "tag" || k == "unresolved"));
        let idx = |s: &str| n.iter().position(|(p, _)| p == s).unwrap() as u64;
        let edges = g["edges"].as_array().unwrap();
        let has = |a: &str, b: &str| {
            edges.iter().any(|e| {
                (e["a"] == idx(a) && e["b"] == idx(b)) || (e["a"] == idx(b) && e["b"] == idx(a))
            })
        };
        assert!(
            has("Home.md", "Ideas.md")
                && has("Ideas.md", "Projects/Tempo.md")
                && has("Projects/Tempo.md", "img/logo.png")
        );
        assert!(g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["used"] == false));
        let all = build(
            &v,
            &GraphOpts {
                layout: false,
                tags: true,
                unresolved: true,
                ..GraphOpts::default()
            },
            now,
        );
        let n = names(&all);
        assert!(
            n.contains(&("#project".into(), "tag".into()))
                && n.contains(&("?missing note".into(), "unresolved".into()))
        );
        let tag = n.iter().position(|(p, _)| p == "#project").unwrap();
        assert_eq!(
            all["nodes"][tag]["chunks"], 1,
            "one note carries #project itself (#project/ideas is its own tag)"
        );
        let local = build(
            &v,
            &GraphOpts {
                center: Some("Archive/Index.md".into()),
                depth: 1,
                layout: false,
                ..GraphOpts::default()
            },
            now,
        );
        let n = names(&local);
        assert_eq!(n.len(), 3, "{n:?}");
        assert!(
            n.contains(&("Archive/Index.md".into(), "current".into()))
                && n.contains(&("Archive/Ideas.md".into(), "note".into()))
                && n.contains(&("Projects/Tempo.md".into(), "note".into()))
        );
        let filtered = build(
            &v,
            &GraphOpts {
                filter: Some("perennos".into()),
                layout: false,
                ..GraphOpts::default()
            },
            now,
        );
        assert_eq!(
            names(&filtered).iter().filter(|(_, k)| k == "note").count(),
            2
        );
        let recent = build(
            &v,
            &GraphOpts {
                layout: false,
                ..GraphOpts::default()
            },
            unix_now(),
        );
        assert!(
            recent["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["used"] == true),
            "just-written notes are recent"
        );
    }

    #[test]
    fn layout_is_stable_bounded_and_scales() {
        let edges = vec![(0, 1, 0.25), (1, 2, 0.25), (2, 0, 0.25), (3, 4, 0.25)];
        let a = layout(5, &edges, 3);
        assert_eq!(
            a,
            layout(5, &edges, 3),
            "seeded: the same input draws the same picture"
        );
        assert!(a.iter().all(|x| x.is_finite() && x.abs() <= 1.0001));
        let flat = layout(5, &edges, 2);
        assert!(flat.chunks(3).all(|p| p[2] == 0.0));
        // a big graph goes through the octree and stays finite and spread out
        let n = 900;
        let big: Vec<(usize, usize, f32)> = (1..n).map(|i| (i, (i * 7 + 3) % i, 0.25)).collect();
        let t0 = std::time::Instant::now();
        let p = layout(n, &big, 3);
        assert!(p.iter().all(|x| x.is_finite()));
        let mut pts: Vec<(i32, i32, i32)> = p
            .chunks(3)
            .map(|c| {
                (
                    (c[0] * 50.0) as i32,
                    (c[1] * 50.0) as i32,
                    (c[2] * 50.0) as i32,
                )
            })
            .collect();
        pts.sort_unstable();
        pts.dedup();
        assert!(
            pts.len() > n / 2,
            "nodes do not collapse onto each other ({} distinct cells)",
            pts.len()
        );
        assert!(t0.elapsed().as_secs() < 60);
        assert!(layout(0, &[], 3).is_empty());
        let one = layout(1, &[], 3);
        assert!(one.iter().all(|x| x.is_finite()));
    }
}
