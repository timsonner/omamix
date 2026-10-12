//! Fader gesture rules. The widget in the UI crate calls these with raw wheel events.
//!
//! The finite track is −60 dB to +12 dB. One wheel notch below −60 dB is −∞ (mute).

use crate::db::{self, BelowMin};

/// Lowest finite detent. The next notch down is mute.
pub const FADER_MIN_DB: f32 = -60.0;
/// Loudest fader boost.
pub const FADER_MAX_DB: f32 = 12.0;

/// Decibels added for one whole notch. The only step size.
pub const WHEEL_STEP_DB: f32 = 0.5;

/// Point deltas that add up to one notch.
///
/// egui's native `InputOptions::line_scroll_speed` is 40, so one mouse-wheel
/// line and 40 points are the same distance. Trackpad events are a few points
/// each and have to accumulate before they step.
pub const POINTS_PER_NOTCH: f32 = 40.0;

/// Unit of a raw mouse-wheel event (egui `MouseWheelUnit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelUnit {
    /// Discrete lines. One line is one notch (a mouse wheel detent).
    Line,
    /// Logical pixels. [`POINTS_PER_NOTCH`] pixels are one notch.
    Point,
    /// Pages. One page is one notch, so a page key does not dump the fader.
    Page,
}

/// Fractional notches in one raw event.
///
/// Positive is wheel-up and raises the fader. egui's positive Y is that direction.
pub fn fractional_notches(unit: WheelUnit, delta_y: f32) -> f32 {
    if !delta_y.is_finite() || delta_y == 0.0 {
        return 0.0;
    }
    match unit {
        WheelUnit::Line | WheelUnit::Page => delta_y,
        WheelUnit::Point => delta_y / POINTS_PER_NOTCH,
    }
}

/// Seconds of silence after which a leftover fraction is dropped.
///
/// A trackpad remainder otherwise sits until the next event in the same
/// direction, which swallows the first detent of a later gesture.
pub const WHEEL_IDLE_SECS: f64 = 0.2;

/// Leftover scroll that has not yet reached a whole notch.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WheelAccum {
    pending: f32,
    last_secs: Option<f64>,
}

impl WheelAccum {
    /// Add one event. Returns the whole notches to apply. The fraction stays here.
    ///
    /// A delta whose sign differs from the leftover resets that leftover first,
    /// so one detent the other way is a step and not absorbed by the fraction.
    pub fn push(&mut self, unit: WheelUnit, delta_y: f32) -> i32 {
        let frac = fractional_notches(unit, delta_y);
        if frac != 0.0 && self.pending != 0.0 && frac.signum() != self.pending.signum() {
            self.pending = 0.0;
        }
        self.pending += frac;
        self.take_whole()
    }

    /// [`Self::push`], after dropping the leftover when `now_secs` is more than
    /// [`WHEEL_IDLE_SECS`] past the previous event.
    pub fn push_at(&mut self, unit: WheelUnit, delta_y: f32, now_secs: f64) -> i32 {
        if let Some(last) = self.last_secs
            && now_secs.is_finite()
            && last.is_finite()
            && now_secs - last > WHEEL_IDLE_SECS
        {
            self.pending = 0.0;
        }
        let notches = self.push(unit, delta_y);
        if fractional_notches(unit, delta_y) != 0.0 && now_secs.is_finite() {
            self.last_secs = Some(now_secs);
        }
        notches
    }

    fn take_whole(&mut self) -> i32 {
        if !self.pending.is_finite() {
            self.pending = 0.0;
            return 0;
        }
        let whole = self.pending.trunc();
        if whole.abs() > 1_000.0 {
            self.pending = 0.0;
            return 0;
        }
        let notches = whole as i32;
        self.pending -= whole;
        notches
    }

    pub fn pending(self) -> f32 {
        self.pending
    }
}

/// NaN becomes mute. +∞ clamps to [`FADER_MAX_DB`]. Below [`FADER_MIN_DB`] is mute.
pub fn sanitize_fader_db(db: f32) -> f32 {
    db::sanitize_db(db, FADER_MIN_DB, FADER_MAX_DB, BelowMin::Mute)
}

/// Apply raw-wheel notches.
///
/// The value changes only when the pointer is over this fader and the fader
/// has focus. A fader that was clicked earlier and is no longer under the
/// pointer does not move. Each notch is [`WHEEL_STEP_DB`]. One notch below
/// [`FADER_MIN_DB`] is −∞. The result is never NaN.
pub fn apply_wheel(db: f32, hovered: bool, focused: bool, notches: i32) -> f32 {
    if !hovered || !focused || notches == 0 {
        return db;
    }
    step_fader(sanitize_fader_db(db), notches)
}

fn step_fader(db: f32, notches: i32) -> f32 {
    let db = sanitize_fader_db(db);
    if notches == 0 {
        return db;
    }
    if db.is_infinite() && db.is_sign_negative() {
        if notches < 0 {
            return f32::NEG_INFINITY;
        }
        let raised = FADER_MIN_DB + (notches - 1) as f32 * WHEEL_STEP_DB;
        return sanitize_fader_db(raised);
    }
    sanitize_fader_db(db + notches as f32 * WHEEL_STEP_DB)
}

