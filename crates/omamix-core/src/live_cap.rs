use std::time::{Duration, Instant};

use crate::comp::{CompUpdate, open as open_comp};
use crate::eq::{EqUpdate, open as open_eq};
use crate::limit::{LimitReader, LimitUpdate, open as open_limit};
use crate::params::{
    STRIP_SLOT_COUNT, StripBlockInput, StripMeterBank, StripMixer, StripParamBank,
};
use crate::scope::ScopeBank;

pub const LIVE_CAP_FALLBACK: u32 = 1;

pub fn quantum_budget(sample_rate: u32, frames: u32) -> Duration {
    let rate = u64::from(sample_rate.max(1));
    let frames = u64::from(frames.max(1));
    Duration::from_nanos(frames.saturating_mul(675_000_000) / rate)
}

pub struct LiveCap {
    pub cap: u32,
    pub budget: Duration,
    pub slots: u32,
    pub master_tries: Vec<Duration>,
    pub all_live_tries: Vec<Duration>,
}

pub fn derive_live_cap(
    master: Duration,
    all_live: Duration,
    slots: u32,
    budget: Duration,
    hard_max: u32,
) -> u32 {
    let hard_max = hard_max.min(STRIP_SLOT_COUNT as u32).max(1);
    let slots = if slots == 0 {
        hard_max
    } else {
        slots.min(hard_max)
    };
    if all_live.is_zero() {
        return LIVE_CAP_FALLBACK.min(hard_max);
    }
    if all_live <= budget {
        return hard_max;
    }
    if master >= budget || all_live <= master {
        return LIVE_CAP_FALLBACK.min(hard_max);
    }
    let variable = all_live.saturating_sub(master).as_nanos();
    let per = variable / u128::from(slots);
    if per == 0 {
        return LIVE_CAP_FALLBACK.min(hard_max);
    }
    let room = budget.saturating_sub(master).as_nanos();
    let fit = u32::try_from(room / per).unwrap_or(hard_max);
    fit.clamp(LIVE_CAP_FALLBACK.min(hard_max), hard_max)
}

pub fn calibrate_live_cap(
    sample_rate: u32,
    frames: u32,
    limiter_on: bool,
    lookahead_ms: f32,
) -> LiveCap {
    let slots = STRIP_SLOT_COUNT as u32;
    let budget = quantum_budget(sample_rate, frames);
    let tries = if cfg!(debug_assertions) { 1 } else { 3 };
    let mut master_tries = Vec::with_capacity(tries);
    let mut all_live_tries = Vec::with_capacity(tries);
    for _ in 0..tries {
        master_tries.push(time_mix(MixTiming {
            rate: sample_rate,
            frames: frames.max(1) as usize,
            slots: 0,
            limiter_on,
            lookahead_ms,
        }));
        all_live_tries.push(time_mix(MixTiming {
            rate: sample_rate,
            frames: frames.max(1) as usize,
            slots: STRIP_SLOT_COUNT,
            limiter_on,
            lookahead_ms,
        }));
    }
    let master = master_tries.iter().copied().max().unwrap_or_default();
    let all_live = all_live_tries.iter().copied().max().unwrap_or_default();
    let cap = derive_live_cap(master, all_live, slots, budget, slots);
    LiveCap {
        cap,
        budget,
        slots,
        master_tries,
        all_live_tries,
    }
}

struct MixTiming {
    rate: u32,
    frames: usize,
    slots: usize,
    limiter_on: bool,
    lookahead_ms: f32,
}

fn time_mix(timing: MixTiming) -> Duration {
    let reps = if cfg!(debug_assertions) { 2u32 } else { 40 };
    let (mut eq_w, mut eq_r) = open_eq();
    let (mut comp_w, mut comp_r) = open_comp();
    let _ = eq_w.set_sample_rate(timing.rate);
    let _ = comp_w.set_sample_rate(timing.rate);
    let strip_slots = timing.slots.min(STRIP_SLOT_COUNT);
    for slot in 0..strip_slots as u16 {
        let _ = eq_w.apply(slot, EqUpdate::HighPassEnabled(true));
        let _ = eq_w.apply(
            slot,
            EqUpdate::Band {
                index: 4,
                gain_db: 6.0,
            },
        );
        let _ = comp_w.apply(slot, CompUpdate::Enabled(true));
    }
    let _ = eq_w.apply_master(EqUpdate::HighPassEnabled(true));
    let _ = eq_w.apply_master(EqUpdate::Band {
        index: 4,
        gain_db: 6.0,
    });
    let _ = comp_w.apply_master(CompUpdate::Enabled(true));
    let mut owned_reader;
    let limit_r: &mut LimitReader = if timing.limiter_on {
        let (mut limit_w, reader) = open_limit();
        let _ = limit_w.set_sample_rate(timing.rate);
        let _ = limit_w.set_processing_quantum(timing.frames.max(1) as u32);
        let _ = limit_w.apply_master(LimitUpdate::Enabled(true));
        let _ = limit_w.apply_master(LimitUpdate::LookaheadMs(timing.lookahead_ms));
        owned_reader = reader;
        &mut owned_reader
    } else {
        owned_reader = LimitReader::bypassed();
        &mut owned_reader
    };
    let mut mixer = Box::new(StripMixer::new(timing.rate));
    let bank = StripParamBank::new();
    let meters = StripMeterBank::new();
    let mut scopes = ScopeBank::new();
    let mut noise = 0x1234_5678u32;
    let mut hot_l = vec![0.0f32; timing.frames];
    let mut hot_r = vec![0.0f32; timing.frames];
    fill_hot(&mut hot_l, &mut hot_r, &mut noise);
    let inputs: Vec<Option<StripBlockInput<'_>>> = (0..strip_slots)
        .map(|_| {
            Some(StripBlockInput {
                left: hot_l.as_slice(),
                right: hot_r.as_slice(),
            })
        })
        .collect();
    let mut left = vec![0.0f32; timing.frames];
    let mut right = vec![0.0f32; timing.frames];
    let mut run = |mixer: &mut StripMixer| {
        mixer.process(
            &bank,
            &mut eq_r,
            &mut comp_r,
            limit_r,
            &meters,
            &mut scopes,
            &inputs,
            &mut left,
            &mut right,
            timing.rate,
        );
    };
    run(&mut mixer);
    let start = Instant::now();
    for _ in 0..reps {
        run(&mut mixer);
    }
    std::hint::black_box((&left, &right));
    start.elapsed() / reps
}

