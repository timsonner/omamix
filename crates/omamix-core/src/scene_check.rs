use crate::comp::{
    COMP_ATTACK_DEFAULT_MS, COMP_KNEE_DEFAULT_DB, COMP_MAKEUP_DEFAULT_DB, COMP_RAMP_SAMPLES,
    COMP_RATIO_DEFAULT, COMP_RELEASE_DEFAULT_MS, COMP_THRESHOLD_DEFAULT_DB,
};
use crate::eq::{
    EQ_BAND_HZ, EQ_BANDS, EQ_BANDWIDTH_OCTAVES, EQ_RAMP_SAMPLES, HPF_DEFAULT_HZ, HPF_MAX_HZ,
    HPF_MIN_HZ, HPF_Q, NYQUIST_MARGIN,
};
use crate::params::STRIP_SLOT_COUNT;

const RATE_MIN: u32 = 8_000;
const RATE_MAX: u32 = 384_000;
const SECTIONS: usize = 1 + EQ_BANDS;
const COEFFS: usize = 5;
const EQ_PACK: usize = SECTIONS * COEFFS;
const COMP_PACK: usize = 7;
const THRESHOLD: usize = 0;
const SLOPE: usize = 1;
const KNEE: usize = 2;
const MAKEUP: usize = 3;
const ATTACK: usize = 4;
const RELEASE: usize = 5;
const WET: usize = 6;

const SAMPLE_ABS_TOL: f64 = 1.0e-5;
const SAMPLE_RMS_TOL: f64 = 1.0e-6;
const CHANNEL_RMS_TOL: f64 = 3.0e-7;
const BIN_180_TOL: f64 = 1.0e-7;
const BIN_500_TOL: f64 = 4.0e-8;
const WINDOW_TOL: f64 = 2.0e-7;

pub(super) struct Render {
    pub left: Vec<f64>,
    pub right: Vec<f64>,
    pub gr: Vec<f64>,
}

#[derive(Clone, Copy)]
struct Step {
    eq_hot: bool,
    comp_on: bool,
    blocks: usize,
    reset_strips: bool,
}

#[derive(Clone, Copy)]
struct Biquad {
    s1: f64,
    s2: f64,
}

impl Biquad {
    const fn new() -> Self {
        Self { s1: 0.0, s2: 0.0 }
    }
}

struct RefEq {
    current: [f64; EQ_PACK],
    target: [f64; EQ_PACK],
    origin: [f64; EQ_PACK],
    pending: [f64; EQ_PACK],
    left: [Biquad; SECTIONS],
    right: [Biquad; SECTIONS],
    pending_rate: u32,
    block_frames: u32,
    ramp_pos: u32,
    live: u16,
    ramping: bool,
    bypass: bool,
    mask_on: bool,
    dirty: bool,
    pending_bypass: bool,
}

struct RefComp {
    current: [f64; COMP_PACK],
    target: [f64; COMP_PACK],
    origin: [f64; COMP_PACK],
    pending: [f64; COMP_PACK],
    env: f64,
    block_min_gr: f64,
    pending_rate: u32,
    block_frames: u32,
    ramp_pos: u32,
    ramping: bool,
    bypass: bool,
    dirty: bool,
    pending_bypass: bool,
}

pub(super) fn strip_wave(slot: usize, len: usize, rate: u32) -> Vec<f32> {
    let freq = 180.0 + slot as f64 * 37.0;
    let step = std::f64::consts::TAU * freq / f64::from(rate);
    let (sin_d, cos_d) = step.sin_cos();
    let mut sin_p = 0.0f64;
    let mut cos_p = 1.0f64;
    let amp = 0.25118864f32;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push((sin_p as f32) * amp);
        let next_sin = sin_p * cos_d + cos_p * sin_d;
        let next_cos = cos_p * cos_d - sin_p * sin_d;
        sin_p = next_sin;
        cos_p = next_cos;
    }
    out
}

pub(super) fn render_settled(rate: u32, frames: usize) -> Render {
    let warmup = 4096usize.div_ceil(frames).max(8);
    let blocks = warmup + 32;
    let steps = [Step {
        eq_hot: true,
        comp_on: true,
        blocks,
        reset_strips: false,
    }];
    render(rate, frames, &steps)
}

