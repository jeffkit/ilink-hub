//! Client registry and routing-state persistence.

use anyhow::Result;
use sqlx::Row;

use super::Store;

/// Sentinel `from_user` key used to persist the hub's global default client across restarts.
/// This row lives in `routing_state` alongside per-user routes but is never exposed as a
/// real WeChat-user route; the startup loader checks for it explicitly and skips it when
/// populating per-user routes.
pub const HUB_DEFAULT_SENTINEL: &str = "__hub_default__";

impl Store {
    // ─── Clients ─────────────────────────────────────────────────────────────

    pub async fn upsert_client(&self, vtoken: &str, name: &str, label: Option<&str>) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // Update routing_state for any routes pointing to this client's old vtoken
        // before inserting/updating the client's vtoken.
        sqlx::query(
            r#"
            UPDATE routing_state
            SET active_vtoken = $1
            WHERE active_vtoken = (SELECT vtoken FROM clients WHERE name = $2)
            "#,
        )
        .bind(vtoken)
        .bind(name)
        .execute(&mut *tx)
        .await?;

        // ON CONFLICT (name): update vtoken so a post-restart re-registration with a new
        // vtoken wins, keeping DB and in-memory registry consistent.
        sqlx::query(
            r#"
            INSERT INTO clients (vtoken, name, label)
            VALUES ($1, $2, $3)
            ON CONFLICT (name) DO UPDATE
              SET vtoken = EXCLUDED.vtoken,
                  label = COALESCE(EXCLUDED.label, clients.label),
                  last_seen = CURRENT_TIMESTAMP
            "#,
        )
        .bind(vtoken)
        .bind(name)
        .bind(label)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn touch_client(&self, vtoken: &str) -> Result<()> {
        sqlx::query("UPDATE clients SET last_seen = CURRENT_TIMESTAMP WHERE vtoken = $1")
            .bind(vtoken)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_clients(&self) -> Result<Vec<ClientRow>> {
        let rows = sqlx::query(
            "SELECT vtoken, name, label, last_seen, persona_name, persona_emoji FROM clients ORDER BY name",
        )
        .fetch_all(&self.rpool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ClientRow {
                vtoken: r.get("vtoken"),
                name: r.get("name"),
                label: r.get("label"),
                last_seen: r.get::<Option<String>, _>("last_seen"),
                persona_name: r.get("persona_name"),
                persona_emoji: r.get("persona_emoji"),
            })
            .collect())
    }

    pub async fn get_client_by_name(&self, name: &str) -> Result<Option<ClientRow>> {
        let row = sqlx::query(
            "SELECT vtoken, name, label, last_seen, persona_name, persona_emoji FROM clients WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.rpool)
        .await?;

        Ok(row.map(|r| ClientRow {
            vtoken: r.get("vtoken"),
            name: r.get("name"),
            label: r.get("label"),
            last_seen: r.get::<Option<String>, _>("last_seen"),
            persona_name: r.get("persona_name"),
            persona_emoji: r.get("persona_emoji"),
        }))
    }

    pub async fn update_client_persona(
        &self,
        vtoken: &str,
        persona_name: Option<&str>,
        persona_emoji: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE clients SET persona_name = $2, persona_emoji = $3 WHERE vtoken = $1")
            .bind(vtoken)
            .bind(persona_name)
            .bind(persona_emoji)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_client_by_name(&self, name: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM clients WHERE name = $1")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_client_by_vtoken(
        &self,
        vtoken: &str,
        name: &str,
        label: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE clients SET name = $2, label = COALESCE($3, label) WHERE vtoken = $1")
            .bind(vtoken)
            .bind(name)
            .bind(label)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn clear_routes_for_vtoken(&self, vtoken: &str) -> Result<()> {
        sqlx::query("DELETE FROM routing_state WHERE active_vtoken = $1")
            .bind(vtoken)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ─── Routing state ────────────────────────────────────────────────────────

    pub async fn list_routes(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT from_user, active_vtoken FROM routing_state")
            .fetch_all(&self.rpool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("from_user"),
                    r.get::<String, _>("active_vtoken"),
                )
            })
            .collect())
    }

    pub async fn get_route(&self, from_user: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT active_vtoken FROM routing_state WHERE from_user = $1")
            .bind(from_user)
            .fetch_optional(&self.rpool)
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
}

#[derive(Debug, Clone)]
pub struct ClientRow {
    pub vtoken: String,
    pub name: String,
    pub label: Option<String>,
    pub last_seen: Option<String>,
    pub persona_name: Option<String>,
    pub persona_emoji: Option<String>,
}
