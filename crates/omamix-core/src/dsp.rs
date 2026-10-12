//! DSP chain. Stages are in-process and real-time later; this scaffold ships
//! the order, the trait, and a gain stage. Noise reduction, EQ, compression,
//! and limiting are identity placeholders until their implementations land.
//!
//! Strip order, from the spec: noise reduction, high-pass, 10-band graphic EQ,
//! compressor, limiter. Input trim and the fader sit outside this list.

use crate::db::db_to_linear;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectKind {
    NoiseReduction,
    HighPass,
    GraphicEq,
    Compressor,
    Limiter,
}

pub const STRIP_EFFECT_ORDER: [EffectKind; 5] = [
    EffectKind::NoiseReduction,
    EffectKind::HighPass,
    EffectKind::GraphicEq,
    EffectKind::Compressor,
    EffectKind::Limiter,
];

pub trait DspStage: Send {
    fn kind(&self) -> EffectKind;
    fn enabled(&self) -> bool;
    fn set_enabled(&mut self, enabled: bool);
    fn process(&mut self, samples: &mut [f32], channels: usize, sample_rate: u32);
    fn reset(&mut self) {}
}

/// Multiplies every sample by a dB gain. Used for trim and as the test double
/// that proves the chain actually runs enabled stages.
pub struct GainStage {
    pub kind: EffectKind,
    pub decibels: f32,
    pub enabled: bool,
}

impl GainStage {
    pub fn new(kind: EffectKind, decibels: f32) -> Self {
        Self {
            kind,
            decibels,
            enabled: true,
        }
    }
}

impl DspStage for GainStage {
    fn kind(&self) -> EffectKind {
        self.kind
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    fn process(&mut self, samples: &mut [f32], _channels: usize, _sample_rate: u32) {
        let gain = db_to_linear(self.decibels);
        for sample in samples {
            *sample *= gain;
        }
    }
}

/// Placeholder that preserves samples. Disabled stages are also skipped by the chain.
pub struct IdentityStage {
    kind: EffectKind,
    enabled: bool,
}

impl IdentityStage {
    pub fn new(kind: EffectKind) -> Self {
        Self {
            kind,
            enabled: false,
        }
    }
}

impl DspStage for IdentityStage {
    fn kind(&self) -> EffectKind {
        self.kind
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    fn process(&mut self, _samples: &mut [f32], _channels: usize, _sample_rate: u32) {}
}

pub struct DspChain {
    stages: Vec<Box<dyn DspStage>>,
    sample_rate: u32,
}

impl DspChain {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            stages: Vec::new(),
            sample_rate,
        }
    }

    /// The v1 strip order, each stage off and passing audio unchanged.
    pub fn strip_template(sample_rate: u32) -> Self {
        let mut chain = Self::new(sample_rate);
        for kind in STRIP_EFFECT_ORDER {
            chain.push(Box::new(IdentityStage::new(kind)));
        }
        chain
    }

    pub fn push(&mut self, stage: Box<dyn DspStage>) {
        self.stages.push(stage);
    }

    pub fn kinds(&self) -> Vec<EffectKind> {
        self.stages.iter().map(|stage| stage.kind()).collect()
    }

    pub fn process(&mut self, samples: &mut [f32], channels: usize) {
        let sample_rate = self.sample_rate;
        for stage in &mut self.stages {
            if stage.enabled() {
                stage.process(samples, channels, sample_rate);
            }
        }
    }
}

/// Balance pan for a stereo strip. −1 is hard left, 0 is center, +1 is hard right.
///
/// Left is `min(1, 1 - pan)` and right is `min(1, 1 + pan)`, after clamping
/// pan to `-1..=1`. Center is gain 1 on both channels, so a default strip
/// does not change the level. Neither side goes above 1. A non-finite pan
/// is center. [`StripMixer`](crate::params::StripMixer) applies this law.
pub fn balance_gains(pan: f32) -> (f32, f32) {
    let pan = if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    ((1.0 - pan).min(1.0), (1.0 + pan).min(1.0))
}