pub(super) fn render_transition() -> Render {
    let steps = [
        Step {
            eq_hot: false,
            comp_on: false,
            blocks: 2,
            reset_strips: false,
        },
        Step {
            eq_hot: true,
            comp_on: false,
            blocks: 4,
            reset_strips: false,
        },
        Step {
            eq_hot: false,
            comp_on: false,
            blocks: 2,
            reset_strips: true,
        },
        Step {
            eq_hot: true,
            comp_on: false,
            blocks: 4,
            reset_strips: false,
        },
        Step {
            eq_hot: false,
            comp_on: false,
            blocks: 4,
            reset_strips: false,
        },
        Step {
            eq_hot: false,
            comp_on: true,
            blocks: 1,
            reset_strips: false,
        },
        Step {
            eq_hot: false,
            comp_on: false,
            blocks: 1,
            reset_strips: false,
        },
    ];
    render(48_000, 96, &steps)
}

pub(super) fn assert_near(
    label: &str,
    got: (&[f32], &[f32], &[f32]),
    reference: &Render,
    region: (usize, usize, f64),
    peak_tol: f64,
    gr_tol: f64,
) {
    let (left, right, gr) = got;
    let (skip_frames, frames, rate) = region;
    assert_eq!(left.len(), reference.left.len(), "{label} left length");
    assert_eq!(right.len(), reference.right.len(), "{label} right length");
    assert_eq!(
        gr.len(),
        reference.gr.len(),
        "{label} gain reduction length"
    );
    assert!(frames > 0 && skip_frames <= left.len(), "{label} region");
    let gr_skip = skip_frames / frames;
    let left_a = &left[skip_frames..];
    let right_a = &right[skip_frames..];
    let left_b = &reference.left[skip_frames..];
    let right_b = &reference.right[skip_frames..];
    let gr_a = &gr[gr_skip..];
    let gr_b = &reference.gr[gr_skip..];
    let n = left_a.len();
    assert!(n > 0, "{label} empty region");

    let mut max_abs = 0.0f64;
    let mut sum_sq = 0.0f64;
    for index in 0..n {
        let diff_l = (f64::from(left_a[index]) - left_b[index]).abs();
        let diff_r = (f64::from(right_a[index]) - right_b[index]).abs();
        max_abs = max_abs.max(diff_l).max(diff_r);
        sum_sq += diff_l * diff_l + diff_r * diff_r;
    }
    let sample_rms = (sum_sq / (2.0 * n as f64)).sqrt();
    let peak_l = (peak32(left_a) - peak64(left_b)).abs();
    let peak_r = (peak32(right_a) - peak64(right_b)).abs();
    let rms_l = (rms32(left_a) - rms64(left_b)).abs();
    let rms_r = (rms32(right_a) - rms64(right_b)).abs();
    let bin_180 = (bin_mag32(left_a, rate, 180.0) - bin_mag64(left_b, rate, 180.0)).abs();
    let bin_500 = (bin_mag32(left_a, rate, 500.0) - bin_mag64(left_b, rate, 500.0)).abs();
    let window = max_window_gap(left_a, right_a, left_b, right_b);
    let mut gr_max = 0.0f64;
    for index in 0..gr_a.len() {
        gr_max = gr_max.max((f64::from(gr_a[index]) - gr_b[index]).abs());
    }

    let mut misses = Vec::new();
    push_miss(&mut misses, label, "sample abs", max_abs, SAMPLE_ABS_TOL);
    push_miss(&mut misses, label, "sample rms", sample_rms, SAMPLE_RMS_TOL);
    push_miss(&mut misses, label, "left peak", peak_l, peak_tol);
    push_miss(&mut misses, label, "right peak", peak_r, peak_tol);
    push_miss(&mut misses, label, "left rms", rms_l, CHANNEL_RMS_TOL);
    push_miss(&mut misses, label, "right rms", rms_r, CHANNEL_RMS_TOL);
    push_miss(&mut misses, label, "180 Hz bin", bin_180, BIN_180_TOL);
    push_miss(&mut misses, label, "500 Hz bin", bin_500, BIN_500_TOL);
    push_miss(&mut misses, label, "window energy", window, WINDOW_TOL);
    push_miss(&mut misses, label, "master reduction", gr_max, gr_tol);
    assert!(
        misses.is_empty(),
        "{label} left the reference mix: {misses:?}"
    );
}

