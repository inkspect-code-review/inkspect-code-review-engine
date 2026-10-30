//! GitHub App authentication: mint installation tokens with the App's own
//! identity, so the engine clones and comments as the App (not a personal
//! account).
//!
//! Flow (matches the Go engine's app.go):
//!   1. Build an RS256 JWT signed with the App private key (PEM): iss = app id,
//!      iat = now-60s, exp = now+9m.
//!   2. GET /repos/{owner}/{repo}/installation (with the JWT) to confirm the
//!      installation and its id.
//!   3. POST /app/installations/{id}/access_tokens scoped to the single repo
//!      with the minimum permissions (contents:read, pull_requests:write,
//!      metadata:read). The returned `ghs_` token is short-lived.
//!
//! The PEM is read from a file path (never embedded); the token is never
//! logged. Configuration comes from the environment:
//!   INKSPECT_GITHUB_APP_ID          decimal App id
//!   INKSPECT_GITHUB_APP_PRIVATE_KEY_FILE   absolute path to the .pem

use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::RsaPrivateKey;
use serde::Deserialize;

use crate::error::{err, Code, Result};

const API_ORIGIN: &str = "https://api.github.com";

/// An installation access token with its permissions.
pub struct InstallationToken {
    pub token: String,
    pub can_write_pulls: bool,
}

/// Mint an installation token for `owner/repo` using the App credentials in the
/// environment. Returns None if App credentials are not configured, so callers
/// can fall back to an explicit token.
pub async fn installation_token(owner: &str, repo: &str) -> Result<Option<InstallationToken>> {
    let Some(app_id) = std::env::var("INKSPECT_GITHUB_APP_ID").ok().filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let Some(pem_path) = std::env::var("INKSPECT_GITHUB_APP_PRIVATE_KEY_FILE")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };

    let pem = std::fs::read_to_string(&pem_path)
        .map_err(|_| err(Code::GitHub, "could not read app private key"))?;
    let jwt = app_jwt(&app_id, &pem)?;

    let client = reqwest::Client::builder()
        .user_agent("Inkspect-Rust-Engine")
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| err(Code::GitHub, "client build failed"))?;

    // 1. Resolve the installation id for this repository.
    let inst: Installation = client
        .get(format!("{API_ORIGIN}/repos/{owner}/{repo}/installation"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(&jwt)
        .send()
        .await
        .map_err(|_| err(Code::GitHub, "installation lookup failed"))?
        .error_for_status()
        .map_err(|_| err(Code::GitHub, "app not installed on repository"))?
        .json()
        .await
        .map_err(|_| err(Code::GitHub, "installation decode failed"))?;

    // 2. Mint a repo-scoped token with least privilege.
    let body = serde_json::json!({
        "repositories": [repo],
        "permissions": {
            "contents": "read",
            "pull_requests": "write",
            "metadata": "read",
        }
    });
    let token: TokenResponse = client
        .post(format!(
            "{API_ORIGIN}/app/installations/{}/access_tokens",
            inst.id
        ))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(&jwt)
        .json(&body)
        .send()
        .await
        .map_err(|_| err(Code::GitHub, "token request failed"))?
        .error_for_status()
        .map_err(|_| err(Code::GitHub, "token request rejected"))?
        .json()
        .await
        .map_err(|_| err(Code::GitHub, "token decode failed"))?;

    if token.token.is_empty() || token.token.contains(['\r', '\n']) {
        return Err(err(Code::GitHub, "invalid installation token"));
    }
    let can_write_pulls = token
        .permissions
        .get("pull_requests")
        .map(|v| v == "write")
        .unwrap_or(false);

    Ok(Some(InstallationToken {
        token: token.token,
        can_write_pulls,
    }))
}

/// Build and sign an RS256 App JWT. Accepts PKCS#1 ("RSA PRIVATE KEY") or
/// PKCS#8 ("PRIVATE KEY") PEM, matching GitHub's key exports.
fn app_jwt(app_id: &str, pem: &str) -> Result<String> {
    let key = RsaPrivateKey::from_pkcs1_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs8_pem(pem))
        .map_err(|_| err(Code::GitHub, "invalid app private key"))?;
    let signing_key: SigningKey<Sha256> = SigningKey::new(key);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| err(Code::Internal, "clock error"))?
        .as_secs() as i64;
    let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = format!(
        r#"{{"iat":{},"exp":{},"iss":"{}"}}"#,
        now - 60,
        now + 540,
        app_id
    );
    let claims_b64 = b64url(claims.as_bytes());
    let signing_input = format!("{header}.{claims_b64}");

    let signature = signing_key.sign(signing_input.as_bytes());
    let sig_b64 = b64url(&signature.to_bytes());
    Ok(format!("{signing_input}.{sig_b64}"))
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
    #[serde(default)]
    permissions: std::collections::HashMap<String, String>,
}
