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

/// How many times an exhausted tap may be given a new budget.
pub const REVIVE_LIMIT: u8 = 2;

/// After this many live links are removed, the next retry waits longer.
///
/// A success clears tries, wait, and revivals. It does not clear this count,
/// so a peer that is linked and then removed over and over does not retry
/// every couple of ticks.
pub const RELINK_SLOW_AFTER: u8 = 4;

/// Floor on the retry wait, in 50 ms ticks, once [`RELINK_SLOW_AFTER`] is passed.
pub const RELINK_SLOW_WAIT: u8 = 16;

/// Which registry object a budget event came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryKind {
    Node,
    Port,
    Link,
    Other,
}

/// Budget event for a node, port, or link add or remove.
///
/// A port is not a global event. The caller uses [`port_budget_event`] with
/// that port's node.
pub fn registry_budget_event(kind: RegistryKind) -> Option<BudgetEvent> {
    match kind {
        RegistryKind::Node => Some(BudgetEvent::OtherNode),
        RegistryKind::Link => Some(BudgetEvent::OwnLink),
        RegistryKind::Port | RegistryKind::Other => None,
    }
}

/// `PeerPortsChanged` for a port on a tap peer.
///
/// The filter and Mix Mic are skipped. A zero node id is skipped.
pub fn port_budget_event(
    node_id: u32,
    filter_node: Option<u32>,
    mix_mic_node: Option<u32>,
) -> Option<BudgetEvent> {
    if node_id == 0 || Some(node_id) == filter_node || Some(node_id) == mix_mic_node {
        None
    } else {
        Some(BudgetEvent::PeerPortsChanged)
    }
}

/// What just happened in the graph, for the retry budget.
///
/// [`BudgetEvent::OwnLink`] and [`BudgetEvent::OtherNode`] leave the budget
/// alone. A freed mixer slot or a change to the peer's own ports can revive
/// an exhausted tap, up to [`REVIVE_LIMIT`] times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetEvent {
    OwnLink,
    OtherNode,
    SlotFreed,
    PeerPortsChanged,
}

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

/// A link that reached active or paused spends the failure budget.
///
/// The revival cap and the slow-relink count are part of that budget, so a
/// later slot free or a change to this peer's ports can revive the tap again,
/// and the next removal starts the relink count over. The once-per-peer stop
/// log is left set.
pub fn note_tap_success(tries: &mut u8, wait: &mut u8, revivals: &mut u8, relinks: &mut u8) {
    *tries = 0;
    *wait = 0;
    *revivals = 0;
    *relinks = 0;
}

/// Record a failed link call.
///
/// `saw_edges` is cleared so a later retry is not treated as already gone
/// before the registry shows the new edges. The bool is true when this
/// failure used the last attempt.
pub fn fail_budget(
    phase: &mut LinkPhase,
    linked: &mut bool,
    saw_edges: &mut bool,
    tries: &mut u8,
    wait: &mut u8,
) -> bool {
    *phase = link_failed(*phase);
    *linked = false;
    *saw_edges = false;
    let before = *tries;
    if before < RETRY_LIMIT {
        arm_retry(tries, wait);
    }
    before < RETRY_LIMIT && *tries >= RETRY_LIMIT
}

/// Whether this tick may start or continue an attempt.
///
/// [`LinkPhase::PeerVanished`] stays stopped. A [`LinkPhase::Failed`] attempt
/// waits out its backoff, then returns to [`LinkPhase::WaitingForPorts`]
/// with `saw_edges` clear.
pub fn allow_attempt(
    phase: &mut LinkPhase,
    linked: &mut bool,
    saw_edges: &mut bool,
    tries: u8,
    wait: &mut u8,
) -> bool {
    if *phase == LinkPhase::PeerVanished {
        return false;
    }
    if *phase == LinkPhase::Failed && !retry_due(tries, wait) {
        return false;
    }
    if *phase == LinkPhase::Failed {
        *phase = LinkPhase::WaitingForPorts;
        *linked = false;
        *saw_edges = false;
    }
    true
}

/// Give an exhausted failure a new budget of [`RETRY_LIMIT`] attempts.
///
/// A tap still inside its backoff is left alone. After [`REVIVE_LIMIT`]
/// revivals the tap stays exhausted. The bool is true when the budget was reset.
pub fn revive_exhausted(
    phase: LinkPhase,
    tries: &mut u8,
    wait: &mut u8,
    saw_edges: &mut bool,
    revivals: &mut u8,
) -> bool {
    if phase == LinkPhase::Failed && *tries >= RETRY_LIMIT && *revivals < REVIVE_LIMIT {
        *tries = 0;
        *wait = 0;
        *saw_edges = false;
        *revivals = revivals.saturating_add(1);
        return true;
    }
    false
}

