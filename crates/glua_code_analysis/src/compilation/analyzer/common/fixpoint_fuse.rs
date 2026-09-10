//! Safety fuses for write-driven fixpoint loops.
//!
//! Several analyzer passes loop until a round moves nothing (settled tail,
//! floored fixpoint, return re-derivation, call-site publishing, guard
//! frontier). A pass that re-moves the same owners every round oscillates
//! forever, so every such loop carries a fuse: without one the process hangs
//! silently with no output (observed: 5+ minute cold-build stall from two
//! passes ping-ponging seven owners).
//!
//! This is deliberately NOT a performance budget (the repo forbids those: no
//! caps, no prefilters that change answers). Healthy loops are
//! wavefront-advancing: each round settles strictly more state than the last,
//! because every round re-derives the same candidates against a strictly more
//! complete database, so the answers stop moving once every hop of the chain
//! has seen the hop before it (CityRP cold settles in single-digit rounds
//! everywhere this is used).
//!
//! Loop termination policy, in order:
//!
//! 1. The loop's own convergence test ends it: a round that moves nothing.
//! 2. Exact state-repetition detection ([`FixpointFuse::observe_boundary`]) is
//!    the primary oscillation guard: the same complete post-round boundary
//!    state recurring [`FIXPOINT_CYCLE_CONFIRMATIONS`] times is a confirmed
//!    cycle, and the loop breaks out. A converging wavefront advances every
//!    round, so a repeated state means the loop is re-moving the same owners
//!    without settling them.
//! 3. A high absolute round cap ([`FIXPOINT_ROUND_FUSE`]) sits behind both as
//!    a pure hang guard. It sits orders of magnitude above healthy behavior
//!    (single-digit rounds), so tripping means a non-convergence bug, never a
//!    slow workspace.
//!
//! Hang guard versus semantic bound: [`FixpointFuse::trip`] is process
//! survival, not a convergence proof and not a tuning knob. Correctness must
//! come from the loop's own monotonicity (a round that moves nothing ends the
//! loop; a boundary state that repeats is a confirmed cycle — see
//! [`FixpointFuse::observe_boundary`]). Tripping panics in test/debug builds
//! so regressions fail loudly, and in release logs an error and breaks out:
//! degraded diagnostics are strictly better than a hung language server.
//! Never "fix" slowness by raising these bounds; fix the non-monotonicity
//! that trips them.

use std::collections::HashMap;

/// Bound shared by every write-driven fixpoint fuse. See module docs.
///
/// Orders of magnitude above healthy behavior (single-digit rounds), so
/// tripping means a non-convergence bug, never a slow workspace.
pub const FIXPOINT_ROUND_FUSE: usize = 10_000;

/// Sightings of one non-empty boundary state that confirm a cycle. See
/// [`FixpointFuse::observe_boundary`].
pub const FIXPOINT_CYCLE_CONFIRMATIONS: usize = 3;

/// Counts rounds of one fixpoint loop; see module docs.
pub struct FixpointFuse<T = String> {
    name: &'static str,
    rounds: usize,
    boundary_sightings: HashMap<u64, Vec<(T, usize)>>,
}

impl FixpointFuse {
    pub fn new(name: &'static str) -> Self {
        Self::with_state(name)
    }
}

impl<T> FixpointFuse<T> {
    pub fn with_state(name: &'static str) -> Self {
        FixpointFuse {
            name,
            rounds: 0,
            boundary_sightings: HashMap::new(),
        }
    }

    /// Advance one round. Returns true when the loop must stop now: the owner
    /// should break out immediately, leaving current state in place.
    ///
    /// Pure hang guard: this says nothing about whether the loop has
    /// converged, only that it has run absurdly long and is therefore broken.
    pub fn trip(&mut self) -> bool {
        self.rounds += 1;
        if self.rounds > FIXPOINT_ROUND_FUSE {
            log::error!(
                "fixpoint '{}' did not converge after {} rounds; breaking out instead of hanging. This is a hang guard tripping on a non-convergence bug — investigate the loop's monotonicity, do not raise the bound",
                self.name,
                FIXPOINT_ROUND_FUSE,
            );
            debug_assert!(
                false,
                "fixpoint '{}' did not converge after {} rounds",
                self.name, FIXPOINT_ROUND_FUSE,
            );
            return true;
        }
        false
    }

    /// Observe the loop's post-round boundary state. Returns true when the
    /// loop must stop now: the owner should break out immediately, leaving
    /// current state in place.
    ///
    /// `fingerprint` identifies the round's complete post-round boundary state
    /// (every movable owner with its settled type, not just the owners this
    /// round moved); `empty` reports whether the round's net was empty.
    /// `canonical_state` is that same complete state in canonical
    /// (deterministic, comparable) form. The fingerprint is only a lookup
    /// accelerator: on a repeat sighting the canonical states are compared
    /// for exact equality before anything counts towards a cycle, so the same
    /// net delta recurring while another part of the boundary advances — or a
    /// plain hash collision — never confirms a cycle. The same exact state
    /// seen [`FIXPOINT_CYCLE_CONFIRMATIONS`] times is a confirmed
    /// oscillation: a converging wavefront advances every round, so a
    /// repeated state means the loop is re-moving the same owners without
    /// settling them.
    ///
    /// Both forms are equality-only: they are compared against previous
    /// sightings and never used to order, index, or otherwise decide answers,
    /// so a collision can only delay cycle confirmation (the round-count hang
    /// guard still bounds the loop), never change an analysis result.
    pub fn observe_boundary<Q>(
        &mut self,
        fingerprint: u64,
        empty: bool,
        canonical_state: &Q,
    ) -> bool
    where
        Q: Eq + ToOwned<Owned = T> + ?Sized,
        T: std::borrow::Borrow<Q>,
    {
        if empty {
            return false;
        }
        if !self.record_sighting(fingerprint, canonical_state) {
            return false;
        }
        log::error!(
            "fixpoint '{}' revisited the same boundary state {} times; breaking out instead of hanging. This confirms an oscillation bug — investigate the passes' commit gates, do not raise the bound",
            self.name,
            FIXPOINT_CYCLE_CONFIRMATIONS,
        );
        debug_assert!(
            false,
            "fixpoint '{}' oscillates: same boundary state {} times",
            self.name, FIXPOINT_CYCLE_CONFIRMATIONS,
        );
        true
    }

