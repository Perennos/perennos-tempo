//! Plan B: mastering without REAPER.
//!
//! Gain to a target integrated loudness, then a look-ahead true-peak limiter, then re-measure.
//! Up to 3 measuring passes adjust the gain (and the limiter's internal ceiling) until the result
//! is within 0.5 LU of the target with the true peak at most 0.1 dB over the ceiling. Then one
//! writing pass, and the written file is measured again from disk. If no pass gets there, the
//! best one is written and the report says why.
use crate::loudness::{self, Loudness, Meter, TruePeak};
use crate::util::{db_to_amp, jnum, r2, OutFile};
use crate::wav::{OutFormat, WavReader, WavWriter};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    pub name: &'static str,
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
}

pub const PRESETS: [Preset; 5] = [
    Preset {
        name: "streaming",
        target_lufs: -14.0,
        ceiling_dbtp: -1.0,
    },
    Preset {
        name: "apple",
        target_lufs: -16.0,
        ceiling_dbtp: -1.0,
    },
    Preset {
        name: "club",
        target_lufs: -9.0,
        ceiling_dbtp: -0.3,
    },
    Preset {
        name: "podcast",
        target_lufs: -16.0,
        ceiling_dbtp: -1.0,
    },
    Preset {
        name: "broadcast",
        target_lufs: -23.0,
        ceiling_dbtp: -1.0,
    },
];

pub const DEFAULT_PRESET: &str = "streaming";
/// Within this many LU of the target counts as done.
pub const LUFS_TOL: f64 = 0.5;
/// The true peak may sit this far over the ceiling.
pub const TP_TOL: f64 = 0.1;
const MAX_BOOST_DB: f64 = 40.0;
const MAX_CUT_DB: f64 = -60.0;
/// The limiter aims this far under the ceiling on the first pass.
const CEILING_MARGIN_DB: f64 = 0.1;

pub fn preset(name: &str) -> Result<Preset, String> {
    PRESETS
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
        .copied()
        .ok_or_else(|| {
            format!(
                "unknown preset {name} (choose {})",
                PRESETS.map(|p| p.name).join(", ")
            )
        })
}

#[derive(Clone, Debug, PartialEq)]
pub struct MasterSettings {
    pub preset: Option<&'static str>,
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
    pub format: OutFormat,
    pub lookahead_ms: f64,
    pub release_ms: f64,
    pub max_passes: usize,
}

impl MasterSettings {
    pub fn new(target_lufs: f64, ceiling_dbtp: f64) -> Result<MasterSettings, String> {
        if !(-40.0..=-5.0).contains(&target_lufs) {
            return Err(format!(
                "target {target_lufs} LUFS is out of range (-40 to -5)"
            ));
        }
        if !(-20.0..=0.0).contains(&ceiling_dbtp) {
            return Err(format!(
                "ceiling {ceiling_dbtp} dBTP is out of range (-20 to 0); above 0 would clip"
            ));
        }
        Ok(MasterSettings {
            preset: None,
            target_lufs,
            ceiling_dbtp,
            format: OutFormat::Pcm24,
            lookahead_ms: 5.0,
            release_ms: 150.0,
            max_passes: 3,
        })
    }

    pub fn from_preset(p: &Preset) -> MasterSettings {
        let mut s = MasterSettings::new(p.target_lufs, p.ceiling_dbtp).expect("presets are valid");
        s.preset = Some(p.name);
        s
    }

    pub fn to_json(&self) -> Value {
        json!({
            "preset": self.preset,
            "target_lufs": self.target_lufs,
            "ceiling_dbtp": self.ceiling_dbtp,
            "format": self.format.name(),
        })
    }
}

/// Look-ahead true-peak limiter, stereo-linked, any channel count.
///
/// Detection is the 4x-oversampled peak around each sample (the interval before and after it).
/// The required gain goes through a forward-looking minimum over the look-ahead window (hold),
/// an instant-attack exponential release, and a moving average over the same window. Because the
/// hold covers every sample the average reaches, the gain at a peak is never above what that
/// peak needs: no sample leaves above the ceiling. Latency is look-ahead + 5 frames, removed
/// again, so the output lines up with the input sample for sample.
pub struct Limiter {
    ch: usize,
    ceiling: f64,
    look: usize,
    rel_coef: f64,
    tp: Vec<TruePeak>,
    prev_ip: f64,
    audio: VecDeque<f64>,
    minq: VecDeque<(u64, f64)>,
    idx: u64,
    rel: f64,
    boxq: VecDeque<f64>,
    box_sum: f64,
    since_resum: usize,
    /// Deepest gain applied (linear, 1.0 = untouched).
    pub min_gain: f64,
}

