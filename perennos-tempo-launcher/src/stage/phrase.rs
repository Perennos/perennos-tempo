//! Words to Pedals: "re-angle the last clip with a slow orbit", "make it golden hour".
//!
//! One parser, used by Room ("@stage ...", and "@tempo ..." from the owner, which is how a spoken
//! phrase arrives) and by `tempo-launch stage cue`. It uses nothing but the standard library, so
//! the same file is copied, byte for byte, into each crate that needs it (tests included). It
//! knows the Pedal vocabulary in docs/STAGE.md and the workflow ids, and nothing else: it does
//! not know which workflows are added, or what the last run was. The caller checks the Cue against
//! the catalogue (`pedals::check_value` refuses a value a workflow cannot take) and finds the
//! input in the last run, because words cannot carry a file.

/// What the words asked for.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cue {
    /// The workflow the words name ("re-angle" is `re-angle-video`). None: the last one that ran.
    pub workflow: Option<String>,
    /// Pedal name and value, camera first, then its speed, lens, light and the rest.
    pub pedals: Vec<(String, String)>,
}

impl Cue {
    /// "re-angle-video with camera=orbit_left, speed=0.5", for the owner to read before a y/N.
    pub fn describe(&self) -> String {
        let w = self
            .workflow
            .as_deref()
            .unwrap_or("the workflow that ran last");
        if self.pedals.is_empty() {
            return w.to_string();
        }
        let p: Vec<String> = self
            .pedals
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!("{w} with {}", p.join(", "))
    }
}

/// The words as lower case tokens: punctuation and hyphens are spaces, `12°` is `12 degrees`.
fn tokens(text: &str) -> Vec<String> {
    let mut s = String::new();
    let cs: Vec<char> = text.chars().collect();
    for (i, c) in cs.iter().enumerate() {
        match c {
            '°' => s.push_str(" degrees "),
            '.' if i > 0
                && cs.get(i + 1).is_some_and(|n| n.is_ascii_digit())
                && cs[i - 1].is_ascii_digit() =>
            {
                s.push('.')
            }
            c if c.is_alphanumeric() => s.extend(c.to_lowercase()),
            '\'' | '\u{2019}' => {}
            _ => s.push(' '),
        }
    }
    s.split_whitespace().map(String::from).collect()
}

fn at(t: &[String], i: usize, words: &[&str]) -> bool {
    t.len() >= i + words.len() && words.iter().enumerate().all(|(k, w)| t[i + k] == *w)
}

/// Where the first of these phrases starts.
fn find(t: &[String], phrases: &[&[&str]]) -> Option<usize> {
    (0..t.len()).find(|&i| phrases.iter().any(|p| at(t, i, p)))
}

fn has(t: &[String], phrases: &[&[&str]]) -> bool {
    find(t, phrases).is_some()
}

fn has_word(t: &[String], words: &[&str]) -> bool {
    t.iter().any(|w| words.contains(&w.as_str()))
}

/// Left or right, within the three words after `i` ("counter clockwise" is left).
fn side(t: &[String], i: usize) -> Option<&'static str> {
    let near: Vec<&str> = t.iter().skip(i + 1).take(3).map(String::as_str).collect();
    if near.iter().any(|w| {
        matches!(
            *w,
            "left" | "counter" | "anti" | "anticlockwise" | "counterclockwise"
        )
    }) {
        Some("left")
    } else if near.iter().any(|w| matches!(*w, "right" | "clockwise")) {
        Some("right")
    } else {
        None
    }
}