/// Pointer gestures. A click that only focuses the fader leaves the value alone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerGesture {
    FocusClick,
    /// `delta_y` is egui's drag delta: positive means the pointer moved down.
    Drag {
        delta_y: f32,
        height: f32,
    },
}

/// Apply a pointer gesture. [`PointerGesture::FocusClick`] returns `db` unchanged.
pub fn apply_pointer(db: f32, gesture: PointerGesture) -> f32 {
    match gesture {
        PointerGesture::FocusClick => db,
        PointerGesture::Drag { delta_y, height } => apply_drag(db, delta_y, height),
    }
}

fn apply_drag(db: f32, delta_y: f32, height: f32) -> f32 {
    if !delta_y.is_finite() || !height.is_finite() || height <= 0.0 || delta_y == 0.0 {
        return sanitize_fader_db(db);
    }
    let start = sanitize_fader_db(db);
    let span = FADER_MAX_DB - FADER_MIN_DB;
    let db_per_px = span / height;
    if start.is_infinite() && start.is_sign_negative() {
        if delta_y > 0.0 {
            return f32::NEG_INFINITY;
        }
        return sanitize_fader_db(FADER_MIN_DB - delta_y * db_per_px);
    }
    sanitize_fader_db(start - delta_y * db_per_px)
}