impl Limiter {
    pub fn new(
        channels: usize,
        sample_rate: u32,
        ceiling: f64,
        lookahead_ms: f64,
        release_ms: f64,
    ) -> Limiter {
        let fs = sample_rate as f64;
        let look = ((lookahead_ms / 1000.0 * fs).round() as usize).max(1);
        Limiter {
            ch: channels,
            ceiling,
            look,
            rel_coef: (-1.0 / (release_ms.max(1.0) / 1000.0 * fs)).exp(),
            tp: vec![TruePeak::default(); channels],
            prev_ip: 0.0,
            audio: VecDeque::with_capacity((look + 6) * channels),
            minq: VecDeque::with_capacity(look + 1),
            idx: 0,
            rel: 1.0,
            boxq: VecDeque::with_capacity(look + 1),
            box_sum: 0.0,
            since_resum: 0,
            min_gain: 1.0,
        }
    }

    pub fn latency(&self) -> usize {
        self.look + TruePeak::DELAY - 1
    }

    fn step(&mut self, frame: &[f64], out: &mut Vec<f64>) {
        let mut ip = 0.0f64;
        for (c, &x) in frame.iter().enumerate() {
            let t = self.tp[c].push(x);
            ip = ip.max(t).max(self.tp[c].delayed().abs());
        }
        let det = ip.max(self.prev_ip);
        self.prev_ip = ip;
        let req = if det > self.ceiling {
            self.ceiling / det
        } else {
            1.0
        };
        // Hold: minimum of the last `look` required gains.
        while self.minq.back().is_some_and(|&(_, v)| v >= req) {
            self.minq.pop_back();
        }
        self.minq.push_back((self.idx, req));
        while self
            .minq
            .front()
            .is_some_and(|&(i, _)| i + self.look as u64 <= self.idx)
        {
            self.minq.pop_front();
        }
        self.idx += 1;
        let hold = self.minq.front().map(|&(_, v)| v).unwrap_or(1.0);
        // Release: instant down, exponential back up.
        self.rel = if hold < self.rel {
            hold
        } else {
            hold + (self.rel - hold) * self.rel_coef
        };
        // Moving average over the look-ahead window.
        self.boxq.push_back(self.rel);
        self.box_sum += self.rel;
        if self.boxq.len() > self.look {
            self.box_sum -= self.boxq.pop_front().unwrap_or(0.0);
        }
        self.since_resum += 1;
        if self.since_resum >= 4096 {
            self.box_sum = self.boxq.iter().sum();
            self.since_resum = 0;
        }
        let g = (self.box_sum / self.boxq.len() as f64).min(1.0);
        self.audio.extend(frame);
        // The frame leaving now is the one this gain was computed for (latency frames ago).
        if self.audio.len() / self.ch > self.latency() {
            if g < self.min_gain {
                self.min_gain = g;
            }
            for _ in 0..self.ch {
                let x = self.audio.pop_front().unwrap_or(0.0);
                out.push(x * g);
            }
        }
    }

    /// Interleaved input; output frames are appended to `out` (fewer at first: latency).
    pub fn process(&mut self, interleaved: &[f64], out: &mut Vec<f64>) {
        for frame in interleaved.chunks_exact(self.ch) {
            self.step(frame, out);
        }
    }

