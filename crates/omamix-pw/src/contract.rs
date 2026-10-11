//! Node properties the PipeWire backend sets.
//!
//! These are the PipeWire key strings (`PW_KEY_*`), kept here so the contract
//! can be tested with no libpipewire. Mix Mic is an adapter-backed
//! `Audio/Source/Virtual` node. The `pw_filter` is a separate internal node.
//! See docs/ARCHITECTURE.md.

pub use omamix_core::graph::OMAMIX_OWNED_KEY;
pub use omamix_core::route::MIX_MIC_NODE_NAME;

/// `priority.session` and `priority.driver` for Mix Mic.
///
/// WirePlumber ranks candidates by `priority.session`, then `priority.driver`,
/// otherwise 0. A higher candidate always wins. There is no fallback source
/// at another priority. If Mix Mic is the only `Audio/Source`,
/// `Audio/Source/Virtual`, or `Audio/Duplex`, it becomes the default, and
/// omamix then destroys that node. `default.configured.*` is never written.
/// The unplug check is `scripts/unplug-watch.sh`, not this constant.
pub const MIX_MIC_SESSION_PRIORITY: i32 = 1;

/// True when Mix Mic's priority is strictly above every other candidate.
pub fn mix_mic_selected_as_default(other_priorities: &[i32]) -> bool {
    other_priorities
        .iter()
        .all(|priority| *priority < MIX_MIC_SESSION_PRIORITY)
}

/// Declared processing latency of Mix Mic, as samples/rate.
/// This is the contract value, not a measured quantum.
pub const MIX_MIC_NODE_LATENCY: &str = "1024/48000";

/// One `pw_filter` and one process callback. Documented in ARCHITECTURE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockPlan {
    SingleFilter,
}

pub const CLOCK_PLAN: ClockPlan = ClockPlan::SingleFilter;

/// `node.description` shown in source pickers.
pub const MIX_MIC_DESCRIPTION: &str = "Mix Mic";

/// `media.class` of the public Mix Mic node. An adapter so session managers
/// register it as a source.
pub const MIX_MIC_MEDIA_CLASS: &str = "Audio/Source/Virtual";

/// Internal mix engine. Not a source, so it is not a default candidate.
pub const FILTER_NODE_NAME: &str = "omamix.mix-engine";

/// `media.class` of the internal filter. Not `Audio/Source`,
/// `Audio/Source/Virtual`, `Audio/Duplex`, or `Stream/Output/Audio`.
pub const FILTER_MEDIA_CLASS: &str = "Audio/DSP";

/// Playback-app taps use this class. They are not sources.
pub const APP_CAPTURE_MEDIA_CLASS: &str = "Stream/Input/Audio";

/// Property bag as `(key, value)` pairs.
pub fn mix_mic_properties() -> Vec<(&'static str, &'static str)> {
    vec![
        ("node.name", MIX_MIC_NODE_NAME),
        ("node.description", MIX_MIC_DESCRIPTION),
        ("media.class", MIX_MIC_MEDIA_CLASS),
        ("node.virtual", "true"),
        ("media.role", "Production"),
        ("audio.channels", "2"),
        ("audio.position", "FL,FR"),
        ("priority.session", "1"),
        ("priority.driver", "1"),
        ("node.latency", MIX_MIC_NODE_LATENCY),
        ("node.dont-reconnect", "true"),
        ("node.autoconnect", "false"),
        ("object.linger", "false"),
        (OMAMIX_OWNED_KEY, "true"),
    ]
}

