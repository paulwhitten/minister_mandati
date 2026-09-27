//! Per-task loop guards: dedupe of identical successful mutating calls, plus a
//! sliding-window no-progress detector. See `docs/design/design-loop-guards.md`.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use serde_json::Value;

use crate::tools::DedupePolicy;

/// What the loop should do when repetition crosses the threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intervention {
    /// Keep going.
    None,
    /// Inject a one-time nudge asking the model to stop if it is done.
    Nudge,
    /// Abandon the turn; the model ignored the nudge.
    Terminate,
}

pub struct LoopGuards {
    /// Fingerprints of skippable calls that already succeeded, with their result.
    succeeded: HashMap<u64, String>,
    /// Sliding window of recent fingerprints for repeat detection.
    recent: VecDeque<u64>,
    window: usize,
    threshold: usize,
    dedupe: bool,
    nudged: bool,
}

impl LoopGuards {
    pub fn new(dedupe: bool, window: usize, threshold: usize) -> Self {
        Self {
            succeeded: HashMap::new(),
            recent: VecDeque::new(),
            window,
            threshold,
            dedupe,
            nudged: false,
        }
    }

    /// Stable fingerprint of a tool call: name plus canonical (sorted-key) JSON
    /// arguments. `serde_json`'s object map is ordered, so serialization is
    /// deterministic regardless of the key order the model emitted.
    pub fn fingerprint(name: &str, args: &Value) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        serde_json::to_string(args).unwrap_or_default().hash(&mut h);
        h.finish()
    }

    /// Record the call in the sliding window and return how many times this
    /// fingerprint appears within it.
    pub fn observe(&mut self, fp: u64) -> usize {
        if self.window > 0 && self.recent.len() == self.window {
            self.recent.pop_front();
        }
        self.recent.push_back(fp);
        self.recent.iter().filter(|&&h| h == fp).count()
    }

    /// If dedupe is enabled, the policy is skippable, and this fingerprint
    /// already succeeded, return the synthetic result to feed back to the model.
    pub fn skip_result(&self, tool: &str, policy: DedupePolicy, fp: u64) -> Option<String> {
        if self.dedupe
            && policy == DedupePolicy::SkipIfIdenticalSuccess
            && let Some(prev) = self.succeeded.get(&fp)
        {
            return Some(format!(
                "already completed: an identical {tool} call succeeded earlier ({prev}); no changes made"
            ));
        }
        None
    }

    /// Record a successful skippable call so future identical calls are skipped.
    pub fn record_success(&mut self, policy: DedupePolicy, fp: u64, output: &str) {
        if policy == DedupePolicy::SkipIfIdenticalSuccess {
            self.succeeded.insert(fp, output.to_string());
        }
    }

    /// Decide the intervention for a repeat count; transitions to nudged once.
    pub fn intervention(&mut self, repeats: usize) -> Intervention {
        if self.threshold == 0 || repeats < self.threshold {
            return Intervention::None;
        }
        if self.nudged {
            Intervention::Terminate
        } else {
            self.nudged = true;
            Intervention::Nudge
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SKIP: DedupePolicy = DedupePolicy::SkipIfIdenticalSuccess;
    const ALWAYS: DedupePolicy = DedupePolicy::Always;

    #[test]
    fn fingerprint_ignores_key_order() {
        let a = LoopGuards::fingerprint("write_file", &json!({"path": "x", "content": "y"}));
        let b = LoopGuards::fingerprint("write_file", &json!({"content": "y", "path": "x"}));
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_differs_on_args_and_name() {
        let base = LoopGuards::fingerprint("write_file", &json!({"path": "x", "content": "y"}));
        assert_ne!(
            base,
            LoopGuards::fingerprint("write_file", &json!({"path": "x", "content": "z"}))
        );
        assert_ne!(
            base,
            LoopGuards::fingerprint("read_file", &json!({"path": "x", "content": "y"}))
        );
    }

    #[test]
    fn second_identical_success_is_skipped() {
        let mut g = LoopGuards::new(true, 6, 3);
        let fp = LoopGuards::fingerprint("write_file", &json!({"path": "a", "content": "x"}));
        // First occurrence: not yet recorded, so it must execute.
        assert!(g.skip_result("write_file", SKIP, fp).is_none());
        g.record_success(SKIP, fp, "wrote 1 bytes to a");
        // Second identical occurrence: skipped with a synthetic result.
        let skipped = g.skip_result("write_file", SKIP, fp);
        assert!(skipped.unwrap().contains("already completed"));
    }

    #[test]
    fn dedupe_disabled_never_skips() {
        let mut g = LoopGuards::new(false, 6, 3);
        let fp = LoopGuards::fingerprint("write_file", &json!({"path": "a", "content": "x"}));
        g.record_success(SKIP, fp, "wrote 1 bytes to a");
        assert!(g.skip_result("write_file", SKIP, fp).is_none());
    }

    #[test]
    fn always_policy_is_not_recorded() {
        let mut g = LoopGuards::new(true, 6, 3);
        let fp = LoopGuards::fingerprint("execute_bash", &json!({"command": "ls"}));
        g.record_success(ALWAYS, fp, "output");
        assert!(g.skip_result("execute_bash", ALWAYS, fp).is_none());
    }

    #[test]
    fn loop_guard_nudges_then_terminates() {
        let mut g = LoopGuards::new(true, 6, 3);
        let fp = LoopGuards::fingerprint("execute_bash", &json!({"command": "ls"}));
        assert_eq!(g.observe(fp), 1);
        assert_eq!(g.observe(fp), 2);
        assert_eq!(g.observe(fp), 3);
        assert_eq!(g.intervention(3), Intervention::Nudge);
        assert_eq!(g.intervention(3), Intervention::Terminate);
    }

    #[test]
    fn below_threshold_is_no_intervention() {
        let mut g = LoopGuards::new(true, 6, 3);
        assert_eq!(g.intervention(1), Intervention::None);
        assert_eq!(g.intervention(2), Intervention::None);
    }
}