    /// Push the tail out: after this, every input frame has come out once.
    pub fn flush(&mut self, out: &mut Vec<f64>) {
        let zeros = vec![0.0; self.ch];
        for _ in 0..self.latency() {
            self.step(&zeros, out);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pass {
    pub gain_db: f64,
    pub limiter_ceiling_db: f64,
    pub post_trim_db: f64,
    pub result: Loudness,
    pub max_gain_reduction_db: f64,
}

impl Pass {
    fn lufs_ok(&self, s: &MasterSettings) -> bool {
        self.result
            .integrated
            .is_some_and(|l| (l - s.target_lufs).abs() <= LUFS_TOL)
    }

    fn tp_ok(&self, s: &MasterSettings) -> bool {
        self.result
            .true_peak
            .is_none_or(|t| t <= s.ceiling_dbtp + TP_TOL)
    }

    pub fn within(&self, s: &MasterSettings) -> bool {
        self.lufs_ok(s) && self.tp_ok(s)
    }

    fn miss(&self, s: &MasterSettings) -> f64 {
        self.result
            .integrated
            .map(|l| (l - s.target_lufs).abs())
            .unwrap_or(f64::INFINITY)
    }

    pub fn to_json(&self, s: &MasterSettings) -> Value {
        json!({
            "gain_db": r2(self.gain_db),
            "limiter_ceiling_dbtp": r2(self.limiter_ceiling_db),
            "post_trim_db": r2(self.post_trim_db),
            "integrated_lufs": jnum(self.result.integrated),
            "true_peak_dbtp": jnum(self.result.true_peak),
            "max_gain_reduction_db": r2(self.max_gain_reduction_db),
            "within_target": self.within(s),
        })
    }
}

/// Read `input`, apply gain, limit, trim, and hand each output block to `sink`.
/// Returns the deepest limiter gain reduction in dB (positive).
fn run_chain(
    input: &Path,
    gain_db: f64,
    ceiling_db: f64,
    post_db: f64,
    s: &MasterSettings,
    sink: &mut dyn FnMut(&[f64]) -> Result<(), String>,
) -> Result<f64, String> {
    let mut r = WavReader::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let info = r.info().clone();
    let ch = info.channels as usize;
    let mut lim = Limiter::new(
        ch,
        info.sample_rate,
        db_to_amp(ceiling_db),
        s.lookahead_ms,
        s.release_ms,
    );
    let (g, post) = (db_to_amp(gain_db), db_to_amp(post_db));
    let mut inb = Vec::new();
    let mut outb = Vec::new();
    loop {
        let n = r
            .read_frames(&mut inb, 8192)
            .map_err(|e| format!("{}: {e}", input.display()))?;
        outb.clear();
        if n == 0 {
            lim.flush(&mut outb);
        } else {
            for x in inb.iter_mut() {
                *x *= g;
            }
            lim.process(&inb, &mut outb);
        }
        if post_db != 0.0 {
            for x in outb.iter_mut() {
                *x *= post;
            }
        }
        sink(&outb)?;
        if n == 0 {
            break;
        }
    }
    Ok(-20.0 * lim.min_gain.max(1e-12).log10())
}

#[derive(Clone, Debug)]
pub struct MasterReport {
    pub input: loudness::Analysis,
    pub output: loudness::Analysis,
    pub path: PathBuf,
    pub settings: MasterSettings,
    pub passes: Vec<Pass>,
    pub chosen: usize,
    pub within_target: bool,
    pub overs: u64,
    pub notes: Vec<String>,
}

impl MasterReport {
    pub fn to_json(&self) -> Value {
        let p = &self.passes[self.chosen];
        json!({
            "engine": "builtin",
            "path": self.path.display().to_string(),
            "settings": self.settings.to_json(),
            "gain_db": r2(p.gain_db),
            "post_trim_db": r2(p.post_trim_db),
            "within_target": self.within_target,
            "passes": self.passes.iter().map(|x| x.to_json(&self.settings)).collect::<Vec<_>>(),
            "chosen_pass": self.chosen + 1,
            "samples_over_full_scale": self.overs,
            "input": self.input.to_json(),
            "output": self.output.to_json(),
            "notes": self.notes,
        })
    }
}

/// Default output name: `<stem>-master.wav` next to the input.
pub fn default_out(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "song".into());
    input.with_file_name(format!("{stem}-master.wav"))
}

/// The measuring passes for `input`, without writing anything: what `master_file` would do.
#[derive(Clone, Debug)]
pub struct MasterPlan {
    pub input: loudness::Analysis,
    pub passes: Vec<Pass>,
    pub chosen: usize,
    /// The chosen pass, with any plan C trim applied.
    pub best: Pass,
    pub notes: Vec<String>,
}

/// Run the measuring passes (up to `max_passes`) and pick the best one.
pub fn plan_master(input: &Path, s: &MasterSettings) -> Result<MasterPlan, String> {
    let src = loudness::analyze_file(input)?;
    let in_lufs = src.loudness.integrated.ok_or_else(|| {
        format!(
            "{} is silent or shorter than 0.4 s (nothing above -70 LUFS): there is nothing to master",
            input.display()
        )
    })?;
    let mut notes = Vec::new();
    let mut passes: Vec<Pass> = Vec::new();
    let mut gain = s.target_lufs - in_lufs;
    let mut ceil = s.ceiling_dbtp - CEILING_MARGIN_DB;
    for _ in 0..s.max_passes.max(1) {
        let g = gain.clamp(MAX_CUT_DB, MAX_BOOST_DB);
        if g != gain && !notes.iter().any(|n: &String| n.contains("gain limit")) {
            notes.push(format!(
                "the target needs {:+.1} dB of gain; held at the {:+.0} dB gain limit",
                gain, g
            ));
        }
        let mut meter = Meter::for_info(&src.info);
        let gr = run_chain(input, g, ceil, 0.0, s, &mut |b| {
            meter.add(b);
            Ok(())
        })?;
        let pass = Pass {
            gain_db: g,
            limiter_ceiling_db: ceil,
            post_trim_db: 0.0,
            result: meter.result(),
            max_gain_reduction_db: gr,
        };
        let (lufs_ok, tp_ok) = (pass.lufs_ok(s), pass.tp_ok(s));
        let got = pass.result.integrated;
        let tp = pass.result.true_peak;
        passes.push(pass);
        if lufs_ok && tp_ok {
            break;
        }
        if let (false, Some(l)) = (lufs_ok, got) {
            gain = g + (s.target_lufs - l);
        }
        if let (false, Some(t)) = (tp_ok, tp) {
            ceil -= t - s.ceiling_dbtp + 0.05;
        }
    }
    // Best pass: within target first, then true peak in bounds, then closest loudness.
    let chosen = (0..passes.len())
        .min_by(|&a, &b| {
            let ka = (!passes[a].within(s), !passes[a].tp_ok(s), passes[a].miss(s));
            let kb = (!passes[b].within(s), !passes[b].tp_ok(s), passes[b].miss(s));
            ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let mut best = passes[chosen].clone();
    if !best.tp_ok(s) {
        // Plan C: a plain trim after the limiter guarantees the ceiling.
        let over = best.result.true_peak.unwrap_or(0.0) - s.ceiling_dbtp;
        best.post_trim_db = -over;
        notes.push(format!(
            "the true peak stayed {over:.2} dB over the ceiling after {} passes; trimmed {:.2} dB after the limiter",
            passes.len(),
            over
        ));
    }
    if !best.lufs_ok(s) {
        let got = best.result.integrated.unwrap_or(f64::NEG_INFINITY);
        notes.push(if got < s.target_lufs {
            format!(
                "best result {got:.2} LUFS is short of {:.1}: the limiter is already taking up to {:.1} dB off the peaks, \
                 and more would distort. For more level, use REAPER (a compressor before the limiter) or a lower target",
                s.target_lufs, best.max_gain_reduction_db
            )
        } else {
            format!("best result {got:.2} LUFS is over the {:.1} target", s.target_lufs)
        });
    }
    Ok(MasterPlan {
        input: src,
        passes,
        chosen,
        best,
        notes,
    })
}

/// Master `input` to `want` (or a free numbered name next to it; `force` replaces).
pub fn master_file(
    input: &Path,
    want: &Path,
    force: bool,
    s: &MasterSettings,
) -> Result<MasterReport, String> {
    let MasterPlan {
        input: src,
        passes,
        chosen,
        mut best,
        mut notes,
    } = plan_master(input, s)?;
    // Writing pass.
    let (out, file) = OutFile::create(want, force, &[input.to_path_buf()])?;
    let written = (|| -> Result<u64, String> {
        let mut w = WavWriter::new(file, src.info.sample_rate, src.info.channels, s.format)
            .map_err(|e| e.to_string())?;
        run_chain(
            input,
            best.gain_db,
            best.limiter_ceiling_db,
            best.post_trim_db,
            s,
            &mut |b| w.write(b).map_err(|e| e.to_string()),
        )?;
        Ok(w.finish().map_err(|e| e.to_string())?.overs)
    })();
    let overs = written.map_err(|e| format!("{e} {}", out.left_note()))?;
    let path = out.commit()?;
    // Re-measure what is on disk.
    let output = loudness::analyze_file(&path)?;
    best.result = output.loudness.clone();
    let within_target = best.within(s);
    if within_target {
        notes.push(format!(
            "verified from the written file: {} LUFS, {} dBTP",
            crate::util::fmt_db(output.loudness.integrated),
            crate::util::fmt_db(output.loudness.true_peak)
        ));
    }
    Ok(MasterReport {
        input: src,
        output,
        path,
        settings: s.clone(),
        passes,
        chosen,
        within_target,
        overs,
        notes,
    })
}

#[cfg(test)]
pub mod testmusic {
    //! A music-like synthetic signal: bass, chords, a lead, kick and hats (noise bursts).
    use crate::loudness::testsig::Noise;
    use std::f64::consts::PI;

    /// Stereo, interleaved, scaled so the sample peak is `peak` (linear).
    pub fn music(fs: u32, secs: f64, peak: f64, seed: u64) -> Vec<f64> {
        let n = (fs as f64 * secs) as usize;
        let mut noise = Noise(seed | 1);
        let mut v = Vec::with_capacity(n * 2);
        let mut hp = 0.0;
        for i in 0..n {
            let t = i as f64 / fs as f64;
            let beat = t % 0.5;
            let kick = (-beat / 0.06).exp()
                * (2.0 * PI * (50.0 + 80.0 * (-beat / 0.02).exp()) * beat).sin();
            let bass = 0.5 * (2.0 * PI * 55.0 * t).sin() * (0.6 + 0.4 * (2.0 * PI * 2.0 * t).cos());
            let chord: f64 = [220.0, 261.63, 329.63]
                .iter()
                .map(|f| 0.15 * (2.0 * PI * f * t).sin())
                .sum::<f64>()
                * (0.7 + 0.3 * (2.0 * PI * 0.25 * t).sin());
            let lead_f = if (t as usize).is_multiple_of(2) {
                880.0
            } else {
                1318.5
            };
            let lead = 0.2 * (2.0 * PI * lead_f * t).sin();
            let w = noise.sample();
            let hat_env = (-(t % 0.25) / 0.03).exp();
            let hat = 0.3 * hat_env * (w - hp);
            hp = w;
            let l = 0.9 * kick + bass + chord + 1.2 * lead + hat;
            let r = 0.9 * kick + bass + chord + 0.8 * lead + 0.7 * hat;
            v.push(l);
            v.push(r);
        }
        let max = v.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        for x in v.iter_mut() {
            *x *= peak / max;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::testmusic::music;
    use super::*;
    use crate::loudness::testsig::{db, sine};
    use crate::util::testutil::tmpdir;
    use crate::wav::{read_all, write_new};

    fn limit(sig: &[f64], ch: usize, fs: u32, ceiling_db: f64) -> (Vec<f64>, f64) {
        let mut lim = Limiter::new(ch, fs, db_to_amp(ceiling_db), 5.0, 150.0);
        let mut out = Vec::new();
        for b in sig.chunks(1000 * ch) {
            lim.process(b, &mut out);
        }
        lim.flush(&mut out);
        (out, lim.min_gain)
    }

    #[test]
    fn presets_and_settings_validation() {
        assert_eq!(preset("streaming").unwrap().target_lufs, -14.0);
        assert_eq!(preset("Club").unwrap().ceiling_dbtp, -0.3);
        assert_eq!(preset("broadcast").unwrap().target_lufs, -23.0);
        assert_eq!(preset("apple").unwrap().target_lufs, -16.0);
        assert_eq!(preset("podcast").unwrap().ceiling_dbtp, -1.0);
        assert!(preset("loud").unwrap_err().contains("streaming"));
        assert!(MasterSettings::new(-14.0, 0.5)
            .unwrap_err()
            .contains("clip"));
        assert!(MasterSettings::new(3.0, -1.0).is_err());
        assert_eq!(
            MasterSettings::from_preset(&PRESETS[0]).preset,
            Some("streaming")
        );
    }

    #[test]
    fn limiter_holds_the_ceiling_keeps_length_and_is_transparent_below_it() {
        let fs = 48000;
        let sig = music(fs, 4.0, db(12.0), 7); // 12 dB over full scale
        let (out, min_gain) = limit(&sig, 2, fs, -1.0);
        assert_eq!(out.len(), sig.len());
        let ceil = db(-1.0);
        let peak = out.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        assert!(peak <= ceil + 1e-12, "sample peak {peak} over {ceil}");
        assert!(min_gain < db(-12.0));
        let mut m = Meter::new(fs, vec![1.0, 1.0]);
        m.add(&out);
        let tp = m.result().true_peak.unwrap();
        assert!(tp <= -1.0 + TP_TOL, "true peak {tp}");
        // Below the ceiling the limiter is a pure delay that it removes again: bit-identical.
        let quiet = music(fs, 2.0, db(-6.0), 3);
        let (out, g) = limit(&quiet, 2, fs, -1.0);
        assert_eq!(g, 1.0);
        assert_eq!(out, quiet);
        // Mono and 6 channels work too.
        let mono = sine(fs, 1, 1.0, 997.0, 2.0, 0.3);
        let (out, _) = limit(&mono, 1, fs, -0.3);
        assert_eq!(out.len(), mono.len());
        assert!(out.iter().all(|x| x.abs() <= db(-0.3) + 1e-12));
        let six = sine(fs, 6, 0.5, 440.0, 3.0, 0.0);
        let (out, _) = limit(&six, 6, fs, -1.0);
        assert_eq!(out.len(), six.len());
        assert!(out.iter().all(|x| x.abs() <= db(-1.0) + 1e-12));
    }

    fn write_music(dir: &Path, name: &str, peak_db: f64, secs: f64) -> PathBuf {
        let p = dir.join(name);
        write_new(
            &p,
            48000,
            2,
            OutFormat::Float32,
            &music(48000, secs, db(peak_db), 11),
        )
        .unwrap();
        p
    }

    #[test]
    fn masters_music_to_the_streaming_preset_and_verifies_from_disk() {
        let d = tmpdir("master");
        let input = write_music(&d, "song.wav", -12.0, 12.0);
        let before = std::fs::read(&input).unwrap();
        let s = MasterSettings::from_preset(&preset("streaming").unwrap());
        let rep = master_file(&input, &default_out(&input), false, &s).unwrap();
        assert!(rep.within_target, "{:#}", rep.to_json());
        let out = &rep.output.loudness;
        assert!(
            (out.integrated.unwrap() + 14.0).abs() <= LUFS_TOL,
            "{out:?}"
        );
        assert!(out.true_peak.unwrap() <= -1.0 + TP_TOL, "{out:?}");
        assert_eq!(rep.overs, 0);
        assert_eq!(rep.path, d.join("song-master.wav"));
        assert_eq!(rep.output.info.frames, rep.input.info.frames);
        assert_eq!(rep.output.info.bits, 24);
        // An independent re-read of the file agrees, and the input is untouched.
        let again = loudness::analyze_file(&rep.path).unwrap();
        assert_eq!(again.loudness, rep.output.loudness);
        assert_eq!(std::fs::read(&input).unwrap(), before);
        // A second run never overwrites the first master.
        let rep2 = master_file(&input, &default_out(&input), false, &s).unwrap();
        assert_eq!(rep2.path, d.join("song-master-2.wav"));
        let j = rep.to_json();
        assert_eq!(j["engine"], "builtin");
        assert!(j["passes"].as_array().unwrap().len() <= 3);
    }

    #[test]
    fn quiet_and_loud_inputs_reach_their_targets() {
        let d = tmpdir("levels");
        // Very quiet (needs about +25 dB) to apple; loud to broadcast (gain down, no limiting).
        let quiet = write_music(&d, "quiet.wav", -40.0, 8.0);
        let s = MasterSettings::from_preset(&preset("apple").unwrap());
        let r = master_file(&quiet, &d.join("q.wav"), false, &s).unwrap();
        assert!(r.within_target, "{:#}", r.to_json());
        assert!(r.passes[r.chosen].gain_db > 20.0);
        let loud = write_music(&d, "loud.wav", -0.5, 8.0);
        let s = MasterSettings::from_preset(&preset("broadcast").unwrap());
        let r = master_file(&loud, &d.join("b.wav"), false, &s).unwrap();
        assert!(r.within_target, "{:#}", r.to_json());
        assert!(r.passes[r.chosen].gain_db < -5.0);
        assert_eq!(r.passes.len(), 1, "simple gain needs one pass");
        // Float output.
        let mut s = MasterSettings::from_preset(&preset("podcast").unwrap());
        s.format = OutFormat::Float32;
        let r = master_file(&loud, &d.join("f.wav"), false, &s).unwrap();
        assert!(r.within_target);
        assert_eq!(r.output.info.format, crate::wav::SampleFormat::Float);
    }

    #[test]
    fn club_preset_iterates_and_never_clips() {
        let d = tmpdir("club");
        let input = write_music(&d, "dyn.wav", -6.0, 10.0);
        let s = MasterSettings::from_preset(&preset("club").unwrap());
        let r = master_file(&input, &d.join("club.wav"), false, &s).unwrap();
        let j = r.to_json();
        // Loud target: heavy limiting, so more than one pass is expected.
        assert!(r.passes.len() >= 2, "{j:#}");
        let (_, samples) = read_all(&r.path).unwrap();
        let peak = samples.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        assert!(peak <= db(-0.3) + 1.0 / 8_388_608.0, "peak {peak}");
        assert_eq!(r.overs, 0);
        assert!(
            r.output.loudness.true_peak.unwrap() <= -0.3 + TP_TOL,
            "{j:#}"
        );
        if r.within_target {
            assert!((r.output.loudness.integrated.unwrap() + 9.0).abs() <= LUFS_TOL);
        } else {
            assert!(r.notes.iter().any(|n| n.contains("short of")), "{j:#}");
        }
    }

    #[test]
    fn unreachable_target_reports_best_result_and_why() {
        let d = tmpdir("unreach");
        // Sparse full-scale clicks: almost no energy, so -9 LUFS at -0.3 dBTP cannot be reached
        // without crushing every click.
        let fs = 48000;
        let mut sig = vec![0.0; fs as usize * 2 * 6];
        for k in 0..12 {
            for j in 0..48 {
                let x = (1.0 - j as f64 / 48.0) * if j % 2 == 0 { 1.0 } else { -1.0 };
                let i = (k * fs as usize / 2 + j) * 2;
                sig[i] = x;
                sig[i + 1] = x;
            }
        }
        let p = d.join("clicks.wav");
        write_new(&p, fs, 2, OutFormat::Float32, &sig).unwrap();
        let s = MasterSettings::from_preset(&preset("club").unwrap());
        let r = master_file(&p, &d.join("o.wav"), false, &s).unwrap();
        assert!(!r.within_target);
        assert_eq!(r.passes.len(), 3);
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("short of") && n.contains("REAPER")),
            "{:?}",
            r.notes
        );
        assert!(r.output.loudness.true_peak.unwrap() <= -0.3 + TP_TOL);
        assert_eq!(r.overs, 0);
    }

    #[test]
    fn silent_input_is_refused_and_input_is_never_the_output() {
        let d = tmpdir("silent");
        let p = d.join("silence.wav");
        write_new(&p, 48000, 2, OutFormat::Pcm24, &vec![0.0; 48000 * 2 * 3]).unwrap();
        let s = MasterSettings::from_preset(&PRESETS[0]);
        let e = master_file(&p, &d.join("x.wav"), false, &s).unwrap_err();
        assert!(
            e.contains("silent") && e.contains("nothing to master"),
            "{e}"
        );
        assert!(!d.join("x.wav").exists());
        let song = write_music(&d, "s.wav", -10.0, 3.0);
        for force in [false, true] {
            let e = master_file(&song, &song, force, &s).unwrap_err();
            assert!(e.contains("over an input"), "{e}");
        }
        assert!(master_file(&d.join("nope.wav"), &d.join("y.wav"), false, &s).is_err());
    }
}
