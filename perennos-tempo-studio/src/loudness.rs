//! ITU-R BS.1770-4 / EBU R128 loudness meter.
//!
//! - K-weighting: the BS.1770 high-shelf pre-filter and RLB high-pass, designed by bilinear
//!   transform for any sample rate with the constants libebur128 uses (at 48 kHz they give the
//!   BS.1770 table).
//! - Integrated loudness: 400 ms blocks with 75% overlap, -70 LUFS absolute gate, -10 LU relative gate.
//! - Momentary (400 ms) and short-term (3 s) maxima, both on a 100 ms grid.
//! - Loudness range (EBU Tech 3342): 3 s windows every 100 ms, -70 LUFS absolute and -20 LU relative
//!   gates, 95th minus 10th percentile.
//! - Sample peak, and true peak by 4x oversampling with a windowed-sinc polyphase FIR (dBTP).
//! - Channel weights: L/R/C 1.0, surrounds 1.41, LFE excluded (5.1 order L R C LFE Ls Rs).
use crate::util::{amp_to_db, jnum};
use crate::wav::{WavInfo, WavReader};
use serde_json::{json, Value};
use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const ABS_GATE_LUFS: f64 = -70.0;
const OFFSET: f64 = -0.691;

/// One second-order section, transposed direct form II.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Biquad {
    /// BS.1770 stage 1: high shelf, f0 1681.97 Hz, +4 dB, Q 0.7072.
    pub fn pre_filter(fs: f64) -> Biquad {
        let f0 = 1681.974450955533;
        let g = 3.999843853973347;
        let q = 0.7071752369554196;
        let k = (PI * f0 / fs).tan();
        let vh = 10f64.powf(g / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1.0 + k / q + k * k;
        Biquad {
            b0: (vh + vb * k / q + k * k) / a0,
            b1: 2.0 * (k * k - vh) / a0,
            b2: (vh - vb * k / q + k * k) / a0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / q + k * k) / a0,
        }
    }

    /// BS.1770 stage 2: RLB high-pass, f0 38.135 Hz, Q 0.5003. The numerator is [1, -2, 1] at
    /// every rate, exactly as the BS.1770 table (the -0.691 dB offset accounts for its gain).
    pub fn rlb(fs: f64) -> Biquad {
        let f0 = 38.13547087602444;
        let q = 0.5003270373238773;
        let k = (PI * f0 / fs).tan();
        let a0 = 1.0 + k / q + k * k;
        Biquad {
            b0: 1.0,
            b1: -2.0,
            b2: 1.0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / q + k * k) / a0,
        }
    }

    /// Second-order Butterworth high-pass (bilinear, RBJ cookbook), 12 dB/octave.
    pub fn highpass(fs: f64, f0: f64) -> Biquad {
        let w0 = 2.0 * PI * (f0 / fs).min(0.49);
        let alpha = w0.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let c = w0.cos();
        let a0 = 1.0 + alpha;
        Biquad {
            b0: (1.0 + c) / 2.0 / a0,
            b1: -(1.0 + c) / a0,
            b2: (1.0 + c) / 2.0 / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - alpha) / a0,
        }
    }

    #[inline]
    pub fn run(&self, s: &mut [f64; 2], x: f64) -> f64 {
        let y = self.b0 * x + s[0];
        s[0] = self.b1 * x - self.a1 * y + s[1];
        s[1] = self.b2 * x - self.a2 * y;
        y
    }

    /// |H| at frequency `f` Hz.
    pub fn gain_at(&self, fs: f64, f: f64) -> f64 {
        let w = 2.0 * PI * f / fs;
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        ((nr * nr + ni * ni) / (dr * dr + di * di)).sqrt()
    }
}

/// Zero filter states that have decayed to nothing, so long silences never run into denormals.
fn flush_tiny(s: &mut [f64; 2]) {
    for v in s.iter_mut() {
        if v.abs() < 1e-25 {
            *v = 0.0;
        }
    }
}