/// Apply one graph event to a tap budget.
///
/// Our own links, and nodes other than a change to this peer's ports, do not
/// reset `tries`. The bool is true when an exhausted tap was revived.
pub fn apply_budget_event(
    event: BudgetEvent,
    phase: LinkPhase,
    tries: &mut u8,
    wait: &mut u8,
    saw_edges: &mut bool,
    revivals: &mut u8,
) -> bool {
    match event {
        BudgetEvent::OwnLink | BudgetEvent::OtherNode => false,
        BudgetEvent::SlotFreed | BudgetEvent::PeerPortsChanged => {
            revive_exhausted(phase, tries, wait, saw_edges, revivals)
        }
    }
}

/// True the first time this peer exhausts its budget.
pub fn claim_stop_log(logged: &mut bool) -> bool {
    if *logged {
        return false;
    }
    *logged = true;
    true
}

/// Count a live link that disappeared, and stretch the wait after several.
fn note_relink(seen: bool, wait: &mut u8, relinks: &mut u8) {
    if !seen {
        return;
    }
    *relinks = relinks.saturating_add(1);
    if *relinks > RELINK_SLOW_AFTER {
        *wait = (*wait).max(RELINK_SLOW_WAIT);
    }
}

/// The link objects are gone and no error state arrived.
///
/// `linked` becomes false so the tap counts as unlinked. The existing failure
/// budget decides whether another attempt is allowed. `relinks` grows when
/// the registry had already shown the link. A later success clears that
/// count. The bool is true when the link proxies should be dropped.
pub fn note_silent_unlink(
    phase: &mut LinkPhase,
    linked: &mut bool,
    saw_edges: &mut bool,
    tries: &mut u8,
    wait: &mut u8,
    relinks: &mut u8,
) -> bool {
    if !*linked {
        return false;
    }
    let seen = *saw_edges;
    let _ = fail_budget(phase, linked, saw_edges, tries, wait);
    note_relink(seen, wait, relinks);
    true
}

