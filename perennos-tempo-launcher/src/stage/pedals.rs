//! Pedals into the graph. A graph is ComfyUI's API format: node id to `{class_type, inputs}`.
//! Everything an owner or an AI can change goes through here, typed and range-checked, and lands
//! on exactly the widgets the catalogue names. A node that is not there, or has changed class, is
//! an error in words, never a silent skip: that means the upstream graph drifted from the pin.

use super::catalog::{Bind, Catalog, Kind, Pedal, Set, Tier, Via, Workflow};
use crate::setup::json::J;

/// What a run asks for.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Input name to a text value, or a file name already in ComfyUI's input folder.
    pub inputs: Vec<(String, String)>,
    /// Pedal name to the choice or number, as typed.
    pub pedals: Vec<(String, String)>,
}

/// What came out of applying a request.
#[derive(Clone, Debug)]
pub struct Applied {
    pub graph: J,
    pub seed: i64,
    /// The Pedals that took effect, with the value each had (defaults included).
    pub pedals: Vec<(String, String)>,
}

/// A graph without the notes some authors keep beside the nodes (`_comment`): ComfyUI refuses a
/// graph with anything in it that is not a node.
pub fn clean_graph(j: &J) -> Result<J, String> {
    let J::Obj(o) = j else {
        return Err("the workflow is not a graph (an object of nodes)".into());
    };
    let nodes: Vec<(String, J)> = o
        .iter()
        .filter(|(_, v)| {
            v.get("class_type").is_some() && matches!(v.get("inputs"), Some(J::Obj(_)))
        })
        .cloned()
        .collect();
    if nodes.is_empty() {
        return Err(
            "the workflow holds no nodes. It may be a UI export; Stage runs the API format.".into(),
        );
    }
    Ok(J::Obj(nodes))
}

fn node_mut<'a>(g: &'a mut J, node: &str) -> Result<&'a mut J, String> {
    match g {
        J::Obj(o) => o
            .iter_mut()
            .find(|(k, _)| k == node)
            .map(|(_, v)| v)
            .ok_or_else(|| format!("the graph has no node {node} (the upstream workflow changed)")),
        _ => Err("not a graph".into()),
    }
}

fn widget_mut<'a>(g: &'a mut J, b: &str, field: &str, class: &str) -> Result<&'a mut J, String> {
    let n = node_mut(g, b)?;
    if !class.is_empty() && n.str("class_type") != Some(class) {
        return Err(format!(
            "node {b} is {} now, not {class} (the upstream workflow changed)",
            n.str("class_type").unwrap_or("something else")
        ));
    }
    let J::Obj(o) = n else {
        return Err("not a node".into());
    };
    let inputs = o
        .iter_mut()
        .find(|(k, _)| k == "inputs")
        .map(|(_, v)| v)
        .ok_or("a node with no inputs")?;
    let J::Obj(ins) = inputs else {
        return Err("a node with no inputs".into());
    };
    let slot = ins
        .iter_mut()
        .find(|(k, _)| k == field)
        .map(|(_, v)| v)
        .ok_or_else(|| format!("node {b} has no \"{field}\" (the upstream workflow changed)"))?;
    if matches!(slot, J::Arr(_)) {
        return Err(format!(
            "node {b} \"{field}\" is wired to another node, not a value"
        ));
    }
    Ok(slot)
}

fn put(g: &mut J, node: &str, field: &str, class: &str, v: J) -> Result<(), String> {
    *widget_mut(g, node, field, class)? = v;
    Ok(())
}

fn num_j(v: f64, round: bool) -> J {
    if round || v.fract() == 0.0 {
        J::n(v.round())
    } else {
        J::n((v * 1000.0).round() / 1000.0)
    }
}

