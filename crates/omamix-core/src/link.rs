//! Link attempt state. The backend does not create PipeWire link objects yet.
//! When it does, it has to go through these transitions: wait until both
//! ports exist, and drop the attempt if the peer disappears mid-link.
//! Link objects also get `object.linger = false` (see the property bags).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkPhase {
    WaitingForPorts,
    Linking,
    Linked,
    PeerVanished,
    /// The link call failed. The peer may still exist.
    Failed,
}

/// Properties for a link object. Linger stays off so the link dies with the client.
pub fn link_properties() -> [(&'static str, &'static str); 1] {
    [("object.linger", "false")]
}

/// A port the registry has already described.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortEndpoint {
    pub id: u32,
    pub node_id: u32,
    pub direction: String,
    pub channel: String,
}

/// Output ports of `node_id` to link into a stereo tap, as `(fl, fr)`.
///
/// Labeled FL/FR win. A single output is used for both sides, including a
/// port labeled MONO, FC, or AUX. Two unlabeled outputs are used in id order.
/// Several labeled ports with no FL/FR pair stay unpaired.
pub fn stereo_output_ports(ports: &[PortEndpoint], node_id: u32) -> Option<(u32, u32)> {
    stereo_ports(ports, node_id, "out")
}

/// Input ports of `node_id`, with the same pairing rules as [`stereo_output_ports`].
pub fn stereo_input_ports(ports: &[PortEndpoint], node_id: u32) -> Option<(u32, u32)> {
    stereo_ports(ports, node_id, "in")
}

fn stereo_ports(ports: &[PortEndpoint], node_id: u32, direction: &str) -> Option<(u32, u32)> {
    let mut matched: Vec<&PortEndpoint> = ports
        .iter()
        .filter(|port| port.node_id == node_id && port.direction == direction)
        .collect();
    matched.sort_by_key(|port| port.id);
    if matched.is_empty() {
        return None;
    }
    let fl = matched.iter().find(|port| port.channel == "FL");
    let fr = matched.iter().find(|port| port.channel == "FR");
    if let (Some(fl), Some(fr)) = (fl, fr) {
        return Some((fl.id, fr.id));
    }
    if matched.len() == 1 {
        return Some((matched[0].id, matched[0].id));
    }
    if matched.iter().any(|port| !port.channel.is_empty()) {
        return None;
    }
    Some((matched[0].id, matched[1].id))
}

/// Begin a link. Ports that are not ready stay in [`LinkPhase::WaitingForPorts`].
pub fn start_link(ports_ready: bool) -> LinkPhase {
    if ports_ready {
        LinkPhase::Linking
    } else {
        LinkPhase::WaitingForPorts
    }
}

/// The peer node or its ports went away. The attempt does not stay half-linked.
pub fn peer_removed(phase: LinkPhase) -> LinkPhase {
    let _ = phase;
    LinkPhase::PeerVanished
}

/// The link call failed. Distinct from a peer that disappeared.
pub fn link_failed(phase: LinkPhase) -> LinkPhase {
    let _ = phase;
    LinkPhase::Failed
}

/// How many failures a tap or the engine link will schedule before it stops.
pub const RETRY_LIMIT: u8 = 4;

/// One timer tick. True when a new attempt may start.
///
/// `wait` counts down first, so a failure armed on this tick does not run
/// again until a later tick. After [`RETRY_LIMIT`] failures the attempt stays
/// idle.
pub fn retry_due(tries: u8, wait: &mut u8) -> bool {
    if *wait > 0 {
        *wait -= 1;
        return false;
    }
    tries < RETRY_LIMIT
}

/// Schedule the next attempt. The wait doubles from one tick, capped at eight.
pub fn arm_retry(tries: &mut u8, wait: &mut u8) {
    *tries = tries.saturating_add(1);
    let shift = (*tries).saturating_sub(1).min(3);
    *wait = 1_u8 << shift;
}

/// Both engine-to-Mix-Mic links. Ready only when both sides are up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineSides {
    pub up: [bool; 2],
    pub proxies: u8,
}

impl EngineSides {
    pub fn idle() -> Self {
        Self {
            up: [false, false],
            proxies: 0,
        }
    }

    pub fn ready(self) -> bool {
        self.proxies == 2 && self.up == [true, true]
    }
}

/// Record one link info event.
///
/// An error drops both sides: a single failed link is not a live engine.
/// Active or paused sets that side. Any other state clears it, so readiness
/// follows the link rather than the moment `create_object` returned.
/// The bool is true when the proxies should be dropped.
pub fn note_engine_link(sides: &mut EngineSides, index: usize, active: bool, error: bool) -> bool {
    if error {
        sides.up = [false, false];
        sides.proxies = 0;
        return true;
    }
    if index < 2 {
        sides.up[index] = active;
    }
    false
}

/// Both create calls returned a proxy. Neither side is up yet.
pub fn engine_proxies_created(sides: &mut EngineSides) {
    sides.proxies = 2;
    sides.up = [false, false];
}

