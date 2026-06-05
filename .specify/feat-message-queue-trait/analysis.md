# Specification Analysis Report

**Feature**: MessageQueue Trait Abstraction (`feat-message-queue-trait`)
**Analyzed**: 2026-06-05
**Branch**: `feat/message-queue-trait`
**Artifacts**: spec.md · plan.md · tasks.md · constitution.md · src/hub/queue.rs (current baseline)

---

## Executive Summary

- **Total Findings**: 10
- **Critical**: 1 ⚠️
- **High**: 2
- **Medium**: 3
- **Low**: 4

**Overall Status**: BLOCKED

One critical constitution violation must be resolved before implementation begins. Two high-priority issues (task decomposition gap causing non-compilable intermediate state, and silent error suppression) should be addressed in the task definitions before coding starts. Medium and low findings can be resolved incrementally.

---

## Findings

| ID | Category | Severity | Location(s) | Summary | Recommendation |
|----|----------|----------|-------------|---------|----------------|
| C1 | Constitution Violation | CRITICAL | plan.md (P5 check) · tasks.md T004 · src/hub/queue.rs:70-74 | `messages_dropped` metric counter is NOT incremented on overflow drops. Current code only logs `warn!`; plan's P5 check only verifies the 200-cap is preserved, not the metric counter. Constitution P5 explicitly states "a metric counter MUST be incremented" when the cap is reached. | Update `InMemoryQueue::push` (T004) to return a typed result or emit a counter-increment signal on overflow. Simplest fix: pass `&AtomicU64` counter into `InMemoryQueue::new`, or add a dedicated `fn overflow_count(&self) -> u64` to the trait. Alternatively, have `push` return `Ok(bool)` where `true` = overflow occurred, and increment the counter at the call site. |
| H1 | Task Decomposition Gap | HIGH | tasks.md T015 (Phase 3) · tasks.md T018 (Phase 5) | T015 creates `build_queue_backend()` with only the `"memory" \| ""` match arm; T018 adds `"redis"` and catch-all arms two phases later. Between these tasks, the match is non-exhaustive — Rust will refuse to compile. No task specifies a temporary wildcard arm to bridge the gap. | Merge T015 and T018 into a single task so `build_queue_backend()` is complete in one commit, OR add an explicit sub-step to T015: "Add a temporary wildcard arm `_ => Err(anyhow::anyhow!("unsupported backend"))` to keep the match exhaustive until T018 replaces it." |
| H2 | Silent Error Suppression | HIGH | tasks.md T012 · tasks.md T013 | Both tasks use `.await.unwrap_or_default()` on `Result` values from `drain()` and `queue_sizes()` without any logging. If the queue backend returns an error, messages silently vanish and the metrics endpoint returns an empty map. This contradicts constitution P1 ("All fallible operations MUST use `?` or explicit `match`/`map_err`") and P6 (observability). | Replace `unwrap_or_default()` with explicit error handling: `match state.queue.drain(&vtoken).await { Ok(msgs) => msgs, Err(e) => { error!(error = %e, vtoken = %vtoken, "drain failed"); vec![] } }`. Same pattern for `queue_sizes()`. |
| M1 | Spec–Plan Semantic Divergence | MEDIUM | spec.md FR-005 · plan.md Phase 0 decisions · plan.md §1.1 | FR-005 says the trait MUST expose "a method to obtain a notification handle for a vtoken." The plan redesigns this as a blocking `wait_notify(&self, vtoken, timeout_secs) → Result<bool>` — a valid and arguably better design for remote backends, but the spec wording was never updated to reflect the resolved decision. Downstream implementors reading only spec.md will misunderstand the contract. | Update FR-005 in spec.md to match the plan's chosen design: "The `MessageQueue` trait MUST expose an async method `wait_notify(vtoken, timeout_secs) → Result<bool, HubError>` that blocks until a message is available for the vtoken or the timeout elapses, returning `true` if notified." |
| M2 | Coverage Gap — Edge Case 2 | MEDIUM | spec.md Edge Cases §2 · tasks.md T019–T029 | Spec edge case: "How does the system handle a vtoken that receives messages before the client has registered? Messages must be buffered and retrievable." The plan's auto-create-on-push design handles this by design, but no test in T019–T029 explicitly verifies pre-registration buffering. | Add a test `test_push_before_register()` to Phase 6: push messages to a vtoken that has no prior `ensure()` call (i.e., push to a fresh `InMemoryQueue` for a vtoken that was never explicitly initialized), then call `drain()` and assert all messages are present. |
| M3 | SC-003 Unverifiable | MEDIUM | spec.md SC-003 · tasks.md Phase 7 | SC-003 requires "no measurable startup regression" after the refactor. No task establishes a baseline startup time, and T032's verification is manual ("verify the binary starts"). There is no automated performance guard. | Add a note to T032 documenting a manual acceptance procedure: time the current startup (`time ilink-hub &`) before implementing, record the baseline, and compare after. Alternatively, accept that SC-003 is validated by human observation during T032 and document this explicitly in the task. |
| L1 | FR-002 Precision Mismatch | LOW | spec.md FR-002 · plan.md §1.1 · tasks.md T003/T005/T006 | FR-002 says "usable as `Arc<dyn MessageQueue>`", but plan and all tasks consistently use `Arc<dyn MessageQueue + Send + Sync>`. The `+ Send + Sync` bounds are required for `Arc<dyn T>` to be useful across async tasks — the plan is more precise and correct, but the spec is imprecise. | Update FR-002 in spec.md: "The `MessageQueue` trait MUST be object-safe and usable as `Arc<dyn MessageQueue + Send + Sync>` — the trait definition MUST include `Send + Sync` as supertrait bounds." |
| L2 | Edge Case 3 Not Fully Tested | LOW | spec.md Edge Cases §3 · tasks.md T026 · tasks.md T027 | Spec edge case: "when a client disconnects and its queue is removed, but a message arrives for that vtoken concurrently — no crash or panic." T026 tests sequential remove-then-push; T027 tests concurrent pushes. Neither tests a *concurrent* `remove_client` + `push` race. | Extend T026 or add a new test: spawn 10 tasks alternating between `push` and `remove_client` on the same vtoken; assert no panic and no data race. This verifies the `Mutex` inside `InMemoryQueue` serializes the race correctly. |
| L3 | SC-002 Cannot Be Verified by Test Suite | LOW | spec.md SC-002 · tasks.md T029 | SC-002: "downstream crate can implement `MessageQueue` in under 30 minutes." T029 (`test_mock_implementation`) defines `NoopQueue` inside the `ilink-hub` test suite — not an external crate. It verifies compilation but not the developer-experience criterion. | Accept that SC-002 is a human judgment criterion satisfied by T017 (rustdoc example) and T029 (mock compilation). Document this explicitly: "SC-002 is validated by T017 (integration example in rustdoc) and T029 (in-crate mock compilation); the 30-minute criterion is a qualitative goal." |
| L4 | remove_client Is New Functionality, Not Preserved Behavior | LOW | spec.md FR-007 · spec.md FR-013 · src/hub/queue.rs (current) | FR-013 says "existing single-tenant behavior MUST be fully preserved." FR-007 introduces `remove_client`. However, the current `QueueStore` has NO `remove_client` method — queues currently grow indefinitely without cleanup. FR-007 is adding net-new lifecycle management. The pairing of FR-007 with FR-013 creates a false impression that removal already exists. | Clarify FR-013 scope: "The existing message flow behavior (push, drain, notify) is fully preserved. FR-007 intentionally adds new lifecycle management (`remove_client`) that was absent in the pre-trait baseline — this is a deliberate improvement, not a regression risk." Also update the plan's call site table (§1.5) to note that `remove_client` is a new call site with no prior equivalent. |