/// Equal-power pan for a mono source. −1 is hard left, 0 is center, +1 is hard right.
///
/// Angle is `(pan + 1) * pi/4`, left is `cos`, right is `sin`. Center is
/// equal power (about −3 dB each side) and `L² + R² = 1`. Stereo strips do
/// not use this. A stereo strip at center must stay at unity, and this law
/// would cut each side by about 3 dB. [`balance_gains`] is the strip law.
pub fn constant_power_gains(pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_template_follows_spec_order_and_starts_bypassed() {
        let mut chain = DspChain::strip_template(48_000);
        assert_eq!(chain.kinds(), STRIP_EFFECT_ORDER.to_vec());
        let mut samples = [0.25, -0.5, 0.25, -0.5];
        let before = samples;
        chain.process(&mut samples, 2);
        assert_eq!(samples, before);
    }

    #[test]
    fn enabled_gain_runs_and_disabled_gain_does_not() {
        let mut chain = DspChain::new(48_000);
        chain.push(Box::new(GainStage::new(EffectKind::Compressor, 20.0)));
        let mut samples = [0.1, -0.2];
        chain.process(&mut samples, 2);
        assert!((samples[0] - 1.0).abs() < 1e-4);
        assert!((samples[1] + 2.0).abs() < 1e-4);

        let mut skipped = DspChain::new(48_000);
        let mut stage = GainStage::new(EffectKind::Limiter, 20.0);
        stage.set_enabled(false);
        skipped.push(Box::new(stage));
        let mut dry = [0.1, -0.2];
        skipped.process(&mut dry, 2);
        assert_eq!(dry, [0.1, -0.2]);
    }

    #[test]
    fn balance_pan_is_unity_at_center_and_never_above_one() {
        assert_eq!(balance_gains(0.0), (1.0, 1.0));
        assert_eq!(balance_gains(-1.0), (1.0, 0.0));
        assert_eq!(balance_gains(1.0), (0.0, 1.0));
        assert_eq!(balance_gains(0.5), (0.5, 1.0));
        assert_eq!(balance_gains(-0.25), (1.0, 0.75));
        assert_eq!(balance_gains(f32::NAN), (1.0, 1.0));
        for pan in [-1.5, -1.0, -0.3, 0.0, 0.3, 1.0, 1.5] {
            let (left, right) = balance_gains(pan);
            assert!(left <= 1.0 && right <= 1.0, "{pan}");
            assert!(left >= 0.0 && right >= 0.0, "{pan}");
        }
    }

    #[test]
    fn center_pan_is_equal_power() {
        let (left, right) = constant_power_gains(0.0);
        assert!((left - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
        assert!((right - left).abs() < 1e-5);
        let (hard_left, hard_left_right) = constant_power_gains(-1.0);
        assert!((hard_left - 1.0).abs() < 1e-5);
        assert!(hard_left_right.abs() < 1e-5);
        let (hard_right_left, hard_right) = constant_power_gains(1.0);
        assert!(hard_right_left.abs() < 1e-5);
        assert!((hard_right - 1.0).abs() < 1e-5);
    }

    #[test]
    fn non_finite_gain_does_not_emit_nan_samples() {
        let mut chain = DspChain::new(48_000);
        chain.push(Box::new(GainStage::new(EffectKind::Compressor, f32::NAN)));
        let mut samples = [0.25, -0.5];
        chain.process(&mut samples, 2);
        assert_eq!(samples, [0.0, -0.0]);

        let mut hot = DspChain::new(48_000);
        hot.push(Box::new(GainStage::new(
            EffectKind::Compressor,
            f32::INFINITY,
        )));
        let mut boosted = [0.1, -0.1];
        hot.process(&mut boosted, 2);
        assert!(boosted.iter().all(|sample| sample.is_finite()));
    }
}
