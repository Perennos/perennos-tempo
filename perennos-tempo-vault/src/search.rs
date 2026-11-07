//! Full-text search: BM25 over weighted fields (title, file name and aliases x3; headings and tags
//! x2; body x1) plus Obsidian-style operators: `"exact phrase"`, `-exclude`, `a OR b`, `prefix*`,
//! `tag:`, `path:`, `file:`, `task:`, `task-todo:`, `task-done:`, `[property]` and
//! `[property:value]`.
use crate::util::{file_name, head_chars};
use crate::vault::{Note, Vault};
use std::collections::{HashMap, HashSet};

pub fn tokenize(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
}

type Doc = (f32, HashMap<String, f32>);

#[derive(Default)]
pub struct SearchIndex {
    docs: HashMap<String, Doc>,
    postings: HashMap<String, HashSet<String>>,
    total: f64,
}

impl SearchIndex {
    pub fn upsert(&mut self, path: &str, fields: &[(&str, f32)]) {
        self.remove(path);
        let mut tf: HashMap<String, f32> = HashMap::new();
        let mut len = 0.0f32;
        for (text, w) in fields {
            for t in tokenize(text) {
                *tf.entry(t).or_insert(0.0) += w;
                len += w;
            }
        }
        for t in tf.keys() {
            self.postings
                .entry(t.clone())
                .or_default()
                .insert(path.to_string());
        }
        self.total += f64::from(len);
        self.docs.insert(path.to_string(), (len, tf));
    }

    pub fn remove(&mut self, path: &str) {
        if let Some((len, tf)) = self.docs.remove(path) {
            self.total -= f64::from(len);
            for t in tf.keys() {
                if let Some(s) = self.postings.get_mut(t) {
                    s.remove(path);
                    if s.is_empty() {
                        self.postings.remove(t);
                    }
                }
            }
        }
    }