fn camera(t: &[String]) -> Option<&'static str> {
    let moves: [(&[&str], &str); 12] = [
        (&["dolly", "zoom"], "dolly_zoom"),
        (&["crash", "zoom"], "crash_zoom"),
        (&["push", "in"], "push_in"),
        (&["dolly", "in"], "push_in"),
        (&["zoom", "in"], "push_in"),
        (&["pull", "out"], "pull_out"),
        (&["pull", "back"], "pull_out"),
        (&["dolly", "out"], "pull_out"),
        (&["zoom", "out"], "pull_out"),
        (&["crane", "up"], "crane_up"),
        (&["crane", "down"], "crane_down"),
        (&["low", "angle"], "low_angle"),
    ];
    if let Some((_, v)) = moves.iter().find(|(p, _)| has(t, &[p])) {
        return Some(v);
    }
    if has(t, &[&["high", "angle"], &["birds", "eye"]]) {
        return Some("high_angle");
    }
    if let Some(i) = find(t, &[&["pan"]]) {
        return Some(match t.get(i + 1).map(String::as_str) {
            Some("up") => "pan_up",
            Some("down") => "pan_down",
            Some("right") => "pan_right",
            _ => "pan_left",
        });
    }
    if let Some(i) = find(t, &[&["tilt"]]) {
        return match t.get(i + 1).map(String::as_str) {
            Some("up") => Some("pan_up"),
            Some("down") => Some("pan_down"),
            _ => None,
        };
    }
    if let Some(i) = find(t, &[&["roll"]]) {
        return Some(match side(t, i) {
            Some("left") => "roll_ccw",
            _ => "roll_cw",
        });
    }
    if let Some(i) = find(
        t,
        &[&["orbit"], &["orbiting"], &["circle", "around"], &["arc"]],
    ) {
        return Some(match side(t, i) {
            Some("right") => "orbit_right",
            _ => "orbit_left",
        });
    }
    if has(t, &[&["truck"]]) {
        return Some("truck");
    }
    if has(
        t,
        &[
            &["static"],
            &["lock", "off"],
            &["locked", "off"],
            &["hold", "still"],
        ],
    ) {
        return Some("static");
    }
    None
}

fn light(t: &[String]) -> Option<&'static str> {
    let table: [(&[&[&str]], &str); 8] = [
        (&[&["golden", "hour"], &["sunset"]], "golden_hour"),
        (&[&["blue", "hour"], &["twilight"]], "blue_hour"),
        (
            &[
                &["rim", "light"],
                &["rim", "lit"],
                &["rim", "lighting"],
                &["rim"],
            ],
            "rim",
        ),
        (&[&["noir"]], "noir"),
        (&[&["neon"]], "neon"),
        (
            &[&["softbox"], &["soft", "box"], &["soft", "light"]],
            "softbox",
        ),
        (&[&["overcast"], &["cloudy"]], "overcast"),
        (
            &[
                &["candlelight"],
                &["candle", "light"],
                &["candle", "lit"],
                &["candlelit"],
            ],
            "candlelight",
        ),
    ];
    table.iter().find(|(p, _)| has(t, p)).map(|(_, v)| *v)
}

fn lens(t: &[String]) -> Option<&'static str> {
    for (i, w) in t.iter().enumerate() {
        let n = w.strip_suffix("mm").filter(|n| !n.is_empty()).or_else(|| {
            // "50 mm"
            (t.get(i + 1).map(String::as_str) == Some("mm")).then_some(w.as_str())
        });
        match n {
            Some("24") => return Some("24mm"),
            Some("35") => return Some("35mm"),
            Some("50") => return Some("50mm"),
            Some("85") => return Some("85mm"),
            _ => {}
        }
    }
    if has(
        t,
        &[
            &["wide", "lens"],
            &["wide", "angle", "lens"],
            &["wide", "shot"],
        ],
    ) {
        return Some("24mm");
    }
    if has_word(t, &["telephoto"]) || has(t, &[&["portrait", "lens"], &["close", "up"]]) {
        return Some("85mm");
    }
    None
}

/// A number written by itself or with a unit suffix stuck to it ("4s", "12").
fn number_before(t: &[String], i: usize) -> Option<f64> {
    t.get(i.checked_sub(1)?)?.parse().ok()
}

