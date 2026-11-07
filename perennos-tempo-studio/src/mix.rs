//! Plan B: mixdown without REAPER.
//!
//! Stems (same sample rate) are gain-staged to a common reference loudness plus a small offset
//! from a name-based role hint, kept centred (mono stems go to both sides, nothing is panned),
//! high-passed where the role says so, summed, and trimmed so the mix peaks below -6 dBFS before
//! mastering. Writes the mix WAV plus a JSON plan; `reaper script` turns that plan into a REAPER
//! session.
use crate::loudness::{self, Biquad, Loudness, Meter};
use crate::master::{self, MasterSettings};
use crate::util::{db_to_amp, jnum, r2, resolve, same_path, OutFile};
use crate::wav::{OutFormat, WavReader, WavWriter};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_REFERENCE_LUFS: f64 = -23.0;
/// The mix is trimmed so its true peak sits here (below -6 dBFS, with margin).
pub const HEADROOM_DBTP: f64 = -6.5;
const MAX_STEM_GAIN_DB: f64 = 30.0;

/// Name-based role hint. It only sets a gain offset, a high-pass and (in REAPER) a gentle
/// compressor; nothing is panned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Kick,
    Drums,
    Bass,
    Vocal,
    Guitar,
    Keys,
    Pad,
    Lead,
    Fx,
    Other,
}

impl Role {
    pub fn from_name(name: &str) -> Role {
        let n: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect();
        let padded = format!(" {n} ");
        let has = |subs: &[&str]| subs.iter().any(|s| n.contains(s));
        let word = |ws: &[&str]| ws.iter().any(|w| padded.contains(&format!(" {w} ")));
        if has(&["kick", "bassdrum", "bass drum"]) || word(&["bd"]) {
            Role::Kick
        } else if has(&[
            "drum", "snare", "hihat", "hi hat", "perc", "cymbal", "overhead", "clap",
        ]) || word(&["hat", "hats", "tom", "toms", "oh"])
        {
            Role::Drums
        } else if has(&["bass"]) || word(&["sub"]) {
            Role::Bass
        } else if has(&["vox", "vocal", "voice", "choir"]) || word(&["voc", "bv", "bvs"]) {
            Role::Vocal
        } else if has(&["gtr", "guitar"]) || word(&["git"]) {
            Role::Guitar
        } else if has(&["keys", "piano", "pno", "rhodes", "organ"]) || word(&["key"]) {
            Role::Keys
        } else if has(&["pad", "string", "atmo"]) {
            Role::Pad
        } else if has(&["lead", "melody", "solo"]) {
            Role::Lead
        } else if has(&["fx", "effect", "riser", "impact"]) {
            Role::Fx
        } else {
            Role::Other
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Kick => "kick",
            Role::Drums => "drums",
            Role::Bass => "bass",
            Role::Vocal => "vocal",
            Role::Guitar => "guitar",
            Role::Keys => "keys",
            Role::Pad => "pad",
            Role::Lead => "lead",
            Role::Fx => "fx",
            Role::Other => "other",
        }
    }

    /// Level relative to the reference loudness, dB.
    pub fn offset_db(self) -> f64 {
        match self {
            Role::Kick => 1.0,
            Role::Drums => 0.0,
            Role::Bass => 0.0,
            Role::Vocal => 2.0,
            Role::Guitar => -3.0,
            Role::Keys => -3.0,
            Role::Pad => -6.0,
            Role::Lead => -1.0,
            Role::Fx => -8.0,
            Role::Other => -3.0,
        }
    }

    pub fn highpass_hz(self) -> Option<f64> {
        match self {
            Role::Vocal => Some(90.0),
            Role::Guitar => Some(80.0),
            Role::Keys => Some(60.0),
            Role::Pad => Some(120.0),
            Role::Lead => Some(100.0),
            Role::Fx => Some(150.0),
            Role::Kick | Role::Drums | Role::Bass | Role::Other => None,
        }
    }

