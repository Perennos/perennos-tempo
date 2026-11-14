//! The Reflector: after takes are judged it reads them, with the lessons that were in play, and
//! proposes small changes: new bullets, and which old ones helped or hurt. It never rewrites the
//! file (see lessons.rs: every change is one appended line).
//!
//! Who plays the Reflector: a local model, through the agent as a guest conversation (no tools),
//! or the owner's own cloud model, which hands over the same small JSON with `reflect --from FILE`
//! (the lessons it writes are marked `cloud`). A cloud model may write lessons; its answers as
//! training text stay out of the LoRA data (see dataset.rs).

use super::dataset::scrub;
use super::lessons::{self, Lesson};
use super::takes::{example_of, read_takes, Take};
use crate::conductor::Chat;
use crate::setup::json::parse;
use crate::Ctx;
use std::io::Write;

/// The shape the Reflector's answer must have (sent to the engine as its `format`).
pub const SCHEMA: &str = r#"{"type":"object","properties":{"add":{"type":"array","items":{"type":"string"}},"helped":{"type":"array","items":{"type":"string"}},"hurt":{"type":"array","items":{"type":"string"}}},"required":["add","helped","hurt"]}"#;

const ROLE: &str = "You keep a short list of lessons for an AI that plays one part in a band. You read judged takes and say what to remember. You only write JSON.";
/// Takes per pass, and characters of each answer shown: a small model reads a short page better.
const TAKES: usize = 6;
const SHOWN: usize = 500;
const MAX_ADD: usize = 3;

#[derive(Debug, Default, PartialEq)]
pub struct Delta {
    pub add: Vec<String>,
    pub helped: Vec<String>,
    pub hurt: Vec<String>,
}

/// The first JSON object in a model's answer (it may wrap it in words or a code fence).
pub fn parse_delta(text: &str) -> Result<Delta, String> {
    let (a, b) = (text.find('{'), text.rfind('}'));
    let j = match (a, b) {
        (Some(a), Some(b)) if a < b => {
            parse(&text[a..=b]).map_err(|e| format!("the Reflector's answer was not JSON ({e})"))?
        }
        _ => return Err("the Reflector's answer had no JSON in it".into()),
    };
    Ok(Delta {
        add: j.strs("add"),
        helped: j.strs("helped"),
        hurt: j.strs("hurt"),
    })
}

fn shown(text: &str) -> String {
    let (t, _, _) = scrub(text);
    let t = crate::redact(&t);
    let n = t.chars().count();
    if n <= SHOWN {
        t
    } else {
        format!("{}...", t.chars().take(SHOWN).collect::<String>())
    }
}

fn verdict_words(t: &Take) -> String {
    let mut v = vec![if t.winner {
        "the best take of its task".to_string()
    } else {
        "not the best take".to_string()
    }];
    match t.owner.as_deref() {
        Some("accept") => v.push("the owner said yes".into()),
        Some("reject") => v.push("the owner said no".into()),
        _ => {}
    }
    if let Some(ok) = t.tests {
        v.push(if ok {
            "its tests passed".into()
        } else {
            "its tests failed".into()
        });
    }
    if let Some(j) = t.judge {
        v.push(format!("the judge gave it {j:.2} of 1"));
    }
    v.join(", ")
}

/// The page the Reflector reads.
pub fn prompt(key: &str, active: &[&Lesson], takes: &[(&Take, String, Option<String>)]) -> String {
    let mut s = format!("Band: {key}\n\nLessons in play now (id: text):\n");
    if active.is_empty() {
        s.push_str("(none yet)\n");
    }
    for l in active {
        s.push_str(&format!("{}: {}\n", l.id, l.text));
    }
    s.push_str("\nJudged takes:\n");
    for (i, (t, answer, other)) in takes.iter().enumerate() {
        s.push_str(&format!(
            "\nTake {} (task: {}): {}.\nIt said: {}\n",
            i + 1,
            shown(&t.task),
            verdict_words(t),
            shown(answer)
        ));
        if let Some(o) = other {
            s.push_str(&format!("The other take of this task said: {}\n", shown(o)));
        }
    }
    s.push_str(&format!(
        "\nAnswer with JSON: {{\"add\": [...], \"helped\": [...], \"hurt\": [...]}}.\n- add: up to {MAX_ADD} new lessons, each one plain sentence of {} to {} characters about what to do or avoid next time. General, not about one take. No names, no keys.\n- helped: ids of lessons in play whose advice shows in the best takes.\n- hurt: ids of lessons in play whose advice shows in the takes that missed.\nUse [] when nothing fits.\n",
        lessons::MIN_CHARS,
        lessons::MAX_CHARS
    ));
    s
}

