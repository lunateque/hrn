//! Session lanes for [`super::runtime::AgyAcpRuntime`].
//!
//! A `tools/call` arriving on the loopback MCP bridge carries no ACP session id, so the bridge
//! cannot tell which hrn session it belongs to. Sharing one bridge between sessions whose turns
//! overlap (the main agent waiting on a subagent, two subagents in parallel) therefore routed
//! calls into whichever turn attached its sink last, and the first turn to finish detached the
//! sink for everyone.
//!
//! Each lane is a dedicated `agy acp` child, MCP bridge and ephemeral agent profile. One hrn
//! session owns a lane from the start of its ACP turn until that turn completes, including the
//! gaps while hrn executes bridged tool calls, so the bridge's single call sink, its tool catalog
//! and `fail_all_pending` only ever touch the owner's calls. Idle lanes are reused (preferring
//! the one holding the session's warm ACP session) and trimmed to [`MAX_IDLE_LANES`].

use super::agent_profile::EphemeralAgentProfile;
use super::bridge::McpBridge;
use super::client::AcpClient;
use super::runtime_turn::ActiveTurn;
use super::session_map::SessionMap;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

/// Idle lanes kept warm after their turns finish; older idle lanes are shut down.
pub(crate) const MAX_IDLE_LANES: usize = 2;

/// Everything one `agy acp` child needs: the process, its bridge and profile, its ACP sessions.
#[derive(Default)]
pub(crate) struct LaneState {
    pub(crate) client: Option<Arc<AcpClient>>,
    pub(crate) bridge: Option<Arc<McpBridge>>,
    pub(crate) agent_profile: Option<EphemeralAgentProfile>,
    pub(crate) session_map: SessionMap,
    /// The suspended turn of the lane's owner (`hrn session id`, turn) between tool legs.
    pub(crate) active_turn: Option<(String, ActiveTurn)>,
}

/// One isolated `agy acp` process with its own bridge.
pub(crate) struct Lane {
    id: u64,
    pub(crate) state: AsyncMutex<LaneState>,
}

struct Slot {
    lane: Arc<Lane>,
    /// The hrn session whose turn currently runs on this lane.
    owner: Option<String>,
    touched: Instant,
    /// hrn sessions whose latest ACP session lives in this lane.
    warm: HashSet<String>,
}

#[derive(Default)]
struct PoolInner {
    slots: Vec<Slot>,
    next_id: u64,
}

/// The lanes of one runtime.
#[derive(Default)]
pub(crate) struct LanePool {
    inner: Mutex<PoolInner>,
}

/// How a lease ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Release {
    /// The owner's turn is suspended waiting for tool results: keep the lane reserved.
    Suspended,
    /// The owner's turn is over; `warm` says whether the lane holds its ACP session.
    Finished { warm: bool },
}

/// Pick the lane `sid` should run on (`None` opens a new lane). A lane owned by another session
/// is only taken once its owner has been silent for `stale_after`, which is at least the ACP turn
/// timeout: by then that turn is dead and the new owner aborts it.
fn pick(slots: &[Slot], sid: &str, now: Instant, stale_after: Duration) -> Option<u64> {
    let idle = |s: &&Slot| s.owner.is_none();
    let stale = |s: &&Slot| s.owner.is_some() && now.duration_since(s.touched) >= stale_after;
    slots
        .iter()
        .find(|s| s.owner.as_deref() == Some(sid))
        .or_else(|| slots.iter().find(|s| idle(s) && s.warm.contains(sid)))
        .or_else(|| {
            slots
                .iter()
                .filter(idle)
                .min_by_key(|s| (s.warm.len(), s.touched))
        })
        .or_else(|| slots.iter().find(stale))
        .map(|s| s.lane.id)
}

