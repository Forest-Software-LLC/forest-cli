use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde_json::Value;

use crate::api_token::{env_api_token, ENV_VAR as API_TOKEN_VAR};
use crate::http::api_request;
use crate::tokens::get_stored_tokens;
use crate::message::{fail, info, success};

/// Show the currently logged-in user, or the API token in FOREST_TOKEN.
pub async fn whoami_command() -> Result<()> {
    if env_api_token().is_some() {
        return token_whoami().await;
    }

    // Fast path: no stored token means not logged in, skip the network call.
    if get_stored_tokens()?.access_token.is_empty() {
        info("You are not logged in. Run `forest login` to sign in.");
        return Ok(());
    }

    let (session_resp, status_code) = api_request("v1/auth/session", reqwest::Method::GET, None, None)
        .await
        .context("Failed to get session information")?;

    if status_code == StatusCode::UNAUTHORIZED {
        info("You are not logged in. Run `forest login` to sign in.");
        return Ok(());
    }

    if !status_code.is_success() {
        fail("Failed to fetch your account information. Please try again.");
        return Ok(());
    }

    let username = session_resp.get("username")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Missing username in session response"))?;

    let plan = if session_resp.get("isPro").and_then(Value::as_bool).unwrap_or(false) {
        "Pro"
    } else {
        "Free"
    };

    success(&format!("Logged in as {} ({})", username, plan));

    Ok(())
}

/// FOREST_TOKEN wins over the stored login, so describe the token.
async fn token_whoami() -> Result<()> {
    let (resp, status) = api_request("v1/auth/token", reqwest::Method::GET, None, None).await?;
    if !status.is_success() {
        fail(&format!("Failed to look up {}. Please try again.", API_TOKEN_VAR));
        return Ok(());
    }

    let field = |key: &str| resp.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let owner = if field("kind") == "organization" {
        format!("studio {}", field("owner"))
    } else {
        field("owner")
    };
    let mut details = Vec::new();
    if let Some(rank) = resp.get("rank").and_then(Value::as_str) {
        details.push(if rank == "admin" { "all packages".to_string() } else { "roles and grants".to_string() });
    }
    let can_publish = resp.get("scopes").and_then(Value::as_array)
        .map_or(false, |scopes| scopes.iter().any(|s| s.as_str() == Some("publish")));
    details.push(if can_publish { "read and publish" } else { "read only" }.to_string());
    details.push(match resp.get("expiresAt").and_then(Value::as_str) {
        Some(at) => format!("expires {}", at.get(..10).unwrap_or(at)),
        None => "never expires".to_string(),
    });

    success(&format!(
        "Using {} \"{}\" for {} ({})",
        API_TOKEN_VAR, field("name"), owner, details.join(", ")
    ));
    Ok(())
}