fn fill_hot(left: &mut [f32], right: &mut [f32], state: &mut u32) {
    for index in 0..left.len().min(right.len()) {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let l = (*state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let r = (*state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
        left[index] = l * 2.0;
        right[index] = r * 2.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_all_live_mix_inside_the_budget_keeps_the_hard_max() {
        let budget = Duration::from_micros(3_600);
        let cap = derive_live_cap(
            Duration::from_micros(400),
            Duration::from_micros(2_000),
            38,
            budget,
            38,
        );
        assert_eq!(cap, 38);
    }

    #[test]
    fn a_slower_all_live_mix_picks_a_smaller_cap() {
        let budget = Duration::from_micros(3_600);
        let fast = derive_live_cap(
            Duration::from_micros(400),
            Duration::from_micros(2_000),
            38,
            budget,
            38,
        );
        let slow = derive_live_cap(
            Duration::from_micros(800),
            Duration::from_micros(8_000),
            38,
            budget,
            38,
        );
        assert_eq!(fast, 38);
        assert_eq!(slow, 14);
        assert!(slow < fast);
        assert!(slow >= LIVE_CAP_FALLBACK);
    }

    #[test]
    fn a_master_that_misses_the_budget_uses_the_fallback() {
        let cap = derive_live_cap(
            Duration::from_micros(4_000),
            Duration::from_micros(8_000),
            38,
            Duration::from_micros(3_600),
            38,
        );
        assert_eq!(cap, LIVE_CAP_FALLBACK);
    }

    #[test]
    fn a_zero_measurement_uses_the_fallback() {
        let cap = derive_live_cap(
            Duration::ZERO,
            Duration::ZERO,
            38,
            Duration::from_millis(4),
            38,
        );
        assert_eq!(cap, LIVE_CAP_FALLBACK);
    }

    #[test]
    fn an_inverted_measurement_over_the_budget_uses_the_fallback() {
        let cap = derive_live_cap(
            Duration::from_micros(5_000),
            Duration::from_micros(4_000),
            38,
            Duration::from_micros(3_600),
            38,
        );
        assert_eq!(cap, LIVE_CAP_FALLBACK);
    }

    #[test]
    fn the_cap_never_passes_the_hard_max() {
        let cap = derive_live_cap(
            Duration::from_micros(1),
            Duration::from_micros(1),
            38,
            Duration::from_secs(1),
            1_000,
        );
        assert_eq!(cap, STRIP_SLOT_COUNT as u32);
    }

    #[test]
    fn a_room_smaller_than_one_strip_stays_on_the_fallback() {
        let cap = derive_live_cap(
            Duration::from_micros(3_590),
            Duration::from_micros(4_350),
            38,
            Duration::from_micros(3_600),
            38,
        );
        assert_eq!(cap, LIVE_CAP_FALLBACK);
    }

    #[test]
    fn the_quantum_budget_is_sixty_seven_and_a_half_percent() {
        let budget = quantum_budget(48_000, 256);
        assert_eq!(budget, Duration::from_nanos(3_600_000));
        let ratio = budget.as_secs_f64() / (256.0 / 48_000.0);
        assert!((ratio - 0.675).abs() < 1.0e-9);
        let other = quantum_budget(44_100, 64);
        let other_ratio = other.as_secs_f64() / (64.0 / 44_100.0);
        assert!((other_ratio - 0.675).abs() < 1.0e-6);
    }

    #[test]
    fn calibration_keeps_the_cap_inside_the_hard_max() {
        let report = calibrate_live_cap(48_000, 256, true, 5.0);
        crate::measured(&format!(
            "MEASURED live cap: cap {} budget {:?} master tries {:?} all-live tries {:?}",
            report.cap, report.budget, report.master_tries, report.all_live_tries
        ));
        assert!(report.cap >= LIVE_CAP_FALLBACK);
        assert!(report.cap <= STRIP_SLOT_COUNT as u32);
        assert_eq!(report.budget, quantum_budget(48_000, 256));
        assert!(!report.master_tries.is_empty());
        assert_eq!(report.master_tries.len(), report.all_live_tries.len());
        let slow_master = report
            .master_tries
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let slow_all = report
            .all_live_tries
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let wide = derive_live_cap(
            slow_master,
            slow_all,
            report.slots,
            slow_all.saturating_mul(2).max(Duration::from_nanos(1)),
            STRIP_SLOT_COUNT as u32,
        );
        let tight = derive_live_cap(
            slow_master,
            slow_all,
            report.slots,
            slow_all / 4,
            STRIP_SLOT_COUNT as u32,
        );
        if !slow_all.is_zero() {
            assert!(tight <= wide);
            assert_eq!(wide, STRIP_SLOT_COUNT as u32);
        }
    }
}
