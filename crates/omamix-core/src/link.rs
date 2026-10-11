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
}