/// Every place a workflow's catalogue entry touches must exist in the graph, with its class.
/// Returns what is wrong (nothing: the pins still fit).
pub fn check_binds(wf: &Workflow, graph: &J) -> Vec<String> {
    let mut g = graph.clone();
    let mut bad = vec![];
    let mut look = |node: &str, field: &str, class: &str, what: String| {
        if field.is_empty() {
            if node_mut(&mut g, node).is_err() {
                bad.push(format!("{what}: no node {node}"));
            }
        } else if let Err(e) = widget_mut(&mut g, node, field, class) {
            // a wired widget is not a problem for a check, only a missing one is
            if !e.contains("wired") {
                bad.push(format!("{what}: {e}"));
            }
        }
    };
    for i in &wf.inputs {
        for b in &i.binds {
            look(&b.node, &b.field, &b.class, format!("input {}", i.name));
        }
    }
    for o in &wf.outputs {
        look(&o.node, "", "", "output".into());
    }
    for t in &wf.tiers {
        for s in &t.sets {
            look(&s.node, &s.field, &s.class, "a tier".into());
        }
    }
    for p in &wf.pedals {
        match &p.via {
            Via::Choices(c) => {
                for (k, sets) in c {
                    for s in sets {
                        look(&s.node, &s.field, &s.class, format!("pedal {}={k}", p.name));
                    }
                }
            }
            Via::Number { binds, .. } | Via::Prompt(binds) => {
                for b in binds {
                    look(&b.node, &b.field, &b.class, format!("pedal {}", p.name));
                }
            }
        }
    }
    bad
}

fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}