/// Per-channel loudness weights. A WAVE_FORMAT_EXTENSIBLE speaker mask is used when it names
/// every channel; otherwise the BS.1770 default order (5.1 = L R C LFE Ls Rs).
pub fn channel_weights(channels: u16, mask: Option<u32>) -> Vec<f64> {
    if let Some(m) = mask.filter(|m| *m != 0 && m.count_ones() >= channels as u32) {
        let mut out = Vec::with_capacity(channels as usize);
        let mut bit = 0u32;
        for _ in 0..channels {
            while bit < 31 && m & (1 << bit) == 0 {
                bit += 1;
            }
            out.push(match 1u32 << bit {
                0x8 => 0.0,                          // LFE
                0x10 | 0x20 | 0x200 | 0x400 => 1.41, // back and side surrounds
                _ => 1.0,
            });
            bit += 1;
        }
        return out;
    }
    match channels {
        0 => vec![],
        1 => vec![1.0],
        2 => vec![1.0, 1.0],
        3 => vec![1.0, 1.0, 1.0],
        4 => vec![1.0, 1.0, 1.41, 1.41],
        5 => vec![1.0, 1.0, 1.0, 1.41, 1.41],
        n => {
            let mut v = vec![1.0, 1.0, 1.0, 0.0];
            v.resize(n as usize, 1.41);
            v
        }
    }
}

const TP_TAPS: usize = 49;
const TP_PER_PHASE: usize = 12;

/// Phases 1-3 of a 49-tap Hann-windowed sinc for 4x oversampling (the design libebur128 uses),
/// each normalised to unity DC gain and stored oldest-sample-first. Phase 0 is the input itself.
fn tp_coefs() -> &'static [[f64; TP_PER_PHASE]; 3] {
    static C: OnceLock<[[f64; TP_PER_PHASE]; 3]> = OnceLock::new();
    C.get_or_init(|| {
        let half = (TP_TAPS - 1) as f64 / 2.0;
        let h: Vec<f64> = (0..TP_TAPS)
            .map(|j| {
                let m = j as f64 - half;
                let sinc = if m == 0.0 {
                    1.0
                } else {
                    (PI * m / 4.0).sin() / (PI * m / 4.0)
                };
                let w = 0.5 * (1.0 - (2.0 * PI * j as f64 / (TP_TAPS - 1) as f64).cos());
                sinc * w
            })
            .collect();
        let mut c = [[0.0; TP_PER_PHASE]; 3];
        for p in 1..4 {
            let taps: Vec<f64> = (0..TP_PER_PHASE).map(|k| h[4 * k + p]).collect();
            let sum: f64 = taps.iter().sum();
            for k in 0..TP_PER_PHASE {
                c[p - 1][TP_PER_PHASE - 1 - k] = taps[k] / sum;
            }
        }
        c
    })
}

/// 4x oversampling true-peak interpolator for one channel.
#[derive(Clone)]
pub struct TruePeak {
    buf: [f64; 2 * TP_PER_PHASE],
    pos: usize,
}

impl Default for TruePeak {
    fn default() -> Self {
        TruePeak {
            buf: [0.0; 2 * TP_PER_PHASE],
            pos: 0,
        }
    }
}

impl TruePeak {
    /// Feed x[n]; returns the largest |value| of the three points interpolated between x[n-6]
    /// and x[n-5] (the filter delay is 6 samples).
    #[inline]
    pub fn push(&mut self, x: f64) -> f64 {
        self.pos = (self.pos + 1) % TP_PER_PHASE;
        self.buf[self.pos] = x;
        self.buf[self.pos + TP_PER_PHASE] = x;
        let w = &self.buf[self.pos + 1..self.pos + 1 + TP_PER_PHASE];
        let c = tp_coefs();
        let mut m = 0.0f64;
        for ph in c {
            let mut y = 0.0;
            for j in 0..TP_PER_PHASE {
                y += ph[j] * w[j];
            }
            m = m.max(y.abs());
        }
        m
    }

    /// x[n-6]: the input sample the latest interpolated points start from.
    pub fn delayed(&self) -> f64 {
        self.buf[self.pos + 1 + 5]
    }

