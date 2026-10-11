//! Scene graph: strips, solo/mute, and the master bus.
//!
//! This is the data the UI edits and the audio engine will apply. It has no
//! PipeWire types.

use crate::db::{self, BelowMin};
use crate::fader;
use crate::route::{self, OwnClient, PlaybackStream, SourceKind};

/// Input trim range from the v1 spec. Independent of any measured mic level.
pub const INPUT_GAIN_MIN_DB: f32 = -24.0;
pub const INPUT_GAIN_MAX_DB: f32 = 24.0;

/// Master limiter ceiling from the v1 spec.
pub const LIMITER_CEILING_DBTP: f32 = -1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StripId(pub u64);

#[derive(Clone, Debug, PartialEq)]
pub struct Strip {
    pub id: StripId,
    pub name: String,
    pub source: SourceKind,
    pub fader_db: f32,
    pub mute: bool,
    pub solo: bool,
    pub pan: f32,
    pub input_gain_db: f32,
    /// Hear this strip in the headphones without requiring it on the virtual mic.
    pub monitor_headphones: bool,
}

impl Strip {
    pub fn new(id: u64, name: impl Into<String>, source: SourceKind) -> Self {
        Self {
            id: StripId(id),
            name: name.into(),
            source,
            fader_db: 0.0,
            mute: false,
            solo: false,
            pan: 0.0,
            input_gain_db: 0.0,
            monitor_headphones: false,
        }
    }

    pub fn set_input_gain_db(&mut self, db: f32) {
        self.input_gain_db =
            db::sanitize_db(db, INPUT_GAIN_MIN_DB, INPUT_GAIN_MAX_DB, BelowMin::Clamp);
    }

