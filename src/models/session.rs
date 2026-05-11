//! `sessions` table model.
//!
//! HomeTube sessions are stored server-side (in SQLite). The signed
//! cookie sent to the browser only contains the random session ID; the
//! mapping to an `account_id` lives in the database. This makes
//! invalidation (logout, account deletion) trivial — just drop the row.

use chrono::{Duration, Utc};
use rand::distributions::Alphanumeric;
use rand::Rng;
use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::account::{Account, AccountType};

/// A row from the `sessions` table.
///
/// `account_id`, `expires_at`, and `created_at` are loaded for
/// completeness even though the current handler set only reads `id` —
/// admin tooling and future per-session listings will need them.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    #[allow(dead_code)]
    pub account_id: i64,
    #[allow(dead_code)]
    pub expires_at: i64,
    #[allow(dead_code)]
    pub created_at: i64,
}

/// Default session lifetime: seven days. The plan calls this out as the
/// "session max age" stored in `app_config`; we hard-code seven days for
/// now (the value never changes in Phase 2/3) but route the constant
/// through one place so it's easy to lift later.
pub const DEFAULT_SESSION_DAYS: i64 = 7;

/// Generate a random opaque session ID (32 alphanumeric characters).
pub fn new_session_id() -> String {
    rand::thread_rng()
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// Insert a fresh session row for `account_id` and return its ID. The
/// caller is responsible for setting the corresponding cookie.
pub async fn create(pool: &SqlitePool, account_id: i64) -> AppResult<Session> {
    let id = new_session_id();
    let expires_at = (Utc::now() + Duration::days(DEFAULT_SESSION_DAYS)).timestamp();

    sqlx::query("INSERT INTO sessions (id, account_id, expires_at) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(account_id)
        .bind(expires_at)
        .execute(pool)
        .await?;

    Ok(Session {
        id,
        account_id,
        expires_at,
        created_at: Utc::now().timestamp(),
    })
}

/// Drop a session row by ID. Idempotent.
pub async fn delete(pool: &SqlitePool, id: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct SessionAccountRow {
    s_id: String,
    s_account_id: i64,
    s_expires_at: i64,
    s_created_at: i64,
    a_id: i64,
    a_google_id: String,
    a_email: String,
    a_display_name: String,
    a_avatar_url: Option<String>,
    a_account_type: String,
    a_pin_hash: Option<String>,
    a_access_token: String,
    a_refresh_token: String,
    a_token_expires_at: i64,
    a_created_at: i64,
    a_updated_at: i64,
}

/// Look up a session and, if present and unexpired, the associated
/// account. Expired rows are deleted on the fly.
pub async fn lookup_with_account(
    pool: &SqlitePool,
    session_id: &str,
) -> AppResult<Option<(Session, Account)>> {
    let row: Option<SessionAccountRow> = sqlx::query_as(
        "SELECT s.id AS s_id, s.account_id AS s_account_id, \
                s.expires_at AS s_expires_at, s.created_at AS s_created_at, \
                a.id AS a_id, a.google_id AS a_google_id, a.email AS a_email, \
                a.display_name AS a_display_name, a.avatar_url AS a_avatar_url, \
                a.account_type AS a_account_type, a.pin_hash AS a_pin_hash, \
                a.access_token AS a_access_token, a.refresh_token AS a_refresh_token, \
                a.token_expires_at AS a_token_expires_at, \
                a.created_at AS a_created_at, a.updated_at AS a_updated_at \
         FROM sessions s INNER JOIN accounts a ON a.id = s.account_id \
         WHERE s.id = ?",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let now = Utc::now().timestamp();
    if row.s_expires_at <= now {
        // Best-effort cleanup; ignore errors here so a transient delete
        // failure can't block the request.
        let _ = delete(pool, &row.s_id).await;
        return Ok(None);
    }

    let session = Session {
        id: row.s_id,
        account_id: row.s_account_id,
        expires_at: row.s_expires_at,
        created_at: row.s_created_at,
    };
    let account = Account {
        id: row.a_id,
        google_id: row.a_google_id,
        email: row.a_email,
        display_name: row.a_display_name,
        avatar_url: row.a_avatar_url,
        account_type: row.a_account_type,
        pin_hash: row.a_pin_hash,
        access_token: row.a_access_token,
        refresh_token: row.a_refresh_token,
        token_expires_at: row.a_token_expires_at,
        created_at: row.a_created_at,
        updated_at: row.a_updated_at,
    };

    Ok(Some((session, account)))
}

/// Convenience: return the [`AccountType`] for a session id, or `None`.
#[allow(dead_code)]
pub async fn account_type_for_session(
    pool: &SqlitePool,
    session_id: &str,
) -> AppResult<Option<AccountType>> {
    Ok(lookup_with_account(pool, session_id)
        .await?
        .map(|(_, a)| a.typed()))
}
