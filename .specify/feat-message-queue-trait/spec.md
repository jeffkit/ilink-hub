# Feature Specification: MessageQueue Trait Abstraction

**Feature Branch**: `feat/message-queue-trait`  
**Created**: 2026-06-05  
**Status**: Draft  
**Input**: Introduce a `MessageQueue` trait abstraction enabling swappable queue backends (in-memory and Redis) for single-tenant and SaaS deployment modes.

---

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Personal User: Zero-Dependency Default (Priority: P1)

As a personal user running ilink-hub on my own machine, I want the software to work exactly as it does today — with no new runtime dependencies — so that I don't need to install or configure Redis.

**Why this priority**: This is the largest current user segment. Any regression or new required dependency for personal use would break backward compatibility and alienate existing users. Preserving the zero-dependency experience is non-negotiable.

**Independent Test**: Can be fully tested by launching ilink-hub with no additional environment variables and verifying that messages flow from WeChat through the in-memory queue to API clients — identical behavior to the pre-trait version.

**Acceptance Scenarios**:

1. **Given** a fresh ilink-hub installation with no `ILINK_QUEUE_BACKEND` environment variable set, **When** the service starts, **Then** it initializes an in-memory queue with no attempt to connect to Redis and no error messages about missing queue configuration.
2. **Given** the service is running with the in-memory backend, **When** a WeChat inbound message arrives, **Then** the message is buffered in memory per virtual context token (vtoken), notifiers are triggered for any waiting long-pollers, and the message is retrievable by the corresponding client.
3. **Given** the service is running with the in-memory backend, **When** a client long-polls for updates, **Then** the client receives buffered messages and is notified of new ones without requiring any external process communication.
4. **Given** the in-memory queue for a vtoken has reached its maximum buffer limit (200 messages), **When** a new message arrives, **Then** the oldest message is silently dropped and the new message is enqueued, preserving the existing overflow behavior.

---

### User Story 2 — Library Consumer: Trait-Based Extension Point (Priority: P2)

As the maintainer of `ilink-saas` (a private downstream project), I want `ilink-hub` to expose a `MessageQueue` trait so that I can implement a `RedisQueue` in my private repository without modifying the open-source core.

**Why this priority**: This is the primary architectural goal of the feature. Without this trait, the SaaS project must fork or patch the core library, creating an unsustainable maintenance burden.

**Independent Test**: Can be fully tested by verifying the trait is publicly exported in the `ilink-hub` library crate, that `HubState` accepts any type implementing the trait, and that a minimal mock implementation compiles and integrates correctly — without shipping a Redis implementation.

**Acceptance Scenarios**:

1. **Given** a downstream Rust crate that depends on `ilink-hub`, **When** a developer implements the `MessageQueue` trait for a custom struct, **Then** the implementation compiles without errors and the struct can be passed to `HubState` initialization.
2. **Given** the `MessageQueue` trait definition, **When** a developer inspects the public API, **Then** all five capabilities are available as trait methods: push a message to a named queue, drain messages from a named queue, await notification of new messages, retrieve queue size metrics, and remove a queue for a disconnected client.
3. **Given** `HubState` is constructed with a custom `MessageQueue` implementation, **When** messages are pushed by the upstream service and drained by the API handler, **Then** they flow through the custom implementation transparently.
4. **Given** the `MessageQueue` trait, **When** a developer attempts to use it as a trait object (`dyn MessageQueue`), **Then** it compiles successfully — confirming object safety.

---

### User Story 3 — Operator: Environment-Based Backend Selection (Priority: P3)

As a developer deploying ilink-hub in a SaaS environment, I want to select the queue backend through an environment variable, so that switching from in-memory to Redis is a configuration change and not a code change.

**Why this priority**: This enables runtime deployment flexibility without recompilation. It is dependent on P2 (the trait must exist), and the concrete Redis backend is out of scope for this feature — so this story only covers the `memory` default and the plumbing for future backends.

**Independent Test**: Can be fully tested by verifying that `ILINK_QUEUE_BACKEND=memory` (explicit) produces the same behavior as unset, and that setting `ILINK_QUEUE_BACKEND=redis` without a Redis URL produces a clear, actionable error at startup.

**Acceptance Scenarios**:

1. **Given** `ILINK_QUEUE_BACKEND` is not set, **When** the service starts, **Then** it uses `InMemoryQueue` as the default — equivalent to `ILINK_QUEUE_BACKEND=memory`.
2. **Given** `ILINK_QUEUE_BACKEND=memory` is set, **When** the service starts, **Then** it uses `InMemoryQueue` and logs a startup message confirming the backend selection.
3. **Given** `ILINK_QUEUE_BACKEND=redis` is set but `ILINK_REDIS_URL` is absent, **When** the service starts, **Then** startup fails with a clear error message: the operator is told which environment variable is missing and what format it expects.
4. **Given** `ILINK_QUEUE_BACKEND` is set to an unrecognized value (e.g., `kafka`), **When** the service starts, **Then** startup fails with an actionable error listing the supported values.

---

### Edge Cases

