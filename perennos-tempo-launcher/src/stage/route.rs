//! Which machine plays a workflow. A run goes to one machine whole (a model is never split
//! across two). The candidates are this computer's ComfyUI and the Fleet machines that say
//! they have ComfyUI up (and Stage's workflow ready), reached through the Fleet messenger and
//! never at their ComfyUI's own port; one with less graphics memory than the
//! workflow's lowest tier is not a candidate. Of those that fit, the best tier wins, then the
//! most free memory, then the shortest line, and this computer wins a tie (no network).

use super::catalog::{Tier, Workflow};
use crate::setup::json::J;

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub name: String,
    pub local: bool,
    /// Where this computer's own ComfyUI answers (127.0.0.1). Empty for a Fleet machine: its
    /// ComfyUI is never reached from here, the Fleet messenger carries the run there.
    pub base: String,
    pub gpu_gb: f64,
    pub free_gb: f64,
    /// Requests already running there.
    pub busy: f64,
}

/// This computer's ComfyUI, if it answers.
pub fn local(base: &str, gpu_gb: f64, free_gb: f64) -> Machine {
    Machine {
        name: "this computer".into(),
        local: true,
        base: base.into(),
        gpu_gb,
        free_gb,
        busy: 0.0,
    }
}

/// The Fleet machines that are up with ComfyUI, from `fleet.json` as the messenger tells it.
/// `ready` limits them to those that list this workflow as set up (when they say anything).
pub fn from_fleet(v: &J, id: &str) -> Vec<Machine> {
    v.arr("machines")
        .iter()
        .filter(|m| m.bool("up") == Some(true))
        .filter_map(|m| {
            let t = m.get("tools")?;
            let c = t.get("comfy").filter(|c| c.bool("up") == Some(true))?;
            // a machine that lists its Stage workflows must list this one
            if let Some(J::Arr(list)) = t.get("stage") {
                if !list.iter().any(|x| matches!(x, J::Str(s) if s == id)) {
                    return None;
                }
            }
            let name = m.str("name")?.to_string();
            let gpu = c.f64("gpu_gb").unwrap_or(0.0);
            Some(Machine {
                base: String::new(),
                gpu_gb: gpu,
                free_gb: c.f64("vram_free_gb").unwrap_or(gpu),
                busy: m
                    .get("health")
                    .and_then(|h| h.f64("in_flight"))
                    .unwrap_or(0.0),
                local: false,
                name,
            })
        })
        .collect()
}

/// Every machine with the tier it would play, or why it cannot.
pub fn rows<'a>(
    boxes: &'a [Machine],
    wf: &'a Workflow,
) -> Vec<(&'a Machine, Result<&'a Tier, String>)> {
    boxes
        .iter()
        .map(|m| {
            (
                m,
                wf.tier_for(m.gpu_gb).ok_or_else(|| {
                    format!(
                        "{:.0} GB of graphics memory; this needs {:.0} GB",
                        m.gpu_gb, wf.min_vram_gb
                    )
                }),
            )
        })
        .collect()
}

/// The machines that fit, best first.
pub fn ranked<'a>(boxes: &'a [Machine], wf: &'a Workflow) -> Vec<(&'a Machine, &'a Tier)> {
    let mut fit: Vec<(&Machine, &Tier)> = rows(boxes, wf)
        .into_iter()
        .filter_map(|(m, t)| t.ok().map(|t| (m, t)))
        .collect();
    fit.sort_by(|a, b| {
        b.1.min_vram_gb
            .total_cmp(&a.1.min_vram_gb)
            .then(b.0.free_gb.total_cmp(&a.0.free_gb))
            .then(a.0.busy.total_cmp(&b.0.busy))
            .then(b.0.local.cmp(&a.0.local))
            .then(a.0.name.cmp(&b.0.name))
    });
    fit
}