impl LanePool {
    fn lock(&self) -> std::sync::MutexGuard<'_, PoolInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserve a lane for `sid`'s turn, opening a new one when every lane is busy.
    pub(crate) fn checkout(&self, sid: &str, stale_after: Duration) -> Lease<'_> {
        let mut pool = self.lock();
        let now = Instant::now();
        let lane_id = if let Some(id) = pick(&pool.slots, sid, now, stale_after) {
            id
        } else {
            pool.next_id += 1;
            let id = pool.next_id;
            pool.slots.push(Slot {
                lane: Arc::new(Lane {
                    id,
                    state: AsyncMutex::new(LaneState::default()),
                }),
                owner: None,
                touched: now,
                warm: HashSet::new(),
            });
            id
        };
        let mut lane = None;
        for slot in pool.slots.iter_mut().filter(|s| s.lane.id == lane_id) {
            slot.owner = Some(sid.to_owned());
            slot.touched = now;
            lane = Some(Arc::clone(&slot.lane));
        }
        drop(pool);
        Lease {
            pool: self,
            // `lane_id` was just picked or pushed under the same lock, so the slot exists.
            lane: lane.unwrap_or_else(|| {
                Arc::new(Lane {
                    id: lane_id,
                    state: AsyncMutex::new(LaneState::default()),
                })
            }),
            sid: sid.to_owned(),
            engaged: false,
            done: false,
        }
    }

    /// Every lane, for diagnostics.
    pub(crate) fn lanes(&self) -> Vec<Arc<Lane>> {
        self.lock()
            .slots
            .iter()
            .map(|s| Arc::clone(&s.lane))
            .collect()
    }

    /// Lanes currently open (busy or idle).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().slots.len()
    }

    /// Apply `how` to `sid`'s claim on `lane_id`; returns lanes trimmed from the pool, which the
    /// caller drops after the pool lock is released (dropping kills their `agy` children).
    fn release(&self, lane_id: u64, sid: &str, how: Release) -> Vec<Arc<Lane>> {
        let mut pool = self.lock();
        let now = Instant::now();
        let owned = pool
            .slots
            .iter()
            .any(|s| s.lane.id == lane_id && s.owner.as_deref() == Some(sid));
        if !owned {
            // Gone, or reclaimed by another session after going stale: that session owns it.
            return Vec::new();
        }
        for slot in &mut pool.slots {
            let mine = slot.lane.id == lane_id;
            match how {
                Release::Suspended if mine => slot.touched = now,
                Release::Suspended => {}
                Release::Finished { warm } if mine => {
                    slot.owner = None;
                    slot.touched = now;
                    if warm {
                        slot.warm.insert(sid.to_owned());
                    } else {
                        slot.warm.remove(sid);
                    }
                }
                Release::Finished { warm } => {
                    if warm {
                        slot.warm.remove(sid);
                    }
                }
            }
        }
        trim_idle(&mut pool.slots)
    }

    /// Drop `lane_id` from the pool outright (its turn was abandoned mid-flight).
    fn evict(&self, lane_id: u64) -> Option<Arc<Lane>> {
        let mut pool = self.lock();
        let idx = pool.slots.iter().position(|s| s.lane.id == lane_id)?;
        Some(pool.slots.remove(idx).lane)
    }

    /// Release an unengaged claim: the lane goes back to idle untouched.
    fn vacate(&self, lane_id: u64, sid: &str) {
        let mut pool = self.lock();
        if let Some(slot) = pool
            .slots
            .iter_mut()
            .find(|s| s.lane.id == lane_id && s.owner.as_deref() == Some(sid))
        {
            slot.owner = None;
        }
    }
}

fn trim_idle(slots: &mut Vec<Slot>) -> Vec<Arc<Lane>> {
    let mut idle: Vec<(Instant, u64)> = slots
        .iter()
        .filter(|s| s.owner.is_none())
        .map(|s| (s.touched, s.lane.id))
        .collect();
    if idle.len() <= MAX_IDLE_LANES {
        return Vec::new();
    }
    idle.sort_unstable();
    let doomed: HashSet<u64> = idle
        .iter()
        .take(idle.len().saturating_sub(MAX_IDLE_LANES))
        .map(|(_, id)| *id)
        .collect();
    let mut trimmed = Vec::new();
    slots.retain(|s| {
        if doomed.contains(&s.lane.id) {
            trimmed.push(Arc::clone(&s.lane));
            false
        } else {
            true
        }
    });
    trimmed
}

/// A session's claim on one lane. Finish it with [`Lease::finish`]; dropping an engaged lease
/// without finishing (the turn future was dropped mid-flight) evicts the lane, since its `agy`
/// child may still be running that turn and must not serve anyone else.
pub(crate) struct Lease<'a> {
    pool: &'a LanePool,
    lane: Arc<Lane>,
    sid: String,
    engaged: bool,
    done: bool,
}

