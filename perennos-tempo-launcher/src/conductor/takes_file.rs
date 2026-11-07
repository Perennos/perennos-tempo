//! The takes file: `<agent data>/takes/takes.jsonl`, one judged attempt per line. Woodshed reads
//! it to turn winning takes into tuned local models, and its schema (version 1) is written up in
//! docs/WOODSHED.md. This module is the only writer; it holds the schema, the winner pick and the
//! owner's accept or reject, and nothing else from the Conductor.
//!
//! ```text
//! {"v":1,"take":"<id>","run":"<run-id>","band":"<band or null>","part":"<part or null>",
//!  "task":"<short task text>","engine":"<engine id>","model":"<model>",
//!  "author":"local"|"cloud"|"owner","prompt_ref":"runs/<date>/<run>.jsonl#<n>","answer_ref":"...",
//!  "checks":{"tests":true|false|null,"soundcheck":{"high":0,"med":0,"low":0}|null,
//!            "judge":0.0-1.0|null,"owner":"accept"|"reject"|null},
//!  "winner":true|false,"pair":"<take id>"|null,"ts":"RFC3339"}
//! ```
//!
//! Lines are only ever added, never rewritten. The owner's verdict adds a fresh line for each
//! take of that part whose `owner`, `winner` or `pair` changed, so a reader keeps the last line
//! it sees for a take id. That keeps the whole history, and Woodshed can read it as a plain log.

use crate::setup::json::{parse, J};
use crate::{redact, Ctx};
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    Local,
    Cloud,
    Owner,
}