fn push_miss(misses: &mut Vec<String>, label: &str, metric: &str, got: f64, tol: f64) {
    if got > tol {
        misses.push(format!("{label} {metric} {got:.6e} > {tol:.6e}"));
    }
}

fn render(rate: u32, frames: usize, steps: &[Step]) -> Render {
    let blocks: usize = steps.iter().map(|step| step.blocks).sum();
    let len = blocks * frames;
    let waves: Vec<Vec<f32>> = (0..STRIP_SLOT_COUNT)
        .map(|slot| strip_wave(slot, len, rate))
        .collect();
    let mut strips_eq: Vec<RefEq> = (0..STRIP_SLOT_COUNT).map(|_| RefEq::new()).collect();
    let mut strips_comp: Vec<RefComp> = (0..STRIP_SLOT_COUNT).map(|_| RefComp::new()).collect();
    let mut master_eq = RefEq::new();
    let mut master_comp = RefComp::new();
    let mut left = Vec::with_capacity(len);
    let mut right = Vec::with_capacity(len);
    let mut gr = Vec::with_capacity(blocks);
    let mut cursor = 0usize;
    for step in steps {
        if step.reset_strips {
            for strip in &mut strips_eq {
                strip.reset_filter();
            }
        }
        let (eq_pack, eq_rate, eq_bypass) = eq_target(rate, step.eq_hot);
        let (comp_pack, comp_rate, comp_bypass) = comp_target(rate, step.comp_on);
        for strip in &mut strips_eq {
            strip.publish(eq_pack, eq_rate, eq_bypass);
        }
        master_eq.publish(eq_pack, eq_rate, eq_bypass);
        for strip in &mut strips_comp {
            strip.publish(comp_pack, comp_rate, comp_bypass);
        }
        master_comp.publish(comp_pack, comp_rate, comp_bypass);
        for _ in 0..step.blocks {
            for strip in &mut strips_eq {
                strip.begin(frames, rate);
            }
            for strip in &mut strips_comp {
                strip.begin(frames, rate);
            }
            master_eq.begin(frames, rate);
            master_comp.begin(frames, rate);
            let mut block_l = vec![0.0f64; frames];
            let mut block_r = vec![0.0f64; frames];
            for frame in 0..frames {
                let mut acc_l = 0.0f32;
                let mut acc_r = 0.0f32;
                for slot in 0..STRIP_SLOT_COUNT {
                    let input = f64::from(waves[slot][cursor + frame]);
                    let (eq_l, eq_r) = strips_eq[slot].tick(input, input);
                    let (comp_l, comp_r) = strips_comp[slot].tick(eq_l, eq_r);
                    acc_l += comp_l as f32;
                    acc_r += comp_r as f32;
                }
                block_l[frame] = f64::from(finite32(acc_l));
                block_r[frame] = f64::from(finite32(acc_r));
            }
            master_eq.process(&mut block_l, &mut block_r);
            let block_gr = master_comp.process(&mut block_l, &mut block_r);
            left.extend_from_slice(&block_l);
            right.extend_from_slice(&block_r);
            gr.push(block_gr);
            cursor += frames;
        }
    }
    Render { left, right, gr }
}

impl RefEq {
    fn new() -> Self {
        let identity = identity_pack();
        Self {
            current: identity,
            target: identity,
            origin: identity,
            pending: identity,
            left: [Biquad::new(); SECTIONS],
            right: [Biquad::new(); SECTIONS],
            pending_rate: 48_000,
            block_frames: 1,
            ramp_pos: 0,
            live: 0,
            ramping: false,
            bypass: true,
            mask_on: false,
            dirty: false,
            pending_bypass: true,
        }
    }

