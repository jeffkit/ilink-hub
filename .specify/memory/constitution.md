<!--
SYNC IMPACT REPORT
==================
Version change: (new) → 1.0.0
Added sections: all (initial ratification)
Modified principles: N/A (initial)
Removed sections: N/A (initial)

Templates checked:
  ✅ .specify/templates/plan-template.md   — Constitution Check section present; references updated
  ✅ .specify/templates/spec-template.md   — No constitution-specific references; compatible as-is
  ✅ .specify/templates/tasks-template.md  — Task structure compatible; no amendment required

Follow-up TODOs:
  - TODO(RATIFICATION_DATE): Confirm exact first-commit date if 2026-06-05 is inaccurate
  - Consider adding a `docs/ARCHITECTURE.md` doc-code-map entry once architecture doc exists
-->

# iLink Hub — Project Constitution

**Version**: 1.0.0
**Ratification Date**: 2026-06-05
**Last Amended**: 2026-06-05

---

## Purpose

This constitution defines the non-negotiable engineering principles, architectural constraints,
and governance rules for the **iLink Hub** project — a Rust-based transparent proxy and multiplexer
for WeChat's iLink API that allows multiple AI backends to share a single WeChat account.

All contributors, code reviewers, and automated agents MUST treat these principles as mandatory
gates. Deviations require explicit documented justification before merge.

---

## Principle 1 — Code Quality & Standards

**Name**: Code Quality & Standards

Every line of production Rust code submitted to `main` MUST meet the following non-negotiable bar:

- **Rust edition and toolchain**: Rust 2021 edition, stable toolchain exclusively. No nightly
  features unless gated behind a `cfg` flag with documented rationale.
- **Formatting**: All code MUST be formatted with `rustfmt` before merge. CI enforces this.
  Any unformatted file MUST cause CI to fail.
- **Lints**: `#![deny(warnings)]` is set in CI via `RUSTFLAGS`. All `clippy` lints MUST pass.
  `clippy::pedantic` warnings MAY be suppressed individually with `#[allow(...)]` and a comment,
  but MUST NOT be suppressed wholesale.
- **Documentation**: Every `pub` item (function, struct, enum, trait, module) MUST have a rustdoc
  comment (`///`). Internal (`pub(crate)`) items SHOULD have rustdoc for non-obvious contracts.
- **Error handling**: `unwrap()` and `expect()` are FORBIDDEN in all non-test production code paths.
  All fallible operations MUST use `?` or explicit `match`/`map_err`. The sole exception is
  top-level `main()` setup code with an `// SAFETY: ...` comment explaining why panic is acceptable.
- **Error types**: Custom error types MUST implement `std::error::Error` and `std::fmt::Display`.
  Prefer `thiserror`-derived types for library-facing errors; `anyhow` MAY be used in binary entry
  points only.

**Rationale**: Consistent style, documented APIs, and proper error propagation are especially
critical in an async Rust service where mishandled errors can cause silent data corruption or
panics that take down the entire server.

---

## Principle 2 — Testing Philosophy

**Name**: Testing Philosophy

- **Unit tests**: Every business logic module (routing, token mapping, queue management, health
  checking) MUST have unit tests covering the primary success path and the primary failure path.
- **Integration tests**: All database operations (sqlx queries) MUST be covered by integration
  tests that use test-scoped transactions (rolled back after each test) to avoid polluting state.
- **CI gate**: CI MUST execute `cargo build`, `cargo clippy`, and `cargo test` — all MUST pass
  before any pull request can be merged. Flaky tests MUST be fixed or quarantined before re-merge.
- **New features**: Any PR that introduces a new feature MUST include tests demonstrating the
  correct behavior. A PR without tests MUST include a written justification in the PR description
  explaining why tests are not feasible for that specific change.
- **Coverage**: No minimum coverage percentage is mandated. Quality over quantity — a meaningful
  test for a core invariant is worth more than ten trivial snapshot tests.