    pub const DELAY: usize = 6;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Loudness {
    /// Gated integrated loudness, LUFS. `None` when nothing passes the -70 LUFS gate (silence)
    /// or the audio is shorter than one 400 ms block.
    pub integrated: Option<f64>,
    pub momentary_max: Option<f64>,
    pub short_term_max: Option<f64>,
    /// Loudness range, LU. `None` below 3 s or for silence.
    pub lra: Option<f64>,
    /// dBFS; `None` for digital silence.
    pub sample_peak: Option<f64>,
    /// dBTP; `None` for digital silence.
    pub true_peak: Option<f64>,
    pub duration_s: f64,
}

impl Loudness {
    pub fn to_json(&self) -> Value {
        json!({
            "integrated_lufs": jnum(self.integrated),
            "momentary_max_lufs": jnum(self.momentary_max),
            "short_term_max_lufs": jnum(self.short_term_max),
            "lra_lu": jnum(self.lra),
            "sample_peak_dbfs": jnum(self.sample_peak),
            "true_peak_dbtp": jnum(self.true_peak),
            "duration_s": crate::util::r2(self.duration_s),
        })
    }
}

fn lufs(mean_energy: f64) -> f64 {
    if mean_energy > 0.0 {
        OFFSET + 10.0 * mean_energy.log10()
    } else {
        f64::NEG_INFINITY
    }
}

fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

/// Streaming meter: feed interleaved blocks with `add`, read results with `result` at any time.
pub struct Meter {
    channels: usize,
    sample_rate: u32,
    weights: Vec<f64>,
    pre: Biquad,
    rlb: Biquad,
    state: Vec<[[f64; 2]; 2]>,
    seg_len: usize,
    seg_pos: usize,
    seg_acc: f64,
    /// Weighted K-filtered energy of each complete 100 ms segment.
    segs: Vec<f64>,
    peak: f64,
    tp: Vec<TruePeak>,
    tp_max: f64,
    frames: u64,
}

impl Meter {
    pub fn new(sample_rate: u32, weights: Vec<f64>) -> Meter {
        let fs = sample_rate as f64;
        let channels = weights.len();
        Meter {
            channels,
            sample_rate,
            weights,
            pre: Biquad::pre_filter(fs),
            rlb: Biquad::rlb(fs),
            state: vec![[[0.0; 2]; 2]; channels],
            seg_len: ((sample_rate as usize + 5) / 10).max(1),
            seg_pos: 0,
            seg_acc: 0.0,
            segs: Vec::new(),
            peak: 0.0,
            tp: vec![TruePeak::default(); channels],
            tp_max: 0.0,
            frames: 0,
        }
    }

    pub fn for_info(info: &WavInfo) -> Meter {
        Meter::new(
            info.sample_rate,
            channel_weights(info.channels, info.channel_mask),
        )
    }