impl Author {
    fn text(self) -> &'static str {
        match self {
            Author::Local => "local",
            Author::Cloud => "cloud",
            Author::Owner => "owner",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Take {
    pub id: String,
    pub run: String,
    pub band: Option<String>,
    pub part: Option<String>,
    pub task: String,
    pub engine: String,
    pub model: String,
    pub author: Author,
    pub prompt_ref: String,
    pub answer_ref: String,
    pub tests: Option<bool>,
    /// (high, med, low)
    pub soundcheck: Option<(u32, u32, u32)>,
    pub judge: Option<f64>,
    /// Some(true) accept, Some(false) reject.
    pub owner: Option<bool>,
    pub winner: bool,
    pub pair: Option<String>,
    pub ts: String,
}

pub fn path(ctx: &Ctx) -> PathBuf {
    ctx.agent_data.join("takes").join("takes.jsonl")
}

fn opt(s: &Option<String>) -> J {
    s.as_deref().map_or(J::Null, J::s)
}

impl Take {
    pub fn to_j(&self) -> J {
        let tests = self.tests.map_or(J::Null, J::Bool);
        let sound = self.soundcheck.map_or(J::Null, |(h, m, l)| {
            J::Obj(vec![
                ("high".into(), J::n(f64::from(h))),
                ("med".into(), J::n(f64::from(m))),
                ("low".into(), J::n(f64::from(l))),
            ])
        });
        let judge = self
            .judge
            .map_or(J::Null, |s| J::Num(format!("{:.2}", s.clamp(0.0, 1.0))));
        let owner = match self.owner {
            Some(true) => J::s("accept"),
            Some(false) => J::s("reject"),
            None => J::Null,
        };
        J::Obj(vec![
            ("v".into(), J::n(1.0)),
            ("take".into(), J::s(&self.id)),
            ("run".into(), J::s(&self.run)),
            ("band".into(), opt(&self.band)),
            ("part".into(), opt(&self.part)),
            ("task".into(), J::Str(short(&self.task))),
            ("engine".into(), J::s(&self.engine)),
            ("model".into(), J::s(&self.model)),
            ("author".into(), J::s(self.author.text())),
            ("prompt_ref".into(), J::s(&self.prompt_ref)),
            ("answer_ref".into(), J::s(&self.answer_ref)),
            (
                "checks".into(),
                J::Obj(vec![
                    ("tests".into(), tests),
                    ("soundcheck".into(), sound),
                    ("judge".into(), judge),
                    ("owner".into(), owner),
                ]),
            ),
            ("winner".into(), J::Bool(self.winner)),
            ("pair".into(), opt(&self.pair)),
            ("ts".into(), J::s(&self.ts)),
        ])
    }

    pub fn to_line(&self) -> String {
        self.to_j().compact()
    }

    pub fn from_j(j: &J) -> Option<Take> {
        let c = j.get("checks")?;
        let s = |k: &str| j.str(k).map(String::from);
        Some(Take {
            id: s("take")?,
            run: s("run")?,
            band: s("band"),
            part: s("part"),
            task: s("task")?,
            engine: s("engine")?,
            model: s("model")?,
            author: match j.str("author")? {
                "local" => Author::Local,
                "cloud" => Author::Cloud,
                "owner" => Author::Owner,
                _ => return None,
            },
            prompt_ref: s("prompt_ref")?,
            answer_ref: s("answer_ref")?,
            tests: c.bool("tests"),
            soundcheck: c.get("soundcheck").and_then(|x| {
                let n = |k: &str| x.f64(k).map(|v| v as u32);
                Some((n("high")?, n("med")?, n("low")?))
            }),
            judge: c.f64("judge"),
            owner: match c.str("owner") {
                Some("accept") => Some(true),
                Some("reject") => Some(false),
                _ => None,
            },
            winner: j.bool("winner")?,
            pair: s("pair"),
            ts: s("ts")?,
        })
    }

    /// Bigger is better. In trust order: the owner's accept, then tests, then Soundcheck's
    /// findings (high, then medium, then low), then the producer's score.
    fn rank(&self) -> (i8, i8, i64, i64, i64, i64) {
        let (h, m, l) = self.soundcheck.unwrap_or((0, 0, 0));
        (
            if self.owner == Some(true) { 1 } else { 0 },
            match self.tests {
                Some(true) => 2,
                None => 1,
                Some(false) => 0,
            },
            -i64::from(h),
            -i64::from(m),
            -i64::from(l),
            (self.judge.unwrap_or(0.0) * 1000.0).round() as i64,
        )
    }
}

/// A short task text for the file: one line, no secrets.
fn short(task: &str) -> String {
    let one = redact(&task.split_whitespace().collect::<Vec<_>>().join(" "));
    if one.chars().count() <= 200 {
        one
    } else {
        format!("{} [cut]", one.chars().take(200).collect::<String>())
    }
}

/// Mark the winner of takes made for the same task, and pair them: the winner points at the
/// runner-up, every other take at the winner. A take the owner rejected cannot win; if all were
/// rejected there is no winner. The earliest take wins a tie.
pub fn pick_winner(takes: &mut [Take]) {
    let best = (0..takes.len())
        .filter(|&i| takes[i].owner != Some(false))
        .fold(None::<usize>, |b, i| match b {
            Some(j) if takes[j].rank() >= takes[i].rank() => Some(j),
            _ => Some(i),
        });
    let runner = (0..takes.len())
        .filter(|&i| Some(i) != best && takes[i].owner != Some(false))
        .fold(None::<usize>, |b, i| match b {
            Some(j) if takes[j].rank() >= takes[i].rank() => Some(j),
            _ => Some(i),
        })
        .or_else(|| (0..takes.len()).find(|&i| Some(i) != best));
    let ids: Vec<String> = takes.iter().map(|t| t.id.clone()).collect();
    let alone = takes.len() < 2;
    for (i, t) in takes.iter_mut().enumerate() {
        t.winner = Some(i) == best;
        t.pair = if alone {
            None
        } else if t.winner {
            runner.map(|r| ids[r].clone())
        } else {
            best.map(|b| ids[b].clone())
        };
    }
}

/// Add takes to the file, a line each.
pub fn append(file: &std::path::Path, takes: &[Take]) -> std::io::Result<()> {
    let p = file;
    std::fs::create_dir_all(p.parent().unwrap_or(std::path::Path::new(".")))?;
    let mut text = String::new();
    for t in takes {
        text.push_str(&t.to_line());
        text.push('\n');
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)?;
    f.write_all(text.as_bytes())
}

/// Every take in the file, as it stands now: when a take appears more than once, its last line
/// wins, in the place it first showed up. A line that is not a take is skipped.
pub fn read_all(file: &std::path::Path) -> Vec<Take> {
    let mut out: Vec<Take> = Vec::new();
    let mut at = std::collections::HashMap::new();
    for t in std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| parse(l).ok())
        .filter_map(|j| Take::from_j(&j))
    {
        match at.get(&t.id) {
            Some(&i) => out[i] = t,
            None => {
                at.insert(t.id.clone(), out.len());
                out.push(t);
            }
        }
    }
    out
}

/// The owner's accept or reject of one take. The takes of the same run, part and task are picked
/// again, so accepting a take that lost makes it the winner, and rejecting the winner hands the
/// win to the next best. Says what changed.
pub fn set_owner(file: &std::path::Path, id: &str, accept: bool) -> Result<String, String> {
    if !file.exists() {
        return Err("there is no takes file yet; no run has made any takes".to_string());
    }
    let all = read_all(file);
    let me = all
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("no take called {id}"))?;
    let key = |t: &Take| (t.run.clone(), t.part.clone(), t.task.clone());
    let group_key = key(me);
    let before: Vec<Take> = all.into_iter().filter(|t| key(t) == group_key).collect();
    let mut group = before.clone();
    for t in group.iter_mut().filter(|t| t.id == id) {
        t.owner = Some(accept);
    }
    pick_winner(&mut group);
    let winner = group.iter().find(|t| t.winner).map(|t| t.id.clone());
    let changed: Vec<Take> = group
        .into_iter()
        .zip(&before)
        .filter(|(now, was)| now.to_line() != was.to_line())
        .map(|(now, _)| now)
        .collect();
    append(file, &changed).map_err(|e| format!("could not write the takes file: {e}"))?;
    Ok(match winner {
        Some(w) if w == id => format!("Take {id} is the winner."),
        Some(w) => format!("Noted. Take {w} is the winner now."),
        None => "Noted. Every take of that part is rejected, so it has no winner.".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::tests::{ctx_in, tmp};

    pub fn take(id: &str, tests: Option<bool>, judge: Option<f64>) -> Take {
        Take {
            id: id.into(),
            run: "r-1".into(),
            band: Some("b".into()),
            part: Some("2".into()),
            task: "write it".into(),
            engine: "local".into(),
            model: "m".into(),
            author: Author::Local,
            prompt_ref: "runs/2026-11-07/r-1.jsonl#3".into(),
            answer_ref: "runs/2026-11-07/r-1.jsonl#4".into(),
            tests,
            soundcheck: None,
            judge,
            owner: None,
            winner: false,
            pair: None,
            ts: "2026-11-07T10:00:00Z".into(),
        }
    }

    #[test]
    fn a_line_has_exactly_the_agreed_keys_in_order() {
        let mut t = take("r-1-2-1", Some(true), Some(0.5));
        t.soundcheck = Some((0, 1, 2));
        t.winner = true;
        t.pair = Some("r-1-2-2".into());
        assert_eq!(
            t.to_line(),
            concat!(
                r#"{"v":1,"take":"r-1-2-1","run":"r-1","band":"b","part":"2","task":"write it","#,
                r#""engine":"local","model":"m","author":"local","#,
                r#""prompt_ref":"runs/2026-11-07/r-1.jsonl#3","answer_ref":"runs/2026-11-07/r-1.jsonl#4","#,
                r#""checks":{"tests":true,"soundcheck":{"high":0,"med":1,"low":2},"judge":0.50,"owner":null},"#,
                r#""winner":true,"pair":"r-1-2-2","ts":"2026-11-07T10:00:00Z"}"#
            )
        );
        let mut n = take("x", None, None);
        n.band = None;
        n.part = None;
        n.author = Author::Cloud;
        assert!(n.to_line().contains(r#""band":null,"part":null"#));
        assert!(n.to_line().contains(r#""author":"cloud""#));
        assert!(n
            .to_line()
            .contains(r#""checks":{"tests":null,"soundcheck":null,"judge":null,"owner":null}"#));
        assert_eq!(Take::from_j(&parse(&t.to_line()).unwrap()), Some(t));
    }

    #[test]
    fn tests_beat_findings_beat_the_judge_and_the_earliest_wins_a_tie() {
        let mut v = vec![
            take("a", Some(false), Some(0.99)),
            take("b", Some(true), Some(0.2)),
            take("c", Some(true), Some(0.9)),
        ];
        pick_winner(&mut v);
        assert!(v[2].winner && !v[0].winner && !v[1].winner);
        assert_eq!(
            v[2].pair.as_deref(),
            Some("b"),
            "the winner points at the runner-up"
        );
        assert_eq!(v[0].pair.as_deref(), Some("c"));
        // findings come before the judge
        let mut w = vec![
            take("a", Some(true), Some(0.1)),
            take("b", Some(true), Some(0.9)),
        ];
        w[1].soundcheck = Some((1, 0, 0));
        pick_winner(&mut w);
        assert!(w[0].winner);
        // a tie goes to the first
        let mut t = vec![take("a", None, Some(0.5)), take("b", None, Some(0.5))];
        pick_winner(&mut t);
        assert!(t[0].winner);
        // one take wins alone, with no pair
        let mut one = vec![take("a", None, None)];
        pick_winner(&mut one);
        assert!(one[0].winner && one[0].pair.is_none());
    }

    #[test]
    fn the_owner_can_change_the_winner_and_no_line_is_lost() {
        let root = tmp("takes");
        let ctx = ctx_in(&root, false);
        let mut v = vec![
            take("a", Some(true), Some(0.9)),
            take("b", Some(false), Some(0.1)),
        ];
        pick_winner(&mut v);
        let mut other = vec![take("z", None, None)];
        other[0].run = "r-2".into();
        pick_winner(&mut other);
        let file = path(&ctx);
        append(&file, &v).unwrap();
        append(&file, &other).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(path(&ctx))
            .unwrap()
            .write_all(b"not a take\n")
            .unwrap();
        assert!(set_owner(&file, "b", true)
            .unwrap()
            .contains("Take b is the winner"));
        let all = read_all(&file);
        assert_eq!(
            all.len(),
            3,
            "a verdict adds lines, and the reader keeps the latest"
        );
        let lines = std::fs::read_to_string(&file).unwrap().lines().count();
        assert!(
            lines > 4,
            "nothing was rewritten: the old lines are all still there"
        );
        assert!(all[1].winner && !all[0].winner && all[1].owner == Some(true));
        assert!(all[2].winner, "another run is untouched");
        assert!(std::fs::read_to_string(path(&ctx))
            .unwrap()
            .contains("not a take"));
        assert!(set_owner(&file, "b", false)
            .unwrap()
            .contains("Take a is the winner now"));
        assert!(set_owner(&file, "a", false).unwrap().contains("no winner"));
        assert!(set_owner(&file, "nope", true).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
