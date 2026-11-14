//! The Rehearsal: base against adapter on the same held-out set with the same scorer, and on a
//! small canary set of general tasks. Promote only if the adapter wins by the margin and the canary
//! does not slip.
//!
//! The scorer is plain: how much of the best take's wording the model's answer shares (token F1,
//! 0 to 100). That is a proxy for "sounds like the takes the owner kept", not a judge of whether the
//! answer is good, and the docs say so. The canary tasks have short answers that are checked by
//! looking for the word, so they cost almost nothing to run.

use crate::conductor::net::request;
use crate::setup::json::{parse, J};
use std::time::Duration;

/// Something that answers: Ollama in the program, a stand-in in tests.
pub trait Gen {
    fn answer(&self, model: &str, messages: &[(String, String)]) -> Result<String, String>;
    /// The answer, and whether the model ran into the token limit (a loop that never found its
    /// way out looks like that). Stand-ins that only know the text say false.
    fn answer_full(
        &self,
        model: &str,
        messages: &[(String, String)],
    ) -> Result<(String, bool), String> {
        self.answer(model, messages).map(|a| (a, false))
    }
}

/// This computer's Ollama, one answer at a time, no sampling noise (temperature 0).
pub struct OllamaGen {
    pub port: u16,
}

impl Gen for OllamaGen {
    fn answer(&self, model: &str, messages: &[(String, String)]) -> Result<String, String> {
        self.answer_full(model, messages).map(|a| a.0)
    }

    fn answer_full(
        &self,
        model: &str,
        messages: &[(String, String)],
    ) -> Result<(String, bool), String> {
        let body = J::Obj(vec![
            ("model".into(), J::s(model)),
            (
                "messages".into(),
                J::Arr(
                    messages
                        .iter()
                        .map(|(r, c)| {
                            J::Obj(vec![("role".into(), J::s(r)), ("content".into(), J::s(c))])
                        })
                        .collect(),
                ),
            ),
            ("stream".into(), J::Bool(false)),
            ("think".into(), J::Bool(false)),
            (
                "options".into(),
                J::Obj(vec![
                    ("temperature".into(), J::n(0.0)),
                    ("num_predict".into(), J::n(400.0)),
                ]),
            ),
        ])
        .compact();
        let (code, text) = request(
            self.port,
            "POST",
            "/api/chat",
            &[],
            Some(&body),
            Duration::from_secs(600),
        )?;
        let j = parse(&text).map_err(|_| format!("Ollama's answer for {model} was not JSON"))?;
        if code != 200 {
            return Err(format!(
                "Ollama said: {}",
                j.str("error").unwrap_or("an error")
            ));
        }
        Ok((
            j.get("message")
                .and_then(|m| m.str("content"))
                .unwrap_or("")
                .to_string(),
            j.str("done_reason") == Some("length"),
        ))
    }
}

/// The stretch of words that, said three times, counts as a loop.
pub const LOOP_WINDOW: usize = 8;

/// An answer loops when it ran into the token limit, or when one run of `LOOP_WINDOW` words shows
/// up three or more times in it. Plain counting, so a stand-in and the real thing agree.
pub fn looping(answer: &str, hit_limit: bool) -> bool {
    if hit_limit {
        return true;
    }
    let w = words(answer);
    if w.len() < LOOP_WINDOW * 3 {
        return false;
    }
    let mut seen: std::collections::HashMap<&[String], usize> = std::collections::HashMap::new();
    w.windows(LOOP_WINDOW).any(|win| {
        let c = seen.entry(win).or_insert(0);
        *c += 1;
        *c >= 3
    })
}

/// The one sentence Rehearsal says when a model loops often (from this share of answers up).
pub const LOOPS_WORTH_SAYING: f64 = 10.0;

