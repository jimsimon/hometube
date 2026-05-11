//! Google OAuth2 service.
//!
//! HomeTube uses Google as its sole identity provider. The OAuth2 client
//! is built lazily per request from values stored in the `app_config`
//! table (populated by the setup wizard) — credentials are never read from
//! environment variables.
//!
//! In addition to the standard authorization-code + PKCE flow, this module
//! exposes a `userinfo` helper that fetches the signed-in user's profile
//! and a [`refresh_if_expired`] helper used by the token refresh strategy
//! described in the implementation plan (refresh proactively if the token
//! is within five minutes of expiry).

use chrono::Utc;
use oauth2::basic::{BasicClient, BasicTokenResponse};
use oauth2::reqwest as oauth_reqwest;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, EndpointNotSet, EndpointSet,
    PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};
use crate::services::setup::{
    get_config_value, KEY_GOOGLE_CLIENT_ID, KEY_GOOGLE_CLIENT_SECRET, KEY_GOOGLE_REDIRECT_URI,
};

/// Google's OAuth2 authorization endpoint.
pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's OAuth2 token endpoint.
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Google userinfo endpoint (returns the OpenID-style profile claims).
pub const GOOGLE_USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// Scopes requested for every HomeTube account. The YouTube scope grants
/// read/write access to the user's subscriptions, playlists, and likes.
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/youtube",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// Buffer used by [`refresh_if_expired`] — refresh proactively if the
/// access token expires within this many seconds.
pub const REFRESH_BUFFER_SECONDS: i64 = 5 * 60;

/// Fully-configured Google OAuth2 client (auth URL, token URL, redirect,
/// and client secret all set).
pub type GoogleOAuthClient = BasicClient<
    EndpointSet,    // HasAuthUrl
    EndpointNotSet, // HasDeviceAuthUrl
    EndpointNotSet, // HasIntrospectionUrl
    EndpointNotSet, // HasRevocationUrl
    EndpointSet,    // HasTokenUrl
>;

/// Subset of fields HomeTube needs from Google's `/userinfo` response.
#[derive(Debug, Clone, Deserialize)]
pub struct GoogleUserInfo {
    /// Google's stable account identifier (the `sub` claim).
    pub sub: String,
    pub email: String,
    /// User-provided display name — may be missing for some accounts.
    #[serde(default)]
    pub name: Option<String>,
    /// Avatar URL — may be missing.
    #[serde(default)]
    pub picture: Option<String>,
}

/// Build a Google OAuth2 client from the credentials stored in
/// `app_config`. Returns [`AppError::BadRequest`] (with a clear message)
/// if the setup wizard has not yet stored credentials.
pub async fn build_client(pool: &SqlitePool) -> AppResult<GoogleOAuthClient> {
    let client_id = get_config_value(pool, KEY_GOOGLE_CLIENT_ID)
        .await?
        .ok_or_else(|| AppError::BadRequest("Google client ID not configured".into()))?;
    let client_secret = get_config_value(pool, KEY_GOOGLE_CLIENT_SECRET)
        .await?
        .ok_or_else(|| AppError::BadRequest("Google client secret not configured".into()))?;
    let redirect_uri = get_config_value(pool, KEY_GOOGLE_REDIRECT_URI)
        .await?
        .ok_or_else(|| AppError::BadRequest("Google redirect URI not configured".into()))?;

    let auth_url = AuthUrl::new(GOOGLE_AUTH_URL.to_string())
        .map_err(|e| AppError::Other(anyhow::anyhow!("invalid auth URL: {e}")))?;
    let token_url = TokenUrl::new(GOOGLE_TOKEN_URL.to_string())
        .map_err(|e| AppError::Other(anyhow::anyhow!("invalid token URL: {e}")))?;
    let redirect = RedirectUrl::new(redirect_uri)
        .map_err(|e| AppError::BadRequest(format!("invalid redirect URI: {e}")))?;

    let client = BasicClient::new(ClientId::new(client_id))
        .set_client_secret(ClientSecret::new(client_secret))
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        .set_redirect_uri(redirect);

    Ok(client)
}

