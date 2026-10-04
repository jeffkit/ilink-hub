//! Opt-in storage retention for `messages` and `active_sessions`.
//!
//! Nothing in the store is ever pruned unless an operator asks for it: the
//! sweeper is off by default, and even when enabled it starts in dry-run mode,
//! which counts (and logs) the rows a sweep *would* delete. Deleting requires
//! setting `ILINK_RETENTION_ENABLED=1` **and** a non-zero TTL **and**
//! `ILINK_RETENTION_DRY_RUN=0`.
//!
//! `backend_sessions_v2` is deliberately not swept: its `created_at` is the
//! *first registration* time (the upsert does not refresh it), so a TTL on it
//! would evict live sessions. See issue #30 §6.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use tracing::{info, warn};

use super::sql::{self, RetentionCutoff};
use super::Store;

/// Retention knobs, read once at startup from the `ILINK_RETENTION_*` variables.
#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// `ILINK_RETENTION_ENABLED` (default `false`) — master switch for the sweeper.
    pub enabled: bool,
    /// `ILINK_RETENTION_DRY_RUN` (default `true`) — log candidates, delete nothing.
    pub dry_run: bool,
    /// `ILINK_RETENTION_SWEEP_SECS` (default 3600) — sweep period; must be > 0.
    pub sweep_interval_secs: u64,
    /// `ILINK_RETENTION_BATCH_SIZE` (default 500) — rows per `DELETE`; must be > 0.
    pub batch_size: u64,
    /// `ILINK_RETENTION_MESSAGES_TTL_SECS` (default 0 = disabled).
    pub messages_ttl_secs: u64,
    /// `ILINK_RETENTION_ACTIVE_SESSIONS_TTL_SECS` (default 0 = disabled).
    pub active_sessions_ttl_secs: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            sweep_interval_secs: 3600,
            batch_size: 500,
            messages_ttl_secs: 0,
            active_sessions_ttl_secs: 0,
        }
    }
}

impl RetentionConfig {
    pub fn from_env() -> Result<Self> {
        let defaults = Self::default();
        Ok(Self {
            enabled: parse_env_bool("ILINK_RETENTION_ENABLED", defaults.enabled),
            dry_run: parse_env_bool("ILINK_RETENTION_DRY_RUN", defaults.dry_run),
            sweep_interval_secs: parse_env_u64(
                "ILINK_RETENTION_SWEEP_SECS",
                defaults.sweep_interval_secs,
            )?,
            batch_size: parse_env_u64("ILINK_RETENTION_BATCH_SIZE", defaults.batch_size)?,
            messages_ttl_secs: parse_env_u64(
                "ILINK_RETENTION_MESSAGES_TTL_SECS",
                defaults.messages_ttl_secs,
            )?,
            active_sessions_ttl_secs: parse_env_u64(
                "ILINK_RETENTION_ACTIVE_SESSIONS_TTL_SECS",
                defaults.active_sessions_ttl_secs,
            )?,
        })
    }
}

/// What one sweep did (or, in dry-run mode, would have done).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    pub dry_run: bool,
    /// Expired `messages` rows seen by the sweep; in dry-run this counts the
    /// candidates of the first batch (at most `batch_size` rows).
    pub messages_matched: u64,
    pub messages_deleted: u64,
    /// Expired `active_sessions` rows seen by the sweep (same dry-run caveat).
    pub active_sessions_matched: u64,
    pub active_sessions_deleted: u64,
    /// Number of `DELETE` statements executed.
    pub batches: u32,
}

impl Store {
    /// Delete rows older than the configured TTLs, or — with `cfg.dry_run` —
    /// only count and log them.
    ///
    /// A TTL of 0 skips its table entirely. `now_epoch_secs` is passed in
    /// rather than read from the clock so the cutoff is deterministic in tests.
    pub async fn sweep_retention(
        &self,
        cfg: &RetentionConfig,
        now_epoch_secs: i64,
    ) -> Result<RetentionReport> {
        let mut report = RetentionReport {
            dry_run: cfg.dry_run,
            ..RetentionReport::default()
        };

        if cfg.messages_ttl_secs > 0 {
            let cutoff = sql::retention_cutoff(
                self.kind,
                cutoff_epoch_secs(cfg.messages_ttl_secs, now_epoch_secs),
            );
            let (matched, deleted, batches) = self
                .sweep_expired(
                    cfg,
                    sql::messages_expired_selection_sql(self.kind),
                    &sql::messages_expired_delete_sql(self.kind),
                    &cutoff,
                )
                .await?;
            report.messages_matched = matched;
            report.messages_deleted = deleted;
            report.batches += batches;
        }

        if cfg.active_sessions_ttl_secs > 0 {
            let cutoff = sql::retention_cutoff(
                self.kind,
                cutoff_epoch_secs(cfg.active_sessions_ttl_secs, now_epoch_secs),
            );
            let (matched, deleted, batches) = self
                .sweep_expired(
                    cfg,
                    sql::active_sessions_expired_selection_sql(self.kind),
                    &sql::active_sessions_expired_delete_sql(self.kind),
                    &cutoff,
                )
                .await?;
            report.active_sessions_matched = matched;
            report.active_sessions_deleted = deleted;
            report.batches += batches;
        }

        Ok(report)
    }

