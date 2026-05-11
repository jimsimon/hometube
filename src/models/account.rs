//! `accounts` table model.
//!
//! HomeTube stores both parents and children in a single `accounts`
//! table — they are distinguished by [`Account::account_type`]. OAuth
//! tokens are persisted alongside the account so that background sync
//! jobs can act on the user's behalf.

use serde::Serialize;
use sqlx::SqlitePool;

use crate::error::AppResult;

/// Account types — distinct names so tests/logs read clearly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountType {
    Parent,
    Child,
}

impl AccountType {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountType::Parent => "parent",
            AccountType::Child => "child",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "parent" => Some(AccountType::Parent),
            "child" => Some(AccountType::Child),
            _ => None,
        }
    }
}

/// Full row from the `accounts` table.
///
/// `access_token` and `refresh_token` are intentionally `pub` so internal
/// services can use them, but the JSON-serialised view used by handlers
/// excludes them via [`AccountSummary`]. The `#[allow(dead_code)]`
/// markers reflect that those fields are loaded by sqlx for token
/// refresh paths even though the current code doesn't read them
/// directly via the struct (the OAuth refresh service issues its own
/// queries against the `accounts` table).
#[derive(Debug, Clone)]
pub struct Account {
    pub id: i64,
    pub google_id: String,
    pub email: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub account_type: String,
    pub pin_hash: Option<String>,
    #[allow(dead_code)]
    pub access_token: String,
    #[allow(dead_code)]
    pub refresh_token: String,
    pub token_expires_at: i64,
    pub created_at: i64,
    #[allow(dead_code)]
    pub updated_at: i64,
}

impl Account {
    pub fn typed(&self) -> AccountType {
        AccountType::parse(&self.account_type).unwrap_or(AccountType::Child)
    }
}

/// Public-facing JSON view of an account — never includes tokens or the
/// PIN hash.
#[derive(Debug, Clone, Serialize)]
pub struct AccountSummary {
    pub id: i64,
    pub email: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub account_type: String,
    pub has_pin: bool,
    pub created_at: i64,
}

impl From<&Account> for AccountSummary {
    fn from(a: &Account) -> Self {
        Self {
            id: a.id,
            email: a.email.clone(),
            display_name: a.display_name.clone(),
            avatar_url: a.avatar_url.clone(),
            account_type: a.account_type.clone(),
            has_pin: a.pin_hash.is_some(),
            created_at: a.created_at,
        }
    }
}

/// Slim profile-picker view: just enough to render the avatar grid.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileSummary {
    pub id: i64,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub account_type: String,
    pub has_pin: bool,
}

const COLS: &str =
    "id, google_id, email, display_name, avatar_url, account_type, pin_hash, access_token, \
     refresh_token, token_expires_at, created_at, updated_at";

fn map_account(row: AccountRow) -> Account {
    let AccountRow {
        id,
        google_id,
        email,
        display_name,
        avatar_url,
        account_type,
        pin_hash,
        access_token,
        refresh_token,
        token_expires_at,
        created_at,
        updated_at,
    } = row;
    Account {
        id,
        google_id,
        email,
        display_name,
        avatar_url,
        account_type,
        pin_hash,
        access_token,
        refresh_token,
        token_expires_at,
        created_at,
        updated_at,
    }
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    id: i64,
    google_id: String,
    email: String,
    display_name: String,
    avatar_url: Option<String>,
    account_type: String,
    pin_hash: Option<String>,
    access_token: String,
    refresh_token: String,
    token_expires_at: i64,
    created_at: i64,
    updated_at: i64,
}

/// Find an account by primary key.
pub async fn find_by_id(pool: &SqlitePool, id: i64) -> AppResult<Option<Account>> {
    let row: Option<AccountRow> =
        sqlx::query_as(&format!("SELECT {COLS} FROM accounts WHERE id = ?"))
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(map_account))
}

/// Find an account by Google subject ID (`sub` claim from userinfo).
pub async fn find_by_google_id(pool: &SqlitePool, google_id: &str) -> AppResult<Option<Account>> {
    let row: Option<AccountRow> =
        sqlx::query_as(&format!("SELECT {COLS} FROM accounts WHERE google_id = ?"))
            .bind(google_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(map_account))
}

