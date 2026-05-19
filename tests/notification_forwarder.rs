//! REST + dispatch coverage for the external notification forwarder.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::notification_forwarders::{self, ForwarderConfig, ForwardingSettings};
use hometube::services::notifications;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn get_config_returns_default_when_unset() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/notifications/config").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["settings"]["enabled"], false);
    assert!(body["settings"]["provider"].is_null());
    assert!(body["known_types"].is_array());
}

#[tokio::test]
async fn put_config_persists_and_redacts_secret() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let payload = json!({
        "enabled": true,
        "provider": {
            "provider": "ntfy",
            "base_url": "https://ntfy.example",
            "topic": "kids",
            "token": "super-secret"
        },
        "enabled_types": ["ytdlp_failure"]
    });
    let res = app
        .server
        .put("/api/notifications/config")
        .json(&payload)
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["settings"]["provider"]["token"], "********");

    // GET still returns the redacted token, but the stored value
    // remains intact.
    let res = app.server.get("/api/notifications/config").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["settings"]["provider"]["token"], "********");

    let stored = notification_forwarders::load(&app.pool).await.unwrap();
    match stored.provider.unwrap() {
        ForwarderConfig::Ntfy { token, .. } => {
            assert_eq!(token.as_deref(), Some("super-secret"));
        }
        _ => panic!("wrong variant"),
    }
}

#[tokio::test]
async fn put_config_rejects_invalid_url() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/notifications/config")
        .json(&json!({
            "enabled": true,
            "provider": { "provider": "ntfy", "base_url": "not a url", "topic": "k" },
            "enabled_types": []
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dispatch_forwards_to_ntfy() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/kids"))
        .and(header("Title", "yt-dlp updated"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let settings = ForwardingSettings {
        enabled: true,
        provider: Some(ForwarderConfig::Ntfy {
            base_url: server.uri(),
            topic: "kids".into(),
            token: None,
            priority: None,
        }),
        enabled_types: vec![notifications::TYPE_SYSTEM_UPDATE.to_string()],
    };
    notification_forwarders::save(&app.pool, &settings)
        .await
        .unwrap();

    notifications::broadcast(
        &app.pool,
        notifications::TYPE_SYSTEM_UPDATE,
        "yt-dlp updated",
        "version bumped",
        &serde_json::json!({}),
    )
    .await
    .unwrap();

    // The forwarder is fire-and-forget; give the spawned task time to
    // hit the mock server.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    server.verify().await;
}

#[tokio::test]
async fn dispatch_skips_when_type_not_in_allowlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let settings = ForwardingSettings {
        enabled: true,
        provider: Some(ForwarderConfig::Ntfy {
            base_url: server.uri(),
            topic: "kids".into(),
            token: None,
            priority: None,
        }),
        // Only forward ytdlp_failure, but we'll dispatch a different type.
        enabled_types: vec![notifications::TYPE_YTDLP_FAILURE.to_string()],
    };
    notification_forwarders::save(&app.pool, &settings)
        .await
        .unwrap();

    notifications::broadcast(
        &app.pool,
        notifications::TYPE_SYSTEM_UPDATE,
        "irrelevant",
        "ignored",
        &serde_json::json!({}),
    )
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    server.verify().await;
}

#[tokio::test]
async fn test_endpoint_hits_provider() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/message"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    notification_forwarders::save(
        &app.pool,
        &ForwardingSettings {
            enabled: true,
            provider: Some(ForwarderConfig::Gotify {
                base_url: server.uri(),
                app_token: "t".into(),
                priority: None,
            }),
            enabled_types: vec![],
        },
    )
    .await
    .unwrap();

    let res = app
        .server
        .post("/api/notifications/config/test")
        .json(&json!({}))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["ok"], true);
    server.verify().await;
}
