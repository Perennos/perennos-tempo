//! The dataset: winning takes of one band, scrubbed, mixed with about 20% general examples, split
//! 85/15 into train and held-out, written as chat JSONL (`train.jsonl`, `valid.jsonl`) with a
//! scrub report beside them so the owner sees exactly what was taken out.
//!
//! Cloud-written text stays out unless `woodshed.allow_cloud_text` is on (see docs/WOODSHED.md for
//! why). An example that holds a key or token is dropped whole, not patched: a half-hidden key
//! teaches the model to write `[hidden]`. Emails, phone numbers and home folders are replaced
//! with a plain stand-in.
//!
//! General examples come from the owner's own winning takes of other bands and of the usual
//! chat, plus an optional `<home>/woodshed/general.jsonl` of their own. Tempo ships none: text
//! a model wrote is not something to hand a different model to learn from.

use super::settings::Settings;
use super::takes::{example_of, Example, Take};
use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::Ctx;

/// What a scrub took out of one text.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub emails: usize,
    pub phones: usize,
    pub paths: usize,
}

impl Counts {
    fn add(&mut self, o: Counts) {
        self.emails += o.emails;
        self.phones += o.phones;
        self.paths += o.paths;
    }
}

fn mailish(c: char, domain: bool) -> bool {
    c.is_ascii_alphanumeric() || (if domain { "-." } else { "._%+-" }).contains(c)
}

fn scrub_emails(s: &str) -> (String, usize) {
    let c: Vec<char> = s.chars().collect();
    let (mut out, mut n, mut i) = (String::new(), 0, 0);
    while i < c.len() {
        if c[i] == '@' {
            // the local part is already in `out`
            let mut l = out.chars().rev().take_while(|x| mailish(*x, false)).count();
            let mut r = i + 1;
            while r < c.len() && mailish(c[r], true) {
                r += 1;
            }
            while r > i + 1 && c[r - 1] == '.' {
                r -= 1;
            }
            let domain: String = c[i + 1..r].iter().collect();
            let label = domain.rsplit('.').next().unwrap_or("");
            if l > 0
                && domain.contains('.')
                && label.len() >= 2
                && label.chars().all(|x| x.is_ascii_alphabetic())
            {
                while l > 0 {
                    out.pop();
                    l -= 1;
                }
                out.push_str("[email]");
                n += 1;
                i = r;
                continue;
            }
        }
        out.push(c[i]);
        i += 1;
    }
    (out, n)
}

fn scrub_phones(s: &str) -> (String, usize) {
    let c: Vec<char> = s.chars().collect();
    let sep = |x: char| " -.()".contains(x);
    let (mut out, mut n, mut i) = (String::new(), 0, 0);
    while i < c.len() {
        let start_ok = (c[i] == '+' || c[i].is_ascii_digit() || c[i] == '(')
            && (i == 0 || !(c[i - 1].is_alphanumeric() || c[i - 1] == '_'));
        if start_ok {
            let mut j = i + 1;
            while j < c.len() && (c[j].is_ascii_digit() || sep(c[j])) {
                j += 1;
            }
            while j > i && sep(c[j - 1]) && c[j - 1] != ')' {
                j -= 1;
            }
            let run = &c[i..j];
            let digits = run.iter().filter(|x| x.is_ascii_digit()).count();
            let groups: Vec<usize> = run
                .split(|x| sep(*x) || *x == '+')
                .filter(|g| !g.is_empty())
                .map(<[char]>::len)
                .collect();
            // a phone number has several groups of digits; a version, an address or a date does not
            let shaped = groups.len() >= 2 && groups.iter().all(|g| *g >= 2);
            let followed = j < c.len() && (c[j].is_alphanumeric() || c[j] == '_');
            if (9..=15).contains(&digits) && shaped && !followed {
                out.push_str("[phone]");
                n += 1;
                i = j;
                continue;
            }
        }
        out.push(c[i]);
        i += 1;
    }
    (out, n)
}