    /// One table's sweep. Returns `(matched, deleted, batches)`.
    ///
    /// Dry-run issues the selection once and never a `DELETE`. The delete path
    /// loops until a batch affects fewer than `batch_size` rows: the SQLite
    /// write pool has a single connection, so one unbounded `DELETE` would hold
    /// the write lock for the whole sweep.
    async fn sweep_expired(
        &self,
        cfg: &RetentionConfig,
        selection_sql: &str,
        delete_sql: &str,
        cutoff: &RetentionCutoff,
    ) -> Result<(u64, u64, u32)> {
        let limit = cfg.batch_size as i64;

        if cfg.dry_run {
            let candidates = match cutoff {
                RetentionCutoff::Text(t) => {
                    sqlx::query(selection_sql)
                        .bind(t)
                        .bind(limit)
                        .fetch_all(&self.rpool)
                        .await?
                }
                RetentionCutoff::Epoch(e) => {
                    sqlx::query(selection_sql)
                        .bind(e)
                        .bind(limit)
                        .fetch_all(&self.rpool)
                        .await?
                }
            };
            let matched = candidates.len() as u64;
            if matched == 0 {
                return Ok((0, 0, 0));
            }
            info!(would_delete = matched, "retention dry-run: no rows deleted");
            return Ok((matched, 0, 1));
        }

        let mut deleted = 0u64;
        let mut batches = 0u32;
        loop {
            let affected = match cutoff {
                RetentionCutoff::Text(t) => sqlx::query(delete_sql)
                    .bind(t)
                    .bind(limit)
                    .execute(&self.pool)
                    .await?
                    .rows_affected(),
                RetentionCutoff::Epoch(e) => sqlx::query(delete_sql)
                    .bind(e)
                    .bind(limit)
                    .execute(&self.pool)
                    .await?
                    .rows_affected(),
            };
            batches += 1;
            deleted += affected;
            if affected < cfg.batch_size {
                break;
            }
        }
        if deleted > 0 {
            info!(deleted, "retention sweep deleted expired rows");
        }
        Ok((deleted, deleted, batches))
    }
}

fn cutoff_epoch_secs(ttl_secs: u64, now_epoch_secs: i64) -> i64 {
    now_epoch_secs.saturating_sub(ttl_secs.min(i64::MAX as u64) as i64)
}

/// Same truthiness contract as `ILINK_ADMIN_INSECURE_NO_AUTH`: only
/// `1` / `true` / `yes` are accepted, anything else (including `0`) is false.
fn parse_env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(default)
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) if v.trim().is_empty() => Ok(default),
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("{name}={v:?} is not a valid non-negative integer")),
    }
}

/// Spawn the periodic sweeper. `cfg.sweep_interval_secs` must be > 0 (enforced by
/// `RuntimeConfig::from_env`); the first sweep runs one interval after startup.
pub fn spawn_retention_sweeper(
    store: Arc<Store>,
    cfg: RetentionConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let interval = Duration::from_secs(cfg.sweep_interval_secs);
        info!(
            interval_secs = cfg.sweep_interval_secs,
            dry_run = cfg.dry_run,
            "retention sweeper started"
        );
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("retention sweeper shutting down");
                        return;
                    }
                }
                _ = tokio::time::sleep(interval) => {
                    let now_epoch_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    match store.sweep_retention(&cfg, now_epoch_secs).await {
                        Ok(report) => info!(
                            dry_run = report.dry_run,
                            messages_deleted = report.messages_deleted,
                            active_sessions_deleted = report.active_sessions_deleted,
                            "retention sweep complete"
                        ),
                        Err(e) => warn!(error = %e, "retention sweep failed"),
                    }
                }
            }
        }
    });
}
