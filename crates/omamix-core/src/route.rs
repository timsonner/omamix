//! How a strip finds its source again after the app restarts or the node id changes.
//!
//! PipeWire node ids are not stable. Re-attach uses the stable fields
//! `application.name`, `application.process.binary`, and `media.name`.
//! Pid and object serial only separate two streams that are live at the same
//! time; a restart gets a new pid and a new serial and still matches.
//!
//! A node global does carry `client.id`, `object.serial`, `node.name`, and
//! `media.class`. It does not carry `application.process.binary`, `media.name`,
//! `application.process.id`, or `omamix.owned`. Those show up after the node
//! is bound. Whether a node is ours is our PipeWire `client.id` (the daemon
//! assigns it; another client cannot stamp it) and, when both sides have it,
//! the process pid.

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
    /// PipeWire client id. Present on the node global; bind is not required.
    pub client_id: Option<u32>,
    /// Object id of this node, once the registry has seen it.
    pub node_id: Option<u32>,
    /// `media.class`. Playback taps require `Stream/Output/Audio`.
    pub media_class: String,
    /// The `omamix.owned` property, once bound. Globals omit it.
    /// The loop guard does not trust this on a playback node; it compares
    /// [`OwnClient`] and walks links.
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
            node_id: None,
            media_class: "Stream/Output/Audio".into(),
            omamix_owned: false,
        }
    }
}

/// `Stream/Output/Audio` is a playback stream. Anything else fails closed.
pub fn is_playback_stream_class(class: &str) -> bool {
    class == "Stream/Output/Audio"
}

/// Device strips attach these capture classes only.
pub fn is_capture_device_class(class: &str) -> bool {
    matches!(
        class,
        "Audio/Source" | "Audio/Source/Virtual" | "Audio/Duplex"
    )
}

/// Node globals often omit `application.process.binary`. The client object has it.
pub fn resolve_binary(node_binary: &str, client_binary: &str) -> String {
    if !node_binary.is_empty() {
        node_binary.to_string()
    } else {
        client_binary.to_string()
    }
}

/// One directed PipeWire link. Samples flow from `output_node` to `input_node`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkEdge {
    pub output_node: u32,
    pub input_node: u32,
}

/// A node id plus the client that owns it, for the Mix Mic loop walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeRef {
    pub id: u32,
    pub client_id: Option<u32>,
}

/// The link graph the loop guard walks. Built from registry globals.
#[derive(Clone, Copy, Debug)]
pub struct LoopGraph<'a> {
    pub mix_mic_node: u32,
    pub nodes: &'a [NodeRef],
    pub links: &'a [LinkEdge],
}

/// True when tapping `node_id` would put Mix Mic back into Mix Mic.
///
/// The walk follows links upstream from `node_id`, including through
/// intermediate nodes. A hit on Mix Mic means this node is already fed by it.
/// The walk stops on a repeated node, so a cycle that never reaches Mix Mic
/// does not run forever and does not refuse the node.
///
/// The walk is per node. [`client_captures_mix_mic`] is the client-level check
/// used for the automatic "other desktop" strip and for an application strip
/// that has no `media.name`. An application strip whose route key includes
/// `media.name` stays on this per-node walk, so one named tab can stay a strip
/// while another tab of the same client captures Mix Mic.
pub fn upstream_includes_mix_mic(node_id: u32, graph: &LoopGraph<'_>) -> bool {
    node_id == graph.mix_mic_node || reaches_mix_mic_upstream(node_id, graph)
}

fn reaches_mix_mic_upstream(start: u32, graph: &LoopGraph<'_>) -> bool {
    let mut seen = vec![start];
    let mut queue = vec![start];
    while let Some(node) = queue.pop() {
        for link in graph.links {
            if link.input_node != node {
                continue;
            }
            if link.output_node == graph.mix_mic_node {
                return true;
            }
            if !seen.contains(&link.output_node) {
                seen.push(link.output_node);
                queue.push(link.output_node);
            }
        }
    }
    false
}

/// True the first time `client_id` is claimed. Later nodes of that client
/// do not log again.
pub fn claim_client_once(logged: &mut std::collections::HashSet<u32>, client_id: u32) -> bool {
    logged.insert(client_id)
}

/// Drop a client from the once-per-client log when that client goes away.
pub fn forget_client(logged: &mut std::collections::HashSet<u32>, client_id: u32) {
    logged.remove(&client_id);
}

/// True when any node of `client_id` is Mix Mic or is fed by Mix Mic.
///
/// That is the OBS / browser-studio case: the capture node and the playback
/// node share a client, and the playback node may have no PipeWire edge of its
/// own. The automatic strip uses this. A named `media.name` strip does not.
pub fn client_captures_mix_mic(client_id: u32, graph: &LoopGraph<'_>) -> bool {
    graph
        .nodes
        .iter()
        .any(|node| node.client_id == Some(client_id) && upstream_includes_mix_mic(node.id, graph))
}

