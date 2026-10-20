//! How far one decoded window's sound trails another's, and how alike the two are: the peak of
//! their cross-correlation, found by FFT over the whole window and placed between samples by a
//! parabola through it and its two neighbours.

// Sample counts, lags and energies, between integers and floats: a window holds 480,000 samples,
// far inside what an f64 holds exactly, and samples go back to the f32 they were decoded as.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// The rate every track is decoded at.
pub const RATE: u32 = 48_000;

/// Offsets up to this many samples either way are searched: a second. FMAB's AC-3 sits 27 ms off
/// its TrueHD, and the head hole mkvmerge left in five of its episodes was 67 ms.
pub const MAX_LAG: usize = RATE as usize;

/// A window quieter than this (RMS, dBFS) holds too little sound to compare.
pub const QUIET_DB: f64 = -50.0;

/// One track's window, ready to correlate with another prepared by the same [`Correlator`].
pub struct Prepared {
    spectrum: Vec<Complex<f32>>,
    /// Sum of squares, after the mean is taken out.
    energy: f64,
    /// RMS of the samples as decoded, in dBFS; `-inf` for none.
    pub level_db: f64,
}

/// What one pair of windows says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lag {
    /// Milliseconds the second track's sound comes after the first's: positive when it is late.
    pub ms: f64,
    /// The correlation peak over both windows' energy: 1 for the same sound, near 0 for unrelated.
    pub score: f64,
}

/// Correlates windows of one length. Keeps its FFT plans, so make one per length.
pub struct Correlator {
    samples: usize,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

impl Correlator {
    /// For windows of `samples` samples: longer input is cut, shorter padded with silence.
    pub fn new(samples: usize) -> Self {
        // Long enough that no lag searched wraps around onto another.
        let n = (samples + MAX_LAG).next_power_of_two();
        let mut planner = FftPlanner::new();
        Correlator {
            samples,
            forward: planner.plan_fft_forward(n),
            inverse: planner.plan_fft_inverse(n),
        }
    }

    pub fn prepare(&self, samples: &[f32]) -> Prepared {
        let samples = &samples[..samples.len().min(self.samples)];
        let level_db = if samples.is_empty() {
            f64::NEG_INFINITY
        } else {
            let power =
                samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / samples.len() as f64;
            10.0 * power.log10()
        };
        let mean = if samples.is_empty() {
            0.0
        } else {
            samples.iter().map(|&s| f64::from(s)).sum::<f64>() / samples.len() as f64
        };
        let mut energy = 0.0;
        let mut spectrum = vec![Complex::new(0.0, 0.0); self.forward.len()];
        for (slot, &s) in spectrum.iter_mut().zip(samples) {
            let v = f64::from(s) - mean;
            energy += v * v;
            let v = v as f32;
            *slot = Complex::new(v, 0.0);
        }
        self.forward.process(&mut spectrum);
        Prepared {
            spectrum,
            energy,
            level_db,
        }
    }

