//! Level conversions.
//!
//! PipeWire stores `channelVolumes` as linear amplitude. The slider fraction
//! pactl prints (and the percent in pavucontrol) is cubic: `linear = cubic³`.
//! A cubic slider at 0.40 is `20·log10(0.40³) ≈ −23.88 dB`, which is the
//! "40% is about −24 dB" rule.

/// Loudest finite boost [`db_to_linear`] will emit. Matches input-trim headroom.
pub const MAX_BOOST_DB: f32 = 24.0;

/// What to do with a finite dB value below the control's floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BelowMin {
    /// Keep the floor (input trim).
    Clamp,
    /// One step below the floor is mute (the channel fader).
    Mute,
}

/// NaN and −∞ become −∞ (mute). +∞ and anything above `max_boost_db` become
/// that boost. The result is never NaN.
pub fn sanitize_db(db: f32, min_db: f32, max_boost_db: f32, below: BelowMin) -> f32 {
    if db.is_nan() || db == f32::NEG_INFINITY {
        return f32::NEG_INFINITY;
    }
    if !db.is_finite() || db > max_boost_db {
        return max_boost_db;
    }
    if db < min_db {
        return match below {
            BelowMin::Clamp => min_db,
            BelowMin::Mute => f32::NEG_INFINITY,
        };
    }
    db
}

/// Linear amplitude of 0, reported as dBFS silence.
/// Non-finite amplitudes become silence, except +∞ which reports [`MAX_BOOST_DB`].
pub fn linear_to_db(linear: f32) -> f32 {
    if !linear.is_finite() || linear <= 0.0 {
        return if linear.is_infinite() && linear.is_sign_positive() {
            MAX_BOOST_DB
        } else {
            f32::NEG_INFINITY
        };
    }
    20.0 * linear.log10()
}