    fn publish(&mut self, pack: [f64; EQ_PACK], rate: u32, bypass: bool) {
        self.pending = pack;
        self.pending_rate = rate;
        self.pending_bypass = bypass;
        self.dirty = true;
    }

    fn reset_filter(&mut self) {
        let identity = identity_pack();
        self.current = identity;
        self.target = identity;
        self.origin = identity;
        self.ramp_pos = 0;
        self.ramping = false;
        self.bypass = true;
        self.mask_on = false;
        self.live = 0;
        self.clear_state();
    }

    fn begin(&mut self, frames: usize, rate: u32) {
        self.block_frames = frames.max(1) as u32;
        let previous = self.target;
        if self.dirty {
            self.dirty = false;
            if self.pending_rate == rate || self.pending_bypass {
                self.target = self.pending;
            }
        }
        if self.current == self.target {
            self.ramping = false;
            self.bypass = pack_is_bypass(&self.current);
            if self.bypass {
                self.clear_state();
            }
        } else if self.ramping && self.target == previous {
            self.bypass = false;
        } else {
            self.origin = self.current;
            self.ramp_pos = 0;
            self.ramping = true;
            self.bypass = false;
        }
        self.arm_mask();
    }

    fn arm_mask(&mut self) {
        if self.ramping || self.bypass {
            self.mask_on = false;
            return;
        }
        let live = live_mask(&self.current);
        for section in 0..SECTIONS {
            if live & (1u16 << section) == 0 {
                self.left[section] = Biquad::new();
                self.right[section] = Biquad::new();
            }
        }
        self.live = live;
        self.mask_on = true;
    }

    fn tick(&mut self, left: f64, right: f64) -> (f64, f64) {
        if self.bypass {
            return (left, right);
        }
        let pack = self.pack_for();
        if self.bypass {
            return (left, right);
        }
        let live = if self.mask_on { Some(self.live) } else { None };
        let left_in = f64::from(finite32(left as f32));
        let right_in = f64::from(finite32(right as f32));
        let left = cascade(&pack, &mut self.left, left_in, live);
        let right = cascade(&pack, &mut self.right, right_in, live);
        (f64::from(left as f32), f64::from(right as f32))
    }

    fn process(&mut self, left: &mut [f64], right: &mut [f64]) {
        if self.bypass {
            return;
        }
        let frames = left.len().min(right.len()).min(self.block_frames as usize);
        for frame in 0..frames {
            let (sample_l, sample_r) = self.tick(left[frame], right[frame]);
            left[frame] = sample_l;
            right[frame] = sample_r;
        }
    }

    fn pack_for(&mut self) -> [f64; EQ_PACK] {
        if !self.ramping {
            return self.current;
        }
        let pos = self.ramp_pos.saturating_add(1);
        self.ramp_pos = pos;
        if pos >= EQ_RAMP_SAMPLES.max(1) {
            self.current = self.target;
            self.ramping = false;
            if pack_is_bypass(&self.current) {
                self.clear_state();
                self.bypass = true;
            }
            return self.current;
        }
        let t = f64::from(pos) / f64::from(EQ_RAMP_SAMPLES.max(1));
        self.current = lerp_eq(self.origin, self.target, t);
        self.current
    }

    fn clear_state(&mut self) {
        self.left = [Biquad::new(); SECTIONS];
        self.right = [Biquad::new(); SECTIONS];
    }
}

impl RefComp {
    fn new() -> Self {
        let (pack, rate, bypass) = comp_target(48_000, false);
        Self {
            current: pack,
            target: pack,
            origin: pack,
            pending: pack,
            env: 0.0,
            block_min_gr: 0.0,
            pending_rate: rate,
            block_frames: 1,
            ramp_pos: 0,
            ramping: false,
            bypass,
            dirty: false,
            pending_bypass: bypass,
        }
    }

    fn publish(&mut self, pack: [f64; COMP_PACK], rate: u32, bypass: bool) {
        self.pending = pack;
        self.pending_rate = rate;
        self.pending_bypass = bypass;
        self.dirty = true;
    }