/// `/home/ana/x`, `/Users/ana/x`, `C:\Users\ana\x` become `~/x`.
fn scrub_paths(s: &str) -> (String, usize) {
    let lower = s.to_ascii_lowercase();
    let b = s.as_bytes();
    let (mut out, mut n, mut i) = (String::new(), 0, 0);
    let name_end = |from: usize| {
        let mut e = from;
        while e < b.len()
            && !matches!(
                b[e],
                b'/' | b'\\' | b' ' | b'"' | b'\'' | b'\n' | b'\t' | b')' | b','
            )
        {
            e += 1;
        }
        e
    };
    while i < s.len() {
        let rest = &lower[i..];
        let boundary =
            i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'.');
        let hit = if boundary && (rest.starts_with("/home/") || rest.starts_with("/users/")) {
            Some((i, i + 6 + usize::from(rest.starts_with("/users/"))))
        } else if rest.starts_with(":\\users\\") || rest.starts_with(":/users/") {
            // the drive letter is already in `out`
            Some((i, i + 8))
        } else {
            None
        };
        if let Some((_, name_at)) = hit {
            let e = name_end(name_at);
            if e > name_at {
                if rest.starts_with(':') {
                    out.pop();
                }
                out.push('~');
                n += 1;
                i = e;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, n)
}

/// A text with its personal details replaced. The flag says it held a key or token (the text is
/// then not to be used).
pub fn scrub(text: &str) -> (String, Counts, bool) {
    let secret = crate::redact(text) != text;
    let (a, emails) = scrub_emails(text);
    let (b, phones) = scrub_phones(&a);
    let (c, paths) = scrub_paths(&b);
    (
        c,
        Counts {
            emails,
            phones,
            paths,
        },
        secret,
    )
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub band: String,
    pub winners: usize,
    pub unreadable: usize,
    pub cloud_left_out: usize,
    pub allow_cloud: bool,
    pub secrets_dropped: usize,
    pub cut_short: usize,
    pub too_short: usize,
    pub duplicates: usize,
    pub counts: Counts,
    pub band_examples: usize,
    pub general_examples: usize,
    pub general_wanted: usize,
    pub train: usize,
    pub valid: usize,
}

impl Report {
    pub fn text(&self) -> String {
        let cloud = if self.allow_cloud {
            "Text written by a cloud model is included, because woodshed.allow_cloud_text is on."
        } else {
            "Text written by a cloud model is left out (woodshed.allow_cloud_text is off)."
        };
        let mut s = format!(
            "Scrub report for {}\n\n{} winning takes read. {} examples kept for the band, {} general ones mixed in ({} train, {} held out).\n\nLeft out:\n  {} that held a key or token (dropped whole)\n  {} written by a cloud model\n  {} that could not be read\n  {} that were cut short\n  {} too short to learn from\n  {} repeats\n\nReplaced:\n  {} email addresses\n  {} phone numbers\n  {} home folders (now ~)\n\n{cloud}\n",
            self.band,
            self.winners,
            self.band_examples,
            self.general_examples,
            self.train,
            self.valid,
            self.secrets_dropped,
            self.cloud_left_out,
            self.unreadable,
            self.cut_short,
            self.too_short,
            self.duplicates,
            self.counts.emails,
            self.counts.phones,
            self.counts.paths,
        );
        if self.general_examples < self.general_wanted {
            s.push_str(&format!(
                "\nOnly {} of the {} general examples wanted were found, so the mix is thinner than 20%. Winning takes from your other bands count, and so does a file of your own at <home>/woodshed/general.jsonl.\n",
                self.general_examples, self.general_wanted
            ));
        }
        s
    }

    pub fn to_j(&self) -> J {
        let n = |v: usize| J::n(v as f64);
        J::Obj(vec![
            ("band".into(), J::s(&self.band)),
            ("winners".into(), n(self.winners)),
            ("band_examples".into(), n(self.band_examples)),
            ("general_examples".into(), n(self.general_examples)),
            ("train".into(), n(self.train)),
            ("valid".into(), n(self.valid)),
            ("allow_cloud_text".into(), J::Bool(self.allow_cloud)),
            (
                "left_out".into(),
                J::Obj(vec![
                    ("secrets".into(), n(self.secrets_dropped)),
                    ("cloud".into(), n(self.cloud_left_out)),
                    ("unreadable".into(), n(self.unreadable)),
                    ("cut_short".into(), n(self.cut_short)),
                    ("too_short".into(), n(self.too_short)),
                    ("duplicates".into(), n(self.duplicates)),
                ]),
            ),
            (
                "replaced".into(),
                J::Obj(vec![
                    ("emails".into(), n(self.counts.emails)),
                    ("phones".into(), n(self.counts.phones)),
                    ("paths".into(), n(self.counts.paths)),
                ]),
            ),
        ])
    }
}

/// One training example: the messages before the answer, and the answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Ex {
    pub id: String,
    pub messages: Vec<(String, String)>,
    pub answer: String,
    pub general: bool,
}