impl Lease<'_> {
    pub(crate) fn lane(&self) -> Arc<Lane> {
        Arc::clone(&self.lane)
    }

    /// Mark that the lane's state is now being driven for this session.
    pub(crate) fn engage(&mut self) {
        self.engaged = true;
    }

    pub(crate) fn finish(mut self, how: Release) {
        self.done = true;
        let trimmed = self.pool.release(self.lane.id, &self.sid, how);
        drop(trimmed);
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if self.engaged {
            drop(self.pool.evict(self.lane.id));
        } else {
            self.pool.vacate(self.lane.id, &self.sid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALE: Duration = Duration::from_mins(10);

    fn owners(pool: &LanePool) -> Vec<Option<String>> {
        pool.lock().slots.iter().map(|s| s.owner.clone()).collect()
    }

    #[test]
    fn overlapping_sessions_get_separate_lanes() {
        let pool = LanePool::default();
        let main = pool.checkout("main", STALE);
        let sub = pool.checkout("sub", STALE);
        assert_ne!(main.lane().id, sub.lane().id);
        main.finish(Release::Suspended);
        sub.finish(Release::Finished { warm: true });
        assert_eq!(owners(&pool), vec![Some("main".to_owned()), None]);
    }

    #[test]
    fn suspended_turn_keeps_its_lane() {
        let pool = LanePool::default();
        let first = pool.checkout("main", STALE);
        let id = first.lane().id;
        first.finish(Release::Suspended);
        let other = pool.checkout("sub", STALE);
        assert_ne!(other.lane().id, id);
        other.finish(Release::Finished { warm: false });
        let again = pool.checkout("main", STALE);
        assert_eq!(again.lane().id, id);
        again.finish(Release::Finished { warm: true });
    }

    #[test]
    fn finished_session_returns_to_its_warm_lane() {
        let pool = LanePool::default();
        let a = pool.checkout("a", STALE);
        let b = pool.checkout("b", STALE);
        let (a_id, b_id) = (a.lane().id, b.lane().id);
        b.finish(Release::Finished { warm: true });
        a.finish(Release::Finished { warm: true });
        let b2 = pool.checkout("b", STALE);
        assert_eq!(b2.lane().id, b_id);
        let a2 = pool.checkout("a", STALE);
        assert_eq!(a2.lane().id, a_id);
        b2.finish(Release::Finished { warm: true });
        a2.finish(Release::Finished { warm: true });
    }

    #[test]
    fn idle_lanes_are_trimmed_oldest_first() {
        let pool = LanePool::default();
        let leases: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .map(|sid| pool.checkout(sid, STALE))
            .collect();
        let ids: Vec<u64> = leases.iter().map(|l| l.lane().id).collect();
        for lease in leases {
            lease.finish(Release::Finished { warm: true });
        }
        assert_eq!(pool.len(), MAX_IDLE_LANES);
        let kept: Vec<u64> = pool.lanes().iter().map(|l| l.id).collect();
        assert_eq!(kept, ids[ids.len() - MAX_IDLE_LANES..]);
    }

    #[test]
    fn stale_owner_is_reclaimed_only_after_timeout() {
        let pool = LanePool::default();
        let stuck = pool.checkout("stuck", STALE);
        let id = stuck.lane().id;
        stuck.finish(Release::Suspended);
        let fresh = pool.checkout("fresh", STALE);
        assert_ne!(fresh.lane().id, id);
        fresh.finish(Release::Finished { warm: false });
        pool.lock().slots.retain(|s| s.lane.id == id);
        let reclaim = pool.checkout("next", Duration::ZERO);
        assert_eq!(reclaim.lane().id, id);
        reclaim.finish(Release::Finished { warm: false });
        // The stale owner's late release must not steal the lane back.
        assert!(pool.release(id, "stuck", Release::Suspended).is_empty());
        assert_eq!(owners(&pool), vec![None]);
    }

    #[test]
    fn dropped_engaged_lease_evicts_and_unengaged_vacates() {
        let pool = LanePool::default();
        let lease = pool.checkout("a", STALE);
        drop(lease);
        assert_eq!(owners(&pool), vec![None]);
        let mut lease = pool.checkout("a", STALE);
        lease.engage();
        drop(lease);
        assert_eq!(pool.len(), 0);
    }
}
