//! Lessons: short notes per band, kept in `<agent data>/lessons/<band-or-default>.jsonl`.
//!
//! The file is only ever appended to. A bullet is one line, `{id, text, helped, hurt, source,
//! created, last_used}`. What happens to it later is a small line after it: `bump` (helped or
//! hurt went up), `retire`, `restore`, `edit`. Reading folds the lines in order, so nothing is
//! rewritten and nothing is lost: a retired bullet stays in the file, in the "retired" section of
//! `tempo-launch woodshed lessons`, and can be restored. A bullet is retired by itself when its hurt
//! count passes its helped count by 2 or more.
//!
//! The top bullets, under a token cap, are also written (atomically replaced, since it is only a
//! cache) to `<key>.prompt.txt`, which is what the agent puts in the system message.

use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::{stamp, write_atomic, Ctx};
use std::io::Write;
use std::path::PathBuf;

pub const MIN_CHARS: usize = 12;
pub const MAX_CHARS: usize = 220;
/// More active bullets than this and a new one is turned away: a long list teaches nothing.
pub const MAX_ACTIVE: usize = 60;
pub const DEFAULT_TOKENS: usize = 400;

#[derive(Clone, Debug, PartialEq)]
pub struct Lesson {
    pub id: String,
    pub text: String,
    pub helped: u32,
    pub hurt: u32,
    /// local, cloud or owner.
    pub source: String,
    pub created: String,
    pub last_used: String,
    pub retired: bool,
    pub why: String,
}

pub fn dir(ctx: &Ctx) -> PathBuf {
    ctx.agent_data.join("lessons")
}

pub fn file(ctx: &Ctx, key: &str) -> PathBuf {
    dir(ctx).join(format!("{key}.jsonl"))
}

fn prompt_file(ctx: &Ctx, key: &str) -> PathBuf {
    dir(ctx).join(format!("{key}.prompt.txt"))
}

pub fn key_ok(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 40
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// A rough token count: four characters a token, and a little for the bullet mark.
pub fn tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4) + 2
}

pub fn id_of(text: &str) -> String {
    let t: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("L{}", &sha256_hex(t.as_bytes())[..8])
}

/// The lessons as the file says they are now.
pub fn load(ctx: &Ctx, key: &str) -> Vec<Lesson> {
    let text = std::fs::read_to_string(file(ctx, key)).unwrap_or_default();
    let mut out: Vec<Lesson> = vec![];
    for j in text.lines().filter_map(|l| parse(l.trim()).ok()) {
        let Some(id) = j.str("id") else { continue };
        let at = j.str("at").unwrap_or("").to_string();
        let slot = out.iter().position(|l| l.id == id);
        match (j.str("op"), slot) {
            (None, None) => {
                let Some(t) = j.str("text") else { continue };
                out.push(Lesson {
                    id: id.to_string(),
                    text: t.to_string(),
                    helped: j.f64("helped").unwrap_or(0.0) as u32,
                    hurt: j.f64("hurt").unwrap_or(0.0) as u32,
                    source: j.str("source").unwrap_or("local").to_string(),
                    created: j.str("created").unwrap_or("").to_string(),
                    last_used: j.str("last_used").unwrap_or("").to_string(),
                    retired: false,
                    why: String::new(),
                });
            }
            (Some("bump"), Some(i)) => {
                out[i].helped += j.f64("helped").unwrap_or(0.0) as u32;
                out[i].hurt += j.f64("hurt").unwrap_or(0.0) as u32;
                out[i].last_used = at;
            }
            (Some("retire"), Some(i)) => {
                out[i].retired = true;
                out[i].why = j.str("why").unwrap_or("").to_string();
            }
            (Some("restore"), Some(i)) => {
                // a second chance starts the count again
                out[i].retired = false;
                out[i].why = String::new();
                out[i].helped = 0;
                out[i].hurt = 0;
            }
            (Some("edit"), Some(i)) => {
                if let Some(t) = j.str("text") {
                    out[i].text = t.to_string();
                }
            }
            // a bullet written twice, an op for a bullet that is not there, an op from a newer
            // Tempo: left alone
            _ => {}
        }
    }
    out
}

