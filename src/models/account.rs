//! `accounts` table model.
//!
//! HomeTube stores both parents and children in a single `accounts`
//! table — they are distinguished by [`Account::account_type`].
//! Authentication is handled entirely via PINs (Argon2-hashed) and the
//! session cookie; no external identity provider is required.

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
#[derive(Debug, Clone)]
pub struct Account {
    pub id: i64,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub account_type: String,
    pub pin_hash: Option<String>,
    pub created_at: i64,
    #[allow(dead_code)]
    pub updated_at: i64,
}

impl Account {
    pub fn typed(&self) -> AccountType {
        AccountType::parse(&self.account_type).unwrap_or(AccountType::Child)
    }
}

/// Public-facing JSON view of an account — never includes the PIN hash.
#[derive(Debug, Clone, Serialize)]
pub struct AccountSummary {
    pub id: i64,
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

const COLS: &str = "id, display_name, avatar_url, account_type, pin_hash, created_at, updated_at";

fn map_account(row: AccountRow) -> Account {
    let AccountRow {
        id,
        display_name,
        avatar_url,
        account_type,
        pin_hash,
        created_at,
        updated_at,
    } = row;
    Account {
        id,
        display_name,
        avatar_url,
        account_type,
        pin_hash,
        created_at,
        updated_at,
    }
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    id: i64,
    display_name: String,
    avatar_url: Option<String>,
    account_type: String,
    pin_hash: Option<String>,
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

/// Atomically insert the first account if and only if no accounts exist
/// yet. Returns `Ok(Some(id))` on success, `Ok(None)` if the table
/// already contains rows (race lost). The caller should treat `None` as
/// a 400 "already set up" error.
pub async fn insert_first_account(
    pool: &SqlitePool,
    display_name: &str,
    account_type: AccountType,
) -> AppResult<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO accounts (display_name, avatar_url, account_type) \
         SELECT ?, NULL, ? \
         WHERE NOT EXISTS (SELECT 1 FROM accounts) \
         RETURNING id",
    )
    .bind(display_name)
    .bind(account_type.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// Insert a new local account (parent or child). Returns the new ID.
pub async fn insert_local(
    pool: &SqlitePool,
    display_name: &str,
    avatar_url: Option<&str>,
    account_type: AccountType,
) -> AppResult<i64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO accounts \
         (display_name, avatar_url, account_type) \
         VALUES (?, ?, ?) \
         RETURNING id",
    )
    .bind(display_name)
    .bind(avatar_url)
    .bind(account_type.as_str())
    .fetch_one(pool)
    .await?;
    Ok(row.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_type_parse_round_trips() {
        assert_eq!(AccountType::parse("parent"), Some(AccountType::Parent));
        assert_eq!(AccountType::parse("child"), Some(AccountType::Child));
        assert_eq!(AccountType::parse("nonsense"), None);
    }

    #[test]
    fn account_type_as_str_round_trips() {
        assert_eq!(AccountType::Parent.as_str(), "parent");
        assert_eq!(AccountType::Child.as_str(), "child");
        for t in [AccountType::Parent, AccountType::Child] {
            assert_eq!(AccountType::parse(t.as_str()), Some(t));
        }
    }

    #[test]
    fn typed_falls_back_to_child_for_garbage() {
        let acct = Account {
            id: 1,
            display_name: "n".into(),
            avatar_url: None,
            account_type: "garbage".into(),
            pin_hash: None,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(acct.typed(), AccountType::Child);
    }

    #[test]
    fn account_summary_strips_pin_hash() {
        let acct = Account {
            id: 7,
            display_name: "Display".into(),
            avatar_url: Some("http://avatar".into()),
            account_type: "parent".into(),
            pin_hash: Some("hash".into()),
            created_at: 100,
            updated_at: 200,
        };
        let summary = AccountSummary::from(&acct);
        assert_eq!(summary.id, 7);
        assert_eq!(summary.display_name, "Display");
        assert!(summary.has_pin);
        // Re-serialise and confirm PIN hash never leaks.
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("hash"));
    }
}
