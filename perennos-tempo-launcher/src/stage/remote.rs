//! The other computer's side of a remote take: what this computer does when a Fleet machine asks
//! it, through the messenger, to play a Stage workflow. It plays it with `run::play` against its
//! own ComfyUI (on 127.0.0.1, never opened to the network), under its own GPU lease, with its
//! own pins and its own rules (a paid node is refused here whatever the other computer says).
//! The wire is in `fleet/stage.rs`.

use super::catalog::{embedded, Catalog};
use super::run::{play, RunOpts, World};
use crate::fleet::stage::{Play, PlayIn, Played, StageRunner};
use crate::Ctx;
use std::path::PathBuf;

/// This computer's own Stage, for `fleet serve`.
pub struct RealRunner;

impl StageRunner for RealRunner {
    fn ready(&self, workflow: &str) -> bool {
        super::ready_ids(&Ctx::from_env())
            .iter()
            .any(|w| w == workflow)
    }

    fn play(&self, p: &Play) -> Result<Played, String> {
        let ctx = Ctx::from_env();
        let world = super::world::RealWorld::new(&ctx);
        play_for_fleet(&ctx, &world, &embedded(), p)
    }
}

/// Play one take for another computer. The files it made, each with its proof beside it.
pub fn play_for_fleet(
    ctx: &Ctx,
    world: &dyn World,
    cat: &Catalog,
    p: &Play,
) -> Result<Played, String> {
    let opts = RunOpts {
        inputs: p
            .inputs
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    match v {
                        PlayIn::Text(t) => t.clone(),
                        PlayIn::File(f) => f.to_string_lossy().into_owned(),
                    },
                )
            })
            .collect(),
        pedals: p.pedals.clone(),
        takes: 1,
        on: Some("here".into()),
        seed: p.seed,
        quiet: true,
        label: Some(p.label.clone()),
        ..RunOpts::default()
    };
    let out = play(ctx, world, cat, &p.workflow, &opts)?;
    let take = out.takes.first().ok_or("no take was made")?;
    let mut files: Vec<(PathBuf, String)> = vec![];
    for f in &take.files {
        files.push((f.clone(), "output".into()));
        let side = f.with_file_name(format!(
            "{}.stage.json",
            f.file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into_owned())
        ));
        if side.is_file() {
            files.push((side, "sidecar".into()));
        }
    }
    Ok(Played {
        files,
        secs: take.secs,
        cached: take.cached,
        notes: out.notes,
    })
}