fn append(ctx: &Ctx, key: &str, lines: &[J]) -> Result<(), String> {
    let p = file(ctx, key);
    std::fs::create_dir_all(dir(ctx))
        .map_err(|e| format!("cannot make {}: {e}", dir(ctx).display()))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map_err(|e| format!("cannot open {}: {e}", p.display()))?;
    let body: String = lines.iter().map(|l| format!("{}\n", l.compact())).collect();
    f.write_all(body.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", p.display()))
}

fn op(id: &str, op: &str, now: u64, more: Vec<(&str, J)>) -> J {
    let mut o = vec![
        ("id".to_string(), J::s(id)),
        ("op".into(), J::s(op)),
        ("at".into(), J::s(&stamp(now))),
    ];
    o.extend(more.into_iter().map(|(k, v)| (k.to_string(), v)));
    J::Obj(o)
}

/// A lesson in one line: no line breaks, not a key, not too short or long.
pub fn clean(text: &str) -> Result<String, String> {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = crate::redact(&t);
    let n = t.chars().count();
    if n < MIN_CHARS {
        return Err(format!("a lesson needs at least {MIN_CHARS} characters"));
    }
    if n > MAX_CHARS {
        return Err(format!(
            "a lesson is at most {MAX_CHARS} characters; this one is {n}"
        ));
    }
    if t.contains("[hidden]") {
        return Err("that looks like it holds a key or token, so it was not kept".into());
    }
    Ok(t)
}

/// A new bullet. The same words as one already there (retired or not) are not added twice.
pub fn add(ctx: &Ctx, key: &str, text: &str, source: &str, now: u64) -> Result<Lesson, String> {
    let text = clean(text)?;
    let all = load(ctx, key);
    let id = id_of(&text);
    if let Some(l) = all.iter().find(|l| l.id == id) {
        return Err(format!(
            "{} is already a lesson ({})",
            l.id,
            if l.retired {
                "retired; restore it instead"
            } else {
                "active"
            }
        ));
    }
    if all.iter().filter(|l| !l.retired).count() >= MAX_ACTIVE {
        return Err(format!(
            "{MAX_ACTIVE} lessons are active already; retire one first"
        ));
    }
    let line = J::Obj(vec![
        ("id".into(), J::s(&id)),
        ("text".into(), J::s(&text)),
        ("helped".into(), J::n(0.0)),
        ("hurt".into(), J::n(0.0)),
        ("source".into(), J::s(source)),
        ("created".into(), J::s(&stamp(now))),
        ("last_used".into(), J::s("")),
    ]);
    append(ctx, key, &[line])?;
    Ok(Lesson {
        id,
        text,
        helped: 0,
        hurt: 0,
        source: source.into(),
        created: stamp(now),
        last_used: String::new(),
        retired: false,
        why: String::new(),
    })
}

/// `helped` or `hurt` goes up by one. When hurt passes helped by 2 or more, the bullet is retired
/// (and the answer says so).
pub fn bump(ctx: &Ctx, key: &str, id: &str, helped: bool, now: u64) -> Result<bool, String> {
    let all = load(ctx, key);
    let l = all
        .iter()
        .find(|l| l.id == id)
        .ok_or_else(|| format!("no lesson {id}"))?;
    let (h, u) = (l.helped + u32::from(helped), l.hurt + u32::from(!helped));
    let mut lines = vec![op(
        id,
        "bump",
        now,
        vec![
            ("helped", J::n(f64::from(u32::from(helped)))),
            ("hurt", J::n(f64::from(u32::from(!helped)))),
        ],
    )];
    let retire = !l.retired && u >= h + 2;
    if retire {
        lines.push(op(
            id,
            "retire",
            now,
            vec![("why", J::s(&format!("hurt {u}, helped {h}")))],
        ));
    }
    append(ctx, key, &lines)?;
    Ok(retire)
}

pub fn retire(ctx: &Ctx, key: &str, id: &str, why: &str, now: u64) -> Result<(), String> {
    find(ctx, key, id)?;
    append(ctx, key, &[op(id, "retire", now, vec![("why", J::s(why))])])
}

pub fn restore(ctx: &Ctx, key: &str, id: &str, now: u64) -> Result<(), String> {
    find(ctx, key, id)?;
    append(ctx, key, &[op(id, "restore", now, vec![])])
}

pub fn edit(ctx: &Ctx, key: &str, id: &str, text: &str, now: u64) -> Result<(), String> {
    let text = clean(text)?;
    find(ctx, key, id)?;
    append(
        ctx,
        key,
        &[op(id, "edit", now, vec![("text", J::s(&text))])],
    )
}

fn find(ctx: &Ctx, key: &str, id: &str) -> Result<Lesson, String> {
    load(ctx, key)
        .into_iter()
        .find(|l| l.id == id)
        .ok_or_else(|| {
            format!("no lesson {id} for {key}; `tempo-launch woodshed lessons {key}` lists them")
        })
}

/// The bullets that go in the prompt: active ones, the best record first (helped minus hurt, the
/// owner's own words ahead on a tie, then the most recently used), as many as fit under `cap`
/// tokens.
pub fn top(all: &[Lesson], cap: usize) -> Vec<&Lesson> {
    let mut v: Vec<&Lesson> = all.iter().filter(|l| !l.retired).collect();
    v.sort_by(|a, b| {
        let score = |l: &Lesson| i64::from(l.helped) - i64::from(l.hurt);
        score(b)
            .cmp(&score(a))
            .then((b.source == "owner").cmp(&(a.source == "owner")))
            .then(b.last_used.cmp(&a.last_used))
            .then(b.created.cmp(&a.created))
    });
    let mut used = 0;
    v.into_iter()
        .take_while(|l| {
            used += tokens(&l.text);
            used <= cap
        })
        .collect()
}

/// Write `<key>.prompt.txt` from the top bullets; an empty list leaves an empty file, so a band
/// whose lessons were all retired stops getting any.
pub fn render(ctx: &Ctx, key: &str, cap: usize) -> Result<usize, String> {
    let all = load(ctx, key);
    let t = top(&all, cap);
    let text: String = t.iter().map(|l| format!("- {}\n", l.text)).collect();
    if all.is_empty() && !prompt_file(ctx, key).exists() {
        return Ok(0);
    }
    write_atomic(&prompt_file(ctx, key), text.as_bytes())
        .map_err(|e| format!("cannot write the prompt lessons: {e}"))?;
    Ok(t.len())
}

/// What `tempo-launch woodshed lessons <band>` prints.
pub fn show(ctx: &Ctx, key: &str, out: &mut dyn Write) {
    let all = load(ctx, key);
    let name = if key == "default" {
        "the usual chat".to_string()
    } else {
        key.to_string()
    };
    if all.is_empty() {
        let _ = writeln!(out, "No lessons for {name} yet. Tempo writes them after judged takes (`tempo-launch woodshed reflect {key}`), or you can: `tempo-launch woodshed lessons {key} --add \"what to do next time\"`.");
        return;
    }
    let line = |l: &Lesson| {
        format!(
            "  {}  helped {}, hurt {}, {}  {}",
            l.id, l.helped, l.hurt, l.source, l.text
        )
    };
    let active: Vec<&Lesson> = all.iter().filter(|l| !l.retired).collect();
    let _ = writeln!(out, "Lessons for {name}: {} active.", active.len());
    for l in &active {
        let _ = writeln!(out, "{}", line(l));
    }
    let retired: Vec<&Lesson> = all.iter().filter(|l| l.retired).collect();
    if !retired.is_empty() {
        let _ = writeln!(out, "\nRetired (kept, not played): {}", retired.len());
        for l in retired {
            let _ = writeln!(out, "{}  [{}]", line(l), l.why);
        }
    }
    let _ = writeln!(
        out,
        "\nFile: {}\nChange one: --add \"text\" | --edit ID \"text\" | --retire ID | --restore ID. Nothing is deleted.",
        file(ctx, key).display()
    );
}

#[cfg(test)]
mod tests {
    use super::super::takes::test_ctx;
    use super::*;

    #[test]
    fn a_new_bullet_is_one_line_and_never_twice() {
        let ctx = test_ctx("l-add");
        let l = add(
            &ctx,
            "b",
            "Open with the number, not the adjective.",
            "local",
            100,
        )
        .unwrap();
        assert!(l.id.starts_with('L') && l.id.len() == 9);
        assert!(add(
            &ctx,
            "b",
            "open with the number,  not the adjective.",
            "cloud",
            101
        )
        .unwrap_err()
        .contains("already a lesson"));
        let text = std::fs::read_to_string(file(&ctx, "b")).unwrap();
        assert_eq!(text.lines().count(), 1);
        let j = parse(text.lines().next().unwrap()).unwrap();
        for k in [
            "id",
            "text",
            "helped",
            "hurt",
            "source",
            "created",
            "last_used",
        ] {
            assert!(j.get(k).is_some(), "{k}");
        }
        assert_eq!(load(&ctx, "b").len(), 1);
    }

    #[test]
    fn bad_text_is_turned_away() {
        let ctx = test_ctx("l-bad");
        assert!(add(&ctx, "b", "short", "owner", 1).is_err());
        assert!(add(&ctx, "b", &"x".repeat(300), "owner", 1).is_err());
        let key = format!("sk-{}", "a1B2".repeat(8));
        assert!(add(
            &ctx,
            "b",
            &format!("use this key {key} for the api"),
            "owner",
            1
        )
        .unwrap_err()
        .contains("key or token"));
        assert!(!file(&ctx, "b").exists());
    }

    #[test]
    fn hurt_two_past_helped_retires_and_keeps_the_bullet() {
        let ctx = test_ctx("l-retire");
        let l = add(
            &ctx,
            "b",
            "Keep the caption under 120 characters.",
            "local",
            1,
        )
        .unwrap();
        assert!(!bump(&ctx, "b", &l.id, true, 2).unwrap());
        assert!(!bump(&ctx, "b", &l.id, false, 3).unwrap());
        assert!(
            !bump(&ctx, "b", &l.id, false, 4).unwrap(),
            "hurt 2, helped 1: not yet"
        );
        assert!(
            bump(&ctx, "b", &l.id, false, 5).unwrap(),
            "hurt 3, helped 1: two past"
        );
        let all = load(&ctx, "b");
        assert_eq!((all[0].helped, all[0].hurt, all[0].retired), (1, 3, true));
        assert!(all[0].why.contains("hurt 3"));
        // kept in the file, out of the prompt
        assert!(top(&all, 400).is_empty());
        assert_eq!(render(&ctx, "b", 400).unwrap(), 0);
        let mut out = Vec::new();
        show(&ctx, "b", &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("Retired") && s.contains("Keep the caption"),
            "{s}"
        );
        // a second chance starts the count again
        restore(&ctx, "b", &l.id, 6).unwrap();
        let all = load(&ctx, "b");
        assert_eq!((all[0].helped, all[0].hurt, all[0].retired), (0, 0, false));
        // the file only grew: bullet, bump x4, retire, restore
        assert_eq!(
            std::fs::read_to_string(file(&ctx, "b"))
                .unwrap()
                .lines()
                .count(),
            7
        );
    }

    #[test]
    fn the_file_is_only_appended_to() {
        let ctx = test_ctx("l-append");
        let a = add(&ctx, "b", "First lesson, plain words here.", "local", 1).unwrap();
        let before = std::fs::read_to_string(file(&ctx, "b")).unwrap();
        let b = add(&ctx, "b", "Second lesson, plain words here.", "cloud", 2).unwrap();
        bump(&ctx, "b", &a.id, true, 3).unwrap();
        edit(&ctx, "b", &b.id, "Second lesson, said better now.", 4).unwrap();
        retire(&ctx, "b", &a.id, "owner said so", 5).unwrap();
        let after = std::fs::read_to_string(file(&ctx, "b")).unwrap();
        assert!(after.starts_with(&before), "earlier lines are untouched");
        let all = load(&ctx, "b");
        assert_eq!(all[1].text, "Second lesson, said better now.");
        assert!(all[0].retired);
        assert!(retire(&ctx, "b", "Lnope", "x", 6).is_err());
    }

    #[test]
    fn top_bullets_fit_the_cap_and_the_best_come_first() {
        let ctx = test_ctx("l-top");
        let mut ids = vec![];
        for (i, t) in [
            "Lead with the number, then the claim.",
            "Never end a caption with a question.",
            "Name the instrument in the first line.",
            "Write for one listener, not a crowd.",
        ]
        .iter()
        .enumerate()
        {
            ids.push(
                add(
                    &ctx,
                    "b",
                    t,
                    if i == 3 { "owner" } else { "local" },
                    10 + i as u64,
                )
                .unwrap()
                .id,
            );
        }
        bump(&ctx, "b", &ids[1], true, 20).unwrap();
        bump(&ctx, "b", &ids[1], true, 21).unwrap();
        bump(&ctx, "b", &ids[0], true, 22).unwrap();
        let all = load(&ctx, "b");
        let t = top(&all, 400);
        let order: Vec<&str> = t.iter().map(|l| l.id.as_str()).collect();
        // 2 helped, 1 helped, then the owner's own words, then the rest
        assert_eq!(
            order,
            [
                ids[1].as_str(),
                ids[0].as_str(),
                ids[3].as_str(),
                ids[2].as_str()
            ]
        );
        // a small cap keeps the best and drops the rest
        let small = top(&all, tokens(&all[1].text) + 1);
        assert_eq!(small.len(), 1);
        assert_eq!(small[0].id, ids[1]);
        // the prompt file is exactly those bullets
        assert_eq!(render(&ctx, "b", 400).unwrap(), 4);
        let p = std::fs::read_to_string(prompt_file(&ctx, "b")).unwrap();
        assert!(
            p.starts_with("- Never end a caption with a question.\n"),
            "{p}"
        );
    }

    #[test]
    fn lines_from_a_newer_tempo_or_a_hand_edit_do_not_break_reading() {
        let ctx = test_ctx("l-odd");
        std::fs::create_dir_all(dir(&ctx)).unwrap();
        std::fs::write(
            file(&ctx, "b"),
            "{\"id\":\"La\",\"text\":\"A fine lesson, kept.\",\"source\":\"owner\"}\nnot json\n{\"id\":\"Lx\",\"op\":\"bump\",\"helped\":1}\n{\"id\":\"La\",\"op\":\"from-the-future\"}\n{\"op\":\"bump\"}\n",
        )
        .unwrap();
        let all = load(&ctx, "b");
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].helped, all[0].source.as_str()), (0, "owner"));
    }

    #[test]
    fn a_full_list_turns_a_new_one_away() {
        let ctx = test_ctx("l-full");
        for i in 0..MAX_ACTIVE {
            add(
                &ctx,
                "b",
                &format!("Lesson number {i:03}, in plain words."),
                "local",
                i as u64,
            )
            .unwrap();
        }
        assert!(add(&ctx, "b", "One more lesson, too many.", "local", 99)
            .unwrap_err()
            .contains("retire one first"));
    }
}