/// Properties for one playback tap.
///
/// Used both for a dedicated app strip and for each stream the "other desktop
/// audio" strip attaches. `stream.capture.sink` is absent: that would monitor
/// the whole sink, double apps that already have strips, and feed headphone
/// sends back into Mix Mic. `target.object` is absent too: on a capture stream
/// it makes WirePlumber move that app onto our node and off the user's sink.
/// The backend links the app's output ports to this stream as an extra
/// listener.
pub fn app_capture_properties(node_name: &str) -> Vec<(String, String)> {
    vec![
        ("node.name".into(), node_name.into()),
        ("node.description".into(), "omamix capture".into()),
        ("media.class".into(), APP_CAPTURE_MEDIA_CLASS.into()),
        ("node.passive".into(), "true".into()),
        ("stream.dont-remix".into(), "true".into()),
        ("node.autoconnect".into(), "false".into()),
        // Leave this tap where we linked it. Do not send it to the default source.
        ("node.dont-reconnect".into(), "true".into()),
        ("object.linger".into(), "false".into()),
        (OMAMIX_OWNED_KEY.into(), "true".into()),
    ]
}

/// The internal `pw_filter`. One process callback. Not the public Mix Mic node.
pub fn filter_properties() -> Vec<(&'static str, &'static str)> {
    vec![
        ("node.name", FILTER_NODE_NAME),
        ("node.description", "omamix mix engine"),
        ("media.class", FILTER_MEDIA_CLASS),
        ("node.virtual", "true"),
        ("priority.session", "1"),
        ("priority.driver", "1"),
        ("node.latency", MIX_MIC_NODE_LATENCY),
        ("node.dont-reconnect", "true"),
        ("node.autoconnect", "false"),
        ("object.linger", "false"),
        (OMAMIX_OWNED_KEY, "true"),
    ]
}

/// Headphone send. Same client as Mix Mic, so the loop guard excludes it.
pub fn headphone_send_properties(node_name: &str) -> Vec<(String, String)> {
    owned_playback(node_name, "omamix headphones")
}

/// Monitor of the final mix. Same client as Mix Mic.
pub fn monitor_mix_properties(node_name: &str) -> Vec<(String, String)> {
    owned_playback(node_name, "omamix monitor")
}