**Rationale**: iLink Hub is deployed in always-on scenarios where a regression can silently break
WeChat communication for users. Tests are the primary safety net during the ongoing SaaS evolution.

---

## Principle 3 — Architecture Constraints

**Name**: Architecture Constraints

The following structural rules are invariants of the system and MUST NOT be violated:

- **Async runtime**: `tokio` is the sole async runtime. Mixing runtimes is FORBIDDEN. Blocking
  operations (file I/O, heavy CPU work) MUST be dispatched via `tokio::task::spawn_blocking`.
  Blocking calls MUST NOT appear directly on async threads.
- **Database access**: `sqlx` is the only permitted database abstraction. Raw SQL strings MUST
  use `sqlx::query!` or `sqlx::query_as!` macros where compile-time verification is possible.
  Third-party ORMs are FORBIDDEN.
- **Backward compatibility**: The iLink-compatible API surface (`/ilink/bot/*` endpoints) MUST
  remain wire-compatible with the current single-tenant behavior. Existing users MUST be able to
  upgrade iLink Hub without any client-side changes. Breaking changes to this surface require a
  major version bump and a documented migration path.
- **Core design invariant — transparent proxy**: The iLink protocol transparent proxy MUST remain
  intact. Real `context_token` values MUST never be exposed to registered clients; virtual token
  translation MUST be performed in the Hub exclusively.
- **Trait-based abstraction**: Swappable components (e.g., message queue backend, store backend)
  MUST be expressed as Rust traits. Concrete implementations are injected at construction time.
  No component MUST hard-code a specific implementation.
- **Module boundaries**: Circular dependencies between modules are FORBIDDEN. The dependency graph
  MUST be a DAG: `ilink` ← `hub` ← `server`; `store` is a leaf. Cross-cutting concerns (tracing,
  errors) live in `lib.rs` / `error.rs`.

**Rationale**: These constraints protect the core value proposition of the Hub (transparent proxy,
zero client changes) while enabling the safe evolution toward multi-tenancy.

---

## Principle 4 — Security Baseline

**Name**: Security Baseline

Security requirements are non-negotiable for a service that proxies authenticated WeChat sessions:

- **Tenant isolation**: All data access MUST be scoped to the requesting tenant. Cross-tenant
  data access is a critical security defect and MUST be treated as a P0 bug. Any query that could
  return data across tenant boundaries MUST be audited before merge.
- **Secret storage**: Passwords and secrets MUST NEVER be stored in plaintext. User/admin
  passwords MUST be hashed with `argon2` (preferred) or `bcrypt`. MD5 and SHA-1 are FORBIDDEN
  for password hashing.
- **Token storage**: API keys and virtual tokens MUST be stored as hashed values in the database.
  The plaintext token is returned to the client exactly once (at registration) and MUST NOT be
  recoverable from the database afterwards.
- **Log hygiene**: Sensitive values (tokens, passwords, context tokens, WeChat user identifiers)
  MUST NOT appear in log output at any level. Use `[REDACTED]` or structured redaction helpers.
- **Admin endpoint authentication**: All `/hub/` management endpoints MUST require authentication
  when `ILINK_ADMIN_TOKEN` is set. When unset, these endpoints are accessible only in explicit
  development mode — production deployments MUST set this variable.

**Rationale**: iLink Hub acts as an authentication proxy for WeChat accounts. A security breach
would expose real users' WeChat communications. Multi-tenant SaaS amplifies this risk significantly.

---

## Principle 5 — Performance Baseline

**Name**: Performance Baseline

The following performance contracts MUST be maintained and validated in load testing before
any significant architectural change:

- **Non-polling endpoints**: API response time MUST be < 200 ms at p95 under representative load.
  This applies to `sendmessage`, `sendtyping`, `getconfig`, `getuploadurl`, and all Hub management
  endpoints.
