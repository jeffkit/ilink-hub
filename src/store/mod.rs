/// Database persistence layer.
/// Uses sqlx with runtime driver selection via `DATABASE_URL`:
///   sqlite://./ilink-hub.db          → SQLite (default)
///   postgres://user:pass@host/db      → PostgreSQL
///   mysql://user:pass@host/db         → MySQL

use anyhow::Result;
use sqlx::{AnyPool, Row};
use uuid::Uuid;

pub struct Store {
    pool: AnyPool,
}

impl Store {
    /// Connect to the database and run migrations.
    pub async fn connect(url: &str) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(url).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS clients (
                vtoken       TEXT PRIMARY KEY,
                name         TEXT NOT NULL UNIQUE,
                label        TEXT,
                created_at   TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_seen    TIMESTAMPTZ
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS routing_state (
                from_user        TEXT PRIMARY KEY,
                active_vtoken    TEXT NOT NULL,
                updated_at       TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS context_token_map (
                vctx        TEXT PRIMARY KEY,
                real_ctx    TEXT NOT NULL,
                expires_at  TIMESTAMPTZ
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS bot_credentials (
                id        INTEGER PRIMARY KEY,
                token     TEXT NOT NULL,
                base_url  TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
                updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // ─── Clients ─────────────────────────────────────────────────────────────

    pub async fn upsert_client(&self, vtoken: &str, name: &str, label: Option<&str>) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO clients (vtoken, name, label)
            VALUES ($1, $2, $3)
            ON CONFLICT (name) DO UPDATE
              SET label = EXCLUDED.label,
                  last_seen = CURRENT_TIMESTAMP
            "#,
        )
        .bind(vtoken)
        .bind(name)
        .bind(label)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn touch_client(&self, vtoken: &str) -> Result<()> {
        sqlx::query(
            "UPDATE clients SET last_seen = CURRENT_TIMESTAMP WHERE vtoken = $1",
        )
        .bind(vtoken)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_clients(&self) -> Result<Vec<ClientRow>> {
        let rows = sqlx::query(
            "SELECT vtoken, name, label, last_seen FROM clients ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ClientRow {
                vtoken: r.get("vtoken"),
                name: r.get("name"),
                label: r.get("label"),
                last_seen: r.get::<Option<String>, _>("last_seen"),
            })
            .collect())
    }

    pub async fn get_client_by_name(&self, name: &str) -> Result<Option<ClientRow>> {
        let row = sqlx::query(
            "SELECT vtoken, name, label, last_seen FROM clients WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| ClientRow {
            vtoken: r.get("vtoken"),
            name: r.get("name"),
            label: r.get("label"),
            last_seen: r.get::<Option<String>, _>("last_seen"),
        }))
    }

    // ─── Routing state ────────────────────────────────────────────────────────

    pub async fn get_route(&self, from_user: &str) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT active_vtoken FROM routing_state WHERE from_user = $1",
        )
        .bind(from_user)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("active_vtoken")))
    }

    pub async fn set_route(&self, from_user: &str, vtoken: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO routing_state (from_user, active_vtoken)
            VALUES ($1, $2)
            ON CONFLICT (from_user) DO UPDATE
              SET active_vtoken = EXCLUDED.active_vtoken,
                  updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(from_user)
        .bind(vtoken)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ─── Context token map ────────────────────────────────────────────────────

    pub async fn map_context_token(&self, real_ctx: &str) -> Result<String> {
        // Check existing mapping
        let existing = sqlx::query(
            "SELECT vctx FROM context_token_map WHERE real_ctx = $1",
        )
        .bind(real_ctx)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = existing {
            return Ok(row.get("vctx"));
        }

        let vctx = format!("vctx_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO context_token_map (vctx, real_ctx) VALUES ($1, $2)",
        )
        .bind(&vctx)
        .bind(real_ctx)
        .execute(&self.pool)
        .await?;
        Ok(vctx)
    }

    pub async fn resolve_context_token(&self, vctx: &str) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT real_ctx FROM context_token_map WHERE vctx = $1",
        )
        .bind(vctx)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("real_ctx")))
    }

    // ─── Bot credentials ──────────────────────────────────────────────────────

    pub async fn save_credentials(&self, token: &str, base_url: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO bot_credentials (id, token, base_url)
            VALUES (1, $1, $2)
            ON CONFLICT (id) DO UPDATE
              SET token = EXCLUDED.token,
                  base_url = EXCLUDED.base_url,
                  updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(token)
        .bind(base_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_credentials(&self) -> Result<Option<(String, String)>> {
        let row = sqlx::query(
            "SELECT token, base_url FROM bot_credentials WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.get("token"), r.get("base_url"))))
    }
}

#[derive(Debug, Clone)]
pub struct ClientRow {
    pub vtoken: String,
    pub name: String,
    pub label: Option<String>,
    pub last_seen: Option<String>,
}