/// Total parent-account count — used to enforce the "first account is a
/// parent" rule and to guard parent deletions.
pub async fn parent_count(pool: &SqlitePool) -> AppResult<i64> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts WHERE account_type = 'parent'")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// Total accounts of any type.
pub async fn total_count(pool: &SqlitePool) -> AppResult<i64> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// Inserts a brand-new account row. Returns the new ID.
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    pool: &SqlitePool,
    google_id: &str,
    email: &str,
    display_name: &str,
    avatar_url: Option<&str>,
    account_type: AccountType,
    access_token: &str,
    refresh_token: &str,
    token_expires_at: i64,
) -> AppResult<i64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO accounts \
         (google_id, email, display_name, avatar_url, account_type, access_token, refresh_token, \
          token_expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         RETURNING id",
    )
    .bind(google_id)
    .bind(email)
    .bind(display_name)
    .bind(avatar_url)
    .bind(account_type.as_str())
    .bind(access_token)
    .bind(refresh_token)
    .bind(token_expires_at)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Bag of fields that change on every successful OAuth callback. Kept
/// as a struct (rather than a positional argument list) to satisfy
/// clippy's `too_many_arguments` lint and to make call sites readable.
#[derive(Debug, Clone)]
pub struct ProfileUpdate<'a> {
    pub email: &'a str,
    pub display_name: &'a str,
    pub avatar_url: Option<&'a str>,
    pub access_token: &'a str,
    pub refresh_token: &'a str,
    pub token_expires_at: i64,
}

/// Refresh tokens / display name / avatar for an existing account on
/// repeat OAuth login.
pub async fn update_profile_and_tokens(
    pool: &SqlitePool,
    id: i64,
    update: ProfileUpdate<'_>,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE accounts SET \
            email = ?, display_name = ?, avatar_url = ?, \
            access_token = ?, refresh_token = ?, token_expires_at = ?, \
            updated_at = unixepoch() \
         WHERE id = ?",
    )
    .bind(update.email)
    .bind(update.display_name)
    .bind(update.avatar_url)
    .bind(update.access_token)
    .bind(update.refresh_token)
    .bind(update.token_expires_at)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// List every account in the database (parents first, then by created_at).
pub async fn list_all(pool: &SqlitePool) -> AppResult<Vec<Account>> {
    let rows: Vec<AccountRow> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM accounts ORDER BY \
         CASE account_type WHEN 'parent' THEN 0 ELSE 1 END, created_at ASC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(map_account).collect())
}

/// Tuple shape returned by [`list_profiles`].
type ProfileRow = (i64, String, Option<String>, String, Option<String>);

/// Slim listing for the profile picker.
pub async fn list_profiles(pool: &SqlitePool) -> AppResult<Vec<ProfileSummary>> {
    let rows: Vec<ProfileRow> = sqlx::query_as(
        "SELECT id, display_name, avatar_url, account_type, pin_hash FROM accounts \
         ORDER BY CASE account_type WHEN 'parent' THEN 0 ELSE 1 END, created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, display_name, avatar_url, account_type, pin_hash)| ProfileSummary {
                id,
                display_name,
                avatar_url,
                account_type,
                has_pin: pin_hash.is_some(),
            },
        )
        .collect())
}

/// Update display name and/or account type. `None` means "leave alone".
pub async fn update(
    pool: &SqlitePool,
    id: i64,
    display_name: Option<&str>,
    account_type: Option<AccountType>,
) -> AppResult<()> {
    if let (None, None) = (display_name, account_type) {
        return Ok(());
    }
    if let Some(name) = display_name {
        sqlx::query("UPDATE accounts SET display_name = ?, updated_at = unixepoch() WHERE id = ?")
            .bind(name)
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(t) = account_type {
        sqlx::query("UPDATE accounts SET account_type = ?, updated_at = unixepoch() WHERE id = ?")
            .bind(t.as_str())
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Delete an account row. The caller must enforce policy (e.g., refuse
/// to delete the last parent).
pub async fn delete(pool: &SqlitePool, id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM accounts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Persist a (hashed) PIN for an account.
pub async fn set_pin_hash(pool: &SqlitePool, id: i64, pin_hash: &str) -> AppResult<()> {
    sqlx::query("UPDATE accounts SET pin_hash = ?, updated_at = unixepoch() WHERE id = ?")
        .bind(pin_hash)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
