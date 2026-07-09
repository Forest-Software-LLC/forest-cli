use anyhow::{Context, Result};

use crate::http::{api_request, RequestBody};
use crate::tokens::{clear_tokens, get_stored_tokens};
use crate::message::{Message, MessageType, info};

/// Log out: best-effort revoke the refresh token server-side, then remove the
/// locally stored tokens so the CLI is signed out regardless of the outcome.
pub async fn logout_command() -> Result<()> {
    let tokens = get_stored_tokens()?;

    if tokens.access_token.is_empty() {
        info("You are not logged in.");
        return Ok(());
    }

    let message = Message::new("Logging out...");

    // Best-effort server-side revocation of this session's refresh token. We
    // ignore the result: clearing the local tokens below is what signs the
    // user out on this machine, and it always succeeds. (The endpoint replies
    // with a plain-text body, so we don't try to parse or branch on it.)
    let _ = api_request(
        "v1/auth/logout",
        reqwest::Method::POST,
        Some(RequestBody::Json(serde_json::json!({
            "refreshToken": tokens.refresh_token
        }))),
        None,
    )
    .await;

    clear_tokens().context("Failed to clear stored tokens")?;

    message.finish(MessageType::Success, "Logged out.");

    Ok(())
}