/// Linear amplitude from a dB gain.
///
/// NaN and −∞ are silence (gain 0). +∞ and any dB above [`MAX_BOOST_DB`] clamp
/// to that boost. The result is finite and >= 0.
pub fn db_to_linear(db: f32) -> f32 {
    let db = sanitize_db(db, f32::NEG_INFINITY, MAX_BOOST_DB, BelowMin::Clamp);
    if db == f32::NEG_INFINITY {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// Cubic slider position (0.0–1.0, values above 1 allowed) to linear amplitude.
/// NaN and non-positive positions are silence.
pub fn cubic_to_linear(cubic: f32) -> f32 {
    if !cubic.is_finite() || cubic <= 0.0 {
        0.0
    } else {
        cubic * cubic * cubic
    }
}

/// Linear amplitude to a cubic slider position. NaN is 0.
pub fn linear_to_cubic(linear: f32) -> f32 {
    if !linear.is_finite() || linear <= 0.0 {
        0.0
    } else {
        linear.cbrt()
    }
}

/// Cubic slider position to dB. 1.0 is 0 dB. 0.0 is −∞ dB.
pub fn cubic_to_db(cubic: f32) -> f32 {
    linear_to_db(cubic_to_linear(cubic))
}

/// dB to a cubic slider position.
pub fn db_to_cubic(db: f32) -> f32 {
    linear_to_cubic(db_to_linear(db))
}

/// Linear amplitude of -90 dB. A channel at or below this is silent.
pub fn source_mute_floor() -> f32 {
    db_to_linear(-90.0)
}

/// True when `mute` is set, or every channel volume is at or below -90 dB.
///
/// An empty `channel_volumes` slice is not muted unless `mute` is true.
/// One channel above the floor keeps the source audible. The comparison is
/// `<=` [`source_mute_floor`], so a channel sitting on -90 dB still counts.
pub fn source_is_muted(mute: bool, channel_volumes: &[f32]) -> bool {
    if mute {
        return true;
    }
    if channel_volumes.is_empty() {
        return false;
    }
    let floor = source_mute_floor();
    channel_volumes.iter().all(|volume| *volume <= floor)
}

/// Mute state for a source node Props param.
///
/// `channel_volumes` `Some` is the channel list, including an empty list.
/// `None` means that field was absent, and `soft_volumes` is used instead
/// when it was present. `soft_mute` is combined with `mute`. The decision
/// itself is [`source_is_muted`].
pub fn source_feed_muted(
    mute: bool,
    channel_volumes: Option<&[f32]>,
    soft_mute: bool,
    soft_volumes: Option<&[f32]>,
) -> bool {
    let volumes = match (channel_volumes, soft_volumes) {
        (Some(volumes), _) => volumes,
        (None, Some(volumes)) => volumes,
        (None, None) => &[],
    };
    source_is_muted(mute || soft_mute, volumes)
}

/// One log line when a feeding source's muted state changes.
///
/// The first unmuted reading is not a transition. A later return to audible
/// is `omamix: source <id> unmuted`.
pub fn source_mute_log(id: u32, was: Option<bool>, now: bool) -> Option<String> {
    match (was, now) {
        (Some(prev), next) if prev == next => None,
        (_, true) => Some(format!("omamix: source {id} muted")),
        (Some(true), false) => Some(format!("omamix: source {id} unmuted")),
        _ => None,
    }
}

/// Short label for a fader readout. −0.0 is `0.0 dB`. NaN is mute.
pub fn format_db(db: f32) -> String {
    if db.is_nan() || (db.is_infinite() && db.is_sign_negative()) {
        return "-inf dB".to_string();
    }
    if db.is_infinite() {
        return "+inf dB".to_string();
    }
    if db == 0.0 {
        return "0.0 dB".to_string();
    }
    if db > 0.0 {
        format!("+{db:.1} dB")
    } else {
        format!("{db:.1} dB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1e-3, "{actual} != {expected}");
    }

    #[test]
    fn unity_is_zero_db() {
        close(linear_to_db(1.0), 0.0);
        close(cubic_to_db(1.0), 0.0);
        close(db_to_linear(0.0), 1.0);
        close(db_to_cubic(0.0), 1.0);
    }

    #[test]
    fn cubic_forty_percent_is_about_minus_24_db() {
        let db = cubic_to_db(0.4);
        close(db, -23.876);
        assert!((db + 24.0).abs() < 0.2);
    }

    #[test]
    fn silence_and_headroom() {
        assert!(cubic_to_db(0.0).is_infinite() && cubic_to_db(0.0).is_sign_negative());
        assert_eq!(db_to_linear(f32::NEG_INFINITY), 0.0);
        assert_eq!(linear_to_db(0.0), f32::NEG_INFINITY);
        close(db_to_linear(20.0), 10.0);
        close(linear_to_db(10.0), 20.0);
    }

    #[test]
    fn cubic_roundtrip() {
        for cubic in [0.05, 0.25, 0.4, 0.7, 1.0, 1.2] {
            close(db_to_cubic(cubic_to_db(cubic)), cubic);
        }
    }

    #[test]
    fn negative_cubic_is_silence() {
        assert_eq!(cubic_to_linear(-0.2), 0.0);
        assert!(cubic_to_db(-0.2).is_infinite());
    }

    #[test]
    fn format_db_labels() {
        assert_eq!(format_db(0.0), "0.0 dB");
        assert_eq!(format_db(-0.0), "0.0 dB");
        assert_eq!(format_db(6.0), "+6.0 dB");
        assert_eq!(format_db(-12.4), "-12.4 dB");
        assert_eq!(format_db(f32::NEG_INFINITY), "-inf dB");
        assert_eq!(format_db(f32::NAN), "-inf dB");
    }

    #[test]
    fn nan_and_infinities_never_become_a_non_finite_gain() {
        for db in [f32::NAN, f32::NEG_INFINITY, f32::INFINITY, 80.0, -80.0] {
            let gain = db_to_linear(db);
            assert!(gain.is_finite(), "{db} -> {gain}");
            assert!(gain >= 0.0, "{db} -> {gain}");
        }
        assert_eq!(db_to_linear(f32::NAN), 0.0);
        assert_eq!(db_to_linear(f32::NEG_INFINITY), 0.0);
        assert!((db_to_linear(f32::INFINITY) - db_to_linear(MAX_BOOST_DB)).abs() < 1e-4);
        assert_eq!(linear_to_db(f32::NAN), f32::NEG_INFINITY);
        assert_eq!(linear_to_db(f32::INFINITY), MAX_BOOST_DB);
        assert_eq!(linear_to_db(f32::NEG_INFINITY), f32::NEG_INFINITY);
        assert_eq!(cubic_to_linear(f32::NAN), 0.0);
        assert_eq!(linear_to_cubic(f32::NAN), 0.0);
        assert_eq!(linear_to_cubic(f32::NEG_INFINITY), 0.0);
        assert_eq!(
            sanitize_db(f32::NAN, -24.0, MAX_BOOST_DB, BelowMin::Clamp),
            f32::NEG_INFINITY
        );
        assert_eq!(
            sanitize_db(f32::INFINITY, -24.0, MAX_BOOST_DB, BelowMin::Clamp),
            MAX_BOOST_DB
        );
        assert_eq!(
            sanitize_db(f32::NEG_INFINITY, -24.0, MAX_BOOST_DB, BelowMin::Clamp),
            f32::NEG_INFINITY
        );
    }

    #[test]
    fn mute_flag_true_is_muted() {
        assert!(source_is_muted(true, &[1.0, 0.8]));
        assert!(source_is_muted(true, &[]));
        assert!(source_is_muted(true, &[0.0]));
        assert!(!source_is_muted(false, &[1.0, 0.8]));
    }

    #[test]
    fn all_zero_volumes_are_muted() {
        assert!(source_is_muted(false, &[0.0, 0.0]));
        assert!(source_is_muted(false, &[0.0, 0.0, 0.0, 0.0, 0.0]));
        assert!(!source_is_muted(false, &[0.0, 1.0]));
    }

    #[test]
    fn one_nonzero_channel_is_not_muted() {
        assert!(!source_is_muted(false, &[0.0, 1.0]));
        assert!(!source_is_muted(false, &[1.0, 0.0, 0.0]));
        assert!(source_is_muted(false, &[0.0, 0.0]));
    }

    #[test]
    fn mono_zero_is_muted_and_mono_unity_is_not() {
        assert!(source_is_muted(false, &[0.0]));
        assert!(!source_is_muted(false, &[1.0]));
        assert!(source_is_muted(false, &[source_mute_floor()]));
    }

    #[test]
    fn three_or_more_channels_follow_every_channel() {
        assert!(source_is_muted(false, &[0.0, 0.0, 0.0]));
        assert!(source_is_muted(false, &[0.0, 0.0, 0.0, 0.0]));
        assert!(!source_is_muted(false, &[0.0, 0.2, 0.0]));
        assert!(!source_is_muted(false, &[0.0, 0.0, 0.0, 0.5]));
    }

    #[test]
    fn empty_volume_list_with_mute_false_is_not_muted() {
        assert!(!source_is_muted(false, &[]));
        assert!(source_is_muted(true, &[]));
    }

    #[test]
    fn volumes_at_or_below_minus_90_db_are_muted() {
        let floor = source_mute_floor();
        assert!((floor - db_to_linear(-90.0)).abs() < 1e-12);
        assert!(floor > 0.0);
        assert!(source_is_muted(false, &[floor]));
        assert!(source_is_muted(false, &[floor, 0.0, floor]));
        assert!(source_is_muted(false, &[floor / 2.0]));
        let above = db_to_linear(-89.0);
        assert!(above > floor);
        assert!(!source_is_muted(false, &[above]));
        assert!(!source_is_muted(false, &[floor, above]));
        assert!(!source_is_muted(false, &[f32::NAN]));
        assert!(!source_is_muted(false, &[f32::INFINITY]));
    }

    #[test]
    fn soft_fields_fill_gaps_and_do_not_replace_a_seen_channel_list() {
        assert!(source_feed_muted(false, None, true, None));
        assert!(!source_feed_muted(false, None, false, None));
        assert!(source_feed_muted(
            false,
            None,
            false,
            Some(&[0.0, 0.0, 0.0])
        ));
        assert!(!source_feed_muted(false, None, false, Some(&[0.0, 1.0])));
        assert!(!source_feed_muted(
            false,
            Some(&[]),
            false,
            Some(&[0.0, 0.0])
        ));
        assert!(source_feed_muted(true, Some(&[]), false, Some(&[1.0, 1.0])));
        assert!(source_feed_muted(
            false,
            Some(&[0.0, 0.0]),
            false,
            Some(&[1.0, 1.0])
        ));
        assert!(!source_feed_muted(
            false,
            Some(&[1.0, 1.0]),
            false,
            Some(&[0.0, 0.0])
        ));
        assert!(source_feed_muted(false, Some(&[1.0]), true, Some(&[1.0])));
    }

    #[test]
    fn source_mute_log_lines_follow_transitions() {
        assert_eq!(source_mute_log(4, None, false), None);
        assert_eq!(
            source_mute_log(4, None, true).as_deref(),
            Some("omamix: source 4 muted")
        );
        assert_eq!(
            source_mute_log(9, Some(false), true).as_deref(),
            Some("omamix: source 9 muted")
        );
        assert_eq!(
            source_mute_log(9, Some(true), false).as_deref(),
            Some("omamix: source 9 unmuted")
        );
        assert_eq!(source_mute_log(9, Some(true), true), None);
        assert_eq!(source_mute_log(9, Some(false), false), None);
    }
}