/// Watch edges from a linked tap.
///
/// A count above zero sets `saw_edges`. A later zero tears the attempt down
/// so it can be linked again. A retry that has not yet appeared in the
/// registry (`saw_edges` still false) is left up. The bool is true when the
/// link proxies should be dropped.
pub fn observe_edges(
    linked: &mut bool,
    saw_edges: &mut bool,
    phase: &mut LinkPhase,
    tries: &mut u8,
    wait: &mut u8,
    relinks: &mut u8,
    count: usize,
) -> bool {
    if !*linked {
        return false;
    }
    if count > 0 {
        *saw_edges = true;
        return false;
    }
    if !*saw_edges {
        return false;
    }
    let _ = fail_budget(phase, linked, saw_edges, tries, wait);
    note_relink(true, wait, relinks);
    true
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
    fn a_successful_link_resets_the_retry_budget() {
        let mut phase = LinkPhase::Linking;
        let mut linked = true;
        let mut saw_edges = false;
        let mut tries = 0;
        let mut wait = 0;
        assert!(!fail_budget(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            &mut tries,
            &mut wait
        ));
        assert_eq!((tries, wait), (1, 1));
        assert!(!allow_attempt(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            tries,
            &mut wait
        ));
        assert!(allow_attempt(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            tries,
            &mut wait
        ));
        phase = LinkPhase::Linking;
        linked = true;
        let mut revivals = REVIVE_LIMIT;
        let mut relinks = RELINK_SLOW_AFTER + 1;
        note_tap_success(&mut tries, &mut wait, &mut revivals, &mut relinks);
        assert_eq!(
            (phase, tries, wait, revivals, relinks),
            (LinkPhase::Linking, 0, 0, 0, 0)
        );

        assert!(!fail_budget(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            &mut tries,
            &mut wait
        ));
        assert_eq!((tries, wait), (1, 1));
        assert!(!saw_edges);
        assert!(!linked);
    }

    #[test]
    fn an_exhausted_tap_retries_after_a_slot_is_freed() {
        let mut phase = LinkPhase::WaitingForPorts;
        let mut linked = false;
        let mut saw_edges = true;
        let mut tries = 0;
        let mut wait = 0;
        for _ in 0..RETRY_LIMIT {
            assert!(allow_attempt(
                &mut phase,
                &mut linked,
                &mut saw_edges,
                tries,
                &mut wait
            ));
            let _ = fail_budget(
                &mut phase,
                &mut linked,
                &mut saw_edges,
                &mut tries,
                &mut wait,
            );
            while wait > 0 {
                assert!(!allow_attempt(
                    &mut phase,
                    &mut linked,
                    &mut saw_edges,
                    tries,
                    &mut wait
                ));
            }
        }
        assert_eq!(phase, LinkPhase::Failed);
        assert_eq!(tries, RETRY_LIMIT);
        assert!(!saw_edges);
        assert!(!allow_attempt(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            tries,
            &mut wait
        ));

        let mut revivals = 0;
        let mut backing_off = (phase, tries, wait, saw_edges);
        backing_off.1 = 2;
        backing_off.2 = 4;
        assert!(!revive_exhausted(
            backing_off.0,
            &mut backing_off.1,
            &mut backing_off.2,
            &mut backing_off.3,
            &mut revivals
        ));
        assert_eq!(backing_off, (LinkPhase::Failed, 2, 4, false));
        assert_eq!(revivals, 0);

        let mut vanished = LinkPhase::PeerVanished;
        let mut vanished_tries = RETRY_LIMIT;
        let mut vanished_wait = 0;
        let mut vanished_edges = true;
        assert!(!revive_exhausted(
            vanished,
            &mut vanished_tries,
            &mut vanished_wait,
            &mut vanished_edges,
            &mut revivals
        ));
        assert!(!allow_attempt(
            &mut vanished,
            &mut linked,
            &mut vanished_edges,
            vanished_tries,
            &mut vanished_wait
        ));
        assert_eq!(vanished, LinkPhase::PeerVanished);

        assert!(revive_exhausted(
            phase,
            &mut tries,
            &mut wait,
            &mut saw_edges,
            &mut revivals
        ));
        assert_eq!((tries, wait, saw_edges), (0, 0, false));
        assert!(allow_attempt(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            tries,
            &mut wait
        ));
        assert_eq!(phase, LinkPhase::WaitingForPorts);
        assert!(!saw_edges);
    }

    #[test]
    fn a_failed_tap_does_not_tear_down_the_retry_before_edges_appear() {
        let mut phase = LinkPhase::Linked;
        let mut linked = true;
        let mut saw_edges = true;
        let mut tries = 0;
        let mut wait = 0;
        let mut relinks = 0;
        assert!(!observe_edges(
            &mut linked,
            &mut saw_edges,
            &mut phase,
            &mut tries,
            &mut wait,
            &mut relinks,
            2
        ));
        assert!(saw_edges);
        let _ = fail_budget(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            &mut tries,
            &mut wait,
        );
        assert!(!saw_edges);
        assert!(!linked);
        while !allow_attempt(&mut phase, &mut linked, &mut saw_edges, tries, &mut wait) {}
        phase = LinkPhase::Linking;
        linked = true;
        assert!(!observe_edges(
            &mut linked,
            &mut saw_edges,
            &mut phase,
            &mut tries,
            &mut wait,
            &mut relinks,
            0
        ));
        assert!(linked);
        assert_eq!(phase, LinkPhase::Linking);
        assert!(!observe_edges(
            &mut linked,
            &mut saw_edges,
            &mut phase,
            &mut tries,
            &mut wait,
            &mut relinks,
            2
        ));
        assert!(saw_edges);
        assert!(observe_edges(
            &mut linked,
            &mut saw_edges,
            &mut phase,
            &mut tries,
            &mut wait,
            &mut relinks,
            0
        ));
        assert!(!linked);
        assert!(!saw_edges);
        assert_eq!(phase, LinkPhase::Failed);
        assert_eq!(tries, 2);
        assert!(wait > 0);
        assert!(!allow_attempt(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            tries,
            &mut wait
        ));
    }

    #[test]
    fn our_own_link_churn_does_not_revive_and_revivals_are_capped() {
        let phase = LinkPhase::Failed;
        let mut tries = RETRY_LIMIT;
        let mut wait = 0;
        let mut saw_edges = false;
        let mut revivals = 0;
        assert!(!apply_budget_event(
            BudgetEvent::OwnLink,
            phase,
            &mut tries,
            &mut wait,
            &mut saw_edges,
            &mut revivals
        ));
        assert!(!apply_budget_event(
            BudgetEvent::OtherNode,
            phase,
            &mut tries,
            &mut wait,
            &mut saw_edges,
            &mut revivals
        ));
        assert_eq!((tries, wait, revivals), (RETRY_LIMIT, 0, 0));

        assert!(apply_budget_event(
            BudgetEvent::SlotFreed,
            phase,
            &mut tries,
            &mut wait,
            &mut saw_edges,
            &mut revivals
        ));
        assert_eq!((tries, revivals), (0, 1));
        tries = RETRY_LIMIT;
        assert!(apply_budget_event(
            BudgetEvent::PeerPortsChanged,
            phase,
            &mut tries,
            &mut wait,
            &mut saw_edges,
            &mut revivals
        ));
        assert_eq!((tries, revivals), (0, 2));
        tries = RETRY_LIMIT;
        assert!(!apply_budget_event(
            BudgetEvent::SlotFreed,
            phase,
            &mut tries,
            &mut wait,
            &mut saw_edges,
            &mut revivals
        ));
        assert!(!apply_budget_event(
            BudgetEvent::OwnLink,
            phase,
            &mut tries,
            &mut wait,
            &mut saw_edges,
            &mut revivals
        ));
        assert_eq!((tries, revivals), (RETRY_LIMIT, REVIVE_LIMIT));

        let mut logged = false;
        assert!(claim_stop_log(&mut logged));
        assert!(!claim_stop_log(&mut logged));
    }

    #[test]
    fn a_link_that_disappears_without_an_error_retries_inside_the_budget() {
        let mut phase = LinkPhase::Linking;
        let mut linked = true;
        let mut saw_edges = false;
        let mut tries = 1;
        let mut wait = 0;
        let mut revivals = 0;
        let mut relinks = 0;
        assert!(note_silent_unlink(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            &mut tries,
            &mut wait,
            &mut relinks
        ));
        assert!(!linked);
        assert!(!saw_edges);
        assert_eq!(phase, LinkPhase::Failed);
        assert_eq!(tries, 2);
        assert!(wait > 0);
        assert!(!apply_budget_event(
            BudgetEvent::OwnLink,
            phase,
            &mut tries,
            &mut wait,
            &mut saw_edges,
            &mut revivals
        ));
        assert_eq!(tries, 2);
        while wait > 0 {
            assert!(!allow_attempt(
                &mut phase,
                &mut linked,
                &mut saw_edges,
                tries,
                &mut wait
            ));
        }
        assert!(allow_attempt(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            tries,
            &mut wait
        ));
        assert_eq!(phase, LinkPhase::WaitingForPorts);

        tries = RETRY_LIMIT;
        wait = 0;
        phase = LinkPhase::Linked;
        linked = true;
        assert!(note_silent_unlink(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            &mut tries,
            &mut wait,
            &mut relinks
        ));
        assert!(!linked);
        assert_eq!(tries, RETRY_LIMIT);
        assert!(!allow_attempt(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            tries,
            &mut wait
        ));
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

    #[test]
    fn a_node_is_not_a_revive_and_a_peer_port_is() {
        assert_eq!(
            registry_budget_event(RegistryKind::Node),
            Some(BudgetEvent::OtherNode)
        );
        assert_eq!(
            registry_budget_event(RegistryKind::Link),
            Some(BudgetEvent::OwnLink)
        );
        assert_eq!(registry_budget_event(RegistryKind::Port), None);
        assert_eq!(registry_budget_event(RegistryKind::Other), None);
        assert_eq!(port_budget_event(0, None, None), None);
        assert_eq!(port_budget_event(7, Some(7), None), None);
        assert_eq!(port_budget_event(8, None, Some(8)), None);
        assert_eq!(
            port_budget_event(9, Some(7), Some(8)),
            Some(BudgetEvent::PeerPortsChanged)
        );
    }

    #[test]
    fn repeated_removal_of_a_live_link_waits_longer() {
        let mut phase = LinkPhase::Linked;
        let mut linked = true;
        let mut saw_edges = true;
        let mut tries = 0;
        let mut wait = 0;
        let mut relinks = RELINK_SLOW_AFTER;
        assert!(note_silent_unlink(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            &mut tries,
            &mut wait,
            &mut relinks
        ));
        assert!(wait >= RELINK_SLOW_WAIT);
        assert_eq!(relinks, RELINK_SLOW_AFTER + 1);
        let mut revivals = 3;
        note_tap_success(&mut tries, &mut wait, &mut revivals, &mut relinks);
        assert_eq!((tries, wait, revivals, relinks), (0, 0, 0, 0));

        phase = LinkPhase::Linked;
        linked = true;
        saw_edges = true;
        assert!(note_silent_unlink(
            &mut phase,
            &mut linked,
            &mut saw_edges,
            &mut tries,
            &mut wait,
            &mut relinks
        ));
        assert!(wait < RELINK_SLOW_WAIT);
        assert_eq!(relinks, 1);
    }
}
