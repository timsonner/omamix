//! How a strip finds its source again after the app restarts or the node id changes.
//!
//! PipeWire node ids are not stable. Re-attach uses the stable fields
//! `application.name`, `application.process.binary`, and `media.name`.
//! Pid and object serial only separate two streams that are live at the same
//! time; a restart gets a new pid and a new serial and still matches.
//!
//! A registry `global` does not carry binary, `media.name`, pid, or
//! `omamix.owned`. Those show up after the node is bound. Whether a node is
//! ours is our PipeWire `client.id` (other clients cannot set it) and, when
//! both sides have it, the process pid.

use crate::graph::Strip;

/// `node.name` of the virtual microphone.
pub const MIX_MIC_NODE_NAME: &str = "omamix.mix-mic";

/// The PipeWire client this process opened. `client.id` is assigned by the
/// daemon and is not a property another client can stamp on its nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnClient {
    pub client_id: u32,
    pub pid: u32,
}

/// Identity used to re-attach a playback stream. Node id is deliberately absent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteKey {
    pub application_name: String,
    pub binary: String,
    /// Empty until the strip is bound to one stream (a tab, a window).
    pub media_name: String,
    pub pid: Option<u32>,
    /// PipeWire object serial, or a window serial when the backend has one.
    pub object_serial: Option<u64>,
}

impl RouteKey {
    pub fn new(application_name: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            application_name: application_name.into(),
            binary: binary.into(),
            media_name: String::new(),
            pid: None,
            object_serial: None,
        }
    }

    pub fn matches(&self, application_name: &str, binary: &str) -> bool {
        !self.application_name.is_empty()
            && !self.binary.is_empty()
            && self.application_name == application_name
            && self.binary == binary
    }

    /// Stable identity. Pid and object serial are ignored so a restarted app
    /// still matches. `media.name` counts when the key has one, which is how
    /// two browser tabs stay distinct across a restart.
    pub fn matches_stream(&self, stream: &PlaybackStream) -> bool {
        if !self.matches(&stream.application_name, &stream.binary) {
            return false;
        }
        self.media_name.is_empty() || self.media_name == stream.media_name
    }

    /// Among streams that already match [`Self::matches_stream`], keep the one
    /// whose live pid or object serial agrees. A missing value does not reject
    /// the stream: the registry often lacks both until bind, and a restart
    /// replaces them.
    pub fn disambiguates_live(&self, stream: &PlaybackStream) -> bool {
        let pid_ok = self.pid.is_none() || stream.pid.is_none() || self.pid == stream.pid;
        let serial_ok = self.object_serial.is_none()
            || stream.object_serial.is_none()
            || self.object_serial == stream.object_serial;
        pid_ok && serial_ok
    }
}

/// A playback node the registry reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackStream {
    pub application_name: String,
    pub binary: String,
    pub media_name: String,
    pub pid: Option<u32>,
    pub object_serial: Option<u64>,
    /// PipeWire client id, once the node is bound. Globals omit it.
    pub client_id: Option<u32>,
    /// The `omamix.owned` property, once bound. Globals omit it.
    /// The loop guard does not trust this on a playback node; it compares
    /// [`OwnClient`].
    pub omamix_owned: bool,
}

impl PlaybackStream {
    pub fn app(application_name: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            application_name: application_name.into(),
            binary: binary.into(),
            media_name: String::new(),
            pid: None,
            object_serial: None,
            client_id: None,
            omamix_owned: false,
        }
    }
}

/// Where a strip gets audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// A capture device, keyed by `node.name` (for example the X2u source).
    CaptureDevice { node_name: String },
    /// One playback app. Re-attached with [`RouteKey`].
    Application(RouteKey),
    /// Each remaining playback stream, tapped on its own.
    ///
    /// Membership is [`crate::graph::Scene::is_in_other_desktop`]: streams that
    /// already have an [`SourceKind::Application`] strip are left out, and so
    /// is every node with `omamix.owned` set. This variant does not monitor a sink.
    OtherDesktopAudio,
}

/// True when this playback node belongs to the PipeWire client we opened.
pub fn is_our_client(stream: &PlaybackStream, own: &OwnClient) -> bool {
    stream.client_id == Some(own.client_id) || stream.pid == Some(own.pid)
}

/// A node may contribute to Mix Mic only when it is someone else's playback.
///
/// Nodes with `omamix.owned` set are refused. So is any node whose `client.id`
/// or pid is ours: headphone sends and the monitor share our client, and that
/// id is not a property a playback app can put on itself. Matching does not
/// look at a `captures_mix_mic` flag on the playback node.
pub fn feeds_mix_mic(stream: &PlaybackStream, own: Option<&OwnClient>) -> bool {
    if stream.omamix_owned {
        return false;
    }
    if let Some(own) = own
        && is_our_client(stream, own)
    {
        return false;
    }
    !stream.application_name.is_empty() && !stream.binary.is_empty()
}

