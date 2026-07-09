use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde_json::Value;

use crate::http::api_request;
use crate::tokens::get_stored_tokens;
use crate::message::{fail, info, success};

/// Show the currently logged-in user.
pub async fn whoami_command() -> Result<()> {
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