    fn begin(&mut self, frames: usize, rate: u32) {
        self.block_frames = frames.max(1) as u32;
        self.block_min_gr = 0.0;
        let previous = self.target;
        if self.dirty {
            self.dirty = false;
            if self.pending_rate == rate || self.pending_bypass {
                self.target = self.pending;
            }
        }
        if self.current == self.target {
            self.ramping = false;
            self.bypass = comp_is_unity(&self.current);
        } else if self.ramping && self.target == previous {
            self.bypass = false;
        } else {
            self.origin = self.current;
            self.ramp_pos = 0;
            self.ramping = true;
            self.bypass = false;
        }
    }

    fn tick(&mut self, left: f64, right: f64) -> (f64, f64) {
        let pack = self.pack_for();
        let left_s = f64::from(finite32(left as f32));
        let right_s = f64::from(finite32(right as f32));
        let level = left_s.abs().max(right_s.abs());
        self.env = follow(self.env, level, pack[ATTACK], pack[RELEASE]);
        if self.bypass || comp_is_unity(&pack) {
            return (left, right);
        }
        let gr = curve(self.env, pack[THRESHOLD], pack[SLOPE], pack[KNEE]);
        let wet = pack[WET];
        let sample_gr = if wet > 0.0 { gr } else { 0.0 };
        if sample_gr < self.block_min_gr {
            self.block_min_gr = sample_gr;
        }
        let compressed = linear_from_db(gr) * pack[MAKEUP];
        let gain = 1.0 + (compressed - 1.0) * wet;
        if gain == 1.0 {
            return (left_s, right_s);
        }
        let gain = finite32(gain as f32);
        let out_l = finite32((left_s as f32) * gain);
        let out_r = finite32((right_s as f32) * gain);
        (f64::from(out_l), f64::from(out_r))
    }

    fn process(&mut self, left: &mut [f64], right: &mut [f64]) -> f64 {
        let frames = left.len().min(right.len()).min(self.block_frames as usize);
        for frame in 0..frames {
            let (sample_l, sample_r) = self.tick(left[frame], right[frame]);
            left[frame] = sample_l;
            right[frame] = sample_r;
        }
        if frames > 0 { self.block_min_gr } else { 0.0 }
    }

    fn pack_for(&mut self) -> [f64; COMP_PACK] {
        if !self.ramping {
            return self.current;
        }
        let pos = self.ramp_pos.saturating_add(1);
        self.ramp_pos = pos;
        if pos >= COMP_RAMP_SAMPLES.max(1) {
            self.current = self.target;
            self.ramping = false;
            self.bypass = comp_is_unity(&self.current);
            return self.current;
        }
        let t = pos as f32 / COMP_RAMP_SAMPLES.max(1) as f32;
        self.current = lerp_comp(self.origin, self.target, t);
        self.bypass = false;
        self.current
    }
}