/// True when a newly appeared playback stream should be linked to this strip.
///
/// [`SourceKind::OtherDesktopAudio`] is decided by the scene, because that
/// strip has to see every other strip's route key. This function returns
/// false for it. Matching uses stable fields ([`RouteKey::matches_stream`]).
pub fn strip_should_attach(
    strip: &Strip,
    stream: &PlaybackStream,
    own: Option<&OwnClient>,
) -> bool {
    if !feeds_mix_mic(stream, own) {
        return false;
    }
    match &strip.source {
        SourceKind::Application(key) => key.matches_stream(stream),
        SourceKind::OtherDesktopAudio => false,
        SourceKind::CaptureDevice { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Strip;

    #[test]
    fn route_key_ignores_node_id_and_requires_both_fields() {
        let key = RouteKey::new("mpv", "mpv");
        assert!(key.matches("mpv", "mpv"));
        assert!(!key.matches("mpv", "celluloid"));
        assert!(!key.matches("Strudel", "chromium"));
        assert!(!RouteKey::new("", "mpv").matches("", "mpv"));
        assert!(!RouteKey::new("mpv", "").matches("mpv", ""));
    }

    #[test]
    fn app_strip_reattaches_by_name_and_binary() {
        let strip = Strip::new(
            1,
            "OST",
            SourceKind::Application(RouteKey::new("mpv", "mpv")),
        );
        assert!(strip_should_attach(
            &strip,
            &PlaybackStream::app("mpv", "mpv"),
            None
        ));
        assert!(!strip_should_attach(
            &strip,
            &PlaybackStream::app("mpv", "mpv.bin"),
            None
        ));
    }

    #[test]
    fn route_key_reattaches_after_the_app_restarts() {
        let mut key = RouteKey::new("mpv", "mpv");
        key.pid = Some(100);
        key.object_serial = Some(9);
        let mut restarted = PlaybackStream::app("mpv", "mpv");
        restarted.pid = Some(240);
        restarted.object_serial = Some(80);
        assert!(key.matches_stream(&restarted));
        assert!(!key.disambiguates_live(&restarted));
    }

    #[test]
    fn pid_only_splits_two_live_duplicates() {
        let mut key = RouteKey::new("Chromium", "chromium");
        key.pid = Some(10);
        let mut live = PlaybackStream::app("Chromium", "chromium");
        live.pid = Some(10);
        let mut other = PlaybackStream::app("Chromium", "chromium");
        other.pid = Some(11);
        assert!(key.matches_stream(&live) && key.matches_stream(&other));
        assert!(key.disambiguates_live(&live));
        assert!(!key.disambiguates_live(&other));
        let mut unbound = PlaybackStream::app("Chromium", "chromium");
        unbound.pid = None;
        assert!(key.disambiguates_live(&unbound));
    }

    #[test]
    fn finer_key_separates_two_streams_that_share_a_binary() {
        let mut key = RouteKey::new("Chromium", "chromium");
        key.media_name = "tab-a".into();
        let mut tab_a = PlaybackStream::app("Chromium", "chromium");
        tab_a.media_name = "tab-a".into();
        let mut tab_b = PlaybackStream::app("Chromium", "chromium");
        tab_b.media_name = "tab-b".into();
        assert!(key.matches_stream(&tab_a));
        assert!(!key.matches_stream(&tab_b));
    }

    #[test]
    fn capture_device_does_not_attach_to_playback_apps() {
        let strip = Strip::new(
            3,
            "Mic",
            SourceKind::CaptureDevice {
                node_name: "alsa_input.usb-Shure_Incorporated_Shure_Digital-00.analog-stereo"
                    .into(),
            },
        );
        assert!(!strip_should_attach(
            &strip,
            &PlaybackStream::app("mpv", "mpv"),
            None
        ));
    }

    #[test]
    fn same_client_does_not_feed_mix_mic() {
        let own = OwnClient {
            client_id: 7,
            pid: 4242,
        };
        let mut by_client = PlaybackStream::app("firefox", "firefox");
        by_client.client_id = Some(7);
        by_client.pid = Some(9999);
        assert!(is_our_client(&by_client, &own));
        assert!(!feeds_mix_mic(&by_client, Some(&own)));

        let mut by_pid = PlaybackStream::app("firefox", "firefox");
        by_pid.pid = Some(4242);
        by_pid.client_id = Some(8);
        assert!(!feeds_mix_mic(&by_pid, Some(&own)));

        let mut someone_else = PlaybackStream::app(MIX_MIC_NODE_NAME, "firefox");
        someone_else.client_id = Some(3);
        someone_else.pid = Some(50);
        assert!(feeds_mix_mic(&someone_else, Some(&own)));

        let mut marked = PlaybackStream::app("firefox", "firefox");
        marked.omamix_owned = true;
        assert!(!feeds_mix_mic(&marked, None));
    }
}