/// Map a pointer position inside a vertical fader to dB.
/// `t` is 0 at the bottom of the track and 1 at the top.
/// NaN is mute. +∞ is the max boost. This is not used for a focus click.
pub fn db_from_track_pos(t: f32) -> f32 {
    if t.is_nan() {
        return f32::NEG_INFINITY;
    }
    if !t.is_finite() {
        return if t.is_sign_positive() {
            FADER_MAX_DB
        } else {
            f32::NEG_INFINITY
        };
    }
    let t = t.clamp(0.0, 1.0);
    FADER_MIN_DB + t * (FADER_MAX_DB - FADER_MIN_DB)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn wheel_requires_hover_and_focus() {
        let start = -12.0;
        assert_eq!(apply_wheel(start, false, true, 1), start);
        assert_eq!(apply_wheel(start, true, false, 1), start);
        assert_eq!(apply_wheel(start, false, false, 1), start);
        assert_eq!(apply_wheel(start, true, true, 0), start);
    }

    #[test]
    fn each_notch_is_half_a_decibel() {
        assert!((WHEEL_STEP_DB - 0.5).abs() < f32::EPSILON);
        assert_eq!(apply_wheel(-12.0, true, true, 1), -11.5);
        assert_eq!(apply_wheel(-12.0, true, true, -1), -12.5);
        assert_eq!(apply_wheel(-12.0, true, true, 3), -10.5);
    }

    #[test]
    fn mouse_notch_is_one_step_and_wheel_up_raises() {
        let mut up = WheelAccum::default();
        assert_eq!(up.push(WheelUnit::Line, 1.0), 1);
        assert_eq!(up.pending(), 0.0);
        assert_eq!(apply_wheel(-12.0, true, true, 1), -12.0 + WHEEL_STEP_DB);

        let mut down = WheelAccum::default();
        assert_eq!(down.push(WheelUnit::Line, -1.0), -1);
        assert_eq!(apply_wheel(-12.0, true, true, -1), -12.0 - WHEEL_STEP_DB);

        let mut page = WheelAccum::default();
        assert_eq!(page.push(WheelUnit::Page, 1.0), 1);
        assert_eq!(page.push(WheelUnit::Line, 0.0), 0);
    }

    #[test]
    fn small_trackpad_deltas_accumulate_to_one_notch() {
        let mut accum = WheelAccum::default();
        let mut notches = 0;
        // Nine events of 4 points are 36, short of POINTS_PER_NOTCH (40).
        for _ in 0..9 {
            notches += accum.push(WheelUnit::Point, 4.0);
        }
        assert_eq!(notches, 0);
        assert!(accum.pending() > 0.0 && accum.pending() < 1.0);
        notches += accum.push(WheelUnit::Point, 4.0);
        assert_eq!(notches, 1);
        assert!(accum.pending().abs() < 1e-4);

        // A swipe of many small events is a couple of notches, not one per event.
        let mut swipe = WheelAccum::default();
        let mut total = 0;
        for _ in 0..20 {
            total += swipe.push(WheelUnit::Point, 5.0);
        }
        assert_eq!(total, 2);
        let db = apply_wheel(0.0, true, true, total);
        assert!((db - total as f32 * WHEEL_STEP_DB).abs() < 1e-4);
        assert!(db < 10.0);
    }

    #[test]
    fn direction_change_drops_the_remainder() {
        let mut accum = WheelAccum::default();
        assert_eq!(accum.push(WheelUnit::Point, 30.0), 0);
        assert!((accum.pending() - 0.75).abs() < 1e-3);
        // +0.75 pending must not swallow the first notch the other way.
        assert_eq!(accum.push(WheelUnit::Line, -1.0), -1);
        assert_eq!(accum.pending(), 0.0);
    }

    #[test]
    fn idle_drops_the_remainder() {
        let mut accum = WheelAccum::default();
        assert_eq!(accum.push_at(WheelUnit::Point, 30.0, 1.0), 0);
        assert!(accum.pending() > 0.0);
        let notches = accum.push_at(WheelUnit::Point, 4.0, 1.0 + WHEEL_IDLE_SECS + 0.05);
        assert_eq!(notches, 0);
        assert!((accum.pending() - 4.0 / POINTS_PER_NOTCH).abs() < 1e-4);
    }

    #[test]
    fn trackpad_direction_follows_the_sign() {
        let mut up = WheelAccum::default();
        assert_eq!(up.push(WheelUnit::Point, POINTS_PER_NOTCH), 1);
        let mut down = WheelAccum::default();
        assert_eq!(down.push(WheelUnit::Point, -POINTS_PER_NOTCH), -1);
        let mut partial = WheelAccum::default();
        assert_eq!(partial.push(WheelUnit::Point, -POINTS_PER_NOTCH / 2.0), 0);
        assert_eq!(partial.push(WheelUnit::Point, -POINTS_PER_NOTCH / 2.0), -1);
    }

    #[test]
    fn scrolling_elsewhere_does_not_move_a_focused_fader() {
        // Last click left focus here. The pointer is over some other widget.
        let start = -6.0;
        let mut accum = WheelAccum::default();
        let notches = accum.push(WheelUnit::Line, 1.0);
        assert_eq!(notches, 1);
        assert_eq!(apply_wheel(start, false, true, notches), start);
    }

    #[test]
    fn wheel_clamps_to_fader_range() {
        assert_eq!(apply_wheel(FADER_MAX_DB, true, true, 4), FADER_MAX_DB);
        assert_eq!(apply_wheel(FADER_MIN_DB, true, true, -1), f32::NEG_INFINITY);
        assert_eq!(
            apply_wheel(f32::NEG_INFINITY, true, true, -4),
            f32::NEG_INFINITY
        );
    }

    #[test]
    fn one_notch_below_the_floor_is_mute_and_one_notch_up_returns() {
        assert_eq!(apply_wheel(-59.5, true, true, -1), FADER_MIN_DB);
        assert_eq!(apply_wheel(FADER_MIN_DB, true, true, -1), f32::NEG_INFINITY);
        assert_eq!(apply_wheel(f32::NEG_INFINITY, true, true, 1), FADER_MIN_DB);
        assert_eq!(apply_wheel(f32::NEG_INFINITY, true, true, 2), -59.5);
    }

    #[test]
    fn nan_and_infinities_stay_out_of_the_fader() {
        assert!(
            sanitize_fader_db(f32::NAN).is_infinite()
                && sanitize_fader_db(f32::NAN).is_sign_negative()
        );
        assert_eq!(sanitize_fader_db(f32::INFINITY), FADER_MAX_DB);
        assert_eq!(sanitize_fader_db(f32::NEG_INFINITY), f32::NEG_INFINITY);
        assert_eq!(apply_wheel(f32::NAN, true, true, 1), FADER_MIN_DB);
        assert_eq!(apply_wheel(f32::INFINITY, true, true, 1), FADER_MAX_DB);
        assert!(apply_wheel(f32::NAN, false, true, 1).is_nan());
        assert!(
            db_from_track_pos(f32::NAN).is_infinite()
                && db_from_track_pos(f32::NAN).is_sign_negative()
        );
        assert_eq!(db_from_track_pos(f32::INFINITY), FADER_MAX_DB);
        assert_eq!(db_from_track_pos(f32::NEG_INFINITY), f32::NEG_INFINITY);
        let dragged = apply_pointer(
            f32::NAN,
            PointerGesture::Drag {
                delta_y: 0.0,
                height: 220.0,
            },
        );
        assert!(dragged.is_infinite() && dragged.is_sign_negative());
        assert!(db::db_to_linear(sanitize_fader_db(f32::NAN)).is_finite());
        assert!(db::db_to_linear(sanitize_fader_db(f32::INFINITY)).is_finite());
    }

    #[test]
    fn focus_click_does_not_move_the_fader() {
        assert_eq!(apply_pointer(-12.0, PointerGesture::FocusClick), -12.0);
        assert_eq!(apply_pointer(0.0, PointerGesture::FocusClick), 0.0);
        assert_eq!(
            apply_pointer(f32::NEG_INFINITY, PointerGesture::FocusClick),
            f32::NEG_INFINITY
        );
        let dragged = apply_pointer(
            0.0,
            PointerGesture::Drag {
                delta_y: 10.0,
                height: 220.0,
            },
        );
        assert!(dragged < 0.0);
        assert!(dragged.is_finite());
    }

    #[test]
    fn track_ends_match_range() {
        assert_eq!(db_from_track_pos(0.0), FADER_MIN_DB);
        assert_eq!(db_from_track_pos(1.0), FADER_MAX_DB);
        assert_eq!(db_from_track_pos(0.5), (FADER_MIN_DB + FADER_MAX_DB) / 2.0);
    }
}