    /// A gentle compressor in the REAPER session (the built-in engine does not compress).
    pub fn compress(self) -> bool {
        matches!(self, Role::Vocal | Role::Drums | Role::Kick)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stem {
    pub path: PathBuf,
    pub name: String,
    pub role: Role,
    pub channels: u16,
    pub frames: u64,
    /// Integrated loudness as heard in the stereo mix (after the high-pass, before gain).
    pub lufs: Option<f64>,
    pub peak_dbfs: Option<f64>,
    pub gain_db: f64,
    pub highpass_hz: Option<f64>,
    pub compress: bool,
}

impl Stem {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path.display().to_string(),
            "name": self.name,
            "role": self.role.name(),
            "channels": self.channels,
            "frames": self.frames,
            "lufs": jnum(self.lufs),
            "peak_dbfs": jnum(self.peak_dbfs),
            "gain_db": r2(self.gain_db),
            "highpass_hz": self.highpass_hz,
            "compress": self.compress,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MixPlan {
    pub sample_rate: u32,
    pub frames: u64,
    pub reference_lufs: f64,
    pub stems: Vec<Stem>,
    /// Applied to the whole mix for headroom (0 or negative).
    pub trim_db: f64,
    pub notes: Vec<String>,
}

fn is_wav_name(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("wav") || e.eq_ignore_ascii_case("wave"))
}

/// `<out stem>.wav` or `<out stem>-N.wav`: earlier runs of the same output.
fn is_earlier_output(p: &Path, out: Option<&Path>) -> bool {
    let Some(out) = out else { return false };
    if same_path(p, out) {
        return true;
    }
    let (Some(os), Some(ps)) = (out.file_stem(), p.file_stem()) else {
        return false;
    };
    let (os, ps) = (os.to_string_lossy(), ps.to_string_lossy());
    let same_dir = match (p.parent(), out.parent()) {
        (Some(a), Some(b)) => same_path(a, b),
        _ => false,
    };
    same_dir
        && ps
            .strip_prefix(&*os)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Folders expand to their `.wav` files (sorted by name, not recursive); files are kept as given.
/// Earlier outputs of `out` found in a folder are skipped.
pub fn collect_stems(args: &[PathBuf], out: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    let mut stems = Vec::new();
    for a in args {
        if a.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(a)
                .map_err(|e| format!("{}: {e}", a.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && is_wav_name(p))
                .filter(|p| {
                    !p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                })
                .filter(|p| !is_earlier_output(p, out))
                .collect();
            found.sort();
            if found.is_empty() {
                return Err(format!("no .wav files in {}", a.display()));
            }
            stems.extend(found);
        } else if a.is_file() {
            stems.push(a.clone());
        } else {
            return Err(format!("{} does not exist", a.display()));
        }
    }
    if stems.is_empty() {
        return Err("no stems given".into());
    }
    Ok(stems)
}

/// One stem read as centred stereo, high-passed, with gain.
struct StemStream {
    r: WavReader<BufReader<File>>,
    ch: usize,
    hp: Option<(Biquad, [[f64; 2]; 2])>,
    gain: f64,
    buf: Vec<f64>,
    done: bool,
}

impl StemStream {
    fn open(path: &Path, highpass_hz: Option<f64>, gain_db: f64) -> Result<StemStream, String> {
        let r = WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let fs = r.info().sample_rate as f64;
        Ok(StemStream {
            ch: r.info().channels as usize,
            r,
            hp: highpass_hz.map(|f| (Biquad::highpass(fs, f), [[0.0; 2]; 2])),
            gain: db_to_amp(gain_db),
            buf: Vec::new(),
            done: false,
        })
    }

    /// Add up to `frames` stereo frames into `acc` (interleaved L R). Returns frames added.
    fn add_into(&mut self, acc: &mut [f64], frames: usize, path: &Path) -> Result<usize, String> {
        if self.done {
            return Ok(0);
        }
        let n = self
            .r
            .read_frames(&mut self.buf, frames)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            self.done = true;
            return Ok(0);
        }
        for i in 0..n {
            let (mut l, mut r) = if self.ch == 1 {
                let x = self.buf[i];
                (x, x)
            } else {
                (self.buf[i * self.ch], self.buf[i * self.ch + 1])
            };
            if let Some((bq, st)) = self.hp.as_mut() {
                let [sl, sr] = st;
                l = bq.run(sl, l);
                r = bq.run(sr, r);
            }
            acc[2 * i] += l * self.gain;
            acc[2 * i + 1] += r * self.gain;
        }
        Ok(n)
    }
}

/// Measure one stem as it will sound in the mix (stereo, high-passed, unity gain).
fn measure_stem(
    path: &Path,
    highpass_hz: Option<f64>,
    sample_rate: u32,
) -> Result<Loudness, String> {
    let mut s = StemStream::open(path, highpass_hz, 0.0)?;
    let mut m = Meter::new(sample_rate, vec![1.0, 1.0]);
    let mut acc = vec![0.0; 2 * 8192];
    loop {
        acc.iter_mut().for_each(|x| *x = 0.0);
        let n = s.add_into(&mut acc, 8192, path)?;
        if n == 0 {
            break;
        }
        m.add(&acc[..2 * n]);
    }
    Ok(m.result())
}

/// Read every stem's header, check they can be mixed, measure them and set the gains.
pub fn plan_mix(files: &[PathBuf], reference_lufs: f64) -> Result<MixPlan, String> {
    plan_stems(files, reference_lufs, true)
}

/// Plan stems that only REAPER can mix (different sample rates, more than 2 channels): each stem
/// is measured on its own; nothing is summed here.
pub fn plan_for_reaper(files: &[PathBuf], reference_lufs: f64) -> Result<MixPlan, String> {
    let mut plan = plan_stems(files, reference_lufs, false)?;
    let (trim, _) = estimate_sum(&plan);
    plan.trim_db = trim;
    plan.notes.push(format!(
        "no built-in mix (REAPER resamples and mixes these stems); trim {trim:.2} dB is a worst-case estimate from the stem peaks"
    ));
    Ok(plan)
}

/// Without summing: (headroom trim from the worst-case coherent peak, mix loudness from the
/// energy sum of the staged stems after that trim).
pub fn estimate_sum(plan: &MixPlan) -> (f64, Option<f64>) {
    let peak: f64 = plan
        .stems
        .iter()
        .filter_map(|s| s.peak_dbfs.map(|p| db_to_amp(p + s.gain_db)))
        .sum();
    let peak_db = if peak > 0.0 {
        20.0 * peak.log10()
    } else {
        f64::NEG_INFINITY
    };
    let trim = if peak_db > HEADROOM_DBTP {
        HEADROOM_DBTP - peak_db
    } else {
        0.0
    };
    let energy: f64 = plan
        .stems
        .iter()
        .filter_map(|s| s.lufs.map(|l| 10f64.powf((l + s.gain_db + trim) / 10.0)))
        .sum();
    (trim, (energy > 0.0).then(|| 10.0 * energy.log10()))
}

fn plan_stems(files: &[PathBuf], reference_lufs: f64, strict: bool) -> Result<MixPlan, String> {
    if files.is_empty() {
        return Err("no stems given".into());
    }
    let mut heads = Vec::new();
    for f in files {
        let r = WavReader::open(f).map_err(|e| format!("{}: {e}", f.display()))?;
        heads.push((f.clone(), r.info().clone()));
    }
    let sr = heads
        .iter()
        .map(|(_, i)| i.sample_rate)
        .max()
        .unwrap_or(48000);
    if strict && heads.iter().any(|(_, i)| i.sample_rate != sr) {
        let list: Vec<String> = heads
            .iter()
            .map(|(p, i)| format!("{} {} Hz", name_of(p), i.sample_rate))
            .collect();
        return Err(format!(
            "stems have different sample rates ({}). The built-in mixer does not resample: convert them first, \
             or use --engine reaper (REAPER resamples on import)",
            list.join(", ")
        ));
    }
    if let Some((p, i)) = heads.iter().find(|(_, i)| strict && i.channels > 2) {
        return Err(format!(
            "{} has {} channels; the built-in mixer takes mono and stereo stems. Use --engine reaper for multichannel stems",
            p.display(),
            i.channels
        ));
    }
    let mut notes = Vec::new();
    let mut stems = Vec::new();
    for (p, info) in &heads {
        let name = name_of(p);
        let role = Role::from_name(&name);
        let hp = role.highpass_hz();
        let l = if info.channels <= 2 {
            measure_stem(p, hp, info.sample_rate)?
        } else {
            loudness::analyze_file(p)?.loudness
        };
        let gain_db = match l.integrated {
            Some(lufs) => {
                let want = reference_lufs + role.offset_db() - lufs;
                let g = want.clamp(-MAX_STEM_GAIN_DB, MAX_STEM_GAIN_DB);
                if g != want {
                    notes.push(format!(
                        "{name}: needs {want:+.1} dB to reach the reference; held at {g:+.0} dB"
                    ));
                }
                g
            }
            None => {
                notes.push(format!(
                    "{name}: silent (nothing above -70 LUFS); left at 0 dB"
                ));
                0.0
            }
        };
        stems.push(Stem {
            path: p.clone(),
            name,
            role,
            channels: info.channels,
            frames: info.frames,
            lufs: l.integrated,
            peak_dbfs: l.sample_peak,
            gain_db,
            highpass_hz: hp,
            compress: role.compress(),
        });
    }
    let frames = stems.iter().map(|s| s.frames).max().unwrap_or(0);
    if stems.iter().any(|s| s.frames != frames) {
        notes.push("stems differ in length; shorter ones end in silence".into());
    }
    Ok(MixPlan {
        sample_rate: sr,
        frames,
        reference_lufs,
        stems,
        trim_db: 0.0,
        notes,
    })
}

fn name_of(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "stem".into())
}

/// Sum all stems with their gains and `trim_db`, handing stereo blocks to `sink`.
fn sum_pass(
    plan: &MixPlan,
    trim_db: f64,
    sink: &mut dyn FnMut(&[f64]) -> Result<(), String>,
) -> Result<(), String> {
    let trim = db_to_amp(trim_db);
    let mut streams = plan
        .stems
        .iter()
        .map(|s| StemStream::open(&s.path, s.highpass_hz, s.gain_db).map(|st| (st, s.path.clone())))
        .collect::<Result<Vec<_>, _>>()?;
    const B: usize = 8192;
    let mut acc = vec![0.0; 2 * B];
    loop {
        acc.iter_mut().for_each(|x| *x = 0.0);
        let mut n = 0;
        for (st, p) in streams.iter_mut() {
            n = n.max(st.add_into(&mut acc, B, p)?);
        }
        if n == 0 {
            return Ok(());
        }
        if trim != 1.0 {
            acc[..2 * n].iter_mut().for_each(|x| *x *= trim);
        }
        sink(&acc[..2 * n])?;
    }
}

/// Loudness of the summed stems at `trim_db`, without writing anything.
pub fn measure_mix(plan: &MixPlan, trim_db: f64) -> Result<Loudness, String> {
    let mut m = Meter::new(plan.sample_rate, vec![1.0, 1.0]);
    sum_pass(plan, trim_db, &mut |b| {
        m.add(b);
        Ok(())
    })?;
    Ok(m.result())
}

/// What happens to the mix after it is written.
#[derive(Clone, Copy, Debug)]
pub enum MasterStep<'a> {
    None,
    /// Master it with the built-in engine (writes `<mix>-master.wav`).
    Write(&'a MasterSettings),
    /// Only measure what the built-in master would do (the pre-limiter gain goes into the plan
    /// for a REAPER session); nothing is written.
    Estimate(&'a MasterSettings),
}

impl MasterStep<'_> {
    fn settings(&self) -> Option<&MasterSettings> {
        match self {
            MasterStep::None => None,
            MasterStep::Write(s) | MasterStep::Estimate(s) => Some(s),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MixReport {
    pub plan: MixPlan,
    pub mix: loudness::Analysis,
    pub mix_path: PathBuf,
    pub plan_path: PathBuf,
    pub master_settings: Option<MasterSettings>,
    pub master: Option<master::MasterReport>,
    pub master_estimate: Option<master::MasterPlan>,
}

impl MixReport {
    /// The JSON plan written next to the mix (also what `reaper script` reads).
    pub fn plan_json(&self) -> Value {
        let master_settings = self.master_settings.as_ref();
        let l = &self.mix.loudness;
        json!({
            "tool": "perennos-tempo-studio",
            "kind": "mix_plan",
            "version": 1,
            "engine": "builtin",
            "sample_rate": self.plan.sample_rate,
            "frames": self.plan.frames,
            "reference_lufs": self.plan.reference_lufs,
            "headroom_dbtp": HEADROOM_DBTP,
            "trim_db": r2(self.plan.trim_db),
            "stems": self.plan.stems.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
            "mix": {
                "path": self.mix_path.display().to_string(),
                "integrated_lufs": jnum(l.integrated),
                "true_peak_dbtp": jnum(l.true_peak),
                "sample_peak_dbfs": jnum(l.sample_peak),
            },
            "master": master_settings.map(|s| {
                let mut m = s.to_json();
                if let Some(r) = &self.master {
                    m["path"] = json!(r.path.display().to_string());
                    m["gain_db"] = json!(r2(r.passes[r.chosen].gain_db));
                    m["integrated_lufs"] = jnum(r.output.loudness.integrated);
                    m["true_peak_dbtp"] = jnum(r.output.loudness.true_peak);
                } else if let Some(e) = &self.master_estimate {
                    m["path"] = Value::Null;
                    m["gain_db"] = json!(r2(e.best.gain_db));
                    m["estimated_lufs"] = jnum(e.best.result.integrated);
                }
                m
            }),
            "notes": self.plan.notes,
        })
    }

    pub fn to_json(&self) -> Value {
        let mut j = self.plan_json();
        j["kind"] = json!("mix_report");
        j["plan_path"] = json!(self.plan_path.display().to_string());
        j["mix"] = self.mix.to_json();
        if let Some(m) = &self.master {
            j["master_report"] = m.to_json();
        }
        j
    }
}

/// The plan JSON for stems that were not mixed here (REAPER mixes them). Written next to `out`.
pub fn write_plan_only(
    plan: &MixPlan,
    out: &Path,
    master: Option<&MasterSettings>,
) -> Result<(PathBuf, Value), String> {
    let (_, est_lufs) = estimate_sum(plan);
    let j = json!({
        "tool": "perennos-tempo-studio",
        "kind": "mix_plan",
        "version": 1,
        "engine": "reaper",
        "sample_rate": plan.sample_rate,
        "frames": plan.frames,
        "reference_lufs": plan.reference_lufs,
        "headroom_dbtp": HEADROOM_DBTP,
        "trim_db": r2(plan.trim_db),
        "stems": plan.stems.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
        "mix": {"path": Value::Null, "integrated_lufs": jnum(est_lufs), "estimated": true},
        "master": master.map(|s| {
            let mut m = s.to_json();
            m["path"] = Value::Null;
            if let Some(l) = est_lufs {
                m["gain_db"] = json!(r2(s.target_lufs - l));
            }
            m
        }),
        "notes": plan.notes,
    });
    let (pout, mut pf) = OutFile::create(&plan_path_for(out), false, &[])?;
    let text = serde_json::to_string_pretty(&j).map_err(|e| e.to_string())?;
    pf.write_all(text.as_bytes())
        .and_then(|_| pf.sync_all())
        .map_err(|e| format!("{e} {}", pout.left_note()))?;
    drop(pf);
    Ok((pout.commit()?, j))
}

/// Default plan path: `<mix stem>.plan.json` next to the mix.
pub fn plan_path_for(mix: &Path) -> PathBuf {
    let stem = mix
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mix".into());
    mix.with_file_name(format!("{stem}.plan.json"))
}

/// Mix the planned stems to `want` (never overwriting; `force` replaces), then the master step,
/// then write the JSON plan next to the mix.
pub fn render_mix(
    mut plan: MixPlan,
    want: &Path,
    force: bool,
    format: OutFormat,
    step: MasterStep,
) -> Result<MixReport, String> {
    let inputs: Vec<PathBuf> = plan.stems.iter().map(|s| resolve(&s.path)).collect();
    // Pass 1: how loud and how peaky is the plain sum?
    let raw = measure_mix(&plan, 0.0)?;
    let tp = raw.true_peak.unwrap_or(f64::NEG_INFINITY);
    plan.trim_db = if tp > HEADROOM_DBTP {
        HEADROOM_DBTP - tp
    } else {
        0.0
    };
    if plan.trim_db < 0.0 {
        plan.notes.push(format!(
            "the sum peaked at {tp:.2} dBTP; trimmed {:.2} dB so the mix peaks at {HEADROOM_DBTP} dBTP before mastering",
            plan.trim_db
        ));
    }
    // Pass 2: write.
    let (out, file) = OutFile::create(want, force, &inputs)?;
    let written = (|| -> Result<(), String> {
        let mut w = WavWriter::new(file, plan.sample_rate, 2, format).map_err(|e| e.to_string())?;
        sum_pass(&plan, plan.trim_db, &mut |b| {
            w.write(b).map_err(|e| e.to_string())
        })?;
        w.finish().map_err(|e| e.to_string())?;
        Ok(())
    })();
    written.map_err(|e| format!("{e} {}", out.left_note()))?;
    let mix_path = out.commit()?;
    let mix = loudness::analyze_file(&mix_path)?;
    if let Some(sp) = mix.loudness.sample_peak {
        if sp >= -6.0 {
            plan.notes.push(format!(
                "warning: the written mix peaks at {sp:.2} dBFS, not below -6"
            ));
        }
    }
    let (master, master_estimate) = match step {
        MasterStep::None => (None, None),
        MasterStep::Write(s) => (
            Some(master::master_file(
                &mix_path,
                &master::default_out(&mix_path),
                force,
                s,
            )?),
            None,
        ),
        MasterStep::Estimate(s) => (None, Some(master::plan_master(&mix_path, s)?)),
    };
    let mut report = MixReport {
        plan,
        mix,
        mix_path: mix_path.clone(),
        plan_path: PathBuf::new(),
        master_settings: step.settings().cloned(),
        master,
        master_estimate,
    };
    // The plan, next to the mix, never overwriting.
    let (pout, mut pf) = OutFile::create(&plan_path_for(&mix_path), false, &[])?;
    let text = serde_json::to_string_pretty(&report.plan_json()).map_err(|e| e.to_string())?;
    pf.write_all(text.as_bytes())
        .and_then(|_| pf.sync_all())
        .map_err(|e| format!("{e} {}", pout.left_note()))?;
    drop(pf);
    report.plan_path = pout.commit()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loudness::testsig::{db, sine, Noise};
    use crate::util::testutil::tmpdir;
    use crate::wav::{read_all, write_new};

    #[test]
    fn roles_from_names() {
        let cases = [
            ("Kick In", Role::Kick),
            ("01_bass drum", Role::Kick),
            ("BD", Role::Kick),
            ("drums_overhead", Role::Drums),
            ("Snare Top", Role::Drums),
            ("hats", Role::Drums),
            ("Bassline", Role::Bass),
            ("sub", Role::Bass),
            ("Lead Vox", Role::Vocal),
            ("backing vocals", Role::Vocal),
            ("BV", Role::Vocal),
            ("Gtr L", Role::Guitar),
            ("lead guitar", Role::Guitar),
            ("Keys", Role::Keys),
            ("piano", Role::Keys),
            ("Warm Pad", Role::Pad),
            ("synth lead", Role::Lead),
            ("riser fx", Role::Fx),
            ("SFX", Role::Fx),
            ("thing", Role::Other),
        ];
        for (n, want) in cases {
            assert_eq!(Role::from_name(n), want, "{n}");
        }
        assert_eq!(Role::Vocal.highpass_hz(), Some(90.0));
        assert_eq!(Role::Kick.highpass_hz(), None);
        assert!(Role::Vocal.compress() && Role::Drums.compress() && !Role::Pad.compress());
    }

    fn stem(
        dir: &Path,
        name: &str,
        fs: u32,
        ch: usize,
        freq: f64,
        peak_db: f64,
        secs: f64,
    ) -> PathBuf {
        let p = dir.join(name);
        let mut s = sine(fs, ch, secs, freq, db(peak_db), 0.0);
        let mut n = Noise(7 + freq as u64);
        for x in s.iter_mut() {
            *x += db(peak_db - 30.0) * n.sample();
        }
        write_new(&p, fs, ch as u16, OutFormat::Pcm24, &s).unwrap();
        p
    }

    #[test]
    fn three_stems_are_staged_summed_with_headroom_and_planned() {
        let d = tmpdir("mix");
        let stems = d.join("stems");
        std::fs::create_dir_all(&stems).unwrap();
        stem(&stems, "kick.wav", 48000, 1, 60.0, -3.0, 6.0);
        stem(&stems, "bass.wav", 48000, 2, 110.0, -20.0, 6.0);
        stem(&stems, "lead vox.wav", 48000, 1, 440.0, -1.0, 4.0);
        std::fs::write(stems.join("notes.txt"), "not audio").unwrap();
        let files = collect_stems(std::slice::from_ref(&stems), None).unwrap();
        assert_eq!(files.len(), 3);
        let plan = plan_mix(&files, DEFAULT_REFERENCE_LUFS).unwrap();
        // Each stem is gain-staged to the reference plus its role offset.
        for s in &plan.stems {
            let staged = s.lufs.unwrap() + s.gain_db;
            let want = DEFAULT_REFERENCE_LUFS + s.role.offset_db();
            assert!(
                (staged - want).abs() < 0.01,
                "{}: {staged} vs {want}",
                s.name
            );
        }
        let vox = plan.stems.iter().find(|s| s.role == Role::Vocal).unwrap();
        assert_eq!(vox.highpass_hz, Some(90.0));
        assert!(plan.notes.iter().any(|n| n.contains("length")));
        let out = d.join("mix.wav");
        let rep = render_mix(plan, &out, false, OutFormat::Pcm24, MasterStep::None).unwrap();
        assert_eq!(rep.mix_path, out);
        assert_eq!(rep.mix.info.channels, 2);
        assert_eq!(rep.mix.info.frames, 6 * 48000);
        let sp = rep.mix.loudness.sample_peak.unwrap();
        assert!(sp < -6.0, "mix peaks at {sp}");
        assert!(rep.mix.loudness.true_peak.unwrap() <= HEADROOM_DBTP + 0.01);
        // The plan is on disk next to the mix and describes every stem.
        assert_eq!(rep.plan_path, d.join("mix.plan.json"));
        let plan_json: Value =
            serde_json::from_str(&std::fs::read_to_string(&rep.plan_path).unwrap()).unwrap();
        assert_eq!(plan_json["kind"], "mix_plan");
        assert_eq!(plan_json["stems"].as_array().unwrap().len(), 3);
        assert_eq!(plan_json["master"], Value::Null);
        // Centred: a mono stem lands equally in both channels (no panning).
        let (_, s) = read_all(&rep.mix_path).unwrap();
        let (l, r): (f64, f64) = s
            .chunks(2)
            .fold((0.0, 0.0), |(a, b), f| (a + f[0].abs(), b + f[1].abs()));
        assert!((l - r).abs() / l < 0.01, "L {l} R {r}");
        // A second mix of the same folder skips the earlier output and never overwrites it.
        let again =
            collect_stems(std::slice::from_ref(&stems), Some(&stems.join("mix.wav"))).unwrap();
        assert_eq!(again.len(), 3);
    }

    #[test]
    fn mix_with_master_preset_masters_the_mix_too() {
        let d = tmpdir("mixm");
        let a = stem(&d, "drums.wav", 44100, 2, 100.0, -6.0, 5.0);
        let b = stem(&d, "keys.wav", 44100, 2, 523.0, -12.0, 5.0);
        let plan = plan_mix(&[a, b], DEFAULT_REFERENCE_LUFS).unwrap();
        let s = MasterSettings::from_preset(&master::preset("streaming").unwrap());
        let rep = render_mix(
            plan.clone(),
            &d.join("song.wav"),
            false,
            OutFormat::Pcm24,
            MasterStep::Write(&s),
        )
        .unwrap();
        let m = rep.master.as_ref().unwrap();
        assert!(m.within_target, "{:#}", m.to_json());
        assert_eq!(m.path, d.join("song-master.wav"));
        let pj = rep.plan_json();
        assert_eq!(pj["master"]["preset"], "streaming");
        assert_eq!(pj["master"]["path"], m.path.display().to_string());
        assert_eq!(rep.to_json()["kind"], "mix_report");
        // Estimate only (for a REAPER session): the gain is in the plan, no master file is written.
        let rep = render_mix(
            plan,
            &d.join("est.wav"),
            false,
            OutFormat::Pcm24,
            MasterStep::Estimate(&s),
        )
        .unwrap();
        assert!(rep.master.is_none());
        let pj: Value =
            serde_json::from_str(&std::fs::read_to_string(&rep.plan_path).unwrap()).unwrap();
        assert_eq!(pj["master"]["path"], Value::Null);
        assert!(pj["master"]["gain_db"].as_f64().unwrap() > 0.0);
        assert!(!d.join("est-master.wav").exists());
    }

    #[test]
    fn different_rates_and_multichannel_stems_point_to_reaper() {
        let d = tmpdir("mixbad");
        let a = stem(&d, "a.wav", 44100, 1, 200.0, -10.0, 1.0);
        let b = stem(&d, "b.wav", 48000, 1, 200.0, -10.0, 1.0);
        let e = plan_mix(&[a.clone(), b], DEFAULT_REFERENCE_LUFS).unwrap_err();
        assert!(
            e.contains("different sample rates") && e.contains("REAPER"),
            "{e}"
        );
        let c = d.join("surround.wav");
        write_new(&c, 44100, 6, OutFormat::Pcm24, &vec![0.1; 6 * 4410]).unwrap();
        let e = plan_mix(&[a, c], DEFAULT_REFERENCE_LUFS).unwrap_err();
        assert!(e.contains("6 channels") && e.contains("reaper"), "{e}");
        assert!(collect_stems(&[d.join("missing")], None)
            .unwrap_err()
            .contains("does not exist"));
        let empty = d.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(collect_stems(&[empty], None)
            .unwrap_err()
            .contains("no .wav"));
    }

    #[test]
    fn stems_only_reaper_can_mix_still_get_a_plan() {
        let d = tmpdir("mixreaper");
        let a = stem(&d, "vox.wav", 44100, 1, 300.0, -10.0, 2.0);
        let b = stem(&d, "bass.wav", 48000, 2, 80.0, -6.0, 2.0);
        let c = d.join("surround.wav");
        write_new(
            &c,
            48000,
            6,
            OutFormat::Pcm24,
            &sine(48000, 6, 2.0, 500.0, 0.2, 0.0),
        )
        .unwrap();
        let files = vec![a, b, c];
        assert!(plan_mix(&files, DEFAULT_REFERENCE_LUFS).is_err());
        let plan = plan_for_reaper(&files, DEFAULT_REFERENCE_LUFS).unwrap();
        assert_eq!(plan.sample_rate, 48000);
        for s in &plan.stems {
            let staged = s.lufs.unwrap() + s.gain_db;
            assert!(
                (staged - (DEFAULT_REFERENCE_LUFS + s.role.offset_db())).abs() < 0.01,
                "{}",
                s.name
            );
        }
        // The worst-case trim keeps the coherent sum of the staged peaks at the headroom mark.
        let (trim, lufs) = estimate_sum(&plan);
        assert_eq!(trim, plan.trim_db);
        assert!(trim <= 0.0 && lufs.is_some());
        let s = MasterSettings::from_preset(&master::preset("club").unwrap());
        let (p, j) = write_plan_only(&plan, &d.join("mix.wav"), Some(&s)).unwrap();
        assert_eq!(p, d.join("mix.plan.json"));
        assert_eq!(j["engine"], "reaper");
        assert_eq!(j["mix"]["path"], Value::Null);
        let want_gain = -9.0 - lufs.unwrap();
        assert!((j["master"]["gain_db"].as_f64().unwrap() - want_gain).abs() < 0.01);
        assert!(!d.join("mix.wav").exists(), "nothing is mixed here");
    }

    #[test]
    fn silent_stem_is_kept_at_unity_with_a_note_and_inputs_are_safe() {
        let d = tmpdir("mixsil");
        let a = stem(&d, "gtr.wav", 48000, 2, 330.0, -8.0, 2.0);
        let s = d.join("fx.wav");
        write_new(&s, 48000, 2, OutFormat::Pcm24, &vec![0.0; 2 * 48000]).unwrap();
        let plan = plan_mix(&[a.clone(), s], DEFAULT_REFERENCE_LUFS).unwrap();
        assert!(
            plan.notes.iter().any(|n| n.contains("fx: silent")),
            "{:?}",
            plan.notes
        );
        assert_eq!(plan.stems[1].gain_db, 0.0);
        let before = std::fs::read(&a).unwrap();
        let e = render_mix(plan.clone(), &a, true, OutFormat::Pcm24, MasterStep::None).unwrap_err();
        assert!(e.contains("over an input"), "{e}");
        assert_eq!(std::fs::read(&a).unwrap(), before);
    }
}
