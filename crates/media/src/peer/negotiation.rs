//! Perfect negotiation, as a pure state machine.
//!
//! This implements the decision logic of the W3C "perfect negotiation" pattern without
//! touching WebRTC: the caller feeds it events and executes the returned actions against
//! a real peer connection (see `peer::PeerManager`) or, in tests, against a fake.
//!
//! Roles: the peer whose [`PeerId`] is lexicographically smaller is **impolite**; the
//! other is **polite**. When both sides offer at the same time (glare), the impolite
//! side ignores the incoming offer and keeps waiting for the answer to its own; the
//! polite side rolls back its local offer, answers the remote one, and offers again
//! afterwards if it still has changes to negotiate.
//!
//! The engine processes events for one connection strictly sequentially (it awaits each
//! WebRTC operation before handling the next event), so "making an offer" is never
//! observed concurrently with an incoming offer; a collision is simply an incoming
//! offer while our own offer is outstanding.

use chattercrab_protocol::PeerId;

/// Signaling state as tracked by the negotiator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingState {
    /// No offer outstanding.
    Stable,
    /// We sent an offer and wait for the answer.
    HaveLocalOffer,
}

/// What to do with an incoming offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferDecision {
    /// Glare and we are impolite: drop the offer (and its ICE candidates).
    Ignore,
    /// Apply it: optionally roll back our own pending offer first, then set the remote
    /// description, create and send an answer, then call [`Negotiator::answer_sent`].
    Accept {
        /// Roll back the local offer before applying the remote one.
        rollback: bool,
    },
}

/// Perfect-negotiation state for one remote peer.
#[derive(Debug, Clone)]
pub struct Negotiator {
    polite: bool,
    state: SignalingState,
    /// Negotiation was requested while not stable; offer once stable again.
    pending: bool,
    /// The last remote offer was ignored; ICE candidates for it may fail to apply.
    ignoring_offer: bool,
}

impl Negotiator {
    /// Creates a negotiator for the connection between `me` and `remote`.
    pub fn new(me: PeerId, remote: PeerId) -> Self {
        Self::with_politeness(me > remote)
    }

    /// Creates a negotiator with explicit politeness (used by tests).
    pub fn with_politeness(polite: bool) -> Self {
        Self {
            polite,
            state: SignalingState::Stable,
            pending: false,
            ignoring_offer: false,
        }
    }

    /// Whether this side is the polite peer.
    pub fn is_polite(&self) -> bool {
        self.polite
    }

    /// Current signaling state.
    pub fn state(&self) -> SignalingState {
        self.state
    }

    /// Whether a negotiation is queued for when the state returns to stable.
    pub fn has_pending(&self) -> bool {
        self.pending
    }

    /// Whether the most recent remote offer was ignored. Errors while applying remote ICE
    /// candidates are expected (and should be suppressed) in that case.
    pub fn is_ignoring_offer(&self) -> bool {
        self.ignoring_offer
    }

    /// Local changes need negotiating (tracks added/removed, ICE restart).
    ///
    /// Returns true if the caller should create, apply and send an offer now. Otherwise
    /// the request is remembered and returned later by [`Negotiator::answer_sent`] or
    /// [`Negotiator::answer_applied`].
    pub fn negotiation_needed(&mut self) -> bool {
        if self.state == SignalingState::Stable {
            self.state = SignalingState::HaveLocalOffer;
            true
        } else {
            self.pending = true;
            false
        }
    }

    /// Creating or applying our offer failed; return to stable.
    pub fn offer_failed(&mut self) {
        self.state = SignalingState::Stable;
    }

    /// A remote offer arrived.
    pub fn remote_offer(&mut self) -> OfferDecision {
        let collision = self.state != SignalingState::Stable;
        self.ignoring_offer = !self.polite && collision;
        if self.ignoring_offer {
            return OfferDecision::Ignore;
        }
        if collision {
            // Our offer is rolled back; whatever it carried still needs negotiating.
            self.pending = true;
        }
        self.state = SignalingState::Stable;
        OfferDecision::Accept {
            rollback: collision,
        }
    }

