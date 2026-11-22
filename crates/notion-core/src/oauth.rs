//! Notion OAuth 2.0 for a public integration. The desktop app opens Notion's
//! own login/authorization page in the browser and exchanges the returned code
//! for a user access token.
//!
//! The client secret is the app's own; it is not a per-workspace key. Keep it
//! in the local profile file, never in source control.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{Error, Result};

/// The OAuth token endpoint requires the current API version even though the
/// rest of this client still speaks 2025-09-03.
pub const OAUTH_VERSION: &str = "2026-03-11";
pub const DEFAULT_REDIRECT: &str = "http://localhost:8765/callback";

/// The token response. The fields hold secrets: do not derive Debug.
#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub workspace_name: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub owner: Option<Owner>,
}

#[derive(Clone, Deserialize)]
pub struct Owner {
    #[serde(default)]
    pub user: Option<OwnerUser>,
}

#[derive(Clone, Deserialize)]
pub struct OwnerUser {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl TokenResponse {
    /// The authorizing person's user id, when Notion sent one.
    pub fn user_id(&self) -> Option<String> {
        self.owner
            .as_ref()
            .and_then(|o| o.user.as_ref())
            .and_then(|u| u.id.clone())
    }

    pub fn workspace(&self) -> Option<&str> {
        self.workspace_name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
    }
}

/// Percent-encode an OAuth query parameter (RFC 3986 unreserved bytes stay).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The URL that shows Notion's login and page picker.
pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "https://api.notion.com/v1/oauth/authorize?owner=user&client_id={}&redirect_uri={}&response_type=code&state={}",
        encode(client_id),
        encode(redirect_uri),
        encode(state)
    )
}

/// Exchanges the temporary callback code for an access token.
pub async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
) -> Result<TokenResponse> {
    exchange_at(
        "https://api.notion.com",
        client_id,
        client_secret,
        redirect_uri,
        code,
    )
    .await
}

/// [`exchange_code`] with a base URL for tests.
pub async fn exchange_at(
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
) -> Result<TokenResponse> {
    token_request(
        base_url,
        client_id,
        client_secret,
        json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect_uri,
        }),
    )
    .await
}

/// Uses the refresh token from an earlier authorization. Notion may rotate
/// both tokens.
pub async fn refresh(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    refresh_at(
        "https://api.notion.com",
        client_id,
        client_secret,
        refresh_token,
    )
    .await
}

pub async fn refresh_at(
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    token_request(
        base_url,
        client_id,
        client_secret,
        json!({ "grant_type": "refresh_token", "refresh_token": refresh_token }),
    )
    .await
}

async fn token_request(
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    body: Value,
) -> Result<TokenResponse> {
    let url = format!("{}/v1/oauth/token", base_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(url)
        .basic_auth(client_id, Some(client_secret))
        .header("Notion-Version", OAUTH_VERSION)
        .json(&body)
        .send()
        .await?;
    let status = resp.status().as_u16();
    let text = resp.text().await?;
    if !(200..300).contains(&status) {
        let err: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let code = err["error"]
            .as_str()
            .or_else(|| err["code"].as_str())
            .unwrap_or("oauth_error");
        let message = err["error_description"]
            .as_str()
            .or_else(|| err["message"].as_str())
            .unwrap_or(&text);
        return Err(Error::Api {
            status,
            code: code.to_string(),
            message: message.to_string(),
        });
    }
    Ok(serde_json::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn the_authorize_url_encodes_the_redirect_and_state() {
        let u = authorize_url("client-id", "http://localhost:8765/callback", "state-123");
        assert!(u.starts_with("https://api.notion.com/v1/oauth/authorize?owner=user&"));
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8765%2Fcallback"));
        assert!(u.contains("response_type=code"));
        assert!(u.contains("state=state-123"));
        assert_eq!(encode("a b/c?d"), "a%20b%2Fc%3Fd");
    }

    #[tokio::test]
    async fn exchanges_a_code_for_a_token() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_json(json!({
                "grant_type": "authorization_code",
                "code": "abc",
                "redirect_uri": "http://localhost:8765/callback",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "ntn_access",
                "token_type": "bearer",
                "refresh_token": "nrt_refresh",
                "bot_id": "b1",
                "workspace_name": "Acme",
                "workspace_id": "w1",
                "owner": { "type": "user", "user": { "object": "user", "id": "u1" } }
            })))
            .expect(1)
            .mount(&s)
            .await;
        let t = exchange_at(
            &s.uri(),
            "client",
            "secret",
            "http://localhost:8765/callback",
            "abc",
        )
        .await
        .unwrap();
        assert_eq!(t.access_token, "ntn_access");
        assert_eq!(t.refresh_token.as_deref(), Some("nrt_refresh"));
        assert_eq!(t.workspace(), Some("Acme"));
        assert_eq!(t.user_id().as_deref(), Some("u1"));
    }

    #[tokio::test]
    async fn an_oauth_error_is_an_api_error() {
        let s = MockServer::start().await;
        Mock::given(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "bad code"
            })))
            .mount(&s)
            .await;
        match exchange_at(&s.uri(), "c", "s", "http://localhost/cb", "x").await {
            Err(Error::Api {
                status: 400,
                code,
                message,
            }) => {
                assert_eq!(
                    (code.as_str(), message.as_str()),
                    ("invalid_grant", "bad code")
                );
            }
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("unexpected success"),
        }
    }

    #[tokio::test]
    async fn refreshes_a_token() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_json(
                json!({"grant_type": "refresh_token", "refresh_token": "r1"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new",
                "refresh_token": "r2",
            })))
            .mount(&s)
            .await;
        let t = refresh_at(&s.uri(), "c", "s", "r1").await.unwrap();
        assert_eq!(t.access_token, "new");
        assert_eq!(t.refresh_token.as_deref(), Some("r2"));
    }
}
