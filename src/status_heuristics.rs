//! Shared completion-status heuristic, single vocabulary for all three
//! status-classification call sites (#3627)

/// True when a non-canonical status string clearly reads as task completion.
/// Conservative: requires a success cue and no failure cue (#3032).
pub fn status_looks_like_descriptive_completion(status: &str) -> bool {
    let normalized = status.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }

    let has_success_cue = [
        "complete",
        "completed",
        "done",
        "finished",
        "success",
        "succeeded",
        "resolved",
        "applied",
        "patched",
        "handled",
        "closed",
        "acknowledged",
        "fixed",
        "verified",
        "skipped",
        "merged",
        "implemented",
        "addressed",
        "nothing to do",
        "nothing to trade",
        "no changes needed",
        "already implemented",
    ]
    .iter()
    .any(|cue| normalized.contains(cue))
        || normalized.ends_with("_addressed")
        || normalized.ends_with("_skipped")
        || normalized.ends_with("_done")
        || normalized.ends_with("_fixed")
        || normalized.ends_with("_complete")
        || normalized.ends_with("_completed")
        || normalized.ends_with("_resolved")
        || normalized.ends_with("_handled")
        || normalized.ends_with("_applied")
        || normalized.ends_with("_patched");

    if !has_success_cue {
        return false;
    }

    let has_failure_cue = [
        "error",
        "failed",
        "failure",
        "blocked",
        "cannot",
        "can't",
        "unable",
        "retry",
        "rate limit",
        "timed out",
        // Negations of success stems that contain a success substring
        "incomplete",
        "unresolved",
        "unsuccessful",
        "unaddressed",
        "unhandled",
        "unverified",
        "unapplied",
        "unpatched",
        "not fixed",
        "not_fixed",
        "not done",
        "not_done",
        "overdue",
    ]
    .iter()
    .any(|cue| normalized.contains(cue));

    !has_failure_cue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressed_is_completion() {
        assert!(status_looks_like_descriptive_completion("addressed"));
        assert!(status_looks_like_descriptive_completion(
            "duplicate_skipped"
        ));
    }

    #[test]
    fn negations_are_not_completion() {
        assert!(!status_looks_like_descriptive_completion("unresolved"));
        assert!(!status_looks_like_descriptive_completion("incomplete"));
        assert!(!status_looks_like_descriptive_completion("not_fixed"));
        assert!(!status_looks_like_descriptive_completion("unaddressed"));
    }

    #[test]
    fn failure_cues_still_win() {
        assert!(!status_looks_like_descriptive_completion(
            "addressed_but_tests_failed"
        ));
        assert!(!status_looks_like_descriptive_completion(
            "resolved_with_error"
        ));
        assert!(!status_looks_like_descriptive_completion(
            "done_but_blocked_on_ci"
        ));
    }

    #[test]
    fn non_completions_stay_false() {
        assert!(!status_looks_like_descriptive_completion("in_progress"));
        assert!(!status_looks_like_descriptive_completion(
            "waiting_on_dependency"
        ));
        assert!(!status_looks_like_descriptive_completion(""));
    }
}