---

## Coverage Summary

### Functional Requirements → Tasks

| Requirement | Has Task? | Task IDs | Notes |
|-------------|-----------|----------|-------|
| FR-001: Define `MessageQueue` trait | ✓ | T003 | |
| FR-002: Object-safe (`Arc<dyn MessageQueue>`) | ✓ | T003, T028 | Wording mismatch — see L1 |
| FR-003: `push` method | ✓ | T003, T004, T020 | |
| FR-004: `drain` method | ✓ | T003, T004, T021 | |
| FR-005: Notification method | ✓ | T003, T004, T023, T024 | Semantic divergence — see M1 |
| FR-006: `queue_sizes` method | ✓ | T003, T004, T025 | |
| FR-007: `remove_client` method | ✓ | T003, T004, T026 | New functionality — see L4 |
| FR-008: `InMemoryQueue` struct | ✓ | T004 | |
| FR-009: Preserve overflow/notification behavior | ✓ | T004, T022 | Missing metric counter — see C1 |
| FR-010: `HubState` holds `Arc<dyn MessageQueue>` | ✓ | T005, T006 | |
| FR-011: `ILINK_QUEUE_BACKEND` env var | ✓ | T015, T018 | Task split creates compile gap — see H1 |
| FR-012: Redis missing URL → startup error | ✓ | T018 | |
| FR-013: Backward compatibility | ✓ | T005–T015 collectively | |
| FR-014: RedisQueue out of scope | ✓ | T018 (documented) | |

