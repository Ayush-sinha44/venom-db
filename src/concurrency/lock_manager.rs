use super::transaction::Rid;
use std::collections::{HashMap, HashSet, VecDeque};

/// Lock mode — Shared (read) or Exclusive (write)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

/// One entry in a lock queue for a given resource
#[derive(Debug, Clone)]
pub struct LockRequest {
    pub txn_id: u32,
    pub mode: LockMode,
    pub granted: bool,
}

/// Per-resource lock state
#[derive(Debug, Default)]
pub struct LockQueue {
    pub requests: VecDeque<LockRequest>,
}

impl LockQueue {
    /// Count how many granted locks of each type exist
    pub fn granted_counts(&self) -> (usize, usize) {
        let mut shared = 0;
        let mut exclusive = 0;
        for r in &self.requests {
            if r.granted {
                match r.mode {
                    LockMode::Shared => shared += 1,
                    LockMode::Exclusive => exclusive += 1,
                }
            }
        }
        (shared, exclusive)
    }

    /// Can a new request of `mode` be granted right now?
    pub fn can_grant(&self, txn_id: u32, mode: &LockMode) -> bool {
        let (shared, exclusive) = self.granted_counts();

        // If this txn already holds a compatible lock, upgrade is possible
        let already_holds_exclusive = self
            .requests
            .iter()
            .any(|r| r.txn_id == txn_id && r.granted && r.mode == LockMode::Exclusive);

        if already_holds_exclusive {
            return true;
        }

        match mode {
            // Shared lock: ok if no exclusive lock held by anyone else
            LockMode::Shared => exclusive == 0,
            // Exclusive lock: ok if no other locks held at all
            LockMode::Exclusive => {
                let others_hold = self
                    .requests
                    .iter()
                    .any(|r| r.granted && r.txn_id != txn_id);
                !others_hold
            }
        }
    }
}

/// The Lock Manager — central authority for all lock requests.
///
/// Uses Two-Phase Locking (2PL):
///   Growing phase:  acquire locks as needed
///   Shrinking phase: release all locks at commit/abort
///
/// Deadlock detection: waits-for graph cycle detection.
pub struct LockManager {
    /// resource → queue of lock requests
    lock_table: HashMap<Rid, LockQueue>,