pub fn loops_words(model: &str, s: &Scores) -> Option<String> {
    (s.loops >= LOOPS_WORTH_SAYING).then(|| {
        format!(
            "{model} loops on {:.0}% of answers. A retune might fix that (`tempo-launch retune plan {model}`).",
            s.loops
        )
    })
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Token F1 between an answer and a reference, 0 to 100.
pub fn f1(answer: &str, reference: &str) -> f64 {
    let (a, r) = (words(answer), words(reference));
    if a.is_empty() || r.is_empty() {
        return 0.0;
    }
    let mut left = r.clone();
    let mut shared = 0usize;
    for w in &a {
        if let Some(i) = left.iter().position(|x| x == w) {
            left.swap_remove(i);
            shared += 1;
        }
    }
    if shared == 0 {
        return 0.0;
    }
    let (p, rc) = (
        shared as f64 / a.len() as f64,
        shared as f64 / r.len() as f64,
    );
    2.0 * p * rc / (p + rc) * 100.0
}

/// General tasks the adapter must not get worse at. Each is a prompt and the words that count as
/// right (any one of them).
pub const CANARY: &[(&str, &[&str])] = &[
    ("What is 17 plus 25? Answer with the number only.", &["42"]),
    ("What is the capital of France? One word.", &["paris"]),
    ("Reply with exactly the word: ready", &["ready"]),
    ("Write the word banana backwards.", &["ananab"]),
    ("How many days are in a week? Number only.", &["7", "seven"]),
    (
        "What colour do you get by mixing blue and yellow paint? One word.",
        &["green"],
    ),
    ("Is 9 an even number? Answer yes or no.", &["no"]),
    ("What is the chemical formula of water?", &["h2o"]),
    ("What is 6 times 7? Number only.", &["42"]),
    (
        "Complete the line: roses are red, violets are ... One word.",
        &["blue"],
    ),
    ("Convert 2 hours to minutes. Number only.", &["120"]),
    (
        "Reply in JSON with one key named ok set to true.",
        &["\"ok\": true", "\"ok\":true"],
    ),
];

pub fn canary_right(answer: &str, any: &[&str]) -> bool {
    let a = answer.to_lowercase();
    any.iter().any(|w| a.contains(*w))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scores {
    /// Mean F1 on the held-out set, 0 to 100.
    pub eval: f64,
    pub n_eval: usize,
    /// Canary answers right, as a percentage.
    pub canary: f64,
    pub n_canary: usize,
    /// Answers that looped (hit the token limit, or said the same stretch of words over and over),
    /// as a percentage of every answer given, held-out and canary.
    pub loops: f64,
}

/// Most held-out examples scored in one run: a night's run should not turn into a day's.
pub const MAX_EVAL: usize = 60;

/// One model on the held-out set (messages before the answer, and the answer) and the canary.
pub fn score(
    g: &dyn Gen,
    model: &str,
    held_out: &[(Vec<(String, String)>, String)],
) -> Result<Scores, String> {
    let mut sum = 0.0;
    let mut looped = 0usize;
    let held = &held_out[..held_out.len().min(MAX_EVAL)];
    for (messages, reference) in held {
        let (a, cut) = g.answer_full(model, messages)?;
        looped += usize::from(looping(&a, cut));
        sum += f1(&a, reference);
    }
    let mut right = 0usize;
    for (prompt, any) in CANARY {
        let m = vec![("user".to_string(), prompt.to_string())];
        let (a, cut) = g.answer_full(model, &m)?;
        looped += usize::from(looping(&a, cut));
        if canary_right(&a, any) {
            right += 1;
        }
    }
    let r = |x: f64| (x * 10.0).round() / 10.0;
    Ok(Scores {
        eval: r(if held.is_empty() {
            0.0
        } else {
            sum / held.len() as f64
        }),
        n_eval: held.len(),
        canary: r(right as f64 * 100.0 / CANARY.len() as f64),
        n_canary: CANARY.len(),
        loops: r(looped as f64 * 100.0 / (held.len() + CANARY.len()) as f64),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Gate {
    pub passed: bool,
    pub reason: String,
}

/// The adapter passes when it wins on the held-out set by `margin` points and its canary score has
/// not dropped by more than `slack` points.
pub fn gate(base: &Scores, adapter: &Scores, margin: f64, slack: f64) -> Gate {
    let gain = adapter.eval - base.eval;
    let canary = adapter.canary - base.canary;
    let (passed, reason) = if adapter.n_eval < 5 {
        (
            false,
            format!("only {} held-out examples: too few to tell", adapter.n_eval),
        )
    } else if gain < margin {
        (
            false,
            format!(
                "it won by {gain:.1} points on the held-out set, and the margin is {margin:.1}"
            ),
        )
    } else if canary < -slack {
        (
            false,
            format!(
                "the general tasks slipped by {:.1} points ({:.1} to {:.1})",
                -canary, base.canary, adapter.canary
            ),
        )
    } else {
        (true, format!("it won by {gain:.1} points (margin {margin:.1}) and the general tasks held ({:.1} to {:.1})", base.canary, adapter.canary))
    };
    Gate { passed, reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Answers from a table: (model, last user message) -> answer; anything else: "".
    pub struct Table(pub HashMap<(String, String), String>, pub String);
    impl Gen for Table {
        fn answer(&self, model: &str, m: &[(String, String)]) -> Result<String, String> {
            let q = m.last().map(|x| x.1.clone()).unwrap_or_default();
            Ok(self
                .0
                .get(&(model.to_string(), q))
                .cloned()
                .unwrap_or_else(|| self.1.clone()))
        }
    }

    struct Looper;
    impl Gen for Looper {
        fn answer(&self, m: &str, q: &[(String, String)]) -> Result<String, String> {
            self.answer_full(m, q).map(|a| a.0)
        }
        fn answer_full(&self, _: &str, q: &[(String, String)]) -> Result<(String, bool), String> {
            let q = &q.last().unwrap().1;
            Ok(if q.contains("banana") {
                // says the same ten words forever
                ("wait let me think about it again so ".repeat(8), false)
            } else if q.contains("capital") {
                ("Paris".into(), true) // cut off by the token limit
            } else {
                ("fine".into(), false)
            })
        }
    }

    #[test]
    fn looping_is_the_token_limit_or_a_window_said_three_times() {
        assert!(looping("short", true));
        assert!(!looping("short and fine", false));
        let four = "one two three four five six seven eight nine ten ".repeat(4);
        assert!(looping(&four, false));
        // twice is a refrain, not a loop
        let two = format!(
            "{}then something new entirely follows here and goes on",
            "one two three four five six seven eight nine ten ".repeat(2)
        );
        assert!(!looping(&two, false));
        // long and never repeating
        let long: String = (0..200).map(|i| format!("w{i} ")).collect();
        assert!(!looping(&long, false));
    }

    #[test]
    fn a_model_that_loops_is_counted_and_told() {
        let s = score(&Looper, "m", &[]).unwrap();
        // two of the twelve canary answers loop: the one that repeats, and the one that was cut off
        assert_eq!(s.loops, 16.7);
        assert!(loops_words("m", &s)
            .unwrap()
            .contains("loops on 17% of answers"));
        assert!(loops_words("m", &Scores::default()).is_none());
    }

    #[test]
    fn f1_counts_shared_words_once_each() {
        assert_eq!(f1("the same words", "The same words!"), 100.0);
        assert_eq!(f1("nothing alike", "completely different"), 0.0);
        assert_eq!(f1("", "x"), 0.0);
        // 2 shared of 3 and of 4: p = 2/3, r = 1/2, f1 = 4/7
        let v = f1("hear the synth", "hear the synth sing again");
        assert!(
            (v - 2.0 * (3.0 / 3.0) * (3.0 / 5.0) / (1.0 + 3.0 / 5.0) * 100.0).abs() < 1e-9,
            "{v}"
        );
        // a word said twice only counts as often as the reference has it
        assert!(f1("go go go", "go") < 60.0);
    }

    #[test]
    fn the_canary_looks_for_the_word() {
        assert!(canary_right("The answer is 42.", &["42"]));
        assert!(canary_right(
            "{\"ok\":true}",
            &["\"ok\": true", "\"ok\":true"]
        ));
        assert!(!canary_right("I think 43", &["42"]));
        assert_eq!(CANARY.len(), 12);
    }

    #[test]
    fn a_model_is_scored_on_the_held_out_set_and_the_canary() {
        let held = vec![
            (
                vec![("user".to_string(), "hook".to_string())],
                "hear it first".to_string(),
            ),
            (
                vec![("user".to_string(), "bio".to_string())],
                "plain words only".to_string(),
            ),
        ];
        let mut t = HashMap::new();
        t.insert(
            ("m".to_string(), "hook".to_string()),
            "hear it first".to_string(),
        );
        t.insert(
            ("m".to_string(), "bio".to_string()),
            "plain words".to_string(),
        );
        t.insert(("m".to_string(), CANARY[0].0.to_string()), "42".to_string());
        t.insert(
            ("m".to_string(), CANARY[1].0.to_string()),
            "Paris".to_string(),
        );
        let s = score(&Table(t, String::new()), "m", &held).unwrap();
        assert_eq!(s.n_eval, 2);
        assert!((s.eval - (100.0 + 80.0) / 2.0).abs() < 0.1, "{}", s.eval);
        assert_eq!((s.canary, s.n_canary), (16.7, 12));
        // no held-out set scores zero, never divides by it
        assert_eq!(
            score(&Table(HashMap::new(), "x".into()), "m", &[])
                .unwrap()
                .eval,
            0.0
        );
    }

    fn sc(eval: f64, canary: f64, n: usize) -> Scores {
        Scores {
            eval,
            n_eval: n,
            canary,
            n_canary: 12,
            ..Scores::default()
        }
    }

    #[test]
    fn promote_only_on_the_margin_and_a_steady_canary() {
        let base = sc(30.0, 75.0, 20);
        let g = gate(&base, &sc(36.0, 75.0, 20), 5.0, 0.0);
        assert!(g.passed, "{}", g.reason);
        assert!(g.reason.contains("won by 6.0"));
        // exactly the margin passes
        assert!(gate(&base, &sc(35.0, 80.0, 20), 5.0, 0.0).passed);
        // a win that is too small
        let g = gate(&base, &sc(34.9, 75.0, 20), 5.0, 0.0);
        assert!(
            !g.passed && g.reason.contains("margin is 5.0"),
            "{}",
            g.reason
        );
        // a big win that breaks the general tasks
        let g = gate(&base, &sc(50.0, 66.7, 20), 5.0, 0.0);
        assert!(
            !g.passed && g.reason.contains("general tasks slipped"),
            "{}",
            g.reason
        );
        // the same drop with some slack allowed
        assert!(gate(&base, &sc(50.0, 66.7, 20), 5.0, 10.0).passed);
        // too few held-out examples to tell
        let g = gate(&base, &sc(90.0, 100.0, 3), 5.0, 0.0);
        assert!(!g.passed && g.reason.contains("too few"));
    }

    #[test]
    fn ollama_is_asked_for_a_plain_answer_with_no_sampling() {
        // a tiny Ollama stand-in: one answer, the request it got kept
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let got = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let g2 = got.clone();
        l.set_nonblocking(true).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                let mut s = loop {
                    match l.accept() {
                        Ok((s, _)) => break s,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(10))
                        }
                        _ => return,
                    }
                };
                s.set_read_timeout(Some(std::time::Duration::from_secs(1)))
                    .unwrap();
                s.set_write_timeout(Some(std::time::Duration::from_secs(1)))
                    .unwrap();
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).unwrap();
                *g2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body =
                    "{\"message\":{\"role\":\"assistant\",\"content\":\"Paris.\"},\"done\":true}";
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
            let a = OllamaGen { port }
                .answer(
                    "tempo-b-20261107-try",
                    &[("user".into(), "capital of France?".into())],
                )
                .unwrap();
            assert_eq!(a, "Paris.");
            let sent = got.lock().unwrap().clone();
            assert!(sent.starts_with("POST /api/chat"), "{sent}");
            assert!(
                sent.contains("\"temperature\":0")
                    && sent.contains("\"stream\":false")
                    && sent.contains("tempo-b-20261107-try")
            );
        });
        // nothing answering is an error, not an empty answer
        assert!(OllamaGen { port: 9 }.answer("m", &[]).is_err());
    }
}