fn owned_playback(node_name: &str, description: &str) -> Vec<(String, String)> {
    vec![
        ("node.name".into(), node_name.into()),
        ("node.description".into(), description.into()),
        ("media.class".into(), "Stream/Output/Audio".into()),
        ("node.autoconnect".into(), "false".into()),
        ("node.dont-reconnect".into(), "true".into()),
        ("object.linger".into(), "false".into()),
        (OMAMIX_OWNED_KEY.into(), "true".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(props: &'a [(&'static str, &'static str)], key: &str) -> Option<&'a str> {
        props.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    #[test]
    fn mix_mic_is_an_audio_source_the_process_owns() {
        let props = mix_mic_properties();
        assert_eq!(get(&props, "media.class"), Some(MIX_MIC_MEDIA_CLASS));
        assert_eq!(get(&props, "node.name"), Some("omamix.mix-mic"));
        assert_eq!(get(&props, "node.description"), Some("Mix Mic"));
        assert_eq!(get(&props, "node.virtual"), Some("true"));
        assert_eq!(get(&props, "priority.session"), Some("1"));
        assert_eq!(get(&props, "priority.driver"), Some("1"));
        assert_eq!(
            get(&props, "priority.session")
                .unwrap()
                .parse::<i32>()
                .unwrap(),
            MIX_MIC_SESSION_PRIORITY
        );
        assert_eq!(get(&props, "node.latency"), Some(MIX_MIC_NODE_LATENCY));
        assert_eq!(get(&props, "node.dont-reconnect"), Some("true"));
        assert_eq!(get(&props, "node.autoconnect"), Some("false"));
        assert_eq!(get(&props, "object.linger"), Some("false"));
        assert_eq!(get(&props, OMAMIX_OWNED_KEY), Some("true"));
        assert!(props.iter().all(|(key, _)| *key != "factory.name"));
        assert_eq!(CLOCK_PLAN, ClockPlan::SingleFilter);
        let filter = filter_properties();
        assert_ne!(filter, mix_mic_properties());
        assert_eq!(get(&filter, "node.name"), Some(FILTER_NODE_NAME));
        assert_eq!(get(&filter, "media.class"), Some(FILTER_MEDIA_CLASS));
        assert_eq!(get(&filter, OMAMIX_OWNED_KEY), Some("true"));
        assert_eq!(get(&filter, "object.linger"), Some("false"));
        assert_eq!(get(&filter, "priority.session"), Some("1"));
        assert_eq!(get(&filter, "node.autoconnect"), Some("false"));
    }

    #[test]
    fn contract_bags_include_every_core_owned_key() {
        use omamix_core::graph::{Scene, Strip, owned_output_properties};
        use omamix_core::route::SourceKind;

        let mut scene = Scene::empty("t");
        scene.strips.push(Strip::new(
            1,
            "mic",
            SourceKind::CaptureDevice {
                node_name: "x2u".into(),
            },
        ));
        scene.strips[0].monitor_headphones = true;
        scene.master.monitor_mix = true;

        for output in scene.owned_outputs() {
            let core = owned_output_properties(&output);
            let contract = contract_bag(&output);
            for (key, value) in core {
                let found = contract.iter().find(|(k, _)| k == key);
                assert_eq!(found.map(|(_, v)| v.as_str()), Some(value), "{key}");
            }
        }
    }

    fn contract_bag(output: &omamix_core::graph::OwnedOutput) -> Vec<(String, String)> {
        use omamix_core::graph::OwnedRole;
        match output.role {
            OwnedRole::MixMic => mix_mic_properties()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            OwnedRole::HeadphoneSend(_) => headphone_send_properties("omamix.headphone.1"),
            OwnedRole::MonitorFinalMix => monitor_mix_properties("omamix.monitor"),
        }
    }

    #[test]
    fn app_capture_does_not_steal_playback_or_monitor_a_sink() {
        let props = app_capture_properties("omamix.capture.mpv");
        let keys: Vec<_> = props.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"node.passive"));
        assert!(keys.contains(&"node.dont-reconnect"));
        assert!(!keys.contains(&"stream.capture.sink"));
        assert!(!keys.contains(&"target.object"));
        assert!(keys.contains(&"object.linger"));
        assert!(keys.contains(&OMAMIX_OWNED_KEY));
        assert_eq!(
            props
                .iter()
                .find(|(k, _)| k == "node.autoconnect")
                .map(|(_, v)| v.as_str()),
            Some("false")
        );
    }

    #[test]
    fn playback_taps_are_individual_streams_with_linger_off() {
        let props = app_capture_properties("omamix.capture.firefox");
        assert!(
            props
                .iter()
                .all(|(key, _)| key != "stream.capture.sink" && key != "target.object")
        );
        assert!(
            props
                .iter()
                .any(|(key, value)| key == "object.linger" && value == "false")
        );
        assert!(
            props
                .iter()
                .any(|(key, value)| key == OMAMIX_OWNED_KEY && value == "true")
        );
    }

    #[test]
    fn priority_one_loses_to_a_higher_candidate_and_an_empty_list_selects_mix_mic() {
        assert!(!mix_mic_selected_as_default(&[1000]));
        assert!(!mix_mic_selected_as_default(&[2]));
        assert!(mix_mic_selected_as_default(&[]));
    }

    #[test]
    fn filter_headphones_and_monitor_are_owned() {
        assert!(
            filter_properties()
                .iter()
                .any(|(key, value)| *key == OMAMIX_OWNED_KEY && *value == "true")
        );
        for props in [
            headphone_send_properties("omamix.headphone.1"),
            monitor_mix_properties("omamix.monitor"),
        ] {
            assert!(
                props
                    .iter()
                    .any(|(key, value)| key == OMAMIX_OWNED_KEY && value == "true")
            );
            assert!(
                props
                    .iter()
                    .any(|(key, value)| key == "object.linger" && value == "false")
            );
        }
    }
}