    /// waits-for graph: txn_id → set of txn_ids it is waiting for
    waits_for: HashMap<u32, HashSet<u32>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            lock_table: HashMap::new(),
            waits_for: HashMap::new(),
        }
    }

    /// Attempt to acquire a lock. Returns:
    ///   Ok(true)  — lock granted immediately
    ///   Ok(false) — lock queued (txn must wait)
    ///   Err(...)  — deadlock detected, caller must abort
    pub fn acquire(&mut self, txn_id: u32, rid: Rid, mode: LockMode) -> Result<bool, String> {
        let queue = self.lock_table.entry(rid).or_default();

        // Already holds this lock?
        if queue
            .requests
            .iter()
            .any(|r| r.txn_id == txn_id && r.granted)
        {
            // Upgrade: if we hold Shared and want Exclusive
            if mode == LockMode::Exclusive {
                let holds_exclusive = queue
                    .requests
                    .iter()
                    .any(|r| r.txn_id == txn_id && r.granted && r.mode == LockMode::Exclusive);
                if holds_exclusive {
                    return Ok(true);
                }
                // Try upgrade
                if queue.can_grant(txn_id, &LockMode::Exclusive) {
                    for r in queue.requests.iter_mut() {
                        if r.txn_id == txn_id {
                            r.mode = LockMode::Exclusive;
                        }
                    }
                    return Ok(true);
                }
            } else {
                return Ok(true); // already holds S, wants S
            }
        }

        let can_grant = queue.can_grant(txn_id, &mode);

        queue.requests.push_back(LockRequest {
            txn_id,
            mode,
            granted: can_grant,
        });

        if !can_grant {
            // Record who we're waiting for (for deadlock detection)
            let blockers: HashSet<u32> = queue
                .requests
                .iter()
                .filter(|r| r.granted && r.txn_id != txn_id)
                .map(|r| r.txn_id)
                .collect();

            self.waits_for.insert(txn_id, blockers);

            // Check for deadlock
            if self.has_cycle(txn_id) {
                // Remove the waiting request we just added
                let q = self.lock_table.get_mut(&rid).unwrap();
                q.requests.retain(|r| !(r.txn_id == txn_id && !r.granted));
                self.waits_for.remove(&txn_id);
                return Err(format!(
                    "deadlock detected: txn {} is in a wait cycle",
                    txn_id
                ));
            }
        }

        Ok(can_grant)
    }

    /// Release all locks held by a transaction (called on commit or abort).
    /// After releasing, try to grant waiting requests.
    pub fn release_all(&mut self, txn_id: u32) {
        // Collect rids where this txn holds locks
        let rids: Vec<Rid> = self.lock_table.keys().cloned().collect();

        for rid in rids {
            let queue = self.lock_table.get_mut(&rid).unwrap();
            queue.requests.retain(|r| r.txn_id != txn_id);
            self.try_grant_waiting(rid);
        }

        self.waits_for.remove(&txn_id);
        // Remove this txn from other txns' wait sets
        for waiters in self.waits_for.values_mut() {
            waiters.remove(&txn_id);
        }
    }

    /// After a lock is released, try to grant queued waiting requests.
    fn try_grant_waiting(&mut self, rid: Rid) {
        let queue = match self.lock_table.get_mut(&rid) {
            Some(q) => q,
            None => return,
        };

        // Compute current granted counts BEFORE iterating mutably.
        let mut shared = queue
            .requests
            .iter()
            .filter(|r| r.granted && matches!(r.mode, LockMode::Shared))
            .count();

        let mut exclusive = queue
            .requests
            .iter()
            .filter(|r| r.granted && matches!(r.mode, LockMode::Exclusive))
            .count();

        for req in queue.requests.iter_mut() {
            if req.granted {
                continue;
            }

            let can = match req.mode {
                LockMode::Shared => exclusive == 0,
                LockMode::Exclusive => shared == 0 && exclusive == 0,
            };

            if can {
                req.granted = true;

                // Update counts because this request is now granted.
                match req.mode {
                    LockMode::Shared => shared += 1,
                    LockMode::Exclusive => exclusive += 1,
                }
            }
        }
    }

    /// Deadlock detection: DFS cycle check in the waits-for graph.
    fn has_cycle(&self, start: u32) -> bool {
        let mut visited = HashSet::new();
        let mut stack = vec![start];

        while let Some(node) = stack.pop() {
            if !visited.insert(node) {
                return true; // visited twice → cycle
            }
            if let Some(waiters) = self.waits_for.get(&node) {
                for &w in waiters {
                    stack.push(w);
                }
            }
        }
        false
    }

    /// Check if a txn currently holds a lock on a rid
    pub fn holds_lock(&self, txn_id: u32, rid: Rid, mode: &LockMode) -> bool {
        self.lock_table.get(&rid).map_or(false, |q| {
            q.requests.iter().any(|r| {
                r.txn_id == txn_id
                    && r.granted
                    && (
                        r.mode == *mode || r.mode == LockMode::Exclusive
                        // X covers both
                    )
            })
        })
    }

    pub fn lock_count(&self) -> usize {
        self.lock_table
            .values()
            .map(|q| q.requests.iter().filter(|r| r.granted).count())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(p: u32, s: u16) -> Rid {
        Rid {
            page_id: p,
            slot_id: s,
        }
    }

    #[test]
    fn test_shared_locks_compatible() {
        let mut lm = LockManager::new();
        // Two txns can both hold shared locks
        assert!(lm.acquire(1, rid(0, 0), LockMode::Shared).unwrap());
        assert!(lm.acquire(2, rid(0, 0), LockMode::Shared).unwrap());
    }

    #[test]
    fn test_exclusive_blocks_shared() {
        let mut lm = LockManager::new();
        // txn 1 holds exclusive
        assert!(lm.acquire(1, rid(0, 0), LockMode::Exclusive).unwrap());
        // txn 2 wants shared — must wait
        let result = lm.acquire(2, rid(0, 0), LockMode::Shared).unwrap();
        assert!(!result); // queued, not granted
    }

    #[test]
    fn test_release_grants_waiting() {
        let mut lm = LockManager::new();
        assert!(lm.acquire(1, rid(0, 0), LockMode::Exclusive).unwrap());
        // txn 2 queued
        lm.acquire(2, rid(0, 0), LockMode::Shared).unwrap();
        // txn 1 releases
        lm.release_all(1);
        // txn 2 should now be granted
        assert!(lm.holds_lock(2, rid(0, 0), &LockMode::Shared));
    }

    #[test]
    fn test_deadlock_detection() {
        let mut lm = LockManager::new();
        // txn 1 holds lock on row A
        lm.acquire(1, rid(0, 0), LockMode::Exclusive).unwrap();
        // txn 2 holds lock on row B
        lm.acquire(2, rid(0, 1), LockMode::Exclusive).unwrap();
        // txn 1 waits for row B
        lm.acquire(1, rid(0, 1), LockMode::Exclusive).unwrap();
        // txn 2 waits for row A — deadlock!
        let result = lm.acquire(2, rid(0, 0), LockMode::Exclusive);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("deadlock"));
    }

    #[test]
    fn test_multiple_independent_rows() {
        let mut lm = LockManager::new();
        // Different rows — no conflict
        assert!(lm.acquire(1, rid(0, 0), LockMode::Exclusive).unwrap());
        assert!(lm.acquire(2, rid(0, 1), LockMode::Exclusive).unwrap());
        assert!(lm.acquire(3, rid(1, 0), LockMode::Shared).unwrap());
    }

    #[test]
    fn test_release_clears_locks() {
        let mut lm = LockManager::new();
        lm.acquire(1, rid(0, 0), LockMode::Exclusive).unwrap();
        assert_eq!(lm.lock_count(), 1);
        lm.release_all(1);
        assert_eq!(lm.lock_count(), 0);
    }

    #[test]
    fn test_txn_can_reacquire_own_lock() {
        let mut lm = LockManager::new();
        // Same txn acquiring same lock twice is idempotent
        assert!(lm.acquire(1, rid(0, 0), LockMode::Shared).unwrap());
        assert!(lm.acquire(1, rid(0, 0), LockMode::Shared).unwrap());
        assert_eq!(lm.lock_count(), 1);
    }
}