fn speed(t: &[String]) -> Option<String> {
    // "2x speed", "speed 1.5"
    for (i, w) in t.iter().enumerate() {
        if let Some(n) = w.strip_suffix('x').and_then(|n| n.parse::<f64>().ok()) {
            if (0.2..=3.0).contains(&n) {
                return Some(trim(n));
            }
        }
        if w == "speed" {
            if let Some(n) = t.get(i + 1).and_then(|n| n.parse::<f64>().ok()) {
                if (0.2..=3.0).contains(&n) {
                    return Some(trim(n));
                }
            }
        }
    }
    if has(
        t,
        &[&["very", "slow"], &["super", "slow"], &["really", "slow"]],
    ) {
        return Some("0.3".into());
    }
    if has_word(t, &["slow", "slowly", "gentle", "gently", "slower"]) {
        return Some("0.5".into());
    }
    if has_word(t, &["fast", "quick", "quickly", "faster", "snap", "snappy"]) {
        return Some("2".into());
    }
    None
}

fn trim(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n}")
    }
}

fn degrees(t: &[String]) -> Option<String> {
    let i = find(t, &[&["degrees"], &["degree"], &["deg"]])?;
    let n = number_before(t, i)?;
    (5.0..=30.0).contains(&n).then(|| trim(n))
}

fn seconds(t: &[String]) -> Option<String> {
    for (i, w) in t.iter().enumerate() {
        if matches!(w.as_str(), "seconds" | "second" | "secs" | "sec" | "s") {
            if let Some(n) = number_before(t, i) {
                if n > 0.0 && n <= 60.0 {
                    return Some(trim(n));
                }
            }
        }
        if let Some(n) = w.strip_suffix('s').and_then(|n| n.parse::<f64>().ok()) {
            if n > 0.0 && n <= 60.0 {
                return Some(trim(n));
            }
        }
    }
    None
}

fn seed(t: &[String]) -> Option<String> {
    if has(
        t,
        &[
            &["new", "seed"],
            &["fresh", "seed"],
            &["different", "seed"],
            &["another", "seed"],
        ],
    ) {
        return Some("-1".into());
    }
    let i = find(t, &[&["seed"]])?;
    let n = t.get(i + 1)?.parse::<u64>().ok()?;
    Some(n.to_string())
}

fn strength(t: &[String]) -> Option<String> {
    let i = find(t, &[&["strength"]])?;
    let n = t.get(i + 1)?.parse::<f64>().ok()?;
    Some(trim(n))
}

fn workflow(t: &[String]) -> Option<&'static str> {
    let photo = has_word(t, &["photo", "picture", "image", "still", "frame"]);
    if has(
        t,
        &[
            &["re", "angle"],
            &["reangle"],
            &["new", "angle"],
            &["another", "angle"],
            &["different", "angle"],
        ],
    ) {
        return Some("re-angle-video");
    }
    if has(t, &[&["relight"], &["re", "light"]]) {
        return Some("relight-photo");
    }
    if has(t, &[&["lip", "sync"], &["lipsync"]]) || has_word(t, &["dub", "dubbed"]) {
        return Some("lip-sync-video");
    }
    if has(t, &[&["clean", "plate"], &["remove", "the"], &["erase"]])
        && !has(t, &[&["remove", "the", "light"]])
    {
        return Some("clean-plate-video");
    }
    if has_word(t, &["swap", "recast"]) || has(t, &[&["replace", "the", "character"]]) {
        return Some(if photo {
            "swap-character-photo"
        } else {
            "swap-character-video"
        });
    }
    if photo && has(t, &[&["camera", "move"], &["move", "the", "camera"]]) {
        return Some("camera-photo");
    }
    if has(
        t,
        &[&["paint", "edit"], &["inpaint"], &["edit", "the", "photo"]],
    ) {
        return Some("paint-edit-photo");
    }
    if has_word(t, &["effect"]) && !has_word(t, &["effects"]) || has(t, &[&["add", "an", "effect"]])
    {
        return Some("effect-clip");
    }
    None
}

const QUESTION: [&str; 12] = [
    "what", "why", "how", "when", "who", "where", "which", "is", "are", "can", "does", "do",
];

