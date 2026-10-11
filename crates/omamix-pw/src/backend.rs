//! App-owned PipeWire streams.
//!
//! Compiled only with `--features pipewire`. Creating a [`Session`] connects
//! a client; [`connect_mix_mic`] is what publishes the `Audio/Source` node.
//! Unit tests build property objects and do not call either function, so they
//! do not add nodes.

use pipewire as pw;
use pw::properties::PropertiesBox;
use pw::spa::pod::Pod;

use crate::contract;

/// Client, context, and thread loop.
///
/// [`Session::shutdown`] stops the thread loop. Dropping the session drops the
/// core and disconnects the client, and PipeWire removes nodes that client
/// created. Node property bags set `object.linger = false`. This scaffold does
/// not create link objects yet; when it does, those links use the same property
/// and [`omamix_core::link`] waits for ports before linking.
pub struct Session {
    thread_loop: pw::thread_loop::ThreadLoopRc,
    context: pw::context::ContextRc,
    core: pw::core::CoreRc,
}

impl Session {
    /// Open a client. Does not create an audio node until a stream is connected.
    ///
    /// # Safety
    /// `ThreadLoopRc::new` is marked unsafe by pipewire-rs. The loop pointer
    /// stays inside this struct and is not aliased.
    pub fn connect() -> Result<Self, pw::Error> {
        let thread_loop = unsafe { pw::thread_loop::ThreadLoopRc::new(Some("omamix"), None)? };
        let context = pw::context::ContextRc::new(&thread_loop, None)?;
        let core = context.connect_rc(None)?;
        Ok(Self {
            thread_loop,
            context,
            core,
        })
    }

    pub fn core(&self) -> &pw::core::Core {
        &self.core
    }

    /// Stop the loop. Drop the `Session` afterward so the core disconnects
    /// and PipeWire removes this client's nodes and links.
    pub fn shutdown(&mut self) {
        self.thread_loop.stop();
        // context and core drop with self, after stop returns.
        let _ = &self.context;
    }
}

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

/// Local stream object for the virtual mic. The node appears in the graph
/// only after [`connect_mix_mic`].
pub fn create_mix_mic_stream<'a>(
    core: &'a pw::core::Core,
) -> Result<pw::stream::StreamBox<'a>, pw::Error> {
    pw::stream::StreamBox::new(core, contract::MIX_MIC_DESCRIPTION, mix_mic_properties())
}

/// Source-port contract for Mix Mic, typechecked as a `pw_stream`.
///
/// The running graph is one `pw_filter` ([`crate::contract::ClockPlan::SingleFilter`]).
/// Mix Mic is that filter's source port, not a second stream with its own driver.
/// This function is the stand-in: same properties, f32 stereo, no autoconnect.
/// It is replaced by the filter's output port when that node exists.
/// Direction is Output: this process writes the mix, and other clients capture it.
pub fn connect_mix_mic(stream: &pw::stream::Stream) -> Result<(), pw::Error> {
    let format = stereo_f32_format(48_000);
    let pod = Pod::from_bytes(&format).expect("stereo f32 format pod");
    let mut params = [pod];
    stream.connect(
        pw::spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )
}

/// Capture direction for one playback tap. The caller links ports itself
/// after they exist; this function does not autoconnect.
pub fn capture_direction() -> pw::spa::utils::Direction {
    pw::spa::utils::Direction::Input
}

fn stereo_f32_format(rate: u32) -> Vec<u8> {
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