/// Build the underlying HTTP client used to talk to Google. SSRF
/// prevention is not strictly required here (the URL is hard-coded) but
/// disabling redirects matches the recommendation in the `oauth2` crate
/// docs.
///
/// Note: this returns the `reqwest` re-exported by the `oauth2` crate
/// (v0.12 at the time of writing), which is a separate dependency from
/// the application's top-level `reqwest` (v0.13). Do not mix the two.
fn http_client() -> AppResult<oauth_reqwest::Client> {
    let client = oauth_reqwest::ClientBuilder::new()
        .redirect(oauth_reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AppError::Other(anyhow::anyhow!("building HTTP client: {e}")))?;
    Ok(client)
}

/// Exchange an authorization code (received on the OAuth callback) for a
/// token response.
pub async fn exchange_code(
    client: &GoogleOAuthClient,
    code: String,
    pkce_verifier: PkceCodeVerifier,
) -> AppResult<BasicTokenResponse> {
    let http = http_client()?;
    let token = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("exchanging code with Google: {e}")))?;
    Ok(token)
}

/// Use a refresh token to obtain a fresh access token. Google may or may
/// not return a new refresh token; if it does, callers should persist it.
pub async fn refresh_token(
    client: &GoogleOAuthClient,
    refresh_token: &str,
) -> AppResult<BasicTokenResponse> {
    let http = http_client()?;
    let token = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(&http)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("refreshing Google token: {e}")))?;
    Ok(token)
}

/// Fetch profile claims for the user behind `access_token`.
pub async fn userinfo(access_token: &str) -> AppResult<GoogleUserInfo> {
    let res = reqwest::Client::new()
        .get(GOOGLE_USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        warn!(%status, %body, "Google userinfo request failed");
        return Err(AppError::Other(anyhow::anyhow!(
            "Google userinfo returned {status}"
        )));
    }
    let info = res.json::<GoogleUserInfo>().await?;
    Ok(info)
}

/// If the access token for `account_id` will expire within
/// [`REFRESH_BUFFER_SECONDS`], use the stored refresh token to obtain a
/// fresh one and persist it. Returns the (possibly updated) access token
/// suitable for an immediate API call.
///
/// The token-refresh strategy from the plan: "Before any YouTube API call,
/// check if token expires within 5 minutes. If so, refresh proactively. If
/// refresh fails (revoked), mark account as needing re-auth." This helper
/// implements the first half — the "needs re-auth" bookkeeping is layered
/// on top by callers.
pub async fn refresh_if_expired(pool: &SqlitePool, account_id: i64) -> AppResult<String> {
    let row: (String, String, i64) = sqlx::query_as(
        "SELECT access_token, refresh_token, token_expires_at FROM accounts WHERE id = ?",
    )
    .bind(account_id)
    .fetch_one(pool)
    .await?;

    let (access_token, refresh_tok, expires_at) = row;
    let now = Utc::now().timestamp();
    if expires_at - now > REFRESH_BUFFER_SECONDS {
        return Ok(access_token);
    }

    debug!(account_id, "refreshing access token");
    let client = build_client(pool).await?;
    let response = refresh_token(&client, &refresh_tok).await?;

    let new_access = response.access_token().secret().to_string();
    let new_refresh = response
        .refresh_token()
        .map(|t| t.secret().to_string())
        .unwrap_or(refresh_tok);
    let new_expires = now
        + response
            .expires_in()
            .map(|d| d.as_secs() as i64)
            .unwrap_or(3600);

    sqlx::query(
        "UPDATE accounts SET access_token = ?, refresh_token = ?, token_expires_at = ?, \
         updated_at = unixepoch() WHERE id = ?",
    )
    .bind(&new_access)
    .bind(&new_refresh)
    .bind(new_expires)
    .bind(account_id)
    .execute(pool)
    .await?;

    Ok(new_access)
}

/// Build an authorize URL with PKCE and a CSRF state.
pub fn authorize_url(
    client: &GoogleOAuthClient,
) -> (
    oauth2::url::Url,
    oauth2::CsrfToken,
    oauth2::PkceCodeVerifier,
) {
    let (challenge, verifier) = oauth2::PkceCodeChallenge::new_random_sha256();
    let mut req = client.authorize_url(oauth2::CsrfToken::new_random);
    for s in SCOPES {
        req = req.add_scope(Scope::new((*s).to_string()));
    }
    let (url, csrf) = req
        .set_pkce_challenge(challenge)
        // Force a refresh token to be issued.
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .url();
    (url, csrf, verifier)
}