    /// Records one sighting of an exact boundary state; true once that state
    /// reached [`FIXPOINT_CYCLE_CONFIRMATIONS`] sightings.
    ///
    /// Pure counting with no side effects, so tests can drive the real cycle
    /// logic: confirming through [`FixpointFuse::observe_boundary`] trips its
    /// `debug_assert` in test builds by design.
    fn record_sighting<Q>(&mut self, fingerprint: u64, canonical_state: &Q) -> bool
    where
        Q: Eq + ToOwned<Owned = T> + ?Sized,
        T: std::borrow::Borrow<Q>,
    {
        let bucket = self.boundary_sightings.entry(fingerprint).or_default();
        // An index, not a live `&mut`: the `None` arm pushes into the same
        // bucket the search just borrowed.
        if let Some(idx) = bucket
            .iter()
            .position(|(state, _)| state.borrow() == canonical_state)
        {
            bucket[idx].1 += 1;
            bucket[idx].1 >= FIXPOINT_CYCLE_CONFIRMATIONS
        } else {
            bucket.push((canonical_state.to_owned(), 1));
            FIXPOINT_CYCLE_CONFIRMATIONS <= 1
        }
    }

    /// Forget every recorded boundary state.
    ///
    /// Call when loop state invisible to the fingerprint progressed: the
    /// fingerprint covers type-owner state only, so a recurring fingerprint
    /// amid dynamic-field, signature-return, or member-key progress is
    /// continued convergence, not a cycle. Forgetting here keeps that
    /// progress from confirming an oscillation that never happened.
    pub fn reset_boundary_sightings(&mut self) {
        self.boundary_sightings.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_states_require_exact_equality_despite_hash_collisions() {
        let mut fuse = FixpointFuse::<Vec<(u32, Option<u32>)>>::with_state("test");
        let first = vec![(1, Some(2)), (3, None)];
        let second = vec![(1, Some(2)), (3, Some(4))];
        assert!(!fuse.record_sighting(7, &first));
        assert!(!fuse.record_sighting(7, &second));
        assert!(!fuse.record_sighting(7, &first));
        assert!(!fuse.record_sighting(7, &second));
        assert!(fuse.record_sighting(7, &first));
    }

    #[test]
    fn repeated_exact_state_confirms_a_cycle() {
        let mut fuse = FixpointFuse::new("test");
        assert!(!fuse.record_sighting(7, "X=string"));
        assert!(!fuse.record_sighting(7, "X=string"));
        assert!(fuse.record_sighting(7, "X=string"));
    }

    /// The same net delta recurring while another part of the boundary
    /// advances presents the same fingerprint with a different exact state
    /// every round. That is continued convergence and must never break early,
    /// no matter how many rounds it takes.
    #[test]
    fn recurring_fingerprint_with_advancing_state_never_confirms_a_cycle() {
        let mut fuse = FixpointFuse::new("test");
        for round in 0..FIXPOINT_ROUND_FUSE {
            let canonical = format!("X=string;Y=round{round}");
            assert!(!fuse.record_sighting(7, &canonical));
        }
    }

    /// A genuine two-state oscillation still confirms: each exact state is
    /// tracked on its own, so alternating states reach the confirmation count
    /// on their own sightings.
    #[test]
    fn alternating_exact_states_confirm_on_their_own_sightings() {
        let mut fuse = FixpointFuse::new("test");
        assert!(!fuse.record_sighting(7, "A"));
        assert!(!fuse.record_sighting(7, "B"));
        assert!(!fuse.record_sighting(7, "A"));
        assert!(!fuse.record_sighting(7, "B"));
        assert!(fuse.record_sighting(7, "A"));
        assert!(fuse.record_sighting(7, "B"));
    }

    #[test]
    fn non_type_progress_forgets_pending_sightings() {
        let mut fuse = FixpointFuse::new("test");
        assert!(!fuse.record_sighting(7, "X=string"));
        assert!(!fuse.record_sighting(7, "X=string"));
        fuse.reset_boundary_sightings();
        assert!(!fuse.record_sighting(7, "X=string"));
        assert!(!fuse.record_sighting(7, "X=string"));
        assert!(fuse.record_sighting(7, "X=string"));
    }

    #[test]
    fn empty_rounds_record_nothing() {
        let mut fuse = FixpointFuse::new("test");
        for _ in 0..FIXPOINT_CYCLE_CONFIRMATIONS + 1 {
            assert!(!fuse.observe_boundary(7, true, "X=string"));
        }
        // One genuine sighting after all those empty rounds: if the empties
        // had recorded anything this would already confirm.
        assert!(!fuse.record_sighting(7, "X=string"));
    }

    #[test]
    fn non_confirming_rounds_observe_without_confirming() {
        let mut fuse = FixpointFuse::new("test");
        assert!(!fuse.observe_boundary(7, false, "X=string"));
        assert!(!fuse.observe_boundary(7, false, "X=string"));
    }
}