- **Long-poll endpoints**: `/ilink/bot/getupdates` MAY block for up to 30 seconds while waiting
  for a message. This is intentional and MUST NOT be treated as a timeout violation.
- **Per-tenant memory**: Each registered client's memory footprint MUST be bounded. Unbounded
  in-memory growth (e.g., growing queues, leaking connection state) is a defect.
- **Message queue cap**: Each client's per-queue message buffer MUST have a configurable maximum
  size. The default MUST be 200 messages. When the cap is reached, the oldest message MUST be
  dropped (head-drop policy) and a metric counter MUST be incremented.

**Rationale**: iLink Hub runs on modest hardware for open-source users and must scale predictably
for multi-tenant SaaS without surprise OOM kills.

---

## Principle 6 — Operations Baseline

**Name**: Operations Baseline

iLink Hub MUST be operable without special tooling or proprietary knowledge:

- **Docker**: The service MUST be deployable via a single `docker run` or `docker compose up`
  command. The `Dockerfile` MUST use a multi-stage build to minimize image size. Multi-arch
  images (linux/amd64 + linux/arm64) MUST be published to ghcr.io on every release.
- **Structured logging**: All diagnostic output MUST use the `tracing` crate with structured
  fields. `println!`, `eprintln!`, and `dbg!` are FORBIDDEN in production code paths. Log levels
  MUST be respected: `ERROR` for actionable failures, `WARN` for degraded-but-recoverable states,
  `INFO` for lifecycle events, `DEBUG`/`TRACE` for diagnostic detail.
- **Observability**: A Prometheus-compatible metrics endpoint MUST be available at `/metrics`.
  Key counters MUST include: messages routed, messages dropped (queue-full), upstream poll errors,
  active client count, and request latency histograms for each endpoint group.
- **12-factor configuration**: All runtime configuration MUST be injectable via environment
  variables. No configuration MUST require file system access at runtime (except `DATABASE_URL`
  pointing to a SQLite file path). Hardcoded configuration values in source code are FORBIDDEN.
- **Graceful shutdown**: The service MUST handle `SIGTERM` (and `Ctrl-C` / `SIGINT`) by:
  draining in-flight requests (up to a 10-second grace period), closing the upstream iLink
  connection cleanly, and flushing pending log records before exit.
- **Health check**: A `/health` endpoint MUST return `200 OK` with a JSON body when the service
  is healthy. It MUST reflect upstream connection status so orchestrators can detect partial
  degradation.

**Rationale**: Operational simplicity is a first-class user promise. Many iLink Hub users deploy
it without DevOps expertise; every operational friction point is a support burden and a churn risk.

---

## Governance

### Amendment Procedure

1. Any contributor may propose an amendment by opening a pull request that modifies this file.
2. The amendment description MUST state: which principle is affected, why the change is needed,
   and what downstream templates or docs require updates.
3. Amendments require review and approval from the project maintainer before merge.
4. After merge, the `LAST_AMENDED_DATE` MUST be updated to the merge date and the
   `CONSTITUTION_VERSION` MUST be bumped per the versioning policy below.

### Versioning Policy

`CONSTITUTION_VERSION` follows semantic versioning:

| Change type | Bump |
|-------------|------|
| Backward-incompatible removal or redefinition of a principle | **MAJOR** |
| New principle or section added; material expansion of existing guidance | **MINOR** |
| Clarifications, wording fixes, typo corrections, non-semantic refinements | **PATCH** |

### Compliance Review

- Compliance with this constitution is validated at **code review time** by human reviewers and
  automated CI checks.
- The plan template's **Constitution Check** section MUST be filled before a feature implementation
  begins, confirming which principles apply and whether any violations require justification.
- Any intentional deviation from a principle MUST be documented in the plan's **Complexity
  Tracking** table with a rationale for why simpler alternatives were rejected.
- Compliance is re-verified at the **design phase** (after `plan.md` is complete) and again at
  **merge time**.