    /// The lag of `b` behind `a`, or `None` when either is quiet.
    pub fn lag(&self, a: &Prepared, b: &Prepared) -> Option<Lag> {
        if a.level_db < QUIET_DB || b.level_db < QUIET_DB || a.energy == 0.0 || b.energy == 0.0 {
            return None;
        }
        let mut c: Vec<Complex<f32>> = a
            .spectrum
            .iter()
            .zip(&b.spectrum)
            .map(|(x, y)| x * y.conj())
            .collect();
        self.inverse.process(&mut c);
        let n = c.len();
        // c[k] = sum of a[t + k] * b[t]: a peak at k > 0 means a's sound comes k samples after
        // b's, so b is early. Lags past MAX_LAG either way are not searched.
        let at = |k: isize| f64::from(c[k.rem_euclid(n as isize) as usize].re);
        let max = MAX_LAG as isize;
        let best = (-max..=max)
            .max_by(|&x, &y| at(x).total_cmp(&at(y)))
            .expect("the range is not empty");
        let (y0, y1, y2) = (at(best - 1), at(best), at(best + 1));
        let bend = y0 - 2.0 * y1 + y2;
        let between = if bend < 0.0 {
            0.5 * (y0 - y2) / bend
        } else {
            0.0
        };
        // rustfft leaves the round trip scaled by n.
        let score = y1 / n as f64 / (a.energy * b.energy).sqrt();
        Some(Lag {
            ms: -(best as f64 + between) * 1000.0 / f64::from(RATE),
            score,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test signal: tone bursts at random times, sampled at `rate` with every burst `shift_s`
    /// later. The same seed gives the same sound at any rate or shift.
    fn bursts(seed: u64, seconds: f64, rate: u32, shift_s: f64) -> Vec<f32> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let rate_f = f64::from(rate);
        let len = (seconds * rate_f) as usize;
        let mut out = vec![0f32; len];
        for _ in 0..(seconds * 6.0) as usize {
            let centre = 0.2 + next() * (seconds - 0.4) + shift_s;
            let freq = 300.0 + next() * 2700.0;
            let width = 0.002 + next() * 0.003;
            let phase = next() * std::f64::consts::TAU;
            let from = ((centre - 5.0 * width) * rate_f).max(0.0) as usize;
            let to = (((centre + 5.0 * width) * rate_f) as usize).min(len);
            for (i, s) in out.iter_mut().enumerate().take(to).skip(from) {
                let t = i as f64 / rate_f - centre;
                let v = 0.3
                    * (-0.5 * (t / width).powi(2)).exp()
                    * (std::f64::consts::TAU * freq * t + phase).sin();
                *s += v as f32;
            }
        }
        out
    }

    fn lag_of(a: &[f32], b: &[f32]) -> Option<Lag> {
        let c = Correlator::new(a.len().max(b.len()));
        c.lag(&c.prepare(a), &c.prepare(b))
    }

    #[test]
    fn a_late_copy_reads_late_by_its_delay() {
        let a = bursts(1, 10.0, RATE, 0.0);
        let b = bursts(1, 10.0, RATE, 0.027);
        let lag = lag_of(&a, &b).unwrap();
        assert!((lag.ms - 27.0).abs() < 0.01, "{lag:?}");
        assert!(lag.score > 0.99, "{lag:?}");
        let early = lag_of(&b, &a).unwrap();
        assert!((early.ms + 27.0).abs() < 0.01, "{early:?}");
    }

    #[test]
    fn a_shift_between_samples_is_placed_between_them() {
        // 0.3 ms is 14.4 samples at 48 kHz.
        let a = bursts(2, 10.0, RATE, 0.0);
        let b = bursts(2, 10.0, RATE, 0.0003);
        let lag = lag_of(&a, &b).unwrap();
        assert!((lag.ms - 0.3).abs() < 0.01, "{lag:?}");
    }

    #[test]
    fn unrelated_sound_scores_low_and_silence_not_at_all() {
        let a = bursts(3, 10.0, RATE, 0.0);
        let b = bursts(4, 10.0, RATE, 0.0);
        let lag = lag_of(&a, &b).unwrap();
        assert!(lag.score < 0.2, "{lag:?}");
        let quiet: Vec<f32> = a.iter().map(|s| s * 0.001).collect();
        assert_eq!(lag_of(&a, &quiet), None, "-60 dB");
        assert_eq!(lag_of(&a, &[]), None, "nothing decoded");
    }

    #[test]
    fn longer_input_is_cut_to_the_window() {
        let a = bursts(5, 10.0, RATE, 0.0);
        let mut b = bursts(5, 10.0, RATE, 0.005);
        b.extend(bursts(6, 2.0, RATE, 0.0));
        let c = Correlator::new(a.len());
        let lag = c.lag(&c.prepare(&a), &c.prepare(&b)).unwrap();
        assert!((lag.ms - 5.0).abs() < 0.01, "{lag:?}");
    }
}
