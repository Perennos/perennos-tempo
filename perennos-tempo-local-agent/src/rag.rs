//! RAG over markdown files: paragraph chunks, CPU embeddings, cosine + keyword score.
//! If embedding fails, keyword-only search still works.
use crate::config::Config;
use crate::mcp::tok;
use crate::ollama;
use crate::util::{self, glob_files, mtime_of};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct RagFile {
    pub src: String,
    pub mtime: f64,
    pub chunks: Vec<String>,
    pub vecs: Option<Vec<Vec<f64>>>,
    pub from_glob: bool,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub score: f64,
    pub src: String,
    pub chunk: i64,
    pub text: String,
}

#[derive(Default)]
pub struct Rag {
    docs: Option<Vec<RagFile>>,
    pub embed_ok: Option<bool>,
}

/// Same splitting as the Python `chunks_of`: blank-line paragraphs, ~1100 char chunks, hard split > 1800 at 1500.
pub fn chunks_of_text(txt: &str) -> Vec<String> {
    let para_rx = Regex::new(r"\n\s*\n").expect("regex");
    let (mut out, mut cur) = (Vec::<String>::new(), String::new());
    for para in para_rx.split(txt) {
        if cur.chars().count() + para.chars().count() > 1100 && !cur.is_empty() {
            out.push(cur.trim().to_string());
            cur.clear();
        }
        cur.push_str(para);
        cur.push_str("\n\n");
        while cur.chars().count() > 1800 {
            out.push(util::head_chars(&cur, 1500));
            cur = cur.chars().skip(1500).collect();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

pub fn chunks_of(path: &Path) -> Vec<String> {
    let b = std::fs::read(path).unwrap_or_default();
    chunks_of_text(&String::from_utf8_lossy(&b))
}

fn cos(a: &[f64], b: &[f64]) -> f64 {
    let d: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    d / (na * nb + 1e-9)
}

fn sources(cfg: &Config) -> Vec<(String, bool)> {
    let mut files: Vec<(String, bool)> = cfg.rag_files.iter().map(|f| (f.clone(), false)).collect();
    for (g, n) in &cfg.rag_globs {
        let mut fs = glob_files(g);
        if *n > 0 {
            fs.truncate(*n);
        }
        files.extend(
            fs.into_iter()
                .map(|p| (p.to_string_lossy().into_owned(), true)),
        );
    }
    let mut seen = HashSet::new();
    files
        .into_iter()
        .filter(|(f, _)| Path::new(f).is_file() && seen.insert(f.clone()))
        .collect()
}

// serde_json parses floats without exact round-trip (1 ULP drift), so mtimes are compared with a tolerance.

fn index_path(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("rag-index.json")
}

fn load_index(cfg: &Config) -> Vec<RagFile> {
    let Ok(txt) = std::fs::read_to_string(index_path(cfg)) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return vec![];
    };
    v.get("files")
        .and_then(|f| f.as_array())
        .into_iter()
        .flatten()
        .filter_map(|d| {
            Some(RagFile {
                src: d.get("src")?.as_str()?.to_string(),
                mtime: d.get("mtime")?.as_f64()?,
                chunks: d
                    .get("chunks")?
                    .as_array()?
                    .iter()
                    .filter_map(|c| c.as_str().map(String::from))
                    .collect(),
                vecs: d.get("vecs").and_then(|v| v.as_array()).map(|a| {
                    a.iter()
                        .map(|r| {
                            r.as_array()
                                .map(|x| x.iter().filter_map(|n| n.as_f64()).collect())
                                .unwrap_or_default()
                        })
                        .collect()
                }),
                from_glob: d
                    .get("from_glob")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn save_index(cfg: &Config, files: &[RagFile]) {
    let arr: Vec<Value> = files.iter().map(|f| json!({"src": f.src, "mtime": f.mtime, "chunks": f.chunks, "vecs": f.vecs, "from_glob": f.from_glob})).collect();
    let tmp = index_path(cfg).with_extension("json.tmp");
    let _ = std::fs::create_dir_all(&cfg.data_dir);
    if std::fs::write(
        &tmp,
        json!({"files": arr, "built": util::now_str()}).to_string(),
    )
    .is_ok()
    {
        let _ = std::fs::rename(&tmp, index_path(cfg));
    }
}

impl Rag {
    pub fn new() -> Rag {
        Rag::default()
    }

    fn embed_batches(&mut self, cfg: &Config, cs: &[String]) -> Option<Vec<Vec<f64>>> {
        if self.embed_ok == Some(false) {
            return None; // already failed in this process: do not wait on a dead engine again
        }
        let mut vecs = Vec::new();
        for b in cs.chunks(16) {
            match ollama::embed(cfg, b) {
                Some(v) => vecs.extend(v),
                None => {
                    self.embed_ok = Some(false);
                    return None;
                }
            }
        }
        self.embed_ok = Some(true);
        Some(vecs)
    }

    /// Rebuild the index, reusing every file whose mtime is unchanged (and that already has vectors).
    pub fn build_index(&mut self, cfg: &Config, force: bool) -> usize {
        let old: Vec<RagFile> = if force { vec![] } else { load_index(cfg) };
        let mut files = Vec::new();
        for (p, from_glob) in sources(cfg) {
            let m = mtime_of(Path::new(&p));
            if let Some(o) = old
                .iter()
                .find(|o| o.src == p && (o.mtime - m).abs() < 0.001 && o.vecs.is_some())
            {
                files.push(RagFile {
                    from_glob,
                    ..o.clone()
                });
                continue;
            }
            let cs = chunks_of(Path::new(&p));
            let vecs = if cs.is_empty() {
                Some(vec![])
            } else {
                self.embed_batches(cfg, &cs)
            };
            files.push(RagFile {
                src: p,
                mtime: m,
                chunks: cs,
                vecs,
                from_glob,
            });
        }
        save_index(cfg, &files);
        let n = files.len();
        self.docs = Some(files);
        n
    }

    fn ensure(&mut self, cfg: &Config) {
        if self.docs.is_none() {
            self.build_index(cfg, false);
        }
    }

    pub fn search(&mut self, cfg: &Config, q: &str, k: usize) -> Vec<Hit> {
        self.ensure(cfg);
        let docs = self.docs.clone().unwrap_or_default();
        let qv = if docs.iter().any(|d| d.vecs.is_some()) && self.embed_ok != Some(false) {
            match ollama::embed(cfg, &[q.to_string()]) {
                Some(mut e) => {
                    self.embed_ok = Some(true);
                    e.pop()
                }
                None => {
                    self.embed_ok = Some(false);
                    None
                }
            }
        } else {
            None
        };
        search_docs(cfg, &docs, qv.as_deref(), q, k)
    }
}

fn current_rx() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)(working on|work on|current work|doing right now|what.?s (next|going on|on)|status|overnight|morning|today|this (week|morning)|where (are|were) we|left off)").expect("regex"))
}

fn defn_rx() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)^\s*(what|who)('s| is| are)|tell me about|explain").expect("regex")
    })
}

fn is_memory_path(src: &str) -> bool {
    src.contains("/memory/") || src.contains("\\memory\\")
}

fn capture(rx: &str, text: &str) -> String {
    Regex::new(rx)
        .ok()
        .and_then(|r| r.captures(text))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default()
}

/// Pure scoring so it can be tested without a server. `qv` is the query embedding, if any.
pub fn search_docs(
    cfg: &Config,
    docs: &[RagFile],
    qv: Option<&[f64]>,
    q: &str,
    k: usize,
) -> Vec<Hit> {
    let qt = tok(q);
    let stop: HashSet<String> = [
        "what", "the", "and", "for", "are", "how", "you", "that", "this", "with", "who", "does",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let qt_h: HashSet<String> = qt.difference(&stop).cloned().collect();
    let denom = qt_h.len() as f64 + 1.0;
    let t_now = util::unix_now();
    let defn = defn_rx().is_match(q);
    let mut scored: Vec<(f64, &str, usize, &str)> = Vec::new();
    for d in docs {
        let ismem = is_memory_path(&d.src);
        let mut head = 0.0;
        if ismem && !d.chunks.is_empty() {
            let first = &d.chunks[0];
            let n = capture(r"(?m)^name:\s*(.+)$", first);
            let n = if n.is_empty() {
                Path::new(&d.src)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                n
            };
            let desc = capture(r"(?m)^description:\s*(.+)$", first);
            let n_words = tok(&n.replace('-', " "));
            let ht = tok(&format!("{} {}", n.replace('-', " "), desc));
            head = qt_h.intersection(&ht).count() as f64 / denom
                + 0.6 * qt_h.intersection(&n_words).count() as f64 / denom;
        }
        let age_d = (t_now - d.mtime) / 86400.0;
        let rec = if ismem || d.from_glob {
            0.08 * (1.0 - age_d / 14.0).max(0.0)
        } else {
            0.0
        };
        let core = if defn && cfg.rag_files.contains(&d.src) {
            0.15
        } else {
            0.0
        };
        for (i, c) in d.chunks.iter().enumerate() {
            let kw = qt.intersection(&tok(c)).count() as f64 / (qt.len() as f64 + 1.0);
            let sem = match (qv, &d.vecs) {
                (Some(qv), Some(v)) if i < v.len() => cos(qv, &v[i]),
                _ => 0.0,
            };
            let kw_core = if core > 0.0 { 0.5 * kw } else { 0.0 };
            scored.push((
                sem + 0.6 * kw + 1.2 * head + rec + core + kw_core,
                &d.src,
                i,
                c,
            ));
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    if current_rx().is_match(q) {
        for (p, tail) in &cfg.pinned_current_files {
            let Ok(b) = std::fs::read(p) else { continue };
            let txt = String::from_utf8_lossy(&b).into_owned();
            let lines: Vec<&str> = txt.lines().collect();
            let sel = if *tail > 0 {
                lines[lines.len().saturating_sub(*tail)..].join("\n")
            } else {
                lines.join("\n")
            };
            out.push(Hit {
                score: 9.0,
                src: p.clone(),
                chunk: -1,
                text: util::tail_chars(&sel, 3500),
            });
        }
    }
    let pinned: HashSet<String> = out.iter().map(|h| h.src.clone()).collect();
    let take = if out.is_empty() { k } else { 3 };
    out.extend(
        scored
            .into_iter()
            .filter(|(_, s, _, _)| !pinned.contains(*s))
            .take(take)
            .map(|(s, src, i, c)| Hit {
                score: (s * 1000.0).round() / 1000.0,
                src: src.to_string(),
                chunk: i as i64,
                text: c.to_string(),
            }),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testutil::*;

    fn cfg_with(m: Option<&Mock>, d: &Path) -> Config {
        let mut c = Config::defaults(Some(d.to_string_lossy().into_owned()));
        if let Some(m) = m {
            c.ollama_url = m.url.clone();
        } else {
            c.ollama_url = "http://127.0.0.1:1".into();
        }
        c
    }

    #[test]
    fn chunking_matches_python() {
        let t = "a\n\nb\n\nc";
        assert_eq!(chunks_of_text(t), ["a\n\nb\n\nc"]);
        let para = "x".repeat(700);
        let t = format!("{para}\n\n{para}\n\n{para}");
        let cs = chunks_of_text(&t);
        assert_eq!(cs.len(), 3); // 700+700 > 1100 so each paragraph is its own chunk
        let big = "y".repeat(4000);
        let cs = chunks_of_text(&big);
        assert_eq!(cs[0].len(), 1500);
        assert_eq!(cs[1].len(), 1500);
        assert_eq!(cs.last().unwrap().len(), 1000);
        assert!(chunks_of_text("  \n\n  ").is_empty());
    }

    #[test]
    fn embeds_in_batches_of_16_on_cpu_and_reuses_unchanged_files() {
        let d = tmpdir("rag1");
        let f = d.join("notes.md");
        let body: String = (0..20)
            .map(|i| format!("{}{}\n\n", "para ".repeat(250), i))
            .collect();
        std::fs::write(&f, body).unwrap();
        let m = Mock::start(|r| {
            let n = serde_json::from_str::<Value>(&r.body).unwrap()["input"]
                .as_array()
                .unwrap()
                .len();
            (
                200,
                json!({"embeddings": vec![vec![1.0, 0.5]; n]}).to_string(),
            )
        });
        let mut c = cfg_with(Some(&m), &d);
        c.rag_files = vec![f.to_string_lossy().into_owned()];
        let mut rag = Rag::new();
        rag.build_index(&c, false);
        let bodies = m.bodies("/api/embed");
        let sizes: Vec<usize> = bodies
            .iter()
            .map(|b| b["input"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes.iter().sum::<usize>(), chunks_of(&f).len());
        assert!(sizes.iter().all(|s| *s <= 16) && sizes[0] == 16);
        assert!(bodies.iter().all(|b| b["options"]["num_gpu"] == 0));
        let before = m.reqs.lock().unwrap().len();
        Rag::new().build_index(&c, false); // second process: mtime unchanged, nothing re-embedded
        assert_eq!(m.reqs.lock().unwrap().len(), before);
        Rag::new().build_index(&c, true); // forced
        assert!(m.reqs.lock().unwrap().len() > before);
    }

    #[test]
    fn keyword_only_search_when_embedding_fails() {
        let d = tmpdir("rag2");
        let a = d.join("a.md");
        let b = d.join("b.md");
        std::fs::write(&a, "The quarterly invoice schedule is every Friday.").unwrap();
        std::fs::write(&b, "Bananas and apples are fruit.").unwrap();
        let m = Mock::start(|_| (500, "down".into()));
        let mut c = cfg_with(Some(&m), &d);
        c.rag_files = vec![
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ];
        let mut rag = Rag::new();
        let hits = rag.search(&c, "when is the invoice schedule", 2);
        assert_eq!(rag.embed_ok, Some(false));
        assert!(hits[0].src.ends_with("a.md"), "{hits:?}");
        assert!(hits[0].score > hits[1].score);
        // failing engine is only tried once per process (build), not for every search
        assert_eq!(m.bodies("/api/embed").len(), 1);
    }

    #[test]
    fn semantic_score_decides_when_keywords_tie() {
        let c = Config::defaults(None);
        let mk = |s: &str, v: Vec<f64>| RagFile {
            src: s.into(),
            mtime: 0.0,
            chunks: vec!["zzz".into()],
            vecs: Some(vec![v]),
            from_glob: false,
        };
        let docs = [mk("far", vec![0.0, 1.0]), mk("near", vec![1.0, 0.0])];
        let h = search_docs(&c, &docs, Some(&[1.0, 0.0]), "query words", 1);
        assert_eq!(h[0].src, "near");
    }

    #[test]
    fn memory_title_boost_and_pinned_current_files() {
        let d = tmpdir("rag3");
        let mem = d.join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        let note = mem.join("pricing-rules.md");
        std::fs::write(&note, "---\nname: pricing-rules\ndescription: how we price consulting\n---\nBody text without the words.").unwrap();
        let other = d.join("other.md");
        std::fs::write(&other, "Pricing consulting mentioned once in passing here.").unwrap();
        let live = d.join("live.md");
        std::fs::write(
            &live,
            (1..=100).map(|i| format!("line {i}\n")).collect::<String>(),
        )
        .unwrap();
        let mut c = cfg_with(None, &d);
        c.rag_files = vec![
            note.to_string_lossy().into_owned(),
            other.to_string_lossy().into_owned(),
        ];
        c.pinned_current_files = vec![(live.to_string_lossy().into_owned(), 3)];
        let mut rag = Rag::new();
        let h = rag.search(&c, "pricing consulting", 2);
        assert!(h[0].src.ends_with("pricing-rules.md"), "{h:?}");
        let h = rag.search(&c, "what am I working on today", 5);
        assert_eq!(h[0].chunk, -1);
        assert_eq!(h[0].text, "line 98\nline 99\nline 100");
        assert!(h.len() <= 4); // pinned + 3
    }

    #[test]
    fn glob_sources_take_newest_n() {
        let d = tmpdir("rag4");
        for (i, n) in ["old.md", "mid.md", "new.md"].iter().enumerate() {
            let p = d.join(n);
            std::fs::write(&p, format!("doc {n}")).unwrap();
            let t = std::time::SystemTime::now()
                - std::time::Duration::from_secs(1000 - i as u64 * 100);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        let mut c = cfg_with(None, &d);
        c.rag_globs = vec![(format!("{}/*.md", d.display()), 2)];
        let s = sources(&c);
        assert_eq!(s.len(), 2);
        assert!(s[0].0.ends_with("new.md") && s[1].0.ends_with("mid.md"));
    }
}
