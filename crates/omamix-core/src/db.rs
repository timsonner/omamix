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
        // 0.40³ = 0.064; 20·log10(0.064) = −23.876 dB.
        let db = cubic_to_db(0.4);
        close(db, -23.876);
        assert!((db + 24.0).abs() < 0.2);
    }

    #[test]
    fn silence_and_headroom() {
        assert!(cubic_to_db(0.0).is_infinite() && cubic_to_db(0.0).is_sign_negative());
        assert_eq!(db_to_linear(f32::NEG_INFINITY), 0.0);
        assert_eq!(linear_to_db(0.0), f32::NEG_INFINITY);
        // +20 dB is a 10× amplitude. This is the conversion, not a measured mic offset.
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
}