impl Ex {
    pub fn line(&self) -> String {
        let mut m: Vec<J> = self
            .messages
            .iter()
            .map(|(r, c)| J::Obj(vec![("role".into(), J::s(r)), ("content".into(), J::s(c))]))
            .collect();
        m.push(J::Obj(vec![
            ("role".into(), J::s("assistant")),
            ("content".into(), J::s(&self.answer)),
        ]));
        J::Obj(vec![("messages".into(), J::Arr(m))]).compact()
    }
}

pub struct Built {
    pub train: Vec<Ex>,
    pub valid: Vec<Ex>,
    pub report: Report,
}

/// The lessons block the agent adds to a system message is not part of what the model should
/// learn to do on its own.
pub fn strip_lessons(system: &str) -> String {
    match system.find("\nLESSONS (short notes from earlier takes") {
        None => system.to_string(),
        Some(at) => {
            let after = &system[at + 1..];
            let tail = after.find("\nMEMORY EXCERPTS:").map_or("", |e| &after[e..]);
            format!("{}{}", &system[..at], tail)
        }
    }
}

const CUT: &str = "...[truncated";

/// Whether the owner's switches let this take's words into training. An author nobody named is
/// not assumed to be safe.
pub fn usable_author(t: &Take, allow_cloud: bool) -> bool {
    match t.author.as_str() {
        "local" | "owner" => true,
        "cloud" => allow_cloud,
        _ => false,
    }
}

fn hash_key(id: &str) -> String {
    sha256_hex(id.as_bytes())
}

/// Clean one example. The error says why it was left out.
fn prepare(e: Example, r: &mut Report) -> Result<Example, ()> {
    let mut counts = Counts::default();
    let mut secret = false;
    let mut messages = Vec::new();
    for (role, text) in e.messages {
        let text = if role == "system" {
            strip_lessons(&text)
        } else {
            text
        };
        if text.contains(CUT) {
            r.cut_short += 1;
            return Err(());
        }
        let (t, c, s) = scrub(&text);
        counts.add(c);
        secret |= s;
        if !t.trim().is_empty() {
            messages.push((role, t));
        }
    }
    if e.answer.contains(CUT) {
        r.cut_short += 1;
        return Err(());
    }
    let (answer, c, s) = scrub(&e.answer);
    counts.add(c);
    secret |= s;
    if secret {
        r.secrets_dropped += 1;
        return Err(());
    }
    if answer.trim().chars().count() < 20 || !messages.iter().any(|(r, _)| r == "user") {
        r.too_short += 1;
        return Err(());
    }
    r.counts.add(counts);
    Ok(Example { messages, answer })
}

fn from_chat_line(line: &str) -> Option<Example> {
    let j = parse(line).ok()?;
    let mut m: Vec<(String, String)> = j
        .arr("messages")
        .iter()
        .filter_map(|x| Some((x.str("role")?.to_string(), x.str("content")?.to_string())))
        .collect();
    let (role, answer) = m.pop()?;
    (role == "assistant").then_some(Example {
        messages: m,
        answer,
    })
}