/// What happened to the list.
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub added: Vec<String>,
    pub turned_away: Vec<String>,
    pub helped: usize,
    pub hurt: usize,
    pub retired: Vec<String>,
}

/// Put a delta into the list: new bullets (up to three), a bump for each id that is in `known`
/// (the ones the Reflector was shown; it cannot touch any other).
pub fn apply(ctx: &Ctx, key: &str, d: &Delta, known: &[String], source: &str, now: u64) -> Applied {
    let mut a = Applied::default();
    for t in d.add.iter().take(MAX_ADD) {
        match lessons::add(ctx, key, t, source, now) {
            Ok(l) => a.added.push(l.text),
            Err(e) => a.turned_away.push(format!(
                "\"{}\": {e}",
                t.chars().take(60).collect::<String>()
            )),
        }
    }
    for (ids, helped) in [(&d.helped, true), (&d.hurt, false)] {
        let mut done: Vec<&String> = vec![];
        for id in ids.iter().filter(|i| known.contains(i)) {
            if done.contains(&id) {
                continue;
            }
            done.push(id);
            if let Ok(retired) = lessons::bump(ctx, key, id, helped, now) {
                if helped {
                    a.helped += 1;
                } else {
                    a.hurt += 1;
                }
                if retired {
                    a.retired.push(id.clone());
                }
            }
        }
    }
    a
}

fn seen_file(ctx: &Ctx, key: &str) -> std::path::PathBuf {
    lessons::dir(ctx).join(format!("{key}.reflected"))
}