/// Move a live attempt forward. A vanished peer is handled by [`peer_removed`].
pub fn advance_link(phase: LinkPhase, ports_ready: bool) -> LinkPhase {
    match phase {
        LinkPhase::WaitingForPorts if ports_ready => LinkPhase::Linking,
        LinkPhase::WaitingForPorts => LinkPhase::WaitingForPorts,
        LinkPhase::Linking if ports_ready => LinkPhase::Linked,
        LinkPhase::Linking => LinkPhase::WaitingForPorts,
        LinkPhase::Linked if ports_ready => LinkPhase::Linked,
        LinkPhase::Linked => LinkPhase::WaitingForPorts,
        LinkPhase::PeerVanished => LinkPhase::PeerVanished,
        LinkPhase::Failed => LinkPhase::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_waits_until_ports_exist() {
        assert_eq!(start_link(false), LinkPhase::WaitingForPorts);
        assert_eq!(
            advance_link(LinkPhase::WaitingForPorts, false),
            LinkPhase::WaitingForPorts
        );
        assert_eq!(start_link(true), LinkPhase::Linking);
        assert_eq!(advance_link(LinkPhase::Linking, true), LinkPhase::Linked);
    }

    #[test]
    fn peer_vanishing_mid_link_abandons_the_attempt() {
        assert_eq!(peer_removed(LinkPhase::Linking), LinkPhase::PeerVanished);
        assert_eq!(peer_removed(LinkPhase::Linked), LinkPhase::PeerVanished);
        assert_eq!(
            advance_link(LinkPhase::PeerVanished, true),
            LinkPhase::PeerVanished
        );
    }

    #[test]
    fn a_failed_link_stays_failed_and_does_not_linger() {
        assert_eq!(link_failed(LinkPhase::Linking), LinkPhase::Failed);
        assert_eq!(advance_link(LinkPhase::Failed, true), LinkPhase::Failed);
        assert_eq!(link_properties(), [("object.linger", "false")]);
    }

    #[test]
    fn a_failed_tap_retries_with_backoff_then_stops() {
        let mut tries = 0;
        let mut wait = 0;
        assert!(retry_due(tries, &mut wait));
        arm_retry(&mut tries, &mut wait);
        assert_eq!((tries, wait), (1, 1));
        assert!(!retry_due(tries, &mut wait));
        assert_eq!(wait, 0);
        assert!(retry_due(tries, &mut wait));

        tries = 0;
        wait = 0;
        for _ in 0..RETRY_LIMIT {
            assert!(retry_due(tries, &mut wait));
            arm_retry(&mut tries, &mut wait);
            while wait > 0 {
                assert!(!retry_due(tries, &mut wait));
            }
        }
        assert_eq!(tries, RETRY_LIMIT);
        assert!(!retry_due(tries, &mut wait));
        assert!(!retry_due(tries, &mut wait));
    }

    #[test]
    fn an_engine_link_error_clears_ready() {
        let mut sides = EngineSides::idle();
        engine_proxies_created(&mut sides);
        assert!(!sides.ready());
        assert!(!note_engine_link(&mut sides, 0, true, false));
        assert!(!sides.ready());
        assert!(!note_engine_link(&mut sides, 1, true, false));
        assert!(sides.ready());

        assert!(!note_engine_link(&mut sides, 1, false, false));
        assert!(!sides.ready());
        assert_eq!(sides.proxies, 2);

        assert!(note_engine_link(&mut sides, 0, false, true));
        assert!(!sides.ready());
        assert_eq!(sides.proxies, 0);
        assert_eq!(sides.up, [false, false]);
    }

    #[test]
    fn stereo_outputs_prefer_channel_labels() {
        let ports = [
            PortEndpoint {
                id: 10,
                node_id: 3,
                direction: "out".into(),
                channel: "FR".into(),
            },
            PortEndpoint {
                id: 9,
                node_id: 3,
                direction: "out".into(),
                channel: "FL".into(),
            },
            PortEndpoint {
                id: 8,
                node_id: 3,
                direction: "in".into(),
                channel: "FL".into(),
            },
        ];
        assert_eq!(stereo_output_ports(&ports, 3), Some((9, 10)));
        assert_eq!(stereo_output_ports(&ports, 4), None);
    }

    #[test]
    fn unlabeled_outputs_pair_in_id_order_and_mono_duplicates() {
        let stereo = [
            PortEndpoint {
                id: 4,
                node_id: 1,
                direction: "out".into(),
                channel: String::new(),
            },
            PortEndpoint {
                id: 2,
                node_id: 1,
                direction: "out".into(),
                channel: String::new(),
            },
        ];
        assert_eq!(stereo_output_ports(&stereo, 1), Some((2, 4)));
        let mono = [PortEndpoint {
            id: 7,
            node_id: 1,
            direction: "out".into(),
            channel: String::new(),
        }];
        assert_eq!(stereo_output_ports(&mono, 1), Some((7, 7)));
    }

    #[test]
    fn a_single_labeled_mono_port_is_duplicated() {
        for channel in ["MONO", "FC", "AUX"] {
            let ports = [PortEndpoint {
                id: 3,
                node_id: 1,
                direction: "out".into(),
                channel: channel.into(),
            }];
            assert_eq!(stereo_output_ports(&ports, 1), Some((3, 3)), "{channel}");
        }
        let mixed = [
            PortEndpoint {
                id: 1,
                node_id: 1,
                direction: "out".into(),
                channel: "MONO".into(),
            },
            PortEndpoint {
                id: 2,
                node_id: 1,
                direction: "out".into(),
                channel: "AUX".into(),
            },
        ];
        assert_eq!(stereo_output_ports(&mixed, 1), None);
        let input = [PortEndpoint {
            id: 9,
            node_id: 4,
            direction: "in".into(),
            channel: "FL".into(),
        }];
        assert_eq!(stereo_input_ports(&input, 4), Some((9, 9)));
    }
}