**Coverage Metrics**:
- Total Functional Requirements: 14
- Requirements with Tasks: 14 (100%)
- Requirements without Tasks: 0

### Edge Cases → Tests

| Edge Case | Has Test? | Test IDs | Notes |
|-----------|-----------|----------|-------|
| EC1: Concurrent pushes serialized | ✓ | T027 | |
| EC2: Messages before client registers | ✗ | — | No explicit test — see M2 |
| EC3: Concurrent disconnect + push | ~ | T026 (sequential only) | Not truly concurrent — see L2 |
| EC4: `drain()` empty queue | ✓ | T021 | |
| EC5: `wait_notify` no prior notifier | ✓ | T023, T024 (implicit) | Creates notifier on demand |

### Success Criteria → Tasks

| SC | Has Task? | Task IDs | Notes |
|----|-----------|----------|-------|
| SC-001: Existing behavior preserved | ✓ | T005–T015 | |
| SC-002: Downstream integration in <30 min | ~ | T017, T029 | Human judgment criterion — see L3 |
| SC-003: No startup regression | ~ | T032 (manual) | No baseline measurement — see M3 |
| SC-004: Redis switch = env var only | ✓ | T018 | |
| SC-005: All 5 ops tested; coverage ≥ baseline | ✓ | T019–T029 | |
| SC-006: Misconfiguration → actionable error | ✓ | T018, T032 | |

---

## Constitution Alignment

| Principle | Status | Notes |
|-----------|--------|-------|
| P1 — Code Quality & Standards | ⚠️ | H2: `unwrap_or_default()` in T012/T013 suppresses errors without logging, violating the MUST use `?` or explicit `match` rule |
| P2 — Testing Philosophy | ✓ | 10 tests covering all FRs; unit + integration patterns followed |
| P3 — Architecture Constraints | ✓ | Trait injection mandated by P3; module DAG preserved; tokio-only runtime |
| P4 — Security Baseline | ✓ | No new credential exposure; vtokens remain opaque; no cross-tenant risk |
| P5 — Performance Baseline | ❌ **CRITICAL** | C1: "a metric counter MUST be incremented" on overflow drop. Plan's P5 check is incomplete — it verifies the 200-cap but not the counter. Current code already has this violation (only `warn!` log, no counter); the plan fails to fix it. |
| P6 — Operations Baseline | ⚠️ | H2: Swallowed errors from `drain()` and `queue_sizes()` violate the observability requirement ("ERROR for actionable failures") |

---

## Unmapped Tasks

All 33 tasks map to at least one requirement, user story, or constitution principle:

- **T001**: Infrastructure (enables FR-001 through FR-014 by providing `async-trait`)
- **T002–T004**: FR-001 through FR-009 (core trait + implementation)
- **T005–T015**: FR-010 through FR-013, US1 call sites
- **T016–T017**: FR-001 (public export), SC-002
- **T018**: FR-011, FR-012, US3
- **T019–T029**: SC-005, all FR edge cases
- **T030–T033**: Constitution P1 (clippy/fmt), P2 (CI gate)

✓ No unmapped tasks found.

---

## Metrics

- **Total Functional Requirements**: 14
- **Total User Stories**: 3
- **Total Edge Cases**: 5
- **Total Success Criteria**: 6
- **Total Tasks**: 33
- **FR Coverage**: 100% (all 14 FRs have ≥1 task)
- **Edge Case Test Coverage**: 60% (3/5 fully covered; 1 partial; 1 missing)
- **Critical Issues**: 1
- **High Issues**: 2
- **Medium Issues**: 3
- **Low Issues**: 4