    pub fn audible(&self, any_solo: bool) -> bool {
        if self.mute {
            return false;
        }
        if any_solo { self.solo } else { true }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MasterBus {
    fader_db: f32,
    pub eq_enabled: bool,
    pub compressor_enabled: bool,
    /// On by default. Ceiling is [`LIMITER_CEILING_DBTP`].
    pub limiter_enabled: bool,
    pub limiter_ceiling_dbtp: f32,
    pub monitor_mix: bool,
}

impl MasterBus {
    pub fn fader_db(&self) -> f32 {
        self.fader_db
    }

    /// Same finite range as a channel fader. NaN mutes. +∞ clamps to the max boost.
    pub fn set_fader_db(&mut self, db: f32) {
        self.fader_db = fader::sanitize_fader_db(db);
    }
}

impl Default for MasterBus {
    fn default() -> Self {
        Self {
            fader_db: 0.0,
            eq_enabled: false,
            compressor_enabled: false,
            limiter_enabled: true,
            limiter_ceiling_dbtp: LIMITER_CEILING_DBTP,
            monitor_mix: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub name: String,
    pub strips: Vec<Strip>,
    pub master: MasterBus,
}

impl Scene {
    pub fn empty(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            strips: Vec::new(),
            master: MasterBus::default(),
        }
    }

    /// One strip so the UI shell has something to draw. Not a saved preset.
    pub fn placeholder() -> Self {
        let mut scene = Self::empty("Untitled");
        scene.strips.push(Strip::new(
            1,
            "Strip",
            SourceKind::CaptureDevice {
                node_name: String::new(),
            },
        ));
        scene.strips[0].fader_db = -12.0;
        scene
    }

    pub fn any_solo(&self) -> bool {
        self.strips.iter().any(|strip| strip.solo)
    }

    pub fn audible_strips(&self) -> Vec<&Strip> {
        let any_solo = self.any_solo();
        self.strips
            .iter()
            .filter(|strip| strip.audible(any_solo))
            .collect()
    }

    fn has_other_desktop(&self) -> bool {
        self.strips
            .iter()
            .any(|strip| matches!(strip.source, SourceKind::OtherDesktopAudio))
    }

    /// Playback streams the "other desktop audio" strip taps, one capture each.
    ///
    /// A stream that already has an [`SourceKind::Application`] strip is omitted,
    /// so it is not mixed twice. Nodes with `omamix.owned` set are omitted too,
    /// which keeps headphone sends and the monitor of the final mix out of Mix Mic.
    pub fn other_desktop_streams<'a>(
        &self,
        streams: &'a [PlaybackStream],
        own: Option<&OwnClient>,
        graph: Option<&route::LoopGraph<'_>>,
    ) -> Vec<&'a PlaybackStream> {
        streams
            .iter()
            .filter(|stream| self.is_in_other_desktop(stream, own, graph))
            .collect()
    }

    pub fn is_in_other_desktop(
        &self,
        stream: &PlaybackStream,
        own: Option<&OwnClient>,
        graph: Option<&route::LoopGraph<'_>>,
    ) -> bool {
        if !self.has_other_desktop() || !route::feeds_mix_mic(stream, own, graph) {
            return false;
        }
        !self.strips.iter().any(|strip| {
            matches!(&strip.source, SourceKind::Application(key) if key.matches_stream(stream))
        })
    }

    /// Nodes this scene will create. Each one carries [`OMAMIX_OWNED_KEY`].
    ///
    /// Mix Mic is the `pw_filter` node. There is not a second node for it.
    /// Headphone sends and the monitor are separate playback streams.
    pub fn owned_outputs(&self) -> Vec<OwnedOutput> {
        let mut outputs = vec![OwnedOutput {
            role: OwnedRole::MixMic,
        }];
        if self.master.monitor_mix {
            outputs.push(OwnedOutput {
                role: OwnedRole::MonitorFinalMix,
            });
        }
        for strip in &self.strips {
            if strip.monitor_headphones {
                outputs.push(OwnedOutput {
                    role: OwnedRole::HeadphoneSend(strip.id),
                });
            }
        }
        outputs
    }
}

/// A node the process itself publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedOutput {
    pub role: OwnedRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnedRole {
    /// The public Mix Mic source: `media.class` Audio/Source/Virtual,
    /// `node.name` [`route::MIX_MIC_NODE_NAME`]. The mix engine is a separate
    /// filter node. Strip taps are input ports on that filter.
    MixMic,
    HeadphoneSend(StripId),
    MonitorFinalMix,
}

/// Property key stamped on nodes this process creates, after they are bound.
/// Registry globals do not carry it. The loop guard uses [`OwnClient`] first.
pub const OMAMIX_OWNED_KEY: &str = "omamix.owned";

/// `omamix.owned` plus `object.linger=false` for every node we publish.
/// Headphone sends, the monitor, and the filter are included so a later
/// backend copies the same mark.
pub fn owned_output_properties(output: &OwnedOutput) -> Vec<(&'static str, &'static str)> {
    let mut props = vec![(OMAMIX_OWNED_KEY, "true"), ("object.linger", "false")];
    match output.role {
        OwnedRole::MixMic => {
            props.push(("node.name", route::MIX_MIC_NODE_NAME));
            props.push(("node.description", "Mix Mic"));
            props.push(("media.class", "Audio/Source/Virtual"));
            props.push(("node.virtual", "true"));
        }
        OwnedRole::HeadphoneSend(_) => {
            props.push(("node.description", "omamix headphones"));
            props.push(("media.class", "Stream/Output/Audio"));
        }
        OwnedRole::MonitorFinalMix => {
            props.push(("node.description", "omamix monitor"));
            props.push(("media.class", "Stream/Output/Audio"));
        }
    }
    props
}

/// True only when the output's property bag lacks [`OMAMIX_OWNED_KEY`].
/// Every role we publish sets that key, so this returns false for them.
/// Dropping the key makes the test fail.
pub fn owned_output_feeds_mix_mic(output: &OwnedOutput) -> bool {
    !owned_output_properties(output)
        .iter()
        .any(|(key, value)| *key == OMAMIX_OWNED_KEY && *value == "true")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::fader;
    use crate::route::{self, PlaybackStream, RouteKey};

    fn app_strip(id: u64, name: &str, solo: bool, mute: bool) -> Strip {
        let mut strip = Strip::new(id, name, SourceKind::Application(RouteKey::new(name, name)));
        strip.solo = solo;
        strip.mute = mute;
        strip
    }

    #[test]
    fn no_solo_plays_every_unmuted_strip() {
        let mut scene = Scene::empty("t");
        scene.strips.push(app_strip(1, "mic", false, false));
        scene.strips.push(app_strip(2, "mpv", false, true));
        scene.strips.push(app_strip(3, "chrome", false, false));
        let names: Vec<_> = scene
            .audible_strips()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["mic", "chrome"]);
    }