    pub fn doc_has(&self, path: &str, term: &str) -> bool {
        self.docs.get(path).is_some_and(|d| d.1.contains_key(term))
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    fn bm25(&self, path: &str, term: &str) -> f32 {
        let Some((len, tf)) = self.docs.get(path) else {
            return 0.0;
        };
        let Some(f) = tf.get(term) else { return 0.0 };
        let n = self.docs.len() as f32;
        let df = self.postings.get(term).map_or(0, HashSet::len) as f32;
        let avg = (self.total as f32 / n.max(1.0)).max(1.0);
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let (k1, b) = (1.2f32, 0.75f32);
        idf * f * (k1 + 1.0) / (f + k1 * (1.0 - b + b * len / avg))
    }

    /// Best BM25 over every term starting with `prefix` (for `word*` and typeahead).
    fn bm25_prefix(&self, path: &str, prefix: &str) -> Option<f32> {
        let (_, tf) = self.docs.get(path)?;
        tf.keys()
            .filter(|t| t.starts_with(prefix))
            .map(|t| self.bm25(path, t))
            .max_by(f32::total_cmp)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clause {
    Word { text: String, prefix: bool },
    Phrase(String),
    Tag(String),
    Path(String),
    File(String),
    Task { text: String, done: Option<bool> },
    Prop { key: String, value: Option<String> },
}

/// `any_of` groups are OR-ed; the clauses inside a group are AND-ed; `none_of` excludes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub any_of: Vec<Vec<Clause>>,
    pub none_of: Vec<Clause>,
}

fn take_value(chars: &[char], i: &mut usize) -> String {
    let mut out = String::new();
    if chars.get(*i) == Some(&'"') {
        *i += 1;
        while *i < chars.len() && chars[*i] != '"' {
            out.push(chars[*i]);
            *i += 1;
        }
        *i += 1;
    } else {
        while *i < chars.len() && !chars[*i].is_whitespace() {
            out.push(chars[*i]);
            *i += 1;
        }
    }
    out
}

pub fn parse_query(q: &str) -> Query {
    let chars: Vec<char> = q.chars().collect();
    let mut groups: Vec<Vec<Clause>> = vec![vec![]];
    let mut none_of = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let neg = chars[i] == '-' && i + 1 < chars.len() && !chars[i + 1].is_whitespace();
        if neg {
            i += 1;
        }
        let mut clauses: Vec<Clause> = Vec::new();
        if chars[i] == '"' {
            let v = take_value(&chars, &mut i).to_lowercase();
            if !v.trim().is_empty() {
                clauses.push(Clause::Phrase(v));
            }
        } else if chars[i] == '[' {
            let mut inner = String::new();
            i += 1;
            while i < chars.len() && chars[i] != ']' {
                inner.push(chars[i]);
                i += 1;
            }
            i += 1;
            let (k, v) = match inner.split_once(':') {
                Some((k, v)) => (
                    k.trim().to_string(),
                    Some(v.trim().trim_matches('"').to_lowercase()).filter(|v| !v.is_empty()),
                ),
                None => (inner.trim().to_string(), None),
            };
            if !k.is_empty() {
                clauses.push(Clause::Prop { key: k, value: v });
            }
        } else {
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() && chars[i] != ':' {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if i < chars.len() && chars[i] == ':' {
                let op = word.to_lowercase();
                i += 1;
                let v = take_value(&chars, &mut i);
                let lv = v.to_lowercase();
                match op.as_str() {
                    "tag" => clauses.push(Clause::Tag(lv.trim_start_matches('#').to_string())),
                    "path" => clauses.push(Clause::Path(lv)),
                    "file" => clauses.push(Clause::File(lv)),
                    "task" => clauses.push(Clause::Task {
                        text: lv,
                        done: None,
                    }),
                    "task-todo" => clauses.push(Clause::Task {
                        text: lv,
                        done: Some(false),
                    }),
                    "task-done" => clauses.push(Clause::Task {
                        text: lv,
                        done: Some(true),
                    }),
                    _ => {
                        // not an operator: search the words as typed
                        let raw = format!("{word}:{v}");
                        clauses.extend(tokenize(&raw).map(|t| Clause::Word {
                            text: t,
                            prefix: false,
                        }));
                    }
                }
            } else if !neg && word == "OR" {
                groups.push(vec![]);
                continue;
            } else {
                let prefix = word.ends_with('*');
                let toks: Vec<String> = tokenize(word.trim_end_matches('*')).collect();
                let last = toks.len().saturating_sub(1);
                clauses.extend(toks.into_iter().enumerate().map(|(k, t)| Clause::Word {
                    text: t,
                    prefix: prefix && k == last,
                }));
            }
        }
        if neg {
            none_of.extend(clauses);
        } else if let Some(g) = groups.last_mut() {
            g.extend(clauses);
        }
    }
    groups.retain(|g| !g.is_empty());
    Query {
        any_of: groups,
        none_of,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub title: String,
    pub score: f32,
    /// 1-based line of the snippet (0 = none).
    pub line: usize,
    pub snippet: String,
    /// Character ranges in `snippet` to highlight.
    pub ranges: Vec<(usize, usize)>,
}

fn clause_matches(
    idx: &SearchIndex,
    n: &Note,
    c: &Clause,
    content: &mut dyn FnMut() -> String,
) -> bool {
    match c {
        Clause::Word {
            text,
            prefix: false,
        } => idx.doc_has(&n.path, text),
        Clause::Word { text, prefix: true } => idx.bm25_prefix(&n.path, text).is_some(),
        Clause::Phrase(p) => content().to_lowercase().contains(p.as_str()),
        Clause::Tag(t) => Vault::has_tag(n, t),
        Clause::Path(p) => n.path.to_lowercase().contains(p.as_str()),
        Clause::File(f) => file_name(&n.path).to_lowercase().contains(f.as_str()),
        Clause::Task { text, done } => n.parsed.tasks.iter().any(|t| {
            done.is_none_or(|d| t.done == d) && t.text.to_lowercase().contains(text.as_str())
        }),
        Clause::Prop { key, value } => n.parsed.prop(key).is_some_and(|p| {
            value.as_ref().is_none_or(|v| {
                p.items()
                    .iter()
                    .any(|i| i.to_lowercase().contains(v.as_str()))
            })
        }),
    }
}

/// Run a query over the vault; best first.
pub fn run(v: &Vault, q: &str, limit: usize) -> Vec<Hit> {
    let query = parse_query(q);
    if query.any_of.is_empty() && query.none_of.is_empty() {
        return vec![];
    }
    let idx = &v.search;
    let mut scored: Vec<(f32, &Note)> = Vec::new();
    for n in v.notes() {
        let mut cache: Option<String> = None;
        let mut content = || {
            cache
                .get_or_insert_with(|| v.read(&n.path).unwrap_or_default())
                .clone()
        };
        if query
            .none_of
            .iter()
            .any(|c| clause_matches(idx, n, c, &mut content))
        {
            continue;
        }
        let groups: Vec<&Vec<Clause>> = if query.any_of.is_empty() {
            vec![]
        } else {
            query.any_of.iter().collect()
        };
        let mut best: Option<f32> = if groups.is_empty() { Some(0.5) } else { None };
        for g in groups {
            // cheap pre-check: every plain word must be in the document
            if g.iter().any(|c| matches!(c, Clause::Word { text, prefix: false } if !idx.doc_has(&n.path, text))) {
                continue;
            }
            if !g.iter().all(|c| clause_matches(idx, n, c, &mut content)) {
                continue;
            }
            let s: f32 = g
                .iter()
                .map(|c| match c {
                    Clause::Word {
                        text,
                        prefix: false,
                    } => idx.bm25(&n.path, text),
                    Clause::Word { text, prefix: true } => {
                        idx.bm25_prefix(&n.path, text).unwrap_or(0.0)
                    }
                    Clause::Phrase(_) => 2.0,
                    _ => 0.5,
                })
                .sum();
            best = Some(best.map_or(s, |b: f32| b.max(s)));
        }
        if let Some(s) = best {
            scored.push((s, n));
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
    let needles: Vec<String> = query
        .any_of
        .iter()
        .flatten()
        .filter_map(|c| match c {
            Clause::Word { text, .. } | Clause::Phrase(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    scored
        .into_iter()
        .take(limit)
        .map(|(score, n)| {
            let (line, snippet, ranges) = snippet(
                &v.read(&n.path).unwrap_or_default(),
                n.parsed.body_start,
                &needles,
            );
            Hit {
                path: n.path.clone(),
                title: n.title.clone(),
                score,
                line,
                snippet,
                ranges,
            }
        })
        .collect()
}

/// The first body line containing a needle (else the first non-empty body line), trimmed to 240
/// characters, with the character ranges of every needle in it.
pub fn snippet(
    text: &str,
    body_start: usize,
    needles: &[String],
) -> (usize, String, Vec<(usize, usize)>) {
    let skip = text[..body_start.min(text.len())].matches('\n').count();
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .skip(skip)
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    let pick = lines
        .iter()
        .find(|(_, l)| {
            let low = l.to_lowercase();
            needles.iter().any(|n| low.contains(n.as_str()))
        })
        .or_else(|| lines.first());
    let Some((i, l)) = pick else {
        return (0, String::new(), vec![]);
    };
    // the words, not the Markdown around them
    let t = l.trim().trim_start_matches(['#', '>', '-', '*', '+', ' ']);
    let t = t
        .strip_prefix("[ ] ")
        .or_else(|| t.strip_prefix("[x] "))
        .or_else(|| t.strip_prefix("[X] "))
        .unwrap_or(t);
    let s = head_chars(t.trim(), 240).to_string();
    let low: Vec<char> = s.to_lowercase().chars().collect();
    let mut ranges = Vec::new();
    for n in needles {
        let nc: Vec<char> = n.chars().collect();
        if nc.is_empty() || nc.len() > low.len() {
            continue;
        }
        let mut k = 0;
        while k + nc.len() <= low.len() {
            if low[k..k + nc.len()] == nc[..] {
                ranges.push((k, k + nc.len()));
                k += nc.len();
            } else {
                k += 1;
            }
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    (i + 1, s, ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::sample;

    #[test]
    fn parses_queries() {
        let q = parse_query("rust \"exact words\" -draft tag:#project path:\"My Folder\" [status:active] [due] OR vault* task-todo:ship");
        assert_eq!(
            q.any_of,
            vec![
                vec![
                    Clause::Word {
                        text: "rust".into(),
                        prefix: false
                    },
                    Clause::Phrase("exact words".into()),
                    Clause::Tag("project".into()),
                    Clause::Path("my folder".into()),
                    Clause::Prop {
                        key: "status".into(),
                        value: Some("active".into())
                    },
                    Clause::Prop {
                        key: "due".into(),
                        value: None
                    },
                ],
                vec![
                    Clause::Word {
                        text: "vault".into(),
                        prefix: true
                    },
                    Clause::Task {
                        text: "ship".into(),
                        done: Some(false)
                    }
                ],
            ]
        );
        assert_eq!(
            q.none_of,
            vec![Clause::Word {
                text: "draft".into(),
                prefix: false
            }]
        );
        assert_eq!(
            parse_query("rust-core").any_of[0].len(),
            2,
            "punctuation splits words"
        );
        assert_eq!(
            parse_query("http://x.com").any_of[0][0],
            Clause::Word {
                text: "http".into(),
                prefix: false
            },
            "unknown operators are plain words"
        );
        assert_eq!(parse_query("   "), Query::default());
    }

    #[test]
    fn ranks_filters_and_snippets() {
        let (_d, v) = sample();
        let hits = v.search("tempo", 10);
        assert_eq!(
            hits[0].path, "Projects/Tempo.md",
            "title and file name weigh most"
        );
        assert!(hits.iter().any(|h| h.path == "Ideas.md"));
        let ideas = hits.iter().find(|h| h.path == "Ideas.md").unwrap();
        assert_eq!(ideas.line, 3);
        assert!(ideas.snippet.starts_with("Use Perennos Tempo daily"));
        assert_eq!(ideas.ranges, [(13, 18), (26, 31)]);
        assert_eq!(
            v.search("tag:project", 10)
                .iter()
                .map(|h| h.path.as_str())
                .collect::<Vec<_>>(),
            ["Ideas.md", "Projects/Tempo.md"]
        );
        assert_eq!(
            v.search("tempo -daily", 10)
                .iter()
                .filter(|h| h.path == "Ideas.md")
                .count(),
            0
        );
        assert_eq!(
            v.search("\"tempo is great\"", 10)
                .iter()
                .map(|h| h.path.as_str())
                .collect::<Vec<_>>(),
            ["Ideas.md"]
        );
        assert_eq!(v.search("task-todo:ship", 10)[0].path, "Projects/Tempo.md");
        assert!(v.search("task-done:ship", 10).is_empty());
        assert_eq!(
            v.search("[aliases:perennos]", 10)[0].path,
            "Projects/Tempo.md"
        );
        assert_eq!(v.search("path:archive file:index", 10).len(), 1);
        assert_eq!(
            v.search("tem*", 10)[0].path,
            "Projects/Tempo.md",
            "prefix search"
        );
        let or = v.search("nothinghere OR \"old ideas\"", 10);
        assert_eq!(
            or.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            ["Archive/Ideas.md"]
        );
        assert!(v.search("zzzzz", 10).is_empty());
        assert!(v.search("", 10).is_empty());
    }
}