pub fn general_file(ctx: &Ctx) -> std::path::PathBuf {
    ctx.home.join("woodshed").join("general.jsonl")
}

/// Build the dataset for `band` (`default` for the usual chat) from `takes`.
pub fn build(ctx: &Ctx, set: &Settings, band: &str, takes: &[Take]) -> Built {
    let mut r = Report {
        band: band.to_string(),
        allow_cloud: set.allow_cloud_text(),
        ..Report::default()
    };
    let mine = |t: &Take| super::takes::key(t) == band;
    let mut seen: Vec<String> = vec![];
    // `from_band`: the band's own winners (counted in the report), or the general pool
    let mut take_pool = |from_band: bool, r: &mut Report| -> Vec<Ex> {
        let mut pool: Vec<&Take> = takes
            .iter()
            .filter(|t| t.winner && mine(t) == from_band)
            .collect();
        pool.sort_by_key(|t| hash_key(&t.id));
        let mut out = vec![];
        for t in pool {
            let mut tally = Report::default();
            let made = if !usable_author(t, r.allow_cloud) {
                tally.cloud_left_out += 1;
                None
            } else if let Some(e) = example_of(ctx, t) {
                prepare(e, &mut tally).ok()
            } else {
                tally.unreadable += 1;
                None
            };
            let key = made
                .as_ref()
                .map(|e| sha256_hex(format!("{:?}{}", e.messages, e.answer).as_bytes()));
            let repeat = key.as_ref().is_some_and(|k| seen.contains(k));
            if repeat {
                tally.duplicates += 1;
            }
            if from_band {
                r.winners += 1;
                r.secrets_dropped += tally.secrets_dropped;
                r.cut_short += tally.cut_short;
                r.too_short += tally.too_short;
                r.unreadable += tally.unreadable;
                r.cloud_left_out += tally.cloud_left_out;
                r.duplicates += tally.duplicates;
                if !repeat {
                    r.counts.add(tally.counts);
                }
            }
            if let (Some(e), Some(k), false) = (made, key, repeat) {
                seen.push(k);
                out.push(Ex {
                    id: t.id.clone(),
                    messages: e.messages,
                    answer: e.answer,
                    general: !from_band,
                });
            }
        }
        out
    };
    let band_ex = take_pool(true, &mut r);
    let mut general = take_pool(false, &mut r);
    // the owner's own file of general examples
    let own = std::fs::read_to_string(general_file(ctx)).unwrap_or_default();
    for (i, line) in own
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        if let Some(e) = from_chat_line(line) {
            let mut tmp = Report::default();
            if let Ok(e) = prepare(e, &mut tmp) {
                general.push(Ex {
                    id: format!("own-{i}"),
                    messages: e.messages,
                    answer: e.answer,
                    general: true,
                });
            }
        }
    }
    r.band_examples = band_ex.len();
    // 20% of the whole: a quarter as many as the band's own
    r.general_wanted = band_ex.len().div_ceil(4);
    general.sort_by_key(|e| hash_key(&e.id));
    general.truncate(r.general_wanted);
    r.general_examples = general.len();
    let (mut train, mut valid) = (vec![], vec![]);
    for group in [band_ex, general] {
        let mut g = group;
        g.sort_by_key(|e| hash_key(&e.id));
        let held = (g.len() * 15).div_ceil(100);
        valid.extend(g.drain(..held));
        train.extend(g);
    }
    r.train = train.len();
    r.valid = valid.len();
    Built {
        train,
        valid,
        report: r,
    }
}

