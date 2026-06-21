//! Bot credential persistence.

use anyhow::Result;
use sqlx::Row;

use super::Store;

impl Store {
    // ─── Bot credentials ──────────────────────────────────────────────────────

    pub async fn save_credentials(&self, token: &str, base_url: &str) -> Result<()> {
        let key = self
            .master_key
            .get()
            .ok_or_else(|| anyhow::anyhow!("Master key not configured on Store"))?;
        let encrypted_token = crate::runtime::crypto::encrypt_token(token, key);

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
        .bind(encrypted_token)
        .bind(base_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_credentials(&self) -> Result<Option<(String, String)>> {
        let row = sqlx::query("SELECT token, base_url FROM bot_credentials WHERE id = 1")
            .fetch_optional(&self.rpool)
            .await?;
        if let Some(r) = row {
            let token_blob: String = r.get("token");
            let base_url: String = r.get("base_url");
            let key = self
                .master_key
                .get()
                .ok_or_else(|| anyhow::anyhow!("Master key not configured on Store"))?;
            let decrypted_token = crate::runtime::crypto::decrypt_token(&token_blob, key)?;
            Ok(Some((decrypted_token, base_url)))
        } else {
            Ok(None)
        }
    }
}
