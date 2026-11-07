//! How long a shot will take, from how long the same workflow took before. Each finished take
//! is a line in `<Tempo home>/stage/runs/*.jsonl` with the machine and the seconds; the estimate
//! is their average for that workflow on that machine. With none, there is no number: Stage
//! says so rather than inventing one.

use crate::setup::json::parse;
use crate::Ctx;

/// (workflow id, machine, seconds) of every finished take in the run logs.
pub fn history(ctx: &Ctx) -> Vec<(String, String, f64)> {
    let dir = super::install::stage_dir(ctx).join("runs");
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(e.path()).unwrap_or_default();
        let mut id = String::new();
        for l in text.lines().filter_map(|l| parse(l).ok()) {
            if let Some(i) = l.str("id").filter(|_| l.str("run").is_some()) {
                id = i.to_string();
            } else if let (Some(m), Some(s)) = (l.str("machine"), l.f64("secs")) {
                if !id.is_empty() && s > 0.0 {
                    out.push((id.clone(), m.to_string(), s));
                }
            }
        }
    }
    out
}

/// The average seconds of `id` on `machine` (`this computer` for this one), if it has run there.
pub fn secs(hist: &[(String, String, f64)], id: &str, machine: &str) -> Option<f64> {
    let v: Vec<f64> = hist
        .iter()
        .filter(|(i, m, _)| i == id && m == machine)
        .map(|(_, _, s)| *s)
        .collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// "about 40 s", "about 3 min", or "no estimate yet (it has not run on this machine)".
pub fn words(s: Option<f64>) -> String {
    match s {
        None => "no estimate yet (it has not run there)".into(),
        Some(s) if s < 90.0 => format!("about {:.0} s", s.max(1.0)),
        Some(s) => format!("about {:.0} min", s / 60.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::detect::tests::{ctx_in, tmp};

    #[test]
    fn an_estimate_is_the_average_of_earlier_takes_on_that_machine_and_none_without() {
        let d = tmp("est");
        let ctx = ctx_in(&d.join("home"));
        let runs = super::super::install::stage_dir(&ctx).join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(
            runs.join("a.jsonl"),
            "{\"run\":\"a\",\"id\":\"re-angle-video\",\"seed\":1}\n{\"take\":1,\"machine\":\"studio\",\"secs\":40.0}\n{\"take\":2,\"machine\":\"studio\",\"secs\":60.0}\n{\"take\":3,\"machine\":\"this computer\",\"secs\":200.0}\nnot json\n",
        )
        .unwrap();
        let h = history(&ctx);
        assert_eq!(secs(&h, "re-angle-video", "studio"), Some(50.0));
        assert_eq!(secs(&h, "re-angle-video", "this computer"), Some(200.0));
        assert_eq!(secs(&h, "re-angle-video", "other"), None);
        assert_eq!(secs(&h, "relight-photo", "studio"), None);
        assert_eq!(words(Some(50.0)), "about 50 s");
        assert_eq!(words(Some(200.0)), "about 3 min");
        assert!(words(None).contains("no estimate"));
    }
}
