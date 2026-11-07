//! `conductor new --describe "..."`: the local model fits the closest template to a sentence about the
//! owner's work. Whatever it sends back is checked like any other spec, and when it is not usable
//! the template is used as it is. The model never picks models, folders or tools: the launcher does.

use super::spec::{from_j, Band};
use super::templates::{closest, Template};
use super::{Chat, Models};
use crate::setup::json::{parse, J};

const DRAFTER: &str = "You design small teams of AI members for one person's own work. You answer with one JSON object and nothing else: no introduction, no code fence, no closing remarks.";

/// The description as plain text the model is shown: one line of at most 500 characters.
fn tidy(description: &str) -> String {
    description
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(500)
        .collect()
}

fn ask_text(description: &str, example: &str) -> String {
    format!(
        "Someone describes their work: \"{description}\"\n\n\
Adapt the team below to them. Keep the same JSON shape. Keep 3 to 5 members and 1 or 2 flows. Rename the members to suit the work, rewrite each role so it is specific to what they do (at most 1500 characters each), and change each flow's goal and schedule to fit. Rules:\n\
- handle: lowercase letters, digits and hyphens, 2 to 24 characters.\n\
- instrument: one word, a musical instrument or something playful.\n\
- may: \"talk\" (chats only) or \"draft\" (also writes drafts). Flow steps must be members who may \"draft\".\n\
- every: \"manual\", \"daily HH:MM\" or \"weekly <mon|tue|wed|thu|fri|sat|sun> HH:MM\".\n\
- Members only talk and write drafts. Nobody posts, sends, buys or deletes anything.\n\
- Do not add keys that are not in the example. Leave out \"model\" and \"notes_to\".\n\n\
Answer with the JSON object only.\n\nThe team to adapt:\n{}",
        example
    )
}

/// The band's shape as a JSON Schema, so an engine that can hold a model to one does. Only what
/// `from_j` needs; the rest of the checking stays in `spec`.
fn band_schema() -> String {
    let text = || J::Obj(vec![("type".into(), J::s("string"))]);
    let list = |item: J| J::Obj(vec![("type".into(), J::s("array")), ("items".into(), item)]);
    let object = |required: &[&str], props: Vec<(String, J)>| {
        J::Obj(vec![
            ("type".into(), J::s("object")),
            (
                "required".into(),
                J::strings(&required.iter().map(|r| r.to_string()).collect::<Vec<_>>()),
            ),
            ("properties".into(), J::Obj(props)),
        ])
    };
    let member = object(
        &["handle", "name", "instrument", "role", "may"],
        vec![
            ("handle".into(), text()),
            ("name".into(), text()),
            ("instrument".into(), text()),
            ("role".into(), text()),
            (
                "may".into(),
                J::Obj(vec![
                    ("type".into(), J::s("string")),
                    (
                        "enum".into(),
                        J::strings(&["talk".to_string(), "draft".to_string()]),
                    ),
                ]),
            ),
        ],
    );
    let flow = object(
        &["name", "goal", "steps", "every"],
        vec![
            ("name".into(), text()),
            ("goal".into(), text()),
            ("steps".into(), list(text())),
            ("every".into(), text()),
        ],
    );
    object(
        &["name", "about", "members", "flows"],
        vec![
            ("name".into(), text()),
            ("about".into(), text()),
            ("members".into(), list(member)),
            ("flows".into(), list(flow)),
        ],
    )
    .compact()
}

/// The first `{` to the last `}`: models like to wrap JSON in a sentence or a code fence.
fn json_in(text: &str) -> Option<&str> {
    let (a, b) = (text.find('{')?, text.rfind('}')?);
    (a < b).then(|| &text[a..=b])
}

/// The model's object, with what only the launcher decides put back: the name, the folder for
/// notes, and no model (those come from this computer's plan).
fn normalised(mut j: J, name: &str) -> J {
    let strip = |items: &mut J, key: &str, drop: &str| {
        if let Some(J::Arr(list)) = match items {
            J::Obj(o) => o.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        } {
            for item in list {
                if let J::Obj(o) = item {
                    o.retain(|(k, _)| k != drop);
                }
            }
        }
    };
    strip(&mut j, "members", "model");
    strip(&mut j, "members", "engine");
    strip(&mut j, "flows", "notes_to");
    if let J::Obj(o) = &mut j {
        o.retain(|(k, _)| k != "version");
    }
    j.set("name", J::s(name));
    j
}