---

## Critical Finding Detail: C1 — Overflow Counter Gap

This finding warrants additional explanation because it involves a pre-existing violation that the refactor is an opportunity to fix.

### Current Behavior (baseline: `src/hub/queue.rs:70–74`)

```rust
pub fn push(&mut self, msg: InboundMessage) {
    if self.pending.len() >= MAX_QUEUE_SIZE {
        self.pending.pop_front();
        warn!(max = MAX_QUEUE_SIZE, "client queue full, dropping oldest message");
        // ⚠️  messages_dropped counter is NOT incremented here
    }
    self.pending.push_back(msg);
    self.notify.notify_one();
}
```

The `messages_dropped` counter in `Metrics` is only incremented in two places in `hub/mod.rs`:
1. `RoutingDecision::Broadcast` when `online.is_empty()` (entire message dropped, no clients)
2. T007/T008 (new) when `state.queue.push().await` returns `Err`

Overflow drops (silent head-drop at cap) are **never** counted. Constitution P5: "When the cap is reached, the oldest message MUST be dropped (head-drop policy) **and a metric counter MUST be incremented**."

### Impact

- Prometheus metrics will undercount `messages_dropped`
- Operators have no signal when clients are falling behind — they see `warn!` in logs but no counter
- The Prometheus alerting rule `messages_dropped > threshold` will not fire for overflow

### Recommended Fix

**Option A (minimal)**: Add a counter ref to `InMemoryQueue`:

```rust
pub struct InMemoryQueue {
    queues: tokio::sync::Mutex<HashMap<String, ClientQueue>>,
    overflow_count: Arc<AtomicU64>,  // shared with Metrics
}
```

Increment `overflow_count` inside `push` on head-drop. Expose via `queue_sizes()` or a separate accessor, and add to the Prometheus metrics scrape.

**Option B (trait-based)**: Return a typed result from `push`:

```rust
pub enum PushResult { Enqueued, EnqueuedWithOverflow }
async fn push(&self, vtoken: &str, msg: InboundMessage) -> Result<PushResult, HubError>;
```

The call site in `hub/mod.rs` increments `messages_dropped` when `PushResult::EnqueuedWithOverflow`.

Option B keeps `InMemoryQueue` free of `Metrics` coupling and is more appropriate for a trait abstraction — remote backends can also signal overflow this way.

---

## Next Actions

⚠️ **IMPLEMENTATION BLOCKED**

Critical issue C1 must be resolved before proceeding to `/speckit.implement`. Two high issues (H1, H2) should be resolved in task definitions before coding begins.

### Ordered Remediation Steps

1. **C1** (CRITICAL): Update `InMemoryQueue::push` design to signal overflow to the caller so `messages_dropped` can be incremented. Update T004, T007, T008 accordingly. Add a test verifying the counter increments on overflow (extend T022).

2. **H1** (HIGH): Merge T015 + T018, or add an explicit sub-step to T015 specifying a temporary wildcard match arm. Verify `cargo build` passes after T015 before T018 is needed.

3. **H2** (HIGH): Update T012 and T013 task descriptions to use explicit error handling with `error!()` logging instead of `unwrap_or_default()`.

4. **M1** (MEDIUM): Update FR-005 in spec.md to match the plan's `wait_notify` design decision.

5. **M2** (MEDIUM): Add `test_push_before_register` to Phase 6 task list (after T019).

6. **L1** (LOW): Update FR-002 in spec.md to include `+ Send + Sync` bounds.

7. **L4** (LOW): Clarify FR-013 to note that `remove_client` is intentionally net-new.

---

## Remediation Assistance

Would you like suggested concrete edits for any of the findings above?

The highest-impact items for a quick fix are:

- **C1**: New `PushResult` enum + trait signature change (affects T003, T004, T007, T008, T022)
- **H1**: Merged T015+T018 task text
- **H2**: Replacement code snippets for T012 and T013

Specify which items you'd like addressed first and I will provide exact text for spec/plan/tasks edits without applying them automatically.
