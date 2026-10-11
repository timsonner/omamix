//! Property bags and the monitor stream format.
//!
//! Compiled only with `--features pipewire`. Mix Mic itself is the `pw_filter`
//! in [`crate::filter`], not a `pw_stream`. This module does not open a client.
//! Unit tests build property objects and do not connect.

use pipewire as pw;
use pw::properties::PropertiesBox;

use crate::contract;

pub fn mix_mic_properties() -> PropertiesBox {
    let mut props = PropertiesBox::new();
    for (key, value) in contract::mix_mic_properties() {
        props.insert(key, value);
    }
    props
}

pub fn app_capture_properties(node_name: &str) -> PropertiesBox {
    let mut props = PropertiesBox::new();
    for (key, value) in contract::app_capture_properties(node_name) {
        props.insert(key, value);
    }
    props
}

/// Capture direction for one playback tap. The caller links ports itself
/// after they exist; nothing here autoconnects.
pub fn capture_direction() -> pw::spa::utils::Direction {
    pw::spa::utils::Direction::Input
}

pub(crate) fn stereo_f32_format(rate: u32) -> Vec<u8> {
    let mut audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(pw::spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(rate);
    audio_info.set_channels(2);
    let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position);

    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .expect("serialize mix-mic format")
    .0
    .into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_object_carries_the_audio_source_contract() {
        let props = mix_mic_properties();
        assert_eq!(
            props.get("media.class"),
            Some(contract::MIX_MIC_MEDIA_CLASS)
        );
        assert_eq!(props.get("node.name"), Some(contract::MIX_MIC_NODE_NAME));
        assert_eq!(
            props.get("node.description"),
            Some(contract::MIX_MIC_DESCRIPTION)
        );
        assert_eq!(props.get("node.virtual"), Some("true"));
        assert_eq!(props.get("priority.session"), Some("1"));
        assert_eq!(props.get("priority.driver"), Some("1"));
        assert_eq!(
            props.get("node.latency"),
            Some(contract::MIX_MIC_NODE_LATENCY)
        );
        assert_eq!(props.get("node.dont-reconnect"), Some("true"));
        assert_eq!(props.get("object.linger"), Some("false"));
        assert_eq!(props.get(contract::OMAMIX_OWNED_KEY), Some("true"));
    }

    #[test]
    fn capture_direction_is_input() {
        assert_eq!(capture_direction(), pw::spa::utils::Direction::Input);
    }
}