pub fn pick<'a>(boxes: &'a [Machine], wf: &'a Workflow) -> Result<(&'a Machine, &'a Tier), String> {
    match ranked(boxes, wf).first() {
        Some(x) => Ok(*x),
        None => {
            let why: Vec<String> = rows(boxes, wf)
                .into_iter()
                .filter_map(|(m, t)| t.err().map(|e| format!("{} has {e}", m.name)))
                .collect();
            Err(if boxes.is_empty() {
                format!(
                    "No ComfyUI is answering on this computer or in your Fleet. {} needs one with {:.0} GB of graphics memory. Start ComfyUI (`tempo-launch crate show comfyui`), or on another computer allow it: `tempo-launch fleet allow stage`.",
                    wf.title, wf.min_vram_gb
                )
            } else {
                format!("{} will not fit anywhere: {}.", wf.title, why.join("; "))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::json::parse;
    use crate::stage::catalog::embedded;

    fn m(name: &str, local: bool, gpu: f64, free: f64, busy: f64) -> Machine {
        Machine {
            name: name.into(),
            local,
            base: "http://10.0.0.2:8188".into(),
            gpu_gb: gpu,
            free_gb: free,
            busy,
        }
    }

    #[test]
    fn a_machine_without_enough_memory_is_not_a_candidate() {
        let c = embedded();
        let wf = c.find("re-angle-video").unwrap();
        let boxes = [
            m("laptop", true, 8.0, 7.0, 0.0),
            m("studio", false, 16.0, 15.0, 0.0),
        ];
        let (box_, tier) = pick(&boxes, wf).unwrap();
        assert_eq!(box_.name, "studio");
        assert_eq!(tier.min_vram_gb, 12.0);
        let rows = rows(&boxes, wf);
        assert!(
            rows[0].1.as_ref().unwrap_err().contains("8 GB")
                && rows[0].1.as_ref().unwrap_err().contains("12 GB")
        );
    }

    #[test]
    fn the_roomiest_tier_wins_then_free_memory_then_the_shortest_line_then_this_computer() {
        let c = embedded();
        let wf = c.find("re-angle-video").unwrap();
        let boxes = [
            m("a-16", false, 16.0, 16.0, 0.0),
            m("b-24", false, 24.0, 10.0, 0.0),
            m("c-24-roomy", false, 24.0, 22.0, 3.0),
            m("d-24-idle", false, 24.0, 22.0, 0.0),
        ];
        assert_eq!(pick(&boxes, wf).unwrap().0.name, "d-24-idle");
        assert_eq!(pick(&boxes, wf).unwrap().1.min_vram_gb, 20.0);
        let tie = [
            m("far", false, 16.0, 15.0, 0.0),
            m("here", true, 16.0, 15.0, 0.0),
        ];
        assert_eq!(pick(&tie, wf).unwrap().0.name, "here");
    }

    #[test]
    fn nothing_that_fits_says_what_each_machine_has() {
        let c = embedded();
        let wf = c.find("lip-sync-video").unwrap();
        let e = pick(
            &[
                m("laptop", true, 8.0, 8.0, 0.0),
                m("studio", false, 16.0, 16.0, 0.0),
            ],
            wf,
        )
        .unwrap_err();
        assert!(
            e.contains("laptop has 8 GB") && e.contains("studio has 16 GB") && e.contains("24 GB"),
            "{e}"
        );
        let e = pick(&[], wf).unwrap_err();
        assert!(
            e.contains("No ComfyUI is answering") && e.contains("fleet allow stage"),
            "{e}"
        );
    }

    #[test]
    fn the_fleet_view_gives_machines_with_comfy_up_that_have_the_workflow() {
        let v = parse(
            r#"{"machines":[
              {"name":"studio","addr":"10.0.0.7","up":true,"tools":{"comfy":{"up":true,"gpu_gb":24.0},"stage":["re-angle-video"]},"health":{"in_flight":1}},
              {"name":"other","addr":"10.0.0.8","up":true,"tools":{"comfy":{"up":true,"gpu_gb":48.0},"stage":["relight-photo"]}},
              {"name":"old","addr":"10.0.0.9","up":true,"tools":{"comfy":{"up":true,"gpu_gb":24.0}}},
              {"name":"asleep","addr":"10.0.0.10","up":false,"tools":{"comfy":{"up":true,"gpu_gb":24.0}}},
              {"name":"nocomfy","addr":"10.0.0.11","up":true,"tools":{}}]}"#,
        )
        .unwrap();
        let got = from_fleet(&v, "re-angle-video");
        let names: Vec<&str> = got.iter().map(|m| m.name.as_str()).collect();
        // `old` lists no workflows at all, so it is not ruled out; `other` lists some and not this
        assert_eq!(names, ["studio", "old"]);
        // no ComfyUI address for another computer: the Fleet messenger is the only way there
        assert!(got.iter().all(|m| m.base.is_empty() && !m.local));
        assert_eq!(got[0].busy, 1.0);
    }
}
