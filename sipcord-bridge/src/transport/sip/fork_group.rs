//! Fork group tracking for multi-contact outbound call forking.
//!
//! When a user has multiple SIP phones registered, the bridge rings all of them
//! simultaneously. A "fork group" tracks the set of forked call legs for a single
//! logical call (identified by tracking_id). When one leg answers, the siblings
//! are cancelled. When all legs fail, the failure is reported.

use super::CallId;
use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::OnceLock;
use tracing::{debug, info};

/// Global fork group registry, keyed by tracking_id.
static FORK_GROUPS: OnceLock<DashMap<String, ForkGroup>> = OnceLock::new();

fn groups() -> &'static DashMap<String, ForkGroup> {
    FORK_GROUPS.get_or_init(DashMap::new)
}

struct ForkGroup {
    /// Call IDs that were successfully started (active legs)
    sibling_call_ids: HashSet<CallId>,
    /// Call IDs that have failed (answered-then-disconnected, or never-answered)
    failed_call_ids: HashSet<CallId>,
    /// Number of calls that failed to even start (MakeOutboundCall returned error)
    initial_failures: usize,
    /// The call_id that answered first (if any)
    answered_call_id: Option<CallId>,
    /// Total number of fork attempts (successful starts + initial failures)
    expected_total: usize,
}

/// Register a successfully started call leg in a fork group.
///
/// Called from `process_sip_command` after `make_outbound_call` succeeds.
pub fn add_member(tracking_id: &str, call_id: CallId, fork_total: usize) {
    let mut entry = groups()
        .entry(tracking_id.to_string())
        .or_insert_with(|| ForkGroup {
            sibling_call_ids: HashSet::new(),
            failed_call_ids: HashSet::new(),
            initial_failures: 0,
            answered_call_id: None,
            expected_total: fork_total,
        });
    entry.sibling_call_ids.insert(call_id);
    debug!(
        "Fork group {}: added call_id={}, members={}/{}",
        tracking_id,
        call_id,
        entry.sibling_call_ids.len() + entry.initial_failures,
        fork_total
    );
}

/// Track a call that failed to start (make_outbound_call returned error).
///
/// Called from `process_sip_command` when `make_outbound_call` fails.
pub fn add_initial_failure(tracking_id: &str, fork_total: usize) {
    let mut entry = groups()
        .entry(tracking_id.to_string())
        .or_insert_with(|| ForkGroup {
            sibling_call_ids: HashSet::new(),
            failed_call_ids: HashSet::new(),
            initial_failures: 0,
            answered_call_id: None,
            expected_total: fork_total,
        });
    entry.initial_failures += 1;
    debug!(
        "Fork group {}: initial failure, failures={}/{}",
        tracking_id,
        entry.initial_failures + entry.failed_call_ids.len(),
        fork_total
    );
}

/// Mark a fork leg as answered. Returns the sibling call_ids to cancel (if this
/// is the first answer), or `None` if another leg already answered.
///
/// The fork group is removed after this call since the logical call is resolved.
pub fn mark_answered(tracking_id: &str, call_id: CallId) -> Option<Vec<CallId>> {
    // Use remove to get exclusive ownership - prevents races between two simultaneous answers
    let (_, mut group) = groups().remove(tracking_id)?;

    if group.answered_call_id.is_some() {
        // Another leg already answered - this shouldn't happen with DashMap remove,
        // but handle it gracefully
        info!(
            "Fork group {}: call_id={} answered but already resolved",
            tracking_id, call_id
        );
        return None;
    }

    group.answered_call_id = Some(call_id);

    // Collect siblings to cancel (all members except the one that answered, minus already-failed)
    let siblings: Vec<CallId> = group
        .sibling_call_ids
        .iter()
        .filter(|&&id| id != call_id && !group.failed_call_ids.contains(&id))
        .copied()
        .collect();

    info!(
        "Fork group {}: call_id={} answered, cancelling {} siblings",
        tracking_id,
        call_id,
        siblings.len()
    );

    Some(siblings)
}