/// Write the files into `dir` and return the hash of the data (train then valid).
pub fn write(dir: &std::path::Path, b: &Built) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let lines = |v: &[Ex]| {
        v.iter()
            .map(|e| format!("{}\n", e.line()))
            .collect::<String>()
    };
    let (train, valid) = (lines(&b.train), lines(&b.valid));
    let put = |name: &str, body: &str| {
        std::fs::write(dir.join(name), body).map_err(|e| format!("cannot write {name}: {e}"))
    };
    put("train.jsonl", &train)?;
    put("valid.jsonl", &valid)?;
    put("scrub-report.txt", &b.report.text())?;
    put("scrub-report.json", &b.report.to_j().pretty())?;
    Ok(sha256_hex(format!("{train}\n--\n{valid}").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::super::takes::{parse_take, test_ctx};
    use super::*;

    #[test]
    fn emails_phones_and_home_folders_are_replaced() {
        let (t, c, secret) = scrub(
            "Mail ana.lopez+band@example.org or (555) 123-4567, or +44 20 7946 0958. Files in /home/ana/music and C:\\Users\\Ana\\Documents\\x.wav and /Users/ana/a.",
        );
        assert!(!secret);
        assert_eq!(
            c,
            Counts {
                emails: 1,
                phones: 2,
                paths: 3
            }
        );
        assert_eq!(
            t,
            "Mail [email] or [phone], or [phone]. Files in ~/music and ~\\Documents\\x.wav and ~/a."
        );
    }

    #[test]
    fn things_that_look_a_little_like_them_stay() {
        let keep = [
            "Version 10.2.3.4 shipped on 2026-11-07 at 10:30.",
            "The server is at 192.0.2.10 and 10.0.0.5.",
            "It costs 1 2 3 4 5 6 7 8 9 dollars, not 12.",
            "Use @everyone in the room, or user@localhost.",
            "The path /srv/home/data and /mnt/users/x are not home folders.",
            "Call 5551234567 or id 20261107103000.",
        ];
        for k in keep {
            let (t, c, _) = scrub(k);
            assert_eq!((t.as_str(), c), (k, Counts::default()), "{k}");
        }
    }

    #[test]
    fn a_key_marks_the_text_for_dropping() {
        let key = format!("ghp_{}", "x9Y8".repeat(6));
        let (_, _, secret) = scrub(&format!("my token is {key} ok"));
        assert!(secret);
        assert!(!scrub("nothing secret here, just words").2);
    }

    #[test]
    fn the_lessons_block_is_not_part_of_what_is_learned() {
        let s = "RULES\nYOUR ROLE (set by the owner; the rules above still hold):\nwrite hooks\n\nLESSONS (short notes from earlier takes; use them when they fit, the rules above still hold):\n- a\n- b\n";
        assert_eq!(
            strip_lessons(s),
            "RULES\nYOUR ROLE (set by the owner; the rules above still hold):\nwrite hooks\n"
        );
        let m =
            "ROLE\nLESSONS (short notes from earlier takes; x):\n- a\n\nMEMORY EXCERPTS:\nstuff";
        assert_eq!(strip_lessons(m), "ROLE\nMEMORY EXCERPTS:\nstuff");
        assert_eq!(strip_lessons("plain"), "plain");
    }

    /// A run log with `n` winning takes for `band`, authored as given, plus the take lines.
    fn world(tag: &str, rows: &[(&str, &str, &str, bool, &str)]) -> (super::super::takes::TestCtx, Vec<Take>) {
        let ctx = test_ctx(tag);
        let d = ctx.agent_data.join("runs/2026-11-07");
        std::fs::create_dir_all(&d).unwrap();
        let mut log = vec![];
        let mut takes = vec![];
        for (i, (id, band, author, winner, answer)) in rows.iter().enumerate() {
            log.push(format!(
                "{{\"kind\":\"prompt\",\"messages\":[{{\"role\":\"user\",\"content\":\"task number {i} for {band}\"}}]}}"
            ));
            log.push(format!(
                "{{\"kind\":\"answer\",\"text\":{}}}",
                J::s(answer).compact()
            ));
            let band_j = if band.is_empty() {
                "null".to_string()
            } else {
                format!("\"{band}\"")
            };
            takes.push(
                parse_take(&format!(
                    "{{\"take\":\"{id}\",\"band\":{band_j},\"author\":\"{author}\",\"winner\":{winner},\"prompt_ref\":\"runs/2026-11-07/r.jsonl#{}\",\"answer_ref\":\"runs/2026-11-07/r.jsonl#{}\"}}",
                    i * 2,
                    i * 2 + 1
                ))
                .unwrap(),
            );
        }
        std::fs::write(d.join("r.jsonl"), log.join("\n")).unwrap();
        (ctx, takes)
    }

    const GOOD: &str = "This is a long enough answer to learn from.";

    #[test]
    fn cloud_text_stays_out_unless_the_owner_allows_it() {
        let (ctx, takes) = world(
            "d-cloud",
            &[
                ("a", "b", "local", true, GOOD),
                ("c", "b", "cloud", true, GOOD),
                (
                    "o",
                    "b",
                    "owner",
                    true,
                    "The owner wrote this answer by hand, in full.",
                ),
                ("u", "b", "mystery", true, GOOD),
                ("l", "b", "local", false, GOOD),
            ],
        );
        let set = Settings::load(&ctx);
        let b = build(&ctx, &set, "b", &takes);
        assert_eq!(b.report.winners, 4, "a loser is not a winner");
        assert_eq!(
            b.report.cloud_left_out, 2,
            "the cloud one and the one with no known author"
        );
        assert_eq!(b.report.band_examples, 2);
        let ids: Vec<&str> = b
            .train
            .iter()
            .chain(&b.valid)
            .map(|e| e.id.as_str())
            .collect();
        assert!(ids.contains(&"a") && ids.contains(&"o") && !ids.contains(&"c"));
        // allowed: the cloud one comes in, the unknown author still does not
        let mut allow = Settings::load(&ctx);
        allow.set_top("allow_cloud_text", J::Bool(true));
        let b = build(&ctx, &allow, "b", &takes);
        assert_eq!(b.report.band_examples, 3);
        assert!(b.report.text().contains("is included"));
        assert!(b.report.text().contains("woodshed.allow_cloud_text is on"));
    }

    #[test]
    fn a_secret_drops_the_example_and_personal_details_are_replaced() {
        let key = format!("sk-{}", "Q7zP".repeat(8));
        let (ctx, takes) = world(
            "d-scrub",
            &[
                (
                    "a",
                    "b",
                    "local",
                    true,
                    &format!("Use {key} to sign in, it works."),
                ),
                (
                    "c",
                    "b",
                    "local",
                    true,
                    "Write to ana@example.org about the show tonight.",
                ),
                ("d", "b", "local", true, "short"),
                (
                    "e",
                    "b",
                    "local",
                    true,
                    "This one is fine and says nothing personal at all.",
                ),
                (
                    "f",
                    "b",
                    "local",
                    true,
                    "This one is fine and says nothing personal at all.",
                ),
            ],
        );
        let b = build(&ctx, &Settings::load(&ctx), "b", &takes);
        assert_eq!(b.report.secrets_dropped, 1);
        assert_eq!(b.report.too_short, 1);
        assert_eq!(b.report.counts.emails, 1);
        let all: Vec<&Ex> = b.train.iter().chain(&b.valid).collect();
        assert!(all
            .iter()
            .all(|e| !e.answer.contains(&key) && !e.answer.contains("ana@")));
        assert!(all.iter().any(|e| e.answer.contains("[email]")));
        assert_eq!(
            b.report.band_examples, 3,
            "c, e and f: their prompts differ, so they are not repeats"
        );
        assert_eq!(b.report.duplicates, 0);
    }

    #[test]
    fn general_examples_make_about_a_fifth_and_the_split_is_85_15() {
        // 40 band takes, 40 from another band (the pool of general ones)
        let mut rows = vec![];
        let (ids_a, ids_g): (Vec<String>, Vec<String>) = (
            (0..40).map(|i| format!("a{i}")).collect(),
            (0..40).map(|i| format!("g{i}")).collect(),
        );
        let ans: Vec<String> = (0..80)
            .map(|i| format!("Answer number {i}, long enough to keep around."))
            .collect();
        for (i, id) in ids_a.iter().enumerate() {
            rows.push((id.as_str(), "b", "local", true, ans[i].as_str()));
        }
        for (i, id) in ids_g.iter().enumerate() {
            rows.push((id.as_str(), "other", "local", true, ans[40 + i].as_str()));
        }
        let (ctx, takes) = world("d-mix", &rows);
        let b = build(&ctx, &Settings::load(&ctx), "b", &takes);
        assert_eq!(b.report.band_examples, 40);
        assert_eq!(b.report.general_examples, 10, "20% of 50");
        let g = b.train.iter().chain(&b.valid).filter(|e| e.general).count();
        assert_eq!(g, 10);
        assert_eq!(b.valid.len(), 6 + 2, "15% of 40 and of 10, rounded up");
        assert_eq!(b.train.len() + b.valid.len(), 50);
        // the same takes give the same split, and the files carry a stable hash
        let again = build(&ctx, &Settings::load(&ctx), "b", &takes);
        assert_eq!(
            b.valid.iter().map(|e| &e.id).collect::<Vec<_>>(),
            again.valid.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        let d = ctx.home.join("out");
        let h1 = write(&d, &b).unwrap();
        assert_eq!(h1, write(&d, &again).unwrap());
        assert_eq!(h1.len(), 64);
        // chat JSONL: messages ending in the assistant's answer
        let first = std::fs::read_to_string(d.join("train.jsonl")).unwrap();
        let j = parse(first.lines().next().unwrap()).unwrap();
        let m = j.arr("messages");
        assert_eq!(m.last().unwrap().str("role"), Some("assistant"));
        assert!(std::fs::read_to_string(d.join("scrub-report.txt"))
            .unwrap()
            .contains("Scrub report for b"));
        assert!(parse(&std::fs::read_to_string(d.join("scrub-report.json")).unwrap()).is_ok());
    }

    #[test]
    fn too_few_general_examples_is_said_plainly() {
        let rows: Vec<(String, String)> = (0..8)
            .map(|i| {
                (
                    format!("a{i}"),
                    format!("Answer number {i}, long enough to keep around."),
                )
            })
            .collect();
        let rows: Vec<(&str, &str, &str, bool, &str)> = rows
            .iter()
            .map(|(i, a)| (i.as_str(), "b", "local", true, a.as_str()))
            .collect();
        let (ctx, takes) = world("d-thin", &rows);
        // the owner's own file fills one of the two wanted
        std::fs::create_dir_all(ctx.home.join("woodshed")).unwrap();
        std::fs::write(
            general_file(&ctx),
            "{\"messages\":[{\"role\":\"user\",\"content\":\"explain a fade\"},{\"role\":\"assistant\",\"content\":\"A fade lowers the level slowly to nothing.\"}]}\nnot json\n",
        )
        .unwrap();
        let b = build(&ctx, &Settings::load(&ctx), "b", &takes);
        assert_eq!((b.report.general_wanted, b.report.general_examples), (2, 1));
        assert!(
            b.report.text().contains("Only 1 of the 2 general examples"),
            "{}",
            b.report.text()
        );
    }

    #[test]
    fn unreadable_and_cut_short_takes_are_counted() {
        let (ctx, mut takes) = world(
            "d-bad",
            &[
                ("a", "b", "local", true, GOOD),
                (
                    "c",
                    "b",
                    "local",
                    true,
                    "A long answer that was cut ...[truncated 900 chars]",
                ),
            ],
        );
        takes.push(parse_take(r#"{"take":"z","band":"b","author":"local","winner":true,"prompt_ref":"runs/nope.jsonl#1","answer_ref":"runs/nope.jsonl#2"}"#).unwrap());
        let b = build(&ctx, &Settings::load(&ctx), "b", &takes);
        assert_eq!(
            (
                b.report.unreadable,
                b.report.cut_short,
                b.report.band_examples
            ),
            (1, 1, 1)
        );
    }
}