fn seen(ctx: &Ctx, key: &str) -> Vec<String> {
    std::fs::read_to_string(seen_file(ctx, key))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn mark_seen(ctx: &Ctx, key: &str, ids: &[&str]) {
    let _ = std::fs::create_dir_all(lessons::dir(ctx));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(seen_file(ctx, key))
    {
        let _ = writeln!(f, "{}", ids.join("\n"));
    }
}

fn say(out: &mut dyn Write, a: &Applied) {
    for t in &a.added {
        let _ = writeln!(out, "  new lesson: {t}");
    }
    for t in &a.turned_away {
        let _ = writeln!(out, "  not kept: {t}");
    }
    let _ = writeln!(out, "  {} helped, {} hurt.", a.helped, a.hurt);
    for r in &a.retired {
        let _ = writeln!(out, "  retired {r} (hurt passed helped by 2). It stays in the file; `lessons --restore {r}` brings it back.");
    }
}

/// One pass for `key` with a local model. Returns whether there was anything to reflect on.
pub fn run(
    ctx: &Ctx,
    key: &str,
    cap: usize,
    chat: &dyn Chat,
    model: &str,
    now: u64,
    out: &mut dyn Write,
) -> Result<bool, String> {
    let (takes, _) = read_takes(ctx);
    let done = seen(ctx, key);
    let fresh: Vec<&Take> = takes
        .iter()
        .filter(|t| super::takes::key(t) == key && !done.contains(&t.id))
        .collect();
    let batch: Vec<&Take> = fresh.iter().rev().take(TAKES).rev().copied().collect();
    let mut items = vec![];
    for t in &batch {
        let Some(e) = example_of(ctx, t) else {
            continue;
        };
        let other = t
            .pair
            .as_ref()
            .and_then(|p| takes.iter().find(|x| &x.id == p))
            .and_then(|p| example_of(ctx, p))
            .map(|e| e.answer);
        items.push((*t, e.answer, other));
    }
    // takes that cannot be read are not asked about again
    let ids: Vec<&str> = batch.iter().map(|t| t.id.as_str()).collect();
    if items.is_empty() {
        mark_seen(ctx, key, &ids);
        return Ok(false);
    }
    let all = lessons::load(ctx, key);
    let active: Vec<&Lesson> = lessons::top(&all, usize::MAX)
        .into_iter()
        .take(lessons::MAX_ACTIVE)
        .collect();
    let known: Vec<String> = active.iter().map(|l| l.id.clone()).collect();
    let text = chat.say_json(model, ROLE, &prompt(key, &active, &items), SCHEMA)?;
    let delta = parse_delta(&text)?;
    let a = apply(ctx, key, &delta, &known, "local", now);
    mark_seen(ctx, key, &ids);
    let _ = writeln!(out, "Reflected on {} takes for {key}:", items.len());
    say(out, &a);
    lessons::render(ctx, key, cap)?;
    Ok(true)
}

/// A delta the owner's own cloud model wrote (a file, or `-` for what was piped in).
pub fn from_text(
    ctx: &Ctx,
    key: &str,
    cap: usize,
    text: &str,
    now: u64,
    out: &mut dyn Write,
) -> Result<(), String> {
    let delta = parse_delta(text)?;
    let known: Vec<String> = lessons::load(ctx, key).into_iter().map(|l| l.id).collect();
    let a = apply(ctx, key, &delta, &known, "cloud", now);
    let _ = writeln!(out, "Took the notes for {key}:");
    say(out, &a);
    lessons::render(ctx, key, cap)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::takes::{parse_take, test_ctx};
    use super::*;
    use std::cell::RefCell;

    struct Say(RefCell<Vec<String>>, String);
    impl Chat for Say {
        fn say(&self, _: &str, _: &str, text: &str) -> Result<String, String> {
            self.0.borrow_mut().push(text.to_string());
            Ok(self.1.clone())
        }
    }

    fn world(tag: &str) -> super::super::takes::TestCtx {
        let ctx = test_ctx(tag);
        let d = ctx.agent_data.join("runs/2026-11-07");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("r.jsonl"),
            "{\"prompt\":\"write a hook for a synth\"}\n{\"answer\":\"Hear the synth before the chorus lands.\"}\n{\"prompt\":\"write a hook for a synth\"}\n{\"answer\":\"A synthesizer is a musical instrument.\"}\n",
        )
        .unwrap();
        std::fs::create_dir_all(ctx.agent_data.join("takes")).unwrap();
        std::fs::write(
            super::super::takes::takes_file(&ctx),
            "{\"take\":\"w\",\"band\":\"b\",\"task\":\"hook\",\"winner\":true,\"pair\":\"l\",\"checks\":{\"owner\":\"accept\"},\"prompt_ref\":\"runs/2026-11-07/r.jsonl#0\",\"answer_ref\":\"runs/2026-11-07/r.jsonl#1\"}\n{\"take\":\"l\",\"band\":\"b\",\"task\":\"hook\",\"winner\":false,\"pair\":\"w\",\"prompt_ref\":\"runs/2026-11-07/r.jsonl#2\",\"answer_ref\":\"runs/2026-11-07/r.jsonl#3\"}\n{\"take\":\"x\",\"band\":\"other\",\"winner\":true}\n",
        )
        .unwrap();
        ctx
    }

    #[test]
    fn the_answer_may_come_wrapped_in_words() {
        let d = parse_delta("Sure! ```json\n{\"add\":[\"Lead with the sound.\"],\"helped\":[\"L1\"],\"hurt\":[]}\n``` hope that helps").unwrap();
        assert_eq!(
            d,
            Delta {
                add: vec!["Lead with the sound.".into()],
                helped: vec!["L1".into()],
                hurt: vec![]
            }
        );
        assert!(parse_delta("no braces at all").is_err());
        assert!(parse_delta("{not json}").is_err());
        assert_eq!(parse_delta("{}").unwrap(), Delta::default());
    }

    #[test]
    fn a_local_pass_adds_bullets_bumps_the_ones_it_saw_and_marks_the_takes_done() {
        let ctx = world("r-run");
        let old = lessons::add(&ctx, "b", "Keep hooks under ten words.", "owner", 1).unwrap();
        let reply = format!(
            "{{\"add\":[\"Name the instrument in the first line.\",\"x\",\"Third lesson, plain words here.\"],\"helped\":[\"{}\",\"Lmadeup\"],\"hurt\":[]}}",
            old.id
        );
        let chat = Say(RefCell::new(vec![]), reply);
        let mut out = Vec::new();
        assert!(run(&ctx, "b", 400, &chat, "", 100, &mut out).unwrap());
        let prompts = chat.0.borrow();
        let p = &prompts[0];
        // the Reflector sees the lessons in play and both takes, the winner and the one that lost
        assert!(p.contains(&old.id) && p.contains("Keep hooks under ten words."));
        assert!(p.contains("the best take of its task, the owner said yes"));
        assert!(
            p.contains("The other take of this task said: A synthesizer is a musical instrument.")
        );
        assert!(!p.contains("Take 3"), "takes of another band are not shown");
        let all = lessons::load(&ctx, "b");
        assert_eq!(
            all.len(),
            3,
            "the old one, and two new ones; the too-short one was turned away"
        );
        assert_eq!(all[0].helped, 1, "the id it was shown was bumped");
        assert!(all.iter().all(|l| l.id != "Lmadeup"));
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("not kept") && said.contains("new lesson: Name the instrument"),
            "{said}"
        );
        // the prompt file the agent reads has them
        let f = std::fs::read_to_string(ctx.agent_data.join("lessons/b.prompt.txt")).unwrap();
        assert!(f.contains("Keep hooks under ten words.") && f.contains("Third lesson"));
        // a second pass has nothing new to read, and asks nobody
        let again = Say(RefCell::new(vec![]), "{}".into());
        assert!(!run(&ctx, "b", 400, &again, "", 200, &mut Vec::new()).unwrap());
        assert!(again.0.borrow().is_empty());
    }

    #[test]
    fn a_hurt_bullet_is_retired_by_the_pass_and_said_so() {
        let ctx = world("r-hurt");
        let l = lessons::add(&ctx, "b", "Always end on a question.", "local", 1).unwrap();
        lessons::bump(&ctx, "b", &l.id, false, 2).unwrap();
        let chat = Say(
            RefCell::new(vec![]),
            format!("{{\"add\":[],\"helped\":[],\"hurt\":[\"{}\"]}}", l.id),
        );
        let mut out = Vec::new();
        run(&ctx, "b", 400, &chat, "", 3, &mut out).unwrap();
        assert!(lessons::load(&ctx, "b")[0].retired);
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("retired") && said.contains("stays in the file"),
            "{said}"
        );
        let f = std::fs::read_to_string(ctx.agent_data.join("lessons/b.prompt.txt")).unwrap();
        assert_eq!(f, "", "retired bullets leave the prompt");
    }

    #[test]
    fn a_bad_answer_changes_nothing_and_the_takes_are_read_again_later() {
        let ctx = world("r-bad");
        let chat = Say(RefCell::new(vec![]), "I would rather not.".into());
        assert!(run(&ctx, "b", 400, &chat, "", 1, &mut Vec::new()).is_err());
        assert!(!lessons::file(&ctx, "b").exists());
        let good = Say(
            RefCell::new(vec![]),
            "{\"add\":[\"Say what the song sounds like.\"],\"helped\":[],\"hurt\":[]}".into(),
        );
        assert!(run(&ctx, "b", 400, &good, "", 2, &mut Vec::new()).unwrap());
    }

    #[test]
    fn the_owners_cloud_model_hands_over_a_delta_marked_cloud() {
        let ctx = test_ctx("r-cloud");
        let mut out = Vec::new();
        from_text(
            &ctx,
            "b",
            400,
            "{\"add\":[\"Prefer concrete numbers to adjectives.\"],\"helped\":[],\"hurt\":[]}",
            5,
            &mut out,
        )
        .unwrap();
        let all = lessons::load(&ctx, "b");
        assert_eq!((all.len(), all[0].source.as_str()), (1, "cloud"));
        assert!(from_text(&ctx, "b", 400, "nope", 6, &mut out).is_err());
    }

    #[test]
    fn a_take_that_cannot_be_read_is_not_asked_about_twice() {
        let ctx = test_ctx("r-unread");
        std::fs::create_dir_all(ctx.agent_data.join("takes")).unwrap();
        std::fs::write(
            super::super::takes::takes_file(&ctx),
            "{\"take\":\"z\",\"band\":\"b\",\"winner\":true}\n",
        )
        .unwrap();
        let chat = Say(RefCell::new(vec![]), "{}".into());
        assert!(!run(&ctx, "b", 400, &chat, "", 1, &mut Vec::new()).unwrap());
        assert!(chat.0.borrow().is_empty());
        let _ = parse_take("{\"take\":\"q\"}");
        assert_eq!(seen(&ctx, "b"), ["z"]);
    }
}