/// Mark a fork leg as failed. Returns `true` if ALL legs have now failed
/// (meaning the logical call should be reported as failed to the DO).
///
/// The fork group is removed when all legs have failed.
pub fn mark_failed(tracking_id: &str, call_id: CallId) -> bool {
    let mut entry = match groups().get_mut(tracking_id) {
        Some(e) => e,
        None => {
            // Group already removed (answered or all-failed) — this is a late failure
            // from a leg that was being cancelled. Not an error.
            debug!(
                "Fork group {}: call_id={} failed but group already resolved",
                tracking_id, call_id
            );
            return false;
        }
    };

    // If this group was already answered, don't count this failure
    if entry.answered_call_id.is_some() {
        return false;
    }

    entry.sibling_call_ids.remove(&call_id);
    entry.failed_call_ids.insert(call_id);

    let total_resolved = entry.failed_call_ids.len() + entry.initial_failures;
    let all_failed = total_resolved >= entry.expected_total;

    debug!(
        "Fork group {}: call_id={} failed, resolved={}/{}, all_failed={}",
        tracking_id, call_id, total_resolved, entry.expected_total, all_failed
    );

    if all_failed {
        // Drop the mutable ref before removing
        drop(entry);
        groups().remove(tracking_id);
    }

    all_failed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a unique tracking ID per test to avoid interference with the global DashMap
    fn unique_id(base: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!("{}_{}", base, COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn test_add_members_and_answer() {
        let tid = unique_id("answer");
        let c1 = CallId::new(100);
        let c2 = CallId::new(101);
        let c3 = CallId::new(102);

        add_member(&tid, c1, 3);
        add_member(&tid, c2, 3);
        add_member(&tid, c3, 3);

        // c1 answers -> siblings c2, c3 returned for cancellation
        let siblings = mark_answered(&tid, c1).unwrap();
        assert_eq!(siblings.len(), 2);
        assert!(siblings.contains(&c2));
        assert!(siblings.contains(&c3));
    }

    #[test]
    fn test_all_failed() {
        let tid = unique_id("allfail");
        let c1 = CallId::new(200);
        let c2 = CallId::new(201);

        add_member(&tid, c1, 2);
        add_member(&tid, c2, 2);

        assert!(!mark_failed(&tid, c1)); // 1/2 failed
        assert!(mark_failed(&tid, c2)); // 2/2 failed -> all_failed = true
    }

    #[test]
    fn test_answer_on_already_resolved() {
        let tid = unique_id("double_answer");
        let c1 = CallId::new(300);
        let c2 = CallId::new(301);

        add_member(&tid, c1, 2);
        add_member(&tid, c2, 2);

        // First answer removes the group
        mark_answered(&tid, c1);

        // Second answer -> group gone, returns None
        assert!(mark_answered(&tid, c2).is_none());
    }

    #[test]
    fn test_failed_on_already_resolved() {
        let tid = unique_id("fail_after_answer");
        let c1 = CallId::new(400);
        let c2 = CallId::new(401);

        add_member(&tid, c1, 2);
        add_member(&tid, c2, 2);

        mark_answered(&tid, c1);

        // Late failure after answer -> returns false
        assert!(!mark_failed(&tid, c2));
    }

    #[test]
    fn test_initial_failures_plus_call_failures() {
        let tid = unique_id("mixed_fail");
        let c1 = CallId::new(500);

        // 3 total forks: 1 started, 2 failed to start
        add_member(&tid, c1, 3);
        add_initial_failure(&tid, 3);
        add_initial_failure(&tid, 3);

        // The one started leg also fails -> all_failed
        assert!(mark_failed(&tid, c1));
    }

    #[test]
    fn test_single_member_fork_group() {
        let tid = unique_id("single");
        let c1 = CallId::new(600);

        add_member(&tid, c1, 1);

        // Single member answers -> empty siblings list
        let siblings = mark_answered(&tid, c1).unwrap();
        assert!(siblings.is_empty());
    }

    #[test]
    fn test_some_fail_then_answer() {
        let tid = unique_id("partial_fail_answer");
        let c1 = CallId::new(700);
        let c2 = CallId::new(701);
        let c3 = CallId::new(702);

        add_member(&tid, c1, 3);
        add_member(&tid, c2, 3);
        add_member(&tid, c3, 3);

        // c1 fails first
        assert!(!mark_failed(&tid, c1));

        // c2 answers -> should only cancel c3 (c1 already failed)
        let siblings = mark_answered(&tid, c2).unwrap();
        assert_eq!(siblings.len(), 1);
        assert!(siblings.contains(&c3));
    }
}