/// Models for members the draft left empty, by the weight of the closest template's member of
/// the same handle (the quick model for a handle it does not know).
fn assign_models(band: &mut Band, from: &Template, models: &Models) {
    for m in &mut band.members {
        if m.model.is_empty() {
            m.model = models.pick(from.heavy(&m.handle)).to_string();
        }
    }
}

/// A band for `description`: drafted by the model, or the closest template when the model's
/// answer is missing or does not pass. The second value says which, in a sentence.
pub fn from_description(
    description: &str,
    name: Option<&str>,
    models: &Models,
    chat: &dyn Chat,
) -> (Band, String) {
    let description = tidy(description);
    let template = closest(&description);
    let name = name.unwrap_or(template.id);
    let plain = template.band(name, models);
    let fallback = |why: String| {
        (
            plain.clone(),
            format!(
                "The model's draft was not usable ({why}), so this is the {} template as it is.",
                template.id
            ),
        )
    };
    let thinker = if models.heavy.is_empty() {
        &models.fast
    } else {
        &models.heavy
    };
    let answer = match chat.say_json(
        thinker,
        DRAFTER,
        &ask_text(&description, &normalised(plain.to_j(), name).pretty()),
        &band_schema(),
    ) {
        Ok(a) => a,
        Err(e) => return fallback(e),
    };
    let Some(raw) = json_in(&answer) else {
        return fallback("it did not answer with a JSON object".into());
    };
    let j = match parse(raw) {
        Ok(j) => normalised(j, name),
        Err(e) => return fallback(format!("its JSON did not parse: {e}")),
    };
    match from_j(&j) {
        Ok(mut band) => {
            assign_models(&mut band, template, models);
            let how = format!(
                "The model fitted the {} template to what you do.",
                template.id
            );
            (band, how)
        }
        Err(problems) => fallback(problems.first().cloned().unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::spec::problems;
    use std::cell::RefCell;

    struct Canned {
        answer: Result<String, String>,
        seen: RefCell<Vec<(String, String, String)>>,
    }

    impl Canned {
        fn new(answer: Result<&str, &str>) -> Canned {
            Canned {
                answer: answer.map(String::from).map_err(String::from),
                seen: RefCell::new(vec![]),
            }
        }
    }

    impl Chat for Canned {
        fn say(&self, model: &str, role: &str, text: &str) -> Result<String, String> {
            self.seen
                .borrow_mut()
                .push((model.into(), role.into(), text.into()));
            self.answer.clone()
        }
    }

    fn models() -> Models {
        Models {
            heavy: "qwen3:8b".into(),
            fast: "qwen3:4b".into(),
        }
    }

    const BAKERY: &str = "I run a bakery's Instagram and want posts planned weekly";

    #[test]
    fn junk_from_the_model_falls_back_to_the_template() {
        let template = crate::conductor::templates::find("social-media")
            .unwrap()
            .band("social-media", &models());
        for junk in [
            Ok("Sure! Here you go: a lovely team."),
            Ok("{ this is not json }"),
            Ok("```json\n{\"name\": 3}\n```"),
            Ok("{}"),
            Ok("[1,2,3]"),
            Ok(""),
            Err("the model took too long to answer"),
        ] {
            let chat = Canned::new(junk);
            let (band, how) = from_description(BAKERY, None, &models(), &chat);
            assert_eq!(band, template, "{junk:?}");
            assert!(how.contains("not usable"), "{how}");
            assert!(how.contains("social-media template as it is"), "{how}");
            assert!(problems(&band).is_empty());
        }
    }

    #[test]
    fn a_draft_with_a_bad_spec_is_refused_and_the_template_used() {
        // a member with a tool-sounding extra key, a flow pointing at nobody, a role too long
        let mut b = crate::conductor::templates::find("social-media")
            .unwrap()
            .band("x", &Models::default())
            .to_j();
        let mut bad = b.clone();
        if let J::Obj(o) = &mut bad {
            if let Some((_, J::Arr(ms))) = o.iter_mut().find(|(k, _)| k == "members") {
                if let J::Obj(m) = &mut ms[0] {
                    m.push(("tools".into(), J::strings(&["send_email".to_string()])));
                }
            }
        }
        let chat = Canned::new(Ok(&bad.pretty()));
        let (band, how) = from_description(BAKERY, None, &models(), &chat);
        assert!(
            how.contains("not usable") && how.contains("\"tools\""),
            "{how}"
        );
        assert!(band.members.iter().all(|m| !m.role.is_empty()));

        b.set("flows", J::Arr(vec![]));
        let chat = Canned::new(Ok(&b.pretty()));
        assert!(from_description(BAKERY, None, &models(), &chat)
            .1
            .contains("1 to 4 flows"));
    }

    #[test]
    fn describe_holds_the_model_to_the_bands_json_shape() {
        struct Spy(RefCell<Vec<String>>);
        impl Chat for Spy {
            fn say(&self, _: &str, _: &str, _: &str) -> Result<String, String> {
                Err("say_json should have been used".into())
            }
            fn say_json(&self, _: &str, _: &str, _: &str, schema: &str) -> Result<String, String> {
                self.0.borrow_mut().push(schema.into());
                Err("stop here".into())
            }
        }
        let spy = Spy(RefCell::new(vec![]));
        let (_, how) = from_description(BAKERY, None, &models(), &spy);
        assert!(how.contains("stop here"), "{how}");
        let sent = spy.0.borrow();
        let schema = parse(&sent[0]).expect("the schema is JSON");
        assert_eq!(schema.str("type"), Some("object"));
        assert_eq!(
            schema.strs("required"),
            ["name", "about", "members", "flows"]
        );
        let member = schema
            .get("properties")
            .unwrap()
            .get("members")
            .unwrap()
            .get("items")
            .unwrap();
        assert_eq!(
            member
                .get("properties")
                .unwrap()
                .get("may")
                .unwrap()
                .strs("enum"),
            ["talk", "draft"]
        );
    }

    #[test]
    fn a_good_draft_is_used_with_the_launchers_own_models_and_folder() {
        let base = crate::conductor::templates::find("social-media").unwrap();
        let mut j = base.band("whatever", &models()).to_j();
        // the model renames a member and changes its role, wraps the JSON in chatter, and tries
        // to choose a model and a folder
        if let J::Obj(o) = &mut j {
            if let Some((_, J::Arr(ms))) = o.iter_mut().find(|(k, _)| k == "members") {
                if let J::Obj(m) = &mut ms[1] {
                    for (k, v) in m.iter_mut() {
                        match k.as_str() {
                            "name" => *v = J::s("Crumb"),
                            "role" => *v = J::s("You write bakery captions with a smile."),
                            "model" => *v = J::s("some-giant-model:405b"),
                            _ => {}
                        }
                    }
                }
            }
            if let Some((_, J::Arr(fs))) = o.iter_mut().find(|(k, _)| k == "flows") {
                if let J::Obj(f) = &mut fs[0] {
                    for (k, v) in f.iter_mut() {
                        if k == "notes_to" {
                            *v = J::s("../../elsewhere/");
                        }
                    }
                }
            }
        }
        let reply = format!("Here is the team:\n```json\n{}\n```\nEnjoy!", j.pretty());
        let chat = Canned::new(Ok(&reply));
        let (band, how) = from_description(BAKERY, Some("bakery-posts"), &models(), &chat);
        assert!(how.contains("fitted the social-media template"), "{how}");
        assert_eq!(band.name, "bakery-posts");
        let writer = band.member("writer").unwrap();
        assert_eq!(writer.name, "Crumb");
        assert!(writer.role.starts_with("You write bakery captions"));
        // the plan picks the model, by the weight the template gives that job
        assert_eq!(writer.model, "qwen3:8b");
        assert_eq!(band.member("scheduler").unwrap().model, "qwen3:4b");
        assert!(band
            .flows
            .iter()
            .all(|f| f.notes_to == "Bands/bakery-posts/"));
        assert!(problems(&band).is_empty());
        // the model was asked on the thinking model, with the description and the rules
        let seen = chat.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "qwen3:8b");
        assert!(seen[0].1.contains("one JSON object"));
        assert!(
            seen[0].2.contains("bakery's Instagram") && seen[0].2.contains("Nobody posts, sends")
        );
    }

    #[test]
    fn the_description_is_tidied_before_the_model_sees_it() {
        let chat = Canned::new(Err("no"));
        let long = format!("posts\u{7}\n\n{}", "x".repeat(900));
        from_description(&long, None, &Models::default(), &chat);
        let seen = chat.seen.borrow();
        assert!(!seen[0].2.contains('\u{7}'));
        assert!(!seen[0].2.contains(&"x".repeat(501)));
        // no models in the plan: the call still happens, on the agent's usual model
        assert_eq!(seen[0].0, "");
    }
}