fn finite32(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

fn identity_pack() -> [f64; EQ_PACK] {
    let mut pack = [0.0; EQ_PACK];
    for section in 0..SECTIONS {
        pack[section * COEFFS] = 1.0;
    }
    pack
}

fn pack_is_bypass(pack: &[f64; EQ_PACK]) -> bool {
    (0..SECTIONS).all(|section| section_is_identity(pack, section))
}

fn section_is_identity(pack: &[f64; EQ_PACK], section: usize) -> bool {
    let at = section * COEFFS;
    pack[at] == 1.0
        && pack[at + 1] == 0.0
        && pack[at + 2] == 0.0
        && pack[at + 3] == 0.0
        && pack[at + 4] == 0.0
}

fn live_mask(pack: &[f64; EQ_PACK]) -> u16 {
    let mut live = 0u16;
    for section in 0..SECTIONS {
        if !section_is_identity(pack, section) {
            live |= 1u16 << section;
        }
    }
    live
}

fn lerp_eq(from: [f64; EQ_PACK], to: [f64; EQ_PACK], t: f64) -> [f64; EQ_PACK] {
    let mut out = [0.0; EQ_PACK];
    for index in 0..EQ_PACK {
        out[index] = from[index] + (to[index] - from[index]) * t;
    }
    out
}

fn lerp_comp(from: [f64; COMP_PACK], to: [f64; COMP_PACK], t: f32) -> [f64; COMP_PACK] {
    let mut out = [0.0; COMP_PACK];
    for index in 0..COMP_PACK {
        let y = from[index] as f32 + (to[index] as f32 - from[index] as f32) * t;
        out[index] = f64::from(y);
    }
    out
}

fn cascade(
    pack: &[f64; EQ_PACK],
    state: &mut [Biquad; SECTIONS],
    input: f64,
    live: Option<u16>,
) -> f64 {
    let mut y = if input.is_finite() { input } else { 0.0 };
    for (section, memory) in state.iter_mut().enumerate() {
        if live.is_some_and(|mask| mask & (1u16 << section) == 0) {
            continue;
        }
        if live.is_none() && section_is_identity(pack, section) {
            memory.s1 = 0.0;
            memory.s2 = 0.0;
            continue;
        }
        let at = section * COEFFS;
        let out = pack[at] * y + memory.s1;
        let out = if out.is_finite() { out } else { 0.0 };
        memory.s1 = flush64(pack[at + 1] * y - pack[at + 3] * out + memory.s2);
        memory.s2 = flush64(pack[at + 2] * y - pack[at + 4] * out);
        y = out;
    }
    y
}

fn flush64(value: f64) -> f64 {
    if value.is_finite() {
        if value.abs() < 1.0e-15 { 0.0 } else { value }
    } else {
        0.0
    }
}

fn eq_target(rate: u32, hot: bool) -> ([f64; EQ_PACK], u32, bool) {
    let mut pack = identity_pack();
    if hot {
        write_section(&mut pack, 0, highpass(rate, HPF_DEFAULT_HZ, HPF_Q));
    }
    for (index, freq) in EQ_BAND_HZ.iter().enumerate() {
        let gain = if hot && index == 4 { 6.0 } else { 0.0 };
        write_section(&mut pack, index + 1, peaking(rate, *freq, gain));
    }
    let bypass = pack_is_bypass(&pack);
    (pack, rate, bypass)
}

fn write_section(pack: &mut [f64; EQ_PACK], index: usize, section: [f64; COEFFS]) {
    let at = index * COEFFS;
    pack[at..at + COEFFS].copy_from_slice(&section);
}

fn comp_target(rate: u32, enabled: bool) -> ([f64; COMP_PACK], u32, bool) {
    let mut pack = [0.0; COMP_PACK];
    pack[THRESHOLD] = f64::from(COMP_THRESHOLD_DEFAULT_DB);
    pack[SLOPE] = if enabled {
        f64::from(1.0f32 - 1.0f32 / COMP_RATIO_DEFAULT)
    } else {
        0.0
    };
    pack[KNEE] = if enabled {
        f64::from(COMP_KNEE_DEFAULT_DB)
    } else {
        0.0
    };
    pack[MAKEUP] = if enabled {
        f64::from(10f32.powf(COMP_MAKEUP_DEFAULT_DB / 20.0))
    } else {
        1.0
    };
    pack[ATTACK] = f64::from(time_coeff(rate, COMP_ATTACK_DEFAULT_MS * 0.001));
    pack[RELEASE] = f64::from(time_coeff(rate, COMP_RELEASE_DEFAULT_MS * 0.001));
    pack[WET] = if enabled { 1.0 } else { 0.0 };
    let bypass = pack[WET] == 0.0;
    (pack, rate, bypass)
}

fn time_coeff(sample_rate: u32, seconds: f32) -> f32 {
    if sample_rate == 0 || !seconds.is_finite() || seconds <= 0.0 {
        return 1.0;
    }
    let denom = seconds * sample_rate as f32;
    if !denom.is_finite() || denom <= 0.0 {
        return 1.0;
    }
    let coeff = 1.0 - (-1.0 / denom).exp();
    if coeff.is_finite() {
        coeff.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn comp_is_unity(pack: &[f64; COMP_PACK]) -> bool {
    pack[WET] == 0.0 || (pack[SLOPE] == 0.0 && pack[MAKEUP] == 1.0)
}

fn follow(env: f64, level: f64, attack: f64, release: f64) -> f64 {
    let env = finite32(env as f32);
    let level = finite32(level as f32);
    let env = if env >= 0.0 { env } else { 0.0 };
    let level = if level >= 0.0 { level } else { 0.0 };
    let coeff = if level > env {
        attack as f32
    } else {
        release as f32
    };
    let coeff = if coeff.is_finite() {
        coeff.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let y = env + coeff * (level - env);
    let y = if y.is_finite() {
        if y.abs() < 1.0e-15 { 0.0 } else { y }
    } else {
        level
    };
    f64::from(y)
}

fn curve(level: f64, threshold: f64, slope: f64, knee: f64) -> f64 {
    let level = level as f32;
    let threshold_db = threshold as f32;
    let slope = slope as f32;
    let knee_db = knee as f32;
    if slope <= 0.0 || !level.is_finite() || level <= 0.0 {
        return 0.0;
    }
    let x = 20.0 * level.log10();
    if !x.is_finite() {
        return 0.0;
    }
    let gr = if knee_db <= 0.0 {
        let over = x - threshold_db;
        if over <= 0.0 { 0.0 } else { -over * slope }
    } else {
        let half = knee_db * 0.5;
        let lower = threshold_db - half;
        if x <= lower {
            0.0
        } else if x >= threshold_db + half {
            -(x - threshold_db) * slope
        } else {
            let t = x - lower;
            -slope * t * t / (2.0 * knee_db)
        }
    };
    let gr = if gr.is_finite() { gr.min(0.0) } else { 0.0 };
    f64::from(gr)
}

fn linear_from_db(db: f64) -> f64 {
    let db = db as f32;
    if db == 0.0 {
        return 1.0;
    }
    if !db.is_finite() {
        return 0.0;
    }
    let lin = 10f32.powf(db / 20.0);
    f64::from(if lin.is_finite() { lin } else { 0.0 })
}

fn usable_rate(sample_rate: u32) -> Option<u32> {
    if (RATE_MIN..RATE_MAX).contains(&sample_rate) {
        Some(sample_rate)
    } else {
        None
    }
}

fn center_disabled(sample_rate: u32, freq_hz: f32) -> bool {
    let Some(rate) = usable_rate(sample_rate) else {
        return true;
    };
    let nyquist = f64::from(rate) * 0.5;
    let freq = f64::from(freq_hz);
    !freq.is_finite() || freq <= 0.0 || freq >= nyquist * f64::from(NYQUIST_MARGIN)
}

fn sanitize_hz(hz: f32) -> f32 {
    if hz.is_finite() {
        hz.clamp(HPF_MIN_HZ, HPF_MAX_HZ)
    } else {
        HPF_DEFAULT_HZ
    }
}

fn omega(sample_rate: u32, freq_hz: f32) -> Option<(f64, f64, f64)> {
    if center_disabled(sample_rate, freq_hz) {
        return None;
    }
    let rate = usable_rate(sample_rate)?;
    let w0 = std::f64::consts::TAU * f64::from(freq_hz) / f64::from(rate);
    let (sin_w, cos_w) = w0.sin_cos();
    if sin_w.abs() < 1.0e-12 || !sin_w.is_finite() || !cos_w.is_finite() {
        None
    } else {
        Some((w0, sin_w, cos_w))
    }
}

fn octave_alpha(sin_w: f64, w0: f64, bandwidth: f64) -> Option<f64> {
    if bandwidth <= 0.0 || !bandwidth.is_finite() {
        return None;
    }
    let shaped = ((std::f64::consts::LN_2 / 2.0) * bandwidth * w0 / sin_w).sinh();
    let alpha = sin_w * shaped;
    if alpha.is_finite() && alpha > 0.0 {
        Some(alpha)
    } else {
        None
    }
}

fn normalize(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> [f64; COEFFS] {
    if !a0.is_finite() || a0.abs() < 1.0e-18 {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    }
    let inv = 1.0 / a0;
    let coeffs = [b0 * inv, b1 * inv, b2 * inv, a1 * inv, a2 * inv];
    if coeffs.iter().all(|value| value.is_finite()) {
        coeffs
    } else {
        [1.0, 0.0, 0.0, 0.0, 0.0]
    }
}

fn peaking(sample_rate: u32, freq_hz: f32, gain_db: f32) -> [f64; COEFFS] {
    if gain_db.abs() < 1.0e-6 || center_disabled(sample_rate, freq_hz) {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    }
    let Some((w0, sin_w, cos_w)) = omega(sample_rate, freq_hz) else {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    };
    let Some(alpha) = octave_alpha(sin_w, w0, f64::from(EQ_BANDWIDTH_OCTAVES)) else {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    };
    let a = 10f64.powf(f64::from(gain_db) / 40.0);
    if !a.is_finite() || a <= 0.0 {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    }
    normalize(
        1.0 + alpha * a,
        -2.0 * cos_w,
        1.0 - alpha * a,
        1.0 + alpha / a,
        -2.0 * cos_w,
        1.0 - alpha / a,
    )
}

fn highpass(sample_rate: u32, freq_hz: f32, q: f32) -> [f64; COEFFS] {
    let freq_hz = sanitize_hz(freq_hz);
    if !q.is_finite() || q <= 0.0 {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    }
    let Some((_w0, sin_w, cos_w)) = omega(sample_rate, freq_hz) else {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    };
    let alpha = sin_w / (2.0 * f64::from(q));
    if !alpha.is_finite() || alpha <= 0.0 {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    }
    normalize(
        (1.0 + cos_w) * 0.5,
        -(1.0 + cos_w),
        (1.0 + cos_w) * 0.5,
        1.0 + alpha,
        -2.0 * cos_w,
        1.0 - alpha,
    )
}

fn peak32(samples: &[f32]) -> f64 {
    samples
        .iter()
        .fold(0.0f32, |max, sample| max.max(sample.abs()))
        .into()
}

fn peak64(samples: &[f64]) -> f64 {
    samples
        .iter()
        .fold(0.0, |max, sample| max.max(sample.abs()))
}

fn rms32(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>();
    (sum / samples.len() as f64).sqrt()
}

fn rms64(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum = samples.iter().map(|sample| sample * sample).sum::<f64>();
    (sum / samples.len() as f64).sqrt()
}

fn bin_mag32(samples: &[f32], rate: f64, freq: f64) -> f64 {
    bin_mag(samples.len(), rate, freq, |index| f64::from(samples[index]))
}

fn bin_mag64(samples: &[f64], rate: f64, freq: f64) -> f64 {
    bin_mag(samples.len(), rate, freq, |index| samples[index])
}

fn bin_mag(len: usize, rate: f64, freq: f64, sample_at: impl Fn(usize) -> f64) -> f64 {
    if len == 0 || rate <= 0.0 {
        return 0.0;
    }
    let w = std::f64::consts::TAU * freq / rate;
    let mut re = 0.0;
    let mut im = 0.0;
    for index in 0..len {
        let angle = w * index as f64;
        let (sin_a, cos_a) = angle.sin_cos();
        let sample = sample_at(index);
        re += sample * cos_a;
        im -= sample * sin_a;
    }
    (re * re + im * im).sqrt() / len as f64
}

fn max_window_gap(left_a: &[f32], right_a: &[f32], left_b: &[f64], right_b: &[f64]) -> f64 {
    let n = left_a.len();
    if n == 0 {
        return 0.0;
    }
    let mut max_gap = 0.0f64;
    for window in 0..4 {
        let start = window * n / 4;
        let end = (window + 1) * n / 4;
        let span = (end - start).max(1) as f64;
        let energy32 = |buf: &[f32]| -> f64 {
            buf[start..end]
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>()
                / span
        };
        let energy64 = |buf: &[f64]| -> f64 {
            buf[start..end]
                .iter()
                .map(|sample| sample * sample)
                .sum::<f64>()
                / span
        };
        let gap = ((energy32(left_a) - energy64(left_b)).abs()
            + (energy32(right_a) - energy64(right_b)).abs())
            * 0.5;
        max_gap = max_gap.max(gap);
    }
    max_gap
}
