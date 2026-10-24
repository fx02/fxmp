//! DSP helpers: crossfade curve, gain, ceiling limiter, loudness meter.

use ebur128::{EbuR128, Mode};

const KNEE: f32 = 0.8;
/// -1 dBFS.
const CEILING: f32 = 0.891;
const TARGET_LUFS: f32 = -18.0;
const MIN_MEASURED_SECS: u64 = 30;

/// Equal-power crossfade gains `(out_gain, in_gain)`; `t` is clamped to [0, 1].
pub fn equal_power(t: f32) -> (f32, f32) {
    let a = t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    (a.cos(), a.sin())
}

/// ReplayGain if present, else distance to -18 LUFS, else 0. Clamped to [-15, +6] dB.
pub fn track_gain_db(rg_track_gain_db: Option<f32>, measured_lufs: Option<f32>) -> f32 {
    let g = rg_track_gain_db
        .or_else(|| measured_lufs.map(|l| TARGET_LUFS - l))
        .unwrap_or(0.0);
    g.clamp(-15.0, 6.0)
}

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Stateless soft-knee limiter: identity up to 0.8, then a tanh curve that
/// is continuous (C1) at the knee, monotonic and asymptotic to 0.891.
pub fn apply_ceiling(buf: &mut [f32]) {
    let range = CEILING - KNEE;
    for s in buf {
        let a = s.abs();
        if a > KNEE {
            *s = s.signum() * (KNEE + range * ((a - KNEE) / range).tanh()).min(CEILING);
        }
    }
}

/// Integrated-loudness (EBU R128) meter for interleaved stereo f32.
pub struct LoudnessMeter {
    ebu: Option<EbuR128>,
    rate: u32,
    frames: u64,
}

impl LoudnessMeter {
    pub fn new(rate: u32) -> Self {
        Self { ebu: EbuR128::new(2, rate, Mode::I).ok(), rate, frames: 0 }
    }

    pub fn push(&mut self, interleaved: &[f32]) {
        if let Some(e) = self.ebu.as_mut() {
            if e.add_frames_f32(interleaved).is_ok() {
                self.frames += (interleaved.len() / 2) as u64;
            }
        }
    }

    /// Integrated LUFS, or `None` if under 30 s was measured (or not finite).
    pub fn finish(self) -> Option<f32> {
        if self.frames < MIN_MEASURED_SECS * self.rate as u64 {
            return None;
        }
        let l = self.ebu?.loudness_global().ok()? as f32;
        l.is_finite().then_some(l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_power_endpoints_and_constant_power() {
        assert_eq!(equal_power(0.0), (1.0, 0.0));
        let (o, i) = equal_power(1.0);
        assert!(o.abs() < 1e-6 && (i - 1.0).abs() < 1e-6);
        for t in [0.25f32, 0.5, 0.75] {
            let (o, i) = equal_power(t);
            assert!((o * o + i * i - 1.0).abs() < 1e-4);
        }
        assert_eq!(equal_power(-1.0), equal_power(0.0));
        assert_eq!(equal_power(2.0), equal_power(1.0));
    }

    #[test]
    fn gain_prefers_replaygain_then_lufs_then_zero() {
        assert_eq!(track_gain_db(Some(-6.5), Some(-10.0)), -6.5);
        assert_eq!(track_gain_db(None, Some(-10.0)), -8.0);
        assert_eq!(track_gain_db(None, None), 0.0);
        assert_eq!(track_gain_db(None, Some(-40.0)), 6.0);
        assert_eq!(track_gain_db(Some(-30.0), None), -15.0);
        assert!((db_to_lin(-6.0206) - 0.5).abs() < 1e-3);
        assert_eq!(db_to_lin(0.0), 1.0);
    }

    #[test]
    fn ceiling_never_exceeds_and_is_transparent_below_knee() {
        let mut b = [0.5, -0.5, 0.8, -0.8];
        apply_ceiling(&mut b);
        assert_eq!(b, [0.5, -0.5, 0.8, -0.8]);
        let mut b = [1.0, 2.0, -3.0, 1000.0];
        apply_ceiling(&mut b);
        assert!(b.iter().all(|y| y.abs() <= 0.891));
        // monotonic and continuous
        let mut prev = 0.0f32;
        for k in 0..=4000 {
            let mut v = [k as f32 * 0.001];
            apply_ceiling(&mut v);
            assert!(v[0] >= prev && v[0] - prev < 0.01);
            assert!(v[0] <= 0.891);
            prev = v[0];
        }
    }

    fn tone(secs: usize) -> Vec<f32> {
        let rate = 48_000usize;
        let mut v = Vec::with_capacity(rate * secs * 2);
        for n in 0..rate * secs {
            let s = 0.1 * (2.0 * std::f32::consts::PI * 997.0 * n as f32 / rate as f32).sin();
            v.push(s);
            v.push(s);
        }
        v
    }

    #[test]
    fn loudness_of_known_tone() {
        let mut m = LoudnessMeter::new(48_000);
        for chunk in tone(35).chunks(4096) {
            m.push(chunk);
        }
        let l = m.finish().expect("35 s is enough");
        assert!((l + 20.0).abs() <= 1.0, "got {l}");

        let mut m = LoudnessMeter::new(48_000);
        m.push(&tone(10));
        assert_eq!(m.finish(), None);
    }
}