    pub fn add(&mut self, interleaved: &[f64]) {
        let ch = self.channels;
        if ch == 0 {
            return;
        }
        for frame in interleaved.chunks_exact(ch) {
            let mut e = 0.0;
            for (c, &x0) in frame.iter().enumerate() {
                let x = if x0.is_finite() { x0 } else { 0.0 };
                let a = x.abs();
                if a > self.peak {
                    self.peak = a;
                }
                let t = self.tp[c].push(x);
                if t > self.tp_max {
                    self.tp_max = t;
                }
                let w = self.weights[c];
                if w != 0.0 {
                    let [s_pre, s_rlb] = &mut self.state[c];
                    let y = self.rlb.run(s_rlb, self.pre.run(s_pre, x));
                    e += w * y * y;
                }
            }
            self.seg_acc += e;
            self.seg_pos += 1;
            if self.seg_pos == self.seg_len {
                self.segs.push(self.seg_acc);
                self.seg_acc = 0.0;
                self.seg_pos = 0;
            }
        }
        self.frames += (interleaved.len() / ch) as u64;
        for st in self.state.iter_mut() {
            flush_tiny(&mut st[0]);
            flush_tiny(&mut st[1]);
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn result(&self) -> Loudness {
        let seg = self.seg_len as f64;
        // Momentary values: 400 ms blocks every 100 ms (also the gating blocks).
        let blocks: Vec<f64> = self
            .segs
            .windows(4)
            .map(|w| w.iter().sum::<f64>() / (4.0 * seg))
            .collect();
        let integrated = {
            let abs: Vec<f64> = blocks
                .iter()
                .copied()
                .filter(|&e| lufs(e) > ABS_GATE_LUFS)
                .collect();
            if abs.is_empty() {
                None
            } else {
                let rel = lufs(abs.iter().sum::<f64>() / abs.len() as f64) - 10.0;
                let kept: Vec<f64> = abs.into_iter().filter(|&e| lufs(e) > rel).collect();
                (!kept.is_empty()).then(|| lufs(kept.iter().sum::<f64>() / kept.len() as f64))
            }
        };
        let momentary_max = blocks
            .iter()
            .map(|&e| lufs(e))
            .fold(f64::NEG_INFINITY, f64::max);
        // Short-term values: 3 s windows every 100 ms.
        let short: Vec<f64> = self
            .segs
            .windows(30)
            .map(|w| w.iter().sum::<f64>() / (30.0 * seg))
            .collect();
        let short_term_max = short
            .iter()
            .map(|&e| lufs(e))
            .fold(f64::NEG_INFINITY, f64::max);
        let lra = {
            let abs: Vec<f64> = short
                .iter()
                .copied()
                .filter(|&e| lufs(e) > ABS_GATE_LUFS)
                .collect();
            if abs.is_empty() {
                None
            } else {
                let rel = lufs(abs.iter().sum::<f64>() / abs.len() as f64) - 20.0;
                let mut v: Vec<f64> = abs.into_iter().map(lufs).filter(|&l| l > rel).collect();
                v.sort_by(|a, b| a.total_cmp(b));
                if v.is_empty() {
                    None
                } else {
                    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
                    Some(at(0.95) - at(0.10))
                }
            }
        };
        // Flush the interpolators so the last few samples are covered too.
        let mut tp_max = self.tp_max;
        for t in &self.tp {
            let mut t = t.clone();
            for _ in 0..2 * TP_PER_PHASE {
                tp_max = tp_max.max(t.push(0.0));
            }
        }
        let tp = tp_max.max(self.peak);
        Loudness {
            integrated,
            momentary_max: finite(momentary_max),
            short_term_max: finite(short_term_max),
            lra,
            sample_peak: amp_to_db(self.peak),
            true_peak: amp_to_db(tp),
            duration_s: self.frames as f64 / self.sample_rate.max(1) as f64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Analysis {
    pub path: PathBuf,
    pub info: WavInfo,
    pub loudness: Loudness,
    pub nonfinite: u64,
}

impl Analysis {
    pub fn to_json(&self) -> Value {
        let mut warnings = self.info.warnings.clone();
        if self.nonfinite > 0 {
            warnings.push(format!(
                "{} NaN or infinite samples were read as 0",
                self.nonfinite
            ));
        }
        json!({
            "path": self.path.display().to_string(),
            "format": self.info.describe(),
            "sample_rate": self.info.sample_rate,
            "channels": self.info.channels,
            "frames": self.info.frames,
            "loudness": self.loudness.to_json(),
            "warnings": warnings,
        })
    }
}

/// Measure a WAV file in one streaming pass.
pub fn analyze_file(path: &Path) -> Result<Analysis, String> {
    let mut r = WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let info = r.info().clone();
    let mut m = Meter::for_info(&info);
    let mut block = Vec::new();
    while r
        .read_frames(&mut block, 8192)
        .map_err(|e| format!("{}: {e}", path.display()))?
        > 0
    {
        m.add(&block);
    }
    Ok(Analysis {
        path: path.to_path_buf(),
        info,
        loudness: m.result(),
        nonfinite: r.nonfinite,
    })
}

#[cfg(test)]
pub mod testsig {
    //! Deterministic synthetic signals.
    use std::f64::consts::PI;

    /// Interleaved sine, same in every channel. `amp` is the peak amplitude.
    pub fn sine(fs: u32, ch: usize, secs: f64, freq: f64, amp: f64, phase: f64) -> Vec<f64> {
        let n = (fs as f64 * secs).round() as usize;
        let mut v = Vec::with_capacity(n * ch);
        for i in 0..n {
            let x = amp * (2.0 * PI * freq * i as f64 / fs as f64 + phase).sin();
            v.extend(std::iter::repeat_n(x, ch));
        }
        v
    }

    /// Raised-cosine fade in and out over `secs`, so the edges add no step overshoot.
    pub fn fade(mut v: Vec<f64>, fs: u32, ch: usize, secs: f64) -> Vec<f64> {
        let n = v.len() / ch;
        let f = ((fs as f64 * secs) as usize).min(n / 2).max(1);
        for i in 0..f {
            let g = 0.5 - 0.5 * (PI * i as f64 / f as f64).cos();
            for c in 0..ch {
                v[i * ch + c] *= g;
                v[(n - 1 - i) * ch + c] *= g;
            }
        }
        v
    }

    pub fn silence(fs: u32, ch: usize, secs: f64) -> Vec<f64> {
        vec![0.0; (fs as f64 * secs).round() as usize * ch]
    }

    pub fn db(x: f64) -> f64 {
        10f64.powf(x / 20.0)
    }

    /// Small deterministic generator (xorshift), uniform in [-1, 1).
    pub struct Noise(pub u64);
    impl Noise {
        pub fn sample(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 52) as f64 - 1.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testsig::*;
    use super::*;

    fn measure(fs: u32, ch: usize, sig: &[f64]) -> Loudness {
        let mut m = Meter::new(fs, channel_weights(ch as u16, None));
        for blk in sig.chunks(4096 * ch) {
            m.add(blk);
        }
        m.result()
    }

    #[test]
    fn coefficients_at_48k_match_the_bs1770_table() {
        let p = Biquad::pre_filter(48000.0);
        let r = Biquad::rlb(48000.0);
        let want_p = [
            1.53512485958697,
            -2.69169618940638,
            1.19839281085285,
            -1.69065929318241,
            0.73248077421585,
        ];
        let want_r = [1.0, -2.0, 1.0, -1.99004745483398, 0.99007225036621];
        let got_p = [p.b0, p.b1, p.b2, p.a1, p.a2];
        let got_r = [r.b0, r.b1, r.b2, r.a1, r.a2];
        for (g, w) in got_p.iter().zip(want_p).chain(got_r.iter().zip(want_r)) {
            assert!((g - w).abs() < 1e-6, "{g} vs {w}");
        }
        // The shelf is about +4 dB at high frequencies; the high-pass cuts deep lows at any rate.
        for fs in [44100.0, 48000.0, 96000.0, 192000.0] {
            let p = Biquad::pre_filter(fs);
            let hf = 20.0 * p.gain_at(fs, 15000.0).log10();
            assert!((hf - 4.0).abs() < 0.2, "{fs}: {hf}");
            assert!(Biquad::rlb(fs).gain_at(fs, 10.0) < 0.1, "{fs}");
        }
    }

    #[test]
    fn stereo_1k_sine_reads_its_level_in_lufs() {
        for (fs, level) in [
            (48000, -23.0),
            (48000, -33.0),
            (44100, -23.0),
            (96000, -23.0),
        ] {
            let sig = sine(fs, 2, 20.0, 1000.0, db(level), 0.0);
            let l = measure(fs, 2, &sig);
            let i = l.integrated.unwrap();
            assert!((i - level).abs() <= 0.1, "{fs} Hz {level}: {i}");
            // A steady tone: momentary and short-term maxima agree with the integrated value.
            assert!((l.momentary_max.unwrap() - level).abs() <= 0.1);
            assert!((l.short_term_max.unwrap() - level).abs() <= 0.1);
            assert!(l.lra.unwrap() < 0.1);
            assert!((l.sample_peak.unwrap() - level).abs() < 0.01);
        }
    }

    #[test]
    fn silence_is_gated_out_of_the_integrated_value() {
        let fs = 48000;
        let tone = sine(fs, 2, 20.0, 1000.0, db(-23.0), 0.0);
        let plain = measure(fs, 2, &tone).integrated.unwrap();
        let mut gated = silence(fs, 2, 15.0);
        gated.extend(&tone);
        gated.extend(silence(fs, 2, 40.0));
        let g = measure(fs, 2, &gated);
        let gi = g.integrated.unwrap();
        // Only the 6 blocks that straddle the two edges differ from the tone: within 0.1 LU.
        assert!((gi - plain).abs() <= 0.1, "{gi} vs {plain}");
        // Without gating the 75 s file would read 5.7 LU lower.
        assert!(g.duration_s > 74.9);
        // All-silent and too-short inputs have no integrated value.
        assert_eq!(measure(fs, 2, &silence(fs, 2, 5.0)).integrated, None);
        assert_eq!(measure(fs, 2, &silence(fs, 2, 5.0)).sample_peak, None);
        let short = measure(fs, 2, &sine(fs, 2, 0.3, 1000.0, 0.5, 0.0));
        assert_eq!((short.integrated, short.short_term_max), (None, None));
        assert!(short.sample_peak.is_some());
    }

    #[test]
    fn relative_gate_drops_quiet_passages_more_than_10_lu_down() {
        let fs = 48000;
        // -20 then -40: the quiet half is 20 LU down, below the relative gate (about -33).
        let mut sig = sine(fs, 2, 10.0, 1000.0, db(-20.0), 0.0);
        sig.extend(sine(fs, 2, 10.0, 1000.0, db(-40.0), 0.0));
        let i = measure(fs, 2, &sig).integrated.unwrap();
        assert!((i + 20.0).abs() <= 0.1, "{i}");
        // Ungated energy mean would be -22.97; this proves the gate did its job.
        assert!(i > -21.0);
        // -20 then -26: within 10 LU, so both count: energy mean = -22.04.
        let mut sig = sine(fs, 2, 10.0, 1000.0, db(-20.0), 0.0);
        sig.extend(sine(fs, 2, 10.0, 1000.0, db(-26.0), 0.0));
        let want = 10.0 * ((1.0 + 10f64.powf(-0.6)) / 2.0).log10() - 20.0;
        let i = measure(fs, 2, &sig).integrated.unwrap();
        assert!((i - want).abs() <= 0.1, "{i} vs {want}");
    }

    #[test]
    fn loudness_range_of_alternating_levels_is_their_difference() {
        let fs = 48000;
        // Tones: 3 x (10 s at -20 dBFS, 10 s at -30 dBFS). More than 30% of the 3 s windows sit
        // fully inside each level, so the 10th and 95th percentiles land on the two plateaus:
        // LRA = 10.0 LU up to filter settling. Tolerance 0.1 LU.
        let mut sig = Vec::new();
        for _ in 0..3 {
            sig.extend(sine(fs, 2, 10.0, 1000.0, db(-20.0), 0.0));
            sig.extend(sine(fs, 2, 10.0, 1000.0, db(-30.0), 0.0));
        }
        let lra = measure(fs, 2, &sig).lra.unwrap();
        assert!((lra - 10.0).abs() <= 0.1, "tones: {lra}");
        // Noise: the same levels with white noise. 3 s of noise varies by a few hundredths of a
        // dB, which moves the percentiles a little: tolerance 0.3 LU.
        let mut n = Noise(0x9E37_79B9_7F4A_7C15);
        let mut sig = Vec::new();
        for _ in 0..3 {
            for (secs, lvl) in [(10.0, -20.0), (10.0, -30.0)] {
                for _ in 0..(fs as f64 * secs) as usize {
                    let x = db(lvl) * n.sample();
                    sig.extend([x, x]);
                }
            }
        }
        let lra = measure(fs, 2, &sig).lra.unwrap();
        assert!((lra - 10.0).abs() <= 0.3, "noise: {lra}");
        // The -20 LU relative gate: a long passage 30 dB down is gated out of the range.
        let mut sig = sine(fs, 2, 30.0, 1000.0, db(-20.0), 0.0);
        sig.extend(sine(fs, 2, 30.0, 1000.0, db(-50.0), 0.0));
        let lra = measure(fs, 2, &sig).lra.unwrap();
        assert!(lra < 1.0, "gated quiet part: {lra}");
    }

    #[test]
    fn momentary_and_short_term_maxima_follow_their_windows() {
        let fs = 48000;
        // A 1 s burst at -10 dBFS in silence: momentary (400 ms) sees all of it, short-term (3 s)
        // a third: -10 - 4.77.
        let mut sig = silence(fs, 2, 4.0);
        sig.extend(sine(fs, 2, 1.0, 1000.0, db(-10.0), 0.0));
        sig.extend(silence(fs, 2, 4.0));
        let l = measure(fs, 2, &sig);
        assert!((l.momentary_max.unwrap() + 10.0).abs() <= 0.1, "{:?}", l);
        let st = -10.0 + 10.0 * (1.0f64 / 3.0).log10();
        assert!((l.short_term_max.unwrap() - st).abs() <= 0.1, "{:?}", l);
    }

    #[test]
    fn true_peak_catches_inter_sample_peaks() {
        let fs = 48000;
        // fs/4 sine with a 45 degree phase offset: samples land at +-0.707 of the crest, so the
        // sample peak is 3.01 dB under the true peak. Faded in and out so no edge step adds
        // overshoot; the 4x filter reads the crest about 0.04 dB low.
        let sig = fade(
            sine(fs, 1, 2.0, fs as f64 / 4.0, 0.5, PI / 4.0),
            fs,
            1,
            0.05,
        );
        let l = measure(fs, 1, &sig);
        let (sp, tp) = (l.sample_peak.unwrap(), l.true_peak.unwrap());
        assert!((sp + 9.03).abs() < 0.01, "sample peak {sp}");
        assert!(tp > sp + 2.9, "true peak {tp} vs sample peak {sp}");
        assert!((tp + 6.02).abs() < 0.1, "true peak {tp}");
        // An abrupt start is a real step: the interpolated overshoot is reported, not hidden.
        let abrupt = measure(fs, 1, &sine(fs, 1, 2.0, fs as f64 / 4.0, 0.5, PI / 4.0));
        assert!(abrupt.true_peak.unwrap() >= tp);
        // A low tone has no inter-sample overs: true peak equals sample peak.
        let l = measure(fs, 1, &sine(fs, 1, 1.0, 100.0, 0.5, 0.0));
        assert!((l.true_peak.unwrap() - l.sample_peak.unwrap()).abs() < 0.01);
        // A single full-scale click: true peak is at least the sample peak, and the tail of the
        // file is flushed through the interpolator.
        let mut sig = vec![0.0; 1000];
        sig[999] = 1.0;
        let l = measure(fs, 1, &sig);
        assert!(l.true_peak.unwrap() >= l.sample_peak.unwrap() - 1e-9);
        assert!(l.sample_peak.unwrap().abs() < 1e-9);
    }

    #[test]
    fn channel_weights_surrounds_lfe_and_mono() {
        assert_eq!(
            channel_weights(6, None),
            vec![1.0, 1.0, 1.0, 0.0, 1.41, 1.41]
        );
        assert_eq!(channel_weights(8, None)[3], 0.0);
        assert_eq!(channel_weights(1, None), vec![1.0]);
        // Mask: FL FR LFE BL -> LFE excluded, BL a surround.
        assert_eq!(
            channel_weights(4, Some(0x1 | 0x2 | 0x8 | 0x10)),
            vec![1.0, 1.0, 0.0, 1.41]
        );
        // A mask that names too few channels falls back to the default order.
        assert_eq!(channel_weights(2, Some(0x4)), vec![1.0, 1.0]);
        let fs = 48000;
        let tone: Vec<f64> = sine(fs, 1, 10.0, 1000.0, db(-20.0), 0.0);
        let in_ch = |c: usize| -> Vec<f64> {
            let mut v = vec![0.0; tone.len() * 6];
            for (i, x) in tone.iter().enumerate() {
                v[i * 6 + c] = *x;
            }
            v
        };
        let l_front = measure(fs, 6, &in_ch(0)).integrated.unwrap();
        let l_surr = measure(fs, 6, &in_ch(4)).integrated.unwrap();
        assert!((l_surr - l_front - 10.0 * 1.41f64.log10()).abs() < 0.01);
        let lfe = measure(fs, 6, &in_ch(3));
        assert_eq!(lfe.integrated, None, "LFE is not part of loudness");
        assert!(lfe.sample_peak.is_some(), "but it is part of the peaks");
        // Mono -20 dBFS sine: one channel, so 3 dB below the same tone in stereo.
        let mono = measure(fs, 1, &tone).integrated.unwrap();
        assert!((mono + 23.01).abs() < 0.1, "{mono}");
    }

    #[test]
    fn highpass_cuts_lows_and_passes_highs() {
        let h = Biquad::highpass(48000.0, 100.0);
        assert!((h.gain_at(48000.0, 100.0) - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.01);
        assert!(h.gain_at(48000.0, 25.0) < 0.07);
        assert!((h.gain_at(48000.0, 2000.0) - 1.0).abs() < 0.01);
    }

    #[test]
    fn nan_input_does_not_poison_the_meter() {
        let mut sig = sine(48000, 2, 5.0, 1000.0, db(-23.0), 0.0);
        sig[1000] = f64::NAN;
        sig[2001] = f64::INFINITY;
        let l = measure(48000, 2, &sig);
        assert!((l.integrated.unwrap() + 23.0).abs() < 0.1);
    }
}