/// Which strip is asking. Named tabs stay per node. Everything else refuses
/// the whole client once that client captures Mix Mic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapScope {
    NamedStream,
    SharedClient,
}

/// A playback node may be tapped for this scope.
///
/// [`TapScope::SharedClient`] is the automatic "other desktop" strip and an
/// application strip with an empty `media.name`. [`TapScope::NamedStream`] is
/// an application strip whose route key has a `media.name`.
pub fn tap_allowed(
    stream: &PlaybackStream,
    own: Option<&OwnClient>,
    graph: Option<&LoopGraph<'_>>,
    scope: TapScope,
) -> bool {
    if !feeds_mix_mic(stream, own, graph) {
        return false;
    }
    if scope == TapScope::SharedClient
        && let Some(graph) = graph
        && stream
            .client_id
            .is_some_and(|client_id| client_captures_mix_mic(client_id, graph))
    {
        return false;
    }
    true
}

/// Identity fields copied off a node global and, after bind, its info props.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeIdentity {
    pub node_name: String,
    pub media_class: String,
    pub application_name: String,
    pub binary: String,
    pub media_name: String,
    pub client_id: Option<u32>,
    pub pid: Option<u32>,
    pub object_serial: Option<u64>,
    pub omamix_owned: bool,
}

impl NodeIdentity {
    pub fn from_props<'a>(props: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut identity = Self::default();
        for (key, value) in props {
            match key {
                "node.name" => identity.node_name = value.to_string(),
                "media.class" => identity.media_class = value.to_string(),
                "application.name" => identity.application_name = value.to_string(),
                "application.process.binary" => identity.binary = value.to_string(),
                "media.name" => identity.media_name = value.to_string(),
                "client.id" => identity.client_id = value.parse().ok(),
                "application.process.id" => identity.pid = value.parse().ok(),
                "object.serial" => identity.object_serial = value.parse().ok(),
                "omamix.owned" => identity.omamix_owned = value == "true",
                _ => {}
            }
        }
        identity
    }

    /// Fill holes from a later bind. A bound prop does not erase a global one.
    pub fn merge_bind(&mut self, bound: &Self) {
        if self.node_name.is_empty() {
            self.node_name.clone_from(&bound.node_name);
        }
        if self.media_class.is_empty() {
            self.media_class.clone_from(&bound.media_class);
        }
        if self.application_name.is_empty() {
            self.application_name.clone_from(&bound.application_name);
        }
        if self.binary.is_empty() {
            self.binary.clone_from(&bound.binary);
        }
        if self.media_name.is_empty() {
            self.media_name.clone_from(&bound.media_name);
        }
        if self.client_id.is_none() {
            self.client_id = bound.client_id;
        }
        if self.pid.is_none() {
            self.pid = bound.pid;
        }
        if self.object_serial.is_none() {
            self.object_serial = bound.object_serial;
        }
        self.omamix_owned |= bound.omamix_owned;
    }

    pub fn playback_stream(&self, node_id: u32) -> PlaybackStream {
        PlaybackStream {
            application_name: self.application_name.clone(),
            binary: self.binary.clone(),
            media_name: self.media_name.clone(),
            pid: self.pid,
            object_serial: self.object_serial,
            client_id: self.client_id,
            node_id: Some(node_id),
            media_class: self.media_class.clone(),
            omamix_owned: self.omamix_owned,
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
/// Refused:
///
/// - `media.class` is not `Stream/Output/Audio`. Capture nodes, including
///   `Stream/Input/Audio`, stay out. An empty class fails closed.
/// - `omamix.owned` is set (after bind).
/// - `client.id` or pid is ours. Headphone sends and the monitor share our
///   client, and that id is not a property a playback app can put on itself.
/// - [`upstream_includes_mix_mic`] when `graph` is present. The walk is per
///   node. [`tap_allowed`] with [`TapScope::SharedClient`] also refuses every
///   stream of a client that captures Mix Mic. Without a `node_id` the walk
///   cannot run, and the stream is refused until the registry has one.
pub fn feeds_mix_mic(
    stream: &PlaybackStream,
    own: Option<&OwnClient>,
    graph: Option<&LoopGraph<'_>>,
) -> bool {
    if !is_playback_stream_class(&stream.media_class) {
        return false;
    }
    if stream.omamix_owned {
        return false;
    }
    if let Some(own) = own
        && is_our_client(stream, own)
    {
        return false;
    }
    if let Some(graph) = graph {
        let Some(node_id) = stream.node_id else {
            return false;
        };
        if upstream_includes_mix_mic(node_id, graph) {
            return false;
        }
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
    graph: Option<&LoopGraph<'_>>,
) -> bool {
    let scope = match &strip.source {
        SourceKind::Application(key) if !key.media_name.is_empty() => TapScope::NamedStream,
        SourceKind::Application(_) => TapScope::SharedClient,
        SourceKind::OtherDesktopAudio => return false,
        SourceKind::CaptureDevice { .. } => return false,
    };
    if !tap_allowed(stream, own, graph, scope) {
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
            None,
            None
        ));
        assert!(!strip_should_attach(
            &strip,
            &PlaybackStream::app("mpv", "mpv.bin"),
            None,
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
            None,
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
        assert!(!feeds_mix_mic(&by_client, Some(&own), None));

        let mut by_pid = PlaybackStream::app("firefox", "firefox");
        by_pid.pid = Some(4242);
        by_pid.client_id = Some(8);
        assert!(!feeds_mix_mic(&by_pid, Some(&own), None));

        let mut someone_else = PlaybackStream::app(MIX_MIC_NODE_NAME, "firefox");
        someone_else.client_id = Some(3);
        someone_else.pid = Some(50);
        assert!(feeds_mix_mic(&someone_else, Some(&own), None));

        let mut marked = PlaybackStream::app("firefox", "firefox");
        marked.omamix_owned = true;
        assert!(!feeds_mix_mic(&marked, None, None));
    }

    #[test]
    fn bind_fills_identity_the_global_does_not_carry() {
        let global = NodeIdentity::from_props([
            ("node.name", "mpv"),
            ("media.class", "Stream/Output/Audio"),
            ("application.name", "mpv"),
            ("client.id", "12"),
            ("object.serial", "99"),
        ]);
        assert!(global.binary.is_empty());
        assert!(global.media_name.is_empty());
        assert!(global.pid.is_none());
        assert!(!global.omamix_owned);
        assert_eq!(global.client_id, Some(12));

        let bound = NodeIdentity::from_props([
            ("application.process.binary", "mpv"),
            ("media.name", "song"),
            ("application.process.id", "440"),
            ("omamix.owned", "true"),
        ]);
        let mut merged = global;
        merged.merge_bind(&bound);
        assert_eq!(merged.binary, "mpv");
        assert_eq!(merged.media_name, "song");
        assert_eq!(merged.pid, Some(440));
        assert!(merged.omamix_owned);
        assert_eq!(merged.client_id, Some(12));
        assert_eq!(merged.object_serial, Some(99));
    }

    #[test]
    fn upstream_walk_refuses_a_node_fed_by_mix_mic() {
        let nodes = [NodeRef {
            id: 2,
            client_id: Some(20),
        }];
        let links = [LinkEdge {
            output_node: 1,
            input_node: 2,
        }];
        let graph = LoopGraph {
            mix_mic_node: 1,
            nodes: &nodes,
            links: &links,
        };
        assert!(upstream_includes_mix_mic(2, &graph));
        let mut stream = PlaybackStream::app("loopback", "pw-loopback");
        stream.node_id = Some(2);
        stream.client_id = Some(20);
        assert!(!feeds_mix_mic(&stream, None, Some(&graph)));
    }

    #[test]
    fn same_client_capture_refuses_only_the_capturing_node() {
        // Node 4 reads Mix Mic. Node 5 is another stream of that client and
        // has no edge of its own, so it stays eligible. An Application strip
        // can still match it by media.name.
        let nodes = [
            NodeRef {
                id: 4,
                client_id: Some(40),
            },
            NodeRef {
                id: 5,
                client_id: Some(40),
            },
            NodeRef {
                id: 6,
                client_id: Some(50),
            },
        ];
        let links = [LinkEdge {
            output_node: 1,
            input_node: 4,
        }];
        let graph = LoopGraph {
            mix_mic_node: 1,
            nodes: &nodes,
            links: &links,
        };
        assert!(upstream_includes_mix_mic(4, &graph));
        assert!(!upstream_includes_mix_mic(5, &graph));
        assert!(!upstream_includes_mix_mic(6, &graph));

        let mut studio = PlaybackStream::app("Chromium", "chromium");
        studio.node_id = Some(5);
        studio.client_id = Some(40);
        studio.media_name = "music".into();
        assert!(feeds_mix_mic(&studio, None, Some(&graph)));

        let mut capture = PlaybackStream::app("Chromium", "chromium");
        capture.node_id = Some(4);
        capture.client_id = Some(40);
        capture.media_class = "Stream/Input/Audio".into();
        assert!(!feeds_mix_mic(&capture, None, Some(&graph)));

        let mut mpv = PlaybackStream::app("mpv", "mpv");
        mpv.node_id = Some(6);
        mpv.client_id = Some(50);
        assert!(feeds_mix_mic(&mpv, None, Some(&graph)));
    }

    #[test]
    fn other_desktop_refuses_a_same_client_rebroadcast() {
        use crate::graph::{Scene, Strip};

        let nodes = [
            NodeRef {
                id: 4,
                client_id: Some(40),
            },
            NodeRef {
                id: 5,
                client_id: Some(40),
            },
        ];
        let links = [LinkEdge {
            output_node: 1,
            input_node: 4,
        }];
        let graph = LoopGraph {
            mix_mic_node: 1,
            nodes: &nodes,
            links: &links,
        };
        assert!(client_captures_mix_mic(40, &graph));
        let mut logged = std::collections::HashSet::new();
        assert!(claim_client_once(&mut logged, 40));
        assert!(!claim_client_once(&mut logged, 40));
        assert!(claim_client_once(&mut logged, 50));
        forget_client(&mut logged, 40);
        assert!(claim_client_once(&mut logged, 40));

        let mut play = PlaybackStream::app("obs", "pw-loopback");
        play.node_id = Some(5);
        play.client_id = Some(40);
        play.media_name = "studio".into();
        assert!(feeds_mix_mic(&play, None, Some(&graph)));
        assert!(!tap_allowed(
            &play,
            None,
            Some(&graph),
            TapScope::SharedClient
        ));
        assert!(tap_allowed(
            &play,
            None,
            Some(&graph),
            TapScope::NamedStream
        ));

        let mut scene = Scene::empty("t");
        scene
            .strips
            .push(Strip::new(1, "other", SourceKind::OtherDesktopAudio));
        assert!(
            scene
                .other_desktop_streams(&[play.clone()], None, Some(&graph))
                .is_empty()
        );

        let mut named_key = RouteKey::new("obs", "pw-loopback");
        named_key.media_name = "studio".into();
        let named = Strip::new(2, "tab", SourceKind::Application(named_key));
        assert!(strip_should_attach(&named, &play, None, Some(&graph)));

        let broad = Strip::new(
            3,
            "app",
            SourceKind::Application(RouteKey::new("obs", "pw-loopback")),
        );
        assert!(!strip_should_attach(&broad, &play, None, Some(&graph)));
    }

    #[test]
    fn upstream_walk_covers_multiple_hops_cycles_and_late_links() {
        let nodes = [
            NodeRef {
                id: 2,
                client_id: None,
            },
            NodeRef {
                id: 3,
                client_id: None,
            },
        ];
        let before = LoopGraph {
            mix_mic_node: 1,
            nodes: &nodes,
            links: &[],
        };
        assert!(!upstream_includes_mix_mic(3, &before));

        let late = [
            LinkEdge {
                output_node: 1,
                input_node: 2,
            },
            LinkEdge {
                output_node: 2,
                input_node: 3,
            },
        ];
        let after = LoopGraph {
            mix_mic_node: 1,
            nodes: &nodes,
            links: &late,
        };
        assert!(upstream_includes_mix_mic(3, &after));
        assert!(upstream_includes_mix_mic(2, &after));

        let cycle = [
            LinkEdge {
                output_node: 2,
                input_node: 3,
            },
            LinkEdge {
                output_node: 3,
                input_node: 2,
            },
        ];
        let cycled = LoopGraph {
            mix_mic_node: 1,
            nodes: &nodes,
            links: &cycle,
        };
        assert!(!upstream_includes_mix_mic(2, &cycled));
        assert!(!upstream_includes_mix_mic(3, &cycled));
    }

    #[test]
    fn capture_classes_and_client_binary_are_separate_from_playback() {
        assert!(is_playback_stream_class("Stream/Output/Audio"));
        assert!(!is_playback_stream_class("Stream/Input/Audio"));
        assert!(!is_playback_stream_class(""));
        assert!(is_capture_device_class("Audio/Source"));
        assert!(is_capture_device_class("Audio/Source/Virtual"));
        assert!(is_capture_device_class("Audio/Duplex"));
        assert!(!is_capture_device_class("Stream/Input/Audio"));
        assert!(!is_capture_device_class("Audio/DSP"));
        assert_eq!(resolve_binary("", "pw-cat"), "pw-cat");
        assert_eq!(resolve_binary("pw-play", "pipewire"), "pw-play");

        let mut native = PlaybackStream::app("pw-cat", "");
        native.binary.clear();
        native.binary = resolve_binary(&native.binary, "pw-cat");
        assert!(feeds_mix_mic(&native, None, None));
    }
}
