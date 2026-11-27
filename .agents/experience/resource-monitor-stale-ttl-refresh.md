---
name: resource-monitor-stale-ttl-refresh
description: Pending resource-monitor alerts expire after stale_ttl even while the breach continues inside the cooldown; refresh undrained same-key alerts before pruning
metadata:
  type: project
---

## Symptom

A resource warning queued during a long tool call (for example a 90-second build) disappears before `drain_alerts()` runs. The breach is still active the whole time, but the agent never receives a warning.

## Cause

`ResourceEvaluator` only reported an alert when the cooldown expired. During the cooldown it returned `UpdateOutcome::None`, so the tick loop never touched the queued `PendingAlert`. `prune_alerts` then dropped it once `now - created_at > stale_ttl` (30s by default), even though the metric was still breaching and `cooldown_secs` was 300s.

## Fix

- Return `UpdateOutcome::Ongoing` for a latched, still-breaching key inside the cooldown and expose those fresh `ResourceAlert` readings as `EvaluationOutcome::breaching`.
- Apply one evaluation through `apply_outcome`: refresh an existing undrained alert with the same key and `created_at = now`, then prune recovered/expired entries, then enqueue genuinely new alerts.
- Refresh must not insert: if the alert was already drained, the cooldown still prevents re-alerting.

## Verification

`cargo test --locked -p deepseek-code-core --lib services::resource_monitor` covers `sustained_breach_refreshes_undrained_alert_past_stale_ttl`: breach at `t=0`, breach again past `t + stale_ttl + 10s`, assert the pending alert survives with the latest summary and that draining it inside the cooldown does not re-enqueue. The core integration test `crates/core/tests/resource_monitor.rs` (`cooldown_suppresses_alerts_across_iterations`) confirms delivery is still exactly once.