    #[test]
    fn solo_wins_and_mute_still_silences() {
        let mut scene = Scene::empty("t");
        scene.strips.push(app_strip(1, "mic", true, false));
        scene.strips.push(app_strip(2, "mpv", false, false));
        scene.strips.push(app_strip(3, "chrome", true, true));
        let names: Vec<_> = scene
            .audible_strips()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["mic"]);
    }

    #[test]
    fn input_gain_clamps_to_spec_range() {
        let mut strip = Strip::new(
            1,
            "mic",
            SourceKind::CaptureDevice {
                node_name: "x2u".into(),
            },
        );
        strip.set_input_gain_db(48.0);
        assert_eq!(strip.input_gain_db, INPUT_GAIN_MAX_DB);
        strip.set_input_gain_db(-48.0);
        assert_eq!(strip.input_gain_db, INPUT_GAIN_MIN_DB);
    }

    #[test]
    fn input_gain_nan_mutes_and_positive_inf_clamps() {
        let mut strip = Strip::new(
            1,
            "mic",
            SourceKind::CaptureDevice {
                node_name: "x2u".into(),
            },
        );
        strip.set_input_gain_db(f32::NAN);
        assert!(strip.input_gain_db.is_infinite() && strip.input_gain_db.is_sign_negative());
        assert_eq!(db::db_to_linear(strip.input_gain_db), 0.0);
        strip.set_input_gain_db(f32::INFINITY);
        assert_eq!(strip.input_gain_db, INPUT_GAIN_MAX_DB);
        assert!(db::db_to_linear(strip.input_gain_db).is_finite());
        strip.set_input_gain_db(f32::NEG_INFINITY);
        assert_eq!(strip.input_gain_db, f32::NEG_INFINITY);
        assert_eq!(db::db_to_linear(strip.input_gain_db), 0.0);
    }

    fn scene_with_other() -> Scene {
        let mut scene = Scene::empty("t");
        scene.strips.push(Strip::new(
            1,
            "OST",
            SourceKind::Application(RouteKey::new("mpv", "mpv")),
        ));
        scene
            .strips
            .push(Strip::new(2, "Desktop", SourceKind::OtherDesktopAudio));
        scene.strips[0].monitor_headphones = true;
        scene.master.monitor_mix = true;
        scene
    }

    #[test]
    fn a_stream_with_its_own_strip_is_not_in_other() {
        let scene = scene_with_other();
        let streams = [
            PlaybackStream::app("mpv", "mpv"),
            PlaybackStream::app("firefox", "firefox"),
        ];
        let other = scene.other_desktop_streams(&streams, None, None);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].application_name, "firefox");
        assert!(!scene.is_in_other_desktop(&streams[0], None, None));
        assert!(route::feeds_mix_mic(&streams[0], None, None));
    }

    #[test]
    fn omamix_owned_nodes_never_feed_mix_mic() {
        let scene = scene_with_other();
        let mut headphone = PlaybackStream::app("firefox", "firefox");
        headphone.omamix_owned = true;
        let mut monitor = PlaybackStream::app("monitor-final-mix", "some-binary");
        monitor.omamix_owned = true;
        let mut named_like_us = PlaybackStream::app("omamix", "omamix");
        named_like_us.omamix_owned = false;
        let streams = [headphone, monitor, named_like_us];
        let other = scene.other_desktop_streams(&streams, None, None);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].application_name, "omamix");
        assert!(!route::feeds_mix_mic(&streams[0], None, None));
        assert!(!route::feeds_mix_mic(&streams[1], None, None));
        assert!(
            scene
                .owned_outputs()
                .iter()
                .any(|output| output.role == OwnedRole::MixMic)
        );
        assert_eq!(
            scene
                .owned_outputs()
                .iter()
                .filter(|output| output.role == OwnedRole::MixMic)
                .count(),
            1
        );
        for output in scene.owned_outputs() {
            let props = owned_output_properties(&output);
            assert!(
                props
                    .iter()
                    .any(|(key, value)| *key == OMAMIX_OWNED_KEY && *value == "true"),
                "{:?}",
                output.role
            );
            assert!(!owned_output_feeds_mix_mic(&output));
        }
        assert!(
            scene
                .owned_outputs()
                .iter()
                .any(|output| { output.role == OwnedRole::HeadphoneSend(StripId(1)) })
        );
        assert!(
            scene
                .owned_outputs()
                .iter()
                .any(|output| output.role == OwnedRole::MonitorFinalMix)
        );
    }

    #[test]
    fn our_client_is_not_routed_back_into_mix_mic() {
        let scene = scene_with_other();
        let own = route::OwnClient {
            client_id: 4,
            pid: 80,
        };
        let mut headphone = PlaybackStream::app("firefox", "firefox");
        headphone.client_id = Some(own.client_id);
        assert!(!route::feeds_mix_mic(&headphone, Some(&own), None));
        assert!(
            scene
                .other_desktop_streams(&[headphone], Some(&own), None)
                .is_empty()
        );
    }

    #[test]
    fn master_fader_sanitizes_non_finite_values() {
        let mut master = MasterBus::default();
        master.set_fader_db(f32::NAN);
        assert!(master.fader_db().is_infinite() && master.fader_db().is_sign_negative());
        assert_eq!(db::db_to_linear(master.fader_db()), 0.0);
        master.set_fader_db(f32::INFINITY);
        assert_eq!(master.fader_db(), fader::FADER_MAX_DB);
        assert!(db::db_to_linear(master.fader_db()).is_finite());
        master.set_fader_db(f32::NEG_INFINITY);
        assert_eq!(master.fader_db(), f32::NEG_INFINITY);
        assert_eq!(db::db_to_linear(master.fader_db()), 0.0);
    }

    #[test]
    fn master_limiter_defaults_on_at_minus_one() {
        let master = MasterBus::default();
        assert!(master.limiter_enabled);
        assert_eq!(master.limiter_ceiling_dbtp, -1.0);
    }
}
