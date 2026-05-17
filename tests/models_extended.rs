//! Extended tests for models/account and models/session operations.

mod common;

use common::boot;
use hometube::models::account::{self, AccountType};
use hometube::models::session;

// ===========================================================================
// Account CRUD
// ===========================================================================

#[tokio::test]
async fn insert_and_find_by_id() {
    let app = boot().await;
    let id = account::insert_local(
        &app.pool,
        "User One",
        Some("https://avatar.test/1.jpg"),
        AccountType::Parent,
    )
    .await
    .unwrap();

    let found = account::find_by_id(&app.pool, id).await.unwrap().unwrap();
    assert_eq!(found.id, id);
    assert_eq!(found.display_name, "User One");
    assert_eq!(found.avatar_url, Some("https://avatar.test/1.jpg".into()));
    assert_eq!(found.typed(), AccountType::Parent);
}

#[tokio::test]
async fn parent_count_starts_zero() {
    let app = boot().await;
    assert_eq!(account::parent_count(&app.pool).await.unwrap(), 0);
}

#[tokio::test]
async fn parent_count_after_insert() {
    let app = boot().await;
    account::insert_local(&app.pool, "P", None, AccountType::Parent)
        .await
        .unwrap();
    assert_eq!(account::parent_count(&app.pool).await.unwrap(), 1);
}

#[tokio::test]
async fn total_count() {
    let app = boot().await;
    assert_eq!(account::total_count(&app.pool).await.unwrap(), 0);
    account::insert_local(&app.pool, "A", None, AccountType::Parent)
        .await
        .unwrap();
    account::insert_local(&app.pool, "B", None, AccountType::Child)
        .await
        .unwrap();
    assert_eq!(account::total_count(&app.pool).await.unwrap(), 2);
}

#[tokio::test]
async fn insert_local_child() {
    let app = boot().await;
    let id = account::insert_local(&app.pool, "Local Kid", Some("http://av/k.jpg"), AccountType::Child)
        .await
        .unwrap();
    let found = account::find_by_id(&app.pool, id).await.unwrap().unwrap();
    assert_eq!(found.display_name, "Local Kid");
    assert_eq!(found.typed(), AccountType::Child);
}

// ===========================================================================
// AccountSummary conversion
// ===========================================================================

#[tokio::test]
async fn account_summary_hides_pin() {
    let app = boot().await;
    let id = account::insert_local(&app.pool, "X", None, AccountType::Parent)
        .await
        .unwrap();

    // Set a PIN hash.
    sqlx::query("UPDATE accounts SET pin_hash = 'fakehash' WHERE id = ?")
        .bind(id)
        .execute(&app.pool)
        .await
        .unwrap();

    let acct = account::find_by_id(&app.pool, id).await.unwrap().unwrap();
    let summary = account::AccountSummary::from(&acct);
    assert_eq!(summary.id, id);
    assert!(summary.has_pin);
    // JSON should not contain pin_hash.
    let json = serde_json::to_value(&summary).unwrap();
    assert!(json.get("pin_hash").is_none());
}

// ===========================================================================
// Session model
// ===========================================================================

#[tokio::test]
async fn session_id_generation() {
    let id1 = session::new_session_id();
    let id2 = session::new_session_id();
    assert_ne!(id1, id2);
    assert_eq!(id1.len(), 32);
    assert!(id1.chars().all(|c| c.is_ascii_alphanumeric()));
}

// ===========================================================================
// list_all, update, delete, set_pin_hash
// ===========================================================================

#[tokio::test]
async fn list_all_ordered() {
    let app = boot().await;
    account::insert_local(&app.pool, "Parent", None, AccountType::Parent)
        .await
        .unwrap();
    account::insert_local(&app.pool, "Child", None, AccountType::Child)
        .await
        .unwrap();

    let all = account::list_all(&app.pool).await.unwrap();
    assert_eq!(all.len(), 2);
    // Parents first.
    assert_eq!(all[0].account_type, "parent");
    assert_eq!(all[1].account_type, "child");
}

#[tokio::test]
async fn update_display_name() {
    let app = boot().await;
    let id = account::insert_local(&app.pool, "Old", None, AccountType::Child)
        .await
        .unwrap();
    account::update(&app.pool, id, Some("New"), None)
        .await
        .unwrap();
    let found = account::find_by_id(&app.pool, id).await.unwrap().unwrap();
    assert_eq!(found.display_name, "New");
}

#[tokio::test]
async fn update_account_type() {
    let app = boot().await;
    let id = account::insert_local(&app.pool, "U", None, AccountType::Child)
        .await
        .unwrap();
    account::update(&app.pool, id, None, Some(AccountType::Parent))
        .await
        .unwrap();
    let found = account::find_by_id(&app.pool, id).await.unwrap().unwrap();
    assert_eq!(found.typed(), AccountType::Parent);
}

#[tokio::test]
async fn update_with_both_none_is_noop() {
    let app = boot().await;
    let id = account::insert_local(&app.pool, "U", None, AccountType::Child)
        .await
        .unwrap();
    account::update(&app.pool, id, None, None).await.unwrap();
    let found = account::find_by_id(&app.pool, id).await.unwrap().unwrap();
    assert_eq!(found.display_name, "U");
}

#[tokio::test]
async fn delete_account() {
    let app = boot().await;
    let id = account::insert_local(&app.pool, "Del", None, AccountType::Child)
        .await
        .unwrap();
    account::delete(&app.pool, id).await.unwrap();
    let found = account::find_by_id(&app.pool, id).await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn set_pin_hash() {
    let app = boot().await;
    let id = account::insert_local(&app.pool, "U", None, AccountType::Parent)
        .await
        .unwrap();
    account::set_pin_hash(&app.pool, id, "argon2hash")
        .await
        .unwrap();
    let found = account::find_by_id(&app.pool, id).await.unwrap().unwrap();
    assert_eq!(found.pin_hash.as_deref(), Some("argon2hash"));
}

// ===========================================================================
// Profile listing
// ===========================================================================

#[tokio::test]
async fn list_profiles() {
    let app = boot().await;
    account::insert_local(&app.pool, "Parent", None, AccountType::Parent)
        .await
        .unwrap();
    account::insert_local(&app.pool, "Child", None, AccountType::Child)
        .await
        .unwrap();

    let profiles = account::list_profiles(&app.pool).await.unwrap();
    assert_eq!(profiles.len(), 2);
}