    /// Our answer to a remote offer was applied and sent. Returns true if a queued
    /// negotiation should start now (create and send an offer).
    pub fn answer_sent(&mut self) -> bool {
        self.state = SignalingState::Stable;
        self.take_pending()
    }

    /// A remote answer arrived. Returns true if it should be applied (we have an offer
    /// outstanding); false if it is unexpected and must be dropped.
    pub fn remote_answer(&mut self) -> bool {
        self.state == SignalingState::HaveLocalOffer
    }

    /// The remote answer was applied. Returns true if a queued negotiation should start.
    pub fn answer_applied(&mut self) -> bool {
        self.state = SignalingState::Stable;
        self.take_pending()
    }

    fn take_pending(&mut self) -> bool {
        if self.pending {
            self.pending = false;
            self.state = SignalingState::HaveLocalOffer;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    //! Two negotiators connected by a fake signaling channel. Each side also tracks a
    //! simulated "negotiated generation": the set of local changes the remote has seen,
    //! to verify that every requested change is eventually negotiated.

    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Msg {
        /// Offer carrying the sender's local change counter at offer time.
        Offer(u32),
        /// Answer carrying the answerer's local change counter.
        Answer(u32),
    }

    struct Side {
        neg: Negotiator,
        /// Number of local changes made (tracks added, etc.).
        changes: u32,
        /// Changes contained in our outstanding (or last) offer.
        offered: u32,
        /// The remote's changes we have applied.
        seen_remote: u32,
        outbox: VecDeque<Msg>,
        ignored: u32,
        rollbacks: u32,
    }

    impl Side {
        fn new(polite: bool) -> Self {
            Self {
                neg: Negotiator::with_politeness(polite),
                changes: 0,
                offered: 0,
                seen_remote: 0,
                outbox: VecDeque::new(),
                ignored: 0,
                rollbacks: 0,
            }
        }

        fn change(&mut self) {
            self.changes += 1;
            if self.neg.negotiation_needed() {
                self.send_offer();
            }
        }

        fn send_offer(&mut self) {
            self.offered = self.changes;
            self.outbox.push_back(Msg::Offer(self.changes));
        }

        fn receive(&mut self, msg: Msg) {
            match msg {
                Msg::Offer(remote_changes) => match self.neg.remote_offer() {
                    OfferDecision::Ignore => self.ignored += 1,
                    OfferDecision::Accept { rollback } => {
                        if rollback {
                            self.rollbacks += 1;
                        }
                        self.seen_remote = self.seen_remote.max(remote_changes);
                        // An answer can carry our tracks for m-lines the offer created, but
                        // to be conservative we assume it carries none of our new changes.
                        self.outbox.push_back(Msg::Answer(0));
                        if self.neg.answer_sent() {
                            self.send_offer();
                        }
                    }
                },
                Msg::Answer(_) => {
                    if self.neg.remote_answer() && self.neg.answer_applied() {
                        self.send_offer();
                    }
                }
            }
        }
    }

    /// Delivers messages until both outboxes are empty. `interleave` alternates sides one
    /// message at a time; otherwise each side's outbox is flushed in turn.
    fn run(a: &mut Side, b: &mut Side) {
        let mut guard = 0;
        while !a.outbox.is_empty() || !b.outbox.is_empty() {
            guard += 1;
            assert!(guard < 100, "negotiation did not converge");
            if let Some(m) = a.outbox.pop_front() {
                b.receive(m);
            }
            if let Some(m) = b.outbox.pop_front() {
                a.receive(m);
            }
        }
    }

    fn assert_converged(a: &Side, b: &Side) {
        assert_eq!(a.neg.state(), SignalingState::Stable);
        assert_eq!(b.neg.state(), SignalingState::Stable);
        assert!(!a.neg.has_pending() && !b.neg.has_pending());
        assert_eq!(b.seen_remote, a.changes, "b missed some of a's changes");
        assert_eq!(a.seen_remote, b.changes, "a missed some of b's changes");
    }

    #[test]
    fn politeness_follows_peer_id_order() {
        let x: PeerId = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let y: PeerId = "00000000-0000-4000-8000-000000000002".parse().unwrap();
        assert!(!Negotiator::new(x, y).is_polite());
        assert!(Negotiator::new(y, x).is_polite());
    }

    #[test]
    fn simple_offer_answer() {
        let mut a = Side::new(false);
        let mut b = Side::new(true);
        a.change();
        run(&mut a, &mut b);
        assert_converged(&a, &b);
        assert_eq!(a.ignored + b.ignored, 0);
    }

    #[test]
    fn glare_impolite_first_in_queue() {
        // Both sides offer before seeing the other's offer.
        let mut imp = Side::new(false);
        let mut pol = Side::new(true);
        imp.change();
        pol.change();
        run(&mut imp, &mut pol);
        assert_converged(&imp, &pol);
        assert_eq!(
            imp.ignored, 1,
            "impolite side must ignore the colliding offer"
        );
        assert_eq!(pol.rollbacks, 1, "polite side must roll back");
        assert_eq!(pol.ignored, 0);
    }

    #[test]
    fn glare_polite_first_in_queue() {
        // Same race, but the polite side's offer is delivered first.
        let mut pol = Side::new(true);
        let mut imp = Side::new(false);
        pol.change();
        imp.change();
        run(&mut pol, &mut imp);
        assert_converged(&pol, &imp);
        assert_eq!(imp.ignored, 1);
        assert_eq!(pol.rollbacks, 1);
    }

    #[test]
    fn renegotiation_after_connected() {
        let mut a = Side::new(true);
        let mut b = Side::new(false);
        a.change();
        run(&mut a, &mut b);
        // Later: b starts a screen share, then a does, then b stops.
        b.change();
        run(&mut a, &mut b);
        a.change();
        b.change();
        run(&mut a, &mut b);
        assert_converged(&a, &b);
    }

    #[test]
    fn change_while_offer_outstanding_is_queued() {
        let mut a = Side::new(false);
        let mut b = Side::new(true);
        a.change();
        a.change(); // second change before the answer arrives
        assert!(a.neg.has_pending());
        assert_eq!(a.outbox.len(), 1);
        run(&mut a, &mut b);
        assert_converged(&a, &b);
    }

    #[test]
    fn unexpected_answer_is_dropped() {
        let mut n = Negotiator::with_politeness(true);
        assert!(!n.remote_answer());
        assert_eq!(n.state(), SignalingState::Stable);
    }

    #[test]
    fn offer_failure_returns_to_stable() {
        let mut n = Negotiator::with_politeness(false);
        assert!(n.negotiation_needed());
        n.offer_failed();
        assert_eq!(n.state(), SignalingState::Stable);
        assert!(n.negotiation_needed());
    }

    #[test]
    fn ignoring_flag_tracks_last_offer() {
        let mut imp = Negotiator::with_politeness(false);
        assert!(imp.negotiation_needed());
        assert_eq!(imp.remote_offer(), OfferDecision::Ignore);
        assert!(imp.is_ignoring_offer());
        assert!(imp.remote_answer());
        assert!(!imp.answer_applied());
        assert_eq!(
            imp.remote_offer(),
            OfferDecision::Accept { rollback: false }
        );
        assert!(!imp.is_ignoring_offer());
    }

    #[test]
    fn many_random_races_converge() {
        // Deterministic pseudo-random interleavings of changes and deliveries.
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..200 {
            let mut a = Side::new(false);
            let mut b = Side::new(true);
            for _ in 0..20 {
                match next() % 4 {
                    0 => a.change(),
                    1 => b.change(),
                    2 => {
                        if let Some(m) = a.outbox.pop_front() {
                            b.receive(m);
                        }
                    }
                    _ => {
                        if let Some(m) = b.outbox.pop_front() {
                            a.receive(m);
                        }
                    }
                }
            }
            run(&mut a, &mut b);
            assert_converged(&a, &b);
        }
    }
}