- What happens when two concurrent goroutines push to the same vtoken queue simultaneously? The implementation must ensure pushes are serialized without data races.
- How does the system handle a vtoken that receives messages before the client has registered? Messages must be buffered and retrievable when the client eventually calls `ensure()` or equivalent.
- What happens when a client disconnects and its queue is removed, but a message arrives for that vtoken concurrently? The push must either be silently dropped or create a new queue entry — no crash or panic.
- What is the behavior when `drain()` is called on a vtoken with zero pending messages? It must return an empty list without error, identical to the current behavior.
- What happens when `await_notification()` is called for a vtoken and no notifier exists yet? The system must not panic — it should either create a notifier on demand or return an appropriate sentinel.

---

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST define a `MessageQueue` trait in the public `ilink-hub` library crate that encapsulates all queue operations currently performed by `QueueStore`.
- **FR-002**: The `MessageQueue` trait MUST be object-safe — usable as `Arc<dyn MessageQueue>` without compiler errors.
- **FR-003**: The `MessageQueue` trait MUST expose an async method to push a single `InboundMessage` to a named queue identified by a vtoken string.
- **FR-004**: The `MessageQueue` trait MUST expose an async method to drain (retrieve and clear) all pending messages from a named queue, returning them as a list.
- **FR-005**: The `MessageQueue` trait MUST expose a method to obtain a notification handle for a vtoken, enabling long-poll callers to await new messages without busy-waiting.
- **FR-006**: The `MessageQueue` trait MUST expose an async method to retrieve current queue sizes (vtoken → pending message count) for use in health/metrics endpoints.
- **FR-007**: The `MessageQueue` trait MUST expose an async method to remove a queue and its associated notifier when a client disconnects.
- **FR-008**: The system MUST provide an `InMemoryQueue` struct that implements the `MessageQueue` trait using the current `VecDeque` + `tokio::Notify` approach.
- **FR-009**: The `InMemoryQueue` MUST preserve all current behaviors: per-vtoken message buffering, overflow dropping at 200 messages (oldest dropped first), and notification on push.
- **FR-010**: `HubState` (or its equivalent central state struct) MUST hold the queue backend as `Arc<dyn MessageQueue>` rather than a concrete type, so that downstream crates can substitute their own implementation.
- **FR-011**: The system MUST read the `ILINK_QUEUE_BACKEND` environment variable at startup to determine which backend to initialize. Absence defaults to `memory`.
- **FR-012**: When `ILINK_QUEUE_BACKEND=redis` is specified but `ILINK_REDIS_URL` is not set, the system MUST fail at startup with a clear, human-readable error message identifying the missing variable.
- **FR-013**: The existing single-tenant behavior MUST be fully preserved — no change in observable behavior for users who do not set any new environment variables.
- **FR-014**: The `RedisQueue` implementation is explicitly NOT in scope for this feature; `FR-012` only requires the startup validation plumbing for future use.

### Key Entities

- **MessageQueue trait**: The abstraction contract defining the five operations (push, drain, await-notification, queue-sizes, remove). Lives in `ilink-hub` as a public trait. Has no knowledge of whether state is in-process or remote.
- **InMemoryQueue**: The concrete implementation of `MessageQueue` for single-process use. Wraps the existing `QueueStore` logic, protected by an async-safe interior mutex. The default backend.
- **VirtualToken (vtoken)**: A string identifier (e.g., `vctx_<uuid>`) that names a specific client's queue lane. Unchanged by this feature; used as the key for all queue operations.
- **HubState**: The shared application state struct that currently holds `QueueStore` directly. After this feature, it holds `Arc<dyn MessageQueue>` instead — enabling the backend to be injected at startup.
- **QueueBackendConfig**: A startup-time configuration value derived from environment variables that selects which `MessageQueue` implementation to instantiate.

---

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: All existing integration behaviors for personal users pass without modification after the refactor — no new configuration required for existing deployments.
- **SC-002**: A downstream crate author can add `ilink-hub` as a dependency and implement `MessageQueue` for a custom struct in under 30 minutes using only the published documentation and trait definition.
- **SC-003**: The `ilink-hub` binary starts up within the same time window as before this change when using the default in-memory backend — no measurable startup regression.
- **SC-004**: Switching from in-memory to a Redis-backed queue (once implemented) requires only environment variable changes — zero source code modifications in `ilink-hub`.
- **SC-005**: All five queue operations (push, drain, await-notification, queue-sizes, remove) are verifiably exercised by the test suite, and test coverage for queue behavior does not decrease compared to the pre-trait baseline.
- **SC-006**: An operator who misconfigures the queue backend (wrong backend name, missing Redis URL) receives a startup error message that unambiguously identifies the problem and how to fix it — no silent fallback to an unintended backend.

---

## Assumptions

- The `tokio` async runtime remains the execution environment; notification primitives are tokio-based.
- `InboundMessage` type definition is stable and will not change as part of this feature.
- The `MAX_QUEUE_SIZE` constant (200) is preserved unchanged.
- The `ContextTokenMap` (vtoken ↔ real-token mapping) is a separate concern and is NOT folded into the `MessageQueue` trait — it remains a distinct struct.
- Object safety requires that async trait methods be handled via the `async-trait` crate or an equivalent mechanism, since Rust's native `async fn` in traits is not yet object-safe in stable Rust at the time of writing.
- The `ilink-saas` project will supply its own `RedisQueue` implementation; this feature delivers only the trait contract and the `InMemoryQueue` implementation.