/// One Pedal's value, checked against what the workflow allows. Choices come back as their id,
/// numbers as clean text.
pub fn check_value(cat: &Catalog, wf: &Workflow, name: &str, raw: &str) -> Result<String, String> {
    let p = wf.pedal(name).ok_or_else(|| {
        let has: Vec<&str> = wf.pedals.iter().map(|p| p.name.as_str()).collect();
        format!(
            "{} has no \"{name}\" Pedal. It has: {}.",
            wf.id,
            has.join(", ")
        )
    })?;
    match &p.via {
        Via::Choices(c) => {
            let want = norm(raw);
            c.iter()
                .find(|(k, _)| *k == want)
                .map(|(k, _)| k.clone())
                .ok_or_else(|| {
                    format!(
                        "{name} is one of {}, not \"{raw}\".",
                        c.iter()
                            .map(|(k, _)| k.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
        }
        Via::Prompt(_) => {
            let w = cat.word(name).ok_or("not in the vocabulary")?;
            let want = norm(raw);
            w.choices
                .iter()
                .find(|c| norm(&c.id) == want)
                .map(|c| c.id.clone())
                .ok_or_else(|| {
                    format!(
                        "{name} is one of {}, not \"{raw}\".",
                        w.choices
                            .iter()
                            .map(|c| c.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
        }
        Via::Number { min, max, .. } => {
            let unit = cat.word(name).map(|w| w.unit.clone()).unwrap_or_default();
            let v: f64 = raw
                .trim()
                .trim_end_matches(|c: char| c.is_alphabetic() || c == '°')
                .trim()
                .parse()
                .map_err(|_| format!("{name} is a number from {min} to {max}, not \"{raw}\"."))?;
            if !v.is_finite() || v < *min || v > *max {
                return Err(format!(
                    "{name} goes from {min} to {max}{}, not {raw}.",
                    if unit.is_empty() {
                        String::new()
                    } else {
                        format!(" {unit}")
                    }
                ));
            }
            Ok(if cat.word(name).is_some_and(|w| w.kind == Kind::Integer) {
                format!("{}", v.round() as i64)
            } else {
                format!("{v}")
            })
        }
    }
}

fn scaled(x: f64, b: &Bind) -> f64 {
    match b.quantize {
        Some((step, plus)) => (x * b.scale / step).round() * step + plus,
        None => x * b.scale + b.offset,
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Add a sentence to the text already in a widget.
fn append_text(g: &mut J, b: &Bind, phrase: &str) -> Result<(), String> {
    let w = widget_mut(g, &b.node, &b.field, &b.class)?;
    let J::Str(cur) = w else {
        return Err(format!("node {} \"{}\" is not text", b.node, b.field));
    };
    let mut t = cur.trim_end().to_string();
    if !t.is_empty() && !t.ends_with(['.', '!', '?']) {
        t.push('.');
    }
    if !t.is_empty() {
        t.push(' ');
    }
    t.push_str(&capital(phrase));
    t.push('.');
    *cur = t;
    Ok(())
}

/// `{name}` replaced by `value` in every text widget of the graph.
fn fill_placeholder(g: &mut J, name: &str, value: &str) {
    let key = format!("{{{name}}}");
    if let J::Obj(nodes) = g {
        for (_, n) in nodes.iter_mut() {
            if let Some(J::Obj(ins)) = match n {
                J::Obj(o) => o.iter_mut().find(|(k, _)| k == "inputs").map(|(_, v)| v),
                _ => None,
            } {
                for (_, v) in ins.iter_mut() {
                    if let J::Str(s) = v {
                        if s.contains(&key) {
                            *s = s.replace(&key, value);
                        }
                    }
                }
            }
        }
    }
}

fn default_of(p: &Pedal) -> Option<String> {
    p.default.clone()
}

/// Put a request into a copy of the graph: the tier's settings first, then the inputs, then each
/// Pedal (choices, prompt words, numbers), then the seed. `fresh` makes a seed when it is -1.
pub fn apply(
    cat: &Catalog,
    wf: &Workflow,
    graph: &J,
    tier: &Tier,
    req: &Request,
    fresh: &mut dyn FnMut() -> i64,
) -> Result<Applied, String> {
    let mut g = graph.clone();
    for s in &tier.sets {
        put(&mut g, &s.node, &s.field, &s.class, s.value.clone())?;
    }
    // inputs
    for i in &wf.inputs {
        let given = req
            .inputs
            .iter()
            .find(|(k, _)| *k == i.name)
            .map(|(_, v)| v.clone());
        let value = match given {
            Some(v) => v,
            None if i.required && i.default.is_empty() => {
                return Err(format!(
                    "{} needs the input \"{}\"{}.",
                    wf.id,
                    i.name,
                    if i.kind == "text" { "" } else { " (a file)" }
                ))
            }
            None => i.default.clone(),
        };
        if i.kind == "text" && value.len() > 4000 {
            return Err(format!("the input \"{}\" is over 4000 characters", i.name));
        }
        for b in &i.binds {
            let v = match &b.template {
                Some(t) => t.replace("{value}", &value),
                None => value.clone(),
            };
            put(&mut g, &b.node, &b.field, &b.class, J::Str(v))?;
        }
    }
    for (k, _) in &req.inputs {
        if wf.input(k).is_none() {
            return Err(format!(
                "{} has no input \"{k}\". It has: {}.",
                wf.id,
                wf.inputs
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    // Pedals: the ones asked for, checked; then the defaults of the rest
    let mut chosen: Vec<(String, String)> = vec![];
    for (k, raw) in &req.pedals {
        if chosen.iter().any(|(n, _)| n == k) {
            return Err(format!("{k} is given twice"));
        }
        chosen.push((k.clone(), check_value(cat, wf, k, raw)?));
    }
    for p in &wf.pedals {
        if !chosen.iter().any(|(n, _)| *n == p.name) {
            if let Some(d) = default_of(p) {
                chosen.push((p.name.clone(), d));
            }
        }
    }
    let mut seed: i64 = -1;
    let mut taken: Vec<(String, String)> = vec![];
    // choices before prompt words before numbers, so a choice that rewrites the prompt comes first
    for pass in 0..3 {
        for p in &wf.pedals {
            let Some((_, v)) = chosen.iter().find(|(n, _)| *n == p.name) else {
                continue;
            };
            match (&p.via, pass) {
                (Via::Choices(c), 0) => {
                    let sets: &Vec<Set> =
                        &c.iter().find(|(k, _)| k == v).ok_or("no such choice")?.1;
                    for s in sets {
                        put(&mut g, &s.node, &s.field, &s.class, s.value.clone())?;
                    }
                    taken.push((p.name.clone(), v.clone()));
                }
                (Via::Prompt(binds), 1) => {
                    let phrase = cat
                        .word(&p.name)
                        .and_then(|w| w.choices.iter().find(|c| c.id == *v))
                        .and_then(|c| c.prompt.clone())
                        .ok_or("no prompt words for that choice")?;
                    for b in binds {
                        append_text(&mut g, b, &phrase)?;
                    }
                    taken.push((p.name.clone(), v.clone()));
                }
                (Via::Number { binds, .. }, 2) => {
                    let mut x: f64 = v.parse().map_err(|_| "not a number")?;
                    if p.name == "seed" {
                        if x < 0.0 {
                            x = fresh() as f64;
                        }
                        seed = x as i64;
                    }
                    for b in binds {
                        put(
                            &mut g,
                            &b.node,
                            &b.field,
                            &b.class,
                            num_j(scaled(x, b), b.round || b.quantize.is_some()),
                        )?;
                    }
                    taken.push((
                        p.name.clone(),
                        if p.name == "seed" {
                            format!("{}", x as i64)
                        } else {
                            v.clone()
                        },
                    ));
                }
                _ => {}
            }
        }
    }
    if seed < 0 && wf.pedal("seed").is_none() {
        seed = fresh();
    }
    // placeholders last, from the inputs that ask for it
    for i in wf.inputs.iter().filter(|i| i.placeholder) {
        let v = req
            .inputs
            .iter()
            .find(|(k, _)| *k == i.name)
            .map_or(i.default.clone(), |(_, v)| v.clone());
        fill_placeholder(&mut g, &i.name, &v);
    }
    Ok(Applied {
        graph: g,
        seed,
        pedals: taken,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::setup::json::parse;
    use crate::stage::catalog::embedded;

    /// A small graph in the shape of the real ones (a camera node, a prompt, a seed).
    pub const GRAPH: &str = r#"{
      "_comment": "notes beside the nodes",
      "97": {"class_type": "LoadImage", "inputs": {"image": "old.png"}},
      "107": {"class_type": "CLIPTextEncode", "inputs": {"text": "A quiet harbor", "clip": ["38", 0]}},
      "108": {"class_type": "WanCameraEmbedding", "inputs": {"camera_pose": "Static", "speed": 1.0, "width": 640, "height": 640, "length": 81}},
      "102": {"class_type": "KSamplerAdvanced", "inputs": {"noise_seed": 5, "steps": 20}}
    }"#;

    #[test]
    fn a_graph_loses_its_notes_and_keeps_its_nodes() {
        let g = clean_graph(&parse(GRAPH).unwrap()).unwrap();
        assert!(g.get("_comment").is_none() && g.get("107").is_some());
        assert!(clean_graph(&parse("{\"nodes\":[]}").unwrap())
            .unwrap_err()
            .contains("UI export"));
    }

    fn camera_photo() -> (Catalog, Workflow, J) {
        let c = embedded();
        let w = c.find("camera-photo").unwrap().clone();
        (c, w, clean_graph(&parse(GRAPH).unwrap()).unwrap())
    }

    fn widget(g: &J, node: &str, field: &str) -> J {
        g.get(node)
            .unwrap()
            .get("inputs")
            .unwrap()
            .get(field)
            .unwrap()
            .clone()
    }

    #[test]
    fn pedals_land_on_the_widgets_the_catalogue_names() {
        let (c, w, g) = camera_photo();
        let tier = w.tier_for(24.0).unwrap().clone();
        let req = Request {
            inputs: vec![
                ("image".into(), "me.png".into()),
                ("prompt".into(), "A dog on a pier".into()),
            ],
            pedals: vec![
                ("camera".into(), "Pan Left".into()),
                ("lens".into(), "85mm".into()),
                ("light".into(), "golden hour".into()),
                ("speed".into(), "2.5".into()),
                ("duration".into(), "3".into()),
                ("seed".into(), "42".into()),
            ],
        };
        let a = apply(&c, &w, &g, &tier, &req, &mut || 99).unwrap();
        assert_eq!(widget(&a.graph, "97", "image"), J::s("me.png"));
        assert_eq!(widget(&a.graph, "108", "camera_pose"), J::s("Pan Left"));
        assert_eq!(widget(&a.graph, "108", "speed"), J::n(2.5));
        // 3 seconds at 16 frames a second, plus one: the shape the model wants
        assert_eq!(widget(&a.graph, "108", "length"), J::n(49.0));
        assert_eq!(widget(&a.graph, "102", "noise_seed"), J::n(42.0));
        assert_eq!(a.seed, 42);
        let text = match widget(&a.graph, "107", "text") {
            J::Str(s) => s,
            _ => panic!(),
        };
        assert!(text.starts_with("A dog on a pier."), "{text}");
        assert!(
            text.contains("85 mm portrait lens") && text.contains("olden hour sunlight"),
            "{text}"
        );
        // the original graph was not touched
        assert_eq!(widget(&g, "108", "camera_pose"), J::s("Static"));
        let said: Vec<&str> = a.pedals.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            said.contains(&"camera") && said.contains(&"duration"),
            "{said:?}"
        );
    }

    #[test]
    fn one_camera_word_means_the_same_thing_in_each_workflow_that_has_it() {
        // push_in: Zoom In on a photo, push in the re-angle presets
        let c = embedded();
        for (id, field, want) in [
            ("camera-photo", "camera_pose", "Zoom In"),
            ("re-angle-video", "preset", "push"),
        ] {
            let w = c.find(id).unwrap();
            let Some(Pedal {
                via: Via::Choices(ch),
                ..
            }) = w.pedal("camera")
            else {
                panic!("{id}")
            };
            let sets = &ch.iter().find(|(k, _)| k == "push_in").unwrap().1;
            assert_eq!(sets[0].field, field);
            assert_eq!(sets[0].value, J::s(want), "{id}");
        }
    }

    #[test]
    fn a_wrong_value_is_refused_with_the_range_in_words() {
        let (c, w, _) = camera_photo();
        let e = check_value(&c, &w, "speed", "9").unwrap_err();
        assert!(e.contains("0.2 to 3") && e.contains("not 9"), "{e}");
        let e = check_value(&c, &w, "camera", "barrel roll").unwrap_err();
        assert!(e.contains("push_in") && e.contains("barrel roll"), "{e}");
        let e = check_value(&c, &w, "angle", "5").unwrap_err();
        assert!(
            e.contains("no \"angle\" Pedal") && e.contains("speed"),
            "{e}"
        );
        assert_eq!(check_value(&c, &w, "camera", "Push In").unwrap(), "push_in");
        assert_eq!(check_value(&c, &w, "duration", "4s").unwrap(), "4");
    }

    #[test]
    fn a_missing_or_changed_node_is_an_error_not_a_skip() {
        let (c, w, g) = camera_photo();
        let tier = w.tier_for(24.0).unwrap().clone();
        let mut gone = g.clone();
        if let J::Obj(o) = &mut gone {
            o.retain(|(k, _)| k != "108");
        }
        let req = Request {
            inputs: vec![("image".into(), "a.png".into())],
            pedals: vec![("speed".into(), "1".into())],
        };
        let e = apply(&c, &w, &gone, &tier, &req, &mut || 1).unwrap_err();
        assert!(e.contains("no node 108") && e.contains("changed"), "{e}");
        assert!(!check_binds(&w, &gone).is_empty());
        assert!(check_binds(&w, &g)
            .iter()
            .all(|m| !m.contains("no node 108")));
        // a node of another class
        let mut other = g.clone();
        if let J::Obj(o) = &mut other {
            if let Some((_, J::Obj(n))) = o.iter_mut().find(|(k, _)| k == "108") {
                n.iter_mut().find(|(k, _)| k == "class_type").unwrap().1 = J::s("SomethingElse");
            }
        }
        let e = apply(&c, &w, &other, &tier, &req, &mut || 1).unwrap_err();
        assert!(e.contains("SomethingElse now"), "{e}");
    }

    #[test]
    fn a_required_input_that_is_missing_is_named() {
        let (c, w, g) = camera_photo();
        let tier = w.tier_for(24.0).unwrap().clone();
        let e = apply(&c, &w, &g, &tier, &Request::default(), &mut || 1).unwrap_err();
        assert!(e.contains("needs the input \"image\" (a file)"), "{e}");
        let bad = Request {
            inputs: vec![
                ("image".into(), "a.png".into()),
                ("nope".into(), "x".into()),
            ],
            pedals: vec![],
        };
        assert!(apply(&c, &w, &g, &tier, &bad, &mut || 1)
            .unwrap_err()
            .contains("no input \"nope\""));
    }

    #[test]
    fn a_seed_of_minus_one_is_a_fresh_one_and_it_is_reported() {
        let (c, w, g) = camera_photo();
        let tier = w.tier_for(24.0).unwrap().clone();
        let req = Request {
            inputs: vec![("image".into(), "a.png".into())],
            pedals: vec![],
        };
        let a = apply(&c, &w, &g, &tier, &req, &mut || 777).unwrap();
        assert_eq!(a.seed, 777);
        assert_eq!(widget(&a.graph, "102", "noise_seed"), J::n(777.0));
    }

    #[test]
    fn a_placeholder_is_filled_where_it_stands_and_a_choice_can_rewrite_the_prompt() {
        let c = embedded();
        let w = c.find("effect-clip").unwrap().clone();
        let g = parse(
            r#"{"52":{"class_type":"LoadImage","inputs":{"image":"x"}},
                "6":{"class_type":"CLIPTextEncode","inputs":{"text":"old","clip":["38",0]}},
                "ff_effect":{"class_type":"LoraLoaderModelOnly","inputs":{"lora_name":"a","strength_model":1.0,"model":["37",0]}},
                "50":{"class_type":"WanImageToVideo","inputs":{"length":81}},
                "3":{"class_type":"KSampler","inputs":{"seed":1}},
                "56":{"class_type":"SaveVideo","inputs":{}}}"#,
        )
        .unwrap();
        let tier = w.tier_for(24.0).unwrap().clone();
        let req = Request {
            inputs: vec![
                ("image".into(), "cat.png".into()),
                ("subject".into(), "an orange cat".into()),
            ],
            pedals: vec![
                ("effect".into(), "crush".into()),
                ("strength".into(), "1.2".into()),
            ],
        };
        let a = apply(&c, &w, &g, &tier, &req, &mut || 5).unwrap();
        let text = match widget(&a.graph, "6", "text") {
            J::Str(s) => s,
            _ => panic!(),
        };
        assert!(
            text.contains("begins with an orange cat") || text.contains("with an orange cat"),
            "{text}"
        );
        assert!(
            !text.contains("{subject}") && !text.contains("{details}"),
            "{text}"
        );
        assert_eq!(
            widget(&a.graph, "ff_effect", "lora_name"),
            J::s("crush_20_epochs.safetensors")
        );
        assert_eq!(widget(&a.graph, "ff_effect", "strength_model"), J::n(1.2));
    }

    #[test]
    fn frames_snap_to_what_ltx_wants() {
        let b = Bind {
            node: "1".into(),
            field: "n".into(),
            class: String::new(),
            scale: 24.0,
            offset: 0.0,
            round: false,
            quantize: Some((8.0, 1.0)),
            template: None,
        };
        assert_eq!(scaled(1.7, &b), 41.0);
        assert_eq!(scaled(1.0, &b), 25.0);
        assert_eq!(scaled(1.4, &b), 33.0);
    }
}