/// What the words ask Stage for, or None when they are not about Stage.
///
/// `addressed`: the words were said to Stage itself (`@stage ...`), so any one Pedal or workflow
/// is enough. Not addressed (`@tempo ...`, the way a spoken phrase arrives), the words must also
/// read as an instruction, so an ordinary question that happens to say "golden hour" goes to
/// Tempo as the question it is.
pub fn parse(text: &str, addressed: bool) -> Option<Cue> {
    let t = tokens(text);
    if t.is_empty() || text.trim_end().ends_with('?') {
        return None;
    }
    let cam = camera(&t);
    let mut pedals: Vec<(String, String)> = vec![];
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            pedals.push((k.to_string(), v));
        }
    };
    put("camera", cam.map(String::from));
    // a speed word is a camera speed: it needs a camera move to mean anything
    put("speed", cam.and_then(|_| speed(&t)));
    put("lens", lens(&t).map(String::from));
    put("light", light(&t).map(String::from));
    put("angle", degrees(&t));
    put("duration", seconds(&t));
    put("strength", strength(&t));
    put("seed", seed(&t));
    let wf = workflow(&t).map(String::from);
    if pedals.is_empty() && wf.is_none() {
        return None;
    }
    if !addressed {
        let first = t.first().map(String::as_str).unwrap_or("");
        let instruction = wf.is_some()
            || matches!(
                first,
                "make"
                    | "give"
                    | "add"
                    | "try"
                    | "set"
                    | "use"
                    | "put"
                    | "turn"
                    | "change"
                    | "shoot"
                    | "play"
                    | "now"
                    | "and"
                    | "again"
                    | "more"
                    | "less"
                    | "same"
            )
            || t.len() <= 4;
        if QUESTION.contains(&first) || !instruction || t.len() > 14 {
            return None;
        }
    }
    Some(Cue {
        workflow: wf,
        pedals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(text: &str, addressed: bool) -> Cue {
        parse(text, addressed).unwrap_or_else(|| panic!("no cue for {text:?}"))
    }

    fn p(c: &Cue) -> Vec<String> {
        c.pedals.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }

    #[test]
    fn a_slow_orbit_on_the_last_clip() {
        let c = cue("re-angle the last clip with a slow orbit", true);
        assert_eq!(c.workflow.as_deref(), Some("re-angle-video"));
        assert_eq!(p(&c), ["camera=orbit_left", "speed=0.5"]);
        assert_eq!(
            c.describe(),
            "re-angle-video with camera=orbit_left, speed=0.5"
        );
    }

    #[test]
    fn golden_hour_alone_is_a_light_on_whatever_ran_last() {
        for words in [
            "golden hour",
            "make it golden hour",
            "Golden-hour!",
            "give it a golden hour look",
            "make it sunset",
        ] {
            let c = cue(words, true);
            assert_eq!(
                (c.workflow.as_deref(), p(&c)),
                (None, vec!["light=golden_hour".to_string()]),
                "{words}"
            );
        }
    }

    #[test]
    fn the_other_lights_and_lenses() {
        assert_eq!(p(&cue("make it noir", true)), ["light=noir"]);
        assert_eq!(p(&cue("blue hour please", true)), ["light=blue_hour"]);
        assert_eq!(
            p(&cue("rim light and a 50mm lens", true)),
            ["lens=50mm", "light=rim"]
        );
        assert_eq!(p(&cue("neon, 85 mm", true)), ["lens=85mm", "light=neon"]);
        assert_eq!(p(&cue("by candlelight", true)), ["light=candlelight"]);
        assert_eq!(
            p(&cue("a wide lens with softbox light", true)),
            ["lens=24mm", "light=softbox"]
        );
        // a lens that is not in the vocabulary is not guessed
        assert!(parse("a 70mm lens", true).is_none());
    }

    #[test]
    fn camera_moves_with_a_side_a_speed_and_a_length() {
        assert_eq!(p(&cue("orbit right", true)), ["camera=orbit_right"]);
        assert_eq!(
            p(&cue("push in slowly", true)),
            ["camera=push_in", "speed=0.5"]
        );
        assert_eq!(
            p(&cue("crane up fast", true)),
            ["camera=crane_up", "speed=2"]
        );
        assert_eq!(
            p(&cue("dolly zoom for 4 seconds", true)),
            ["camera=dolly_zoom", "duration=4"]
        );
        assert_eq!(
            p(&cue("pan left at 1.5x", true)),
            ["camera=pan_left", "speed=1.5"]
        );
        assert_eq!(
            p(&cue("a very slow pull back", true)),
            ["camera=pull_out", "speed=0.3"]
        );
        assert_eq!(p(&cue("low angle", true)), ["camera=low_angle"]);
        assert_eq!(p(&cue("roll counter clockwise", true)), ["camera=roll_ccw"]);
    }

    #[test]
    fn re_angle_by_so_many_degrees_to_the_left() {
        let c = cue("re-angle this clip 15 degrees to the left", true);
        assert_eq!(c.workflow.as_deref(), Some("re-angle-video"));
        assert_eq!(p(&c), ["angle=15"]);
        // out of the 5 to 30 range is not turned into something else
        assert!(p(&cue("re-angle 90 degrees", true)).is_empty());
        assert_eq!(
            p(&cue("orbit left 12°", true)),
            ["camera=orbit_left", "angle=12"]
        );
    }

    #[test]
    fn a_workflow_named_in_plain_words() {
        assert_eq!(
            cue("relight the photo with candlelight", true)
                .workflow
                .as_deref(),
            Some("relight-photo")
        );
        assert_eq!(
            cue("swap the character in the clip", true)
                .workflow
                .as_deref(),
            Some("swap-character-video")
        );
        assert_eq!(
            cue("swap the character in this photo", true)
                .workflow
                .as_deref(),
            Some("swap-character-photo")
        );
        assert_eq!(
            cue("lip sync it", true).workflow.as_deref(),
            Some("lip-sync-video")
        );
        assert_eq!(
            cue("remove the person from the shot", true)
                .workflow
                .as_deref(),
            Some("clean-plate-video")
        );
        assert_eq!(
            cue("move the camera on this photo, push in", true)
                .workflow
                .as_deref(),
            Some("camera-photo")
        );
        assert_eq!(
            cue("add an effect", true).workflow.as_deref(),
            Some("effect-clip")
        );
    }

    #[test]
    fn a_seed_and_a_strength() {
        assert_eq!(p(&cue("same again with a new seed", true)), ["seed=-1"]);
        assert_eq!(p(&cue("seed 42", true)), ["seed=42"]);
        assert_eq!(
            p(&cue("push in, strength 20", true)),
            ["camera=push_in", "strength=20"]
        );
    }

    #[test]
    fn a_question_is_never_a_cue() {
        for q in [
            "what is golden hour?",
            "is the golden hour good for portraits?",
            "why is my orbit shaky?",
            "tell me a joke",
            "what time is it",
            "",
            "   ",
        ] {
            assert!(parse(q, true).is_none(), "{q:?}");
            assert!(parse(q, false).is_none(), "{q:?}");
        }
    }

    #[test]
    fn said_to_tempo_it_has_to_read_as_an_instruction() {
        // short or starting like an instruction: a cue
        assert!(parse("golden hour", false).is_some());
        assert!(parse("make it golden hour", false).is_some());
        assert!(parse("re-angle the last clip with a slow orbit", false).is_some());
        assert!(parse("and now blue hour", false).is_some());
        // a long ordinary sentence that mentions a light is Tempo's to answer
        assert!(parse(
            "my sister photographs weddings and she loves the golden hour so we talked about it all weekend",
            false
        )
        .is_none());
        assert!(parse("we should talk about the sunset on friday", false).is_none());
        assert!(parse("how do I get golden hour", false).is_none());
    }

    #[test]
    fn nothing_that_stage_does_not_know_is_invented() {
        assert!(parse("hello there", true).is_none());
        assert!(parse("make the sky green", true).is_none());
        let c = cue("go orbit, then crane up", true);
        // one camera Pedal, not two: the first move named wins
        assert_eq!(c.pedals.iter().filter(|(k, _)| k == "camera").count(), 1);
    }

    #[test]
    fn shouting_and_punctuation_do_not_matter() {
        assert_eq!(
            cue("RE-ANGLE THE CLIP, SLOW ORBIT!!!", true),
            cue("re angle the clip slow orbit", true)
        );
        assert_eq!(
            p(&cue("Push-in, 35mm.", true)),
            ["camera=push_in", "lens=35mm"]
        );
    }
}
