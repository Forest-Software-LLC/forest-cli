use anyhow::{Context, Result};
use dialoguer::{Input, Password};
use reqwest::Client;
use serde::Deserialize;
use std::env;
use std::result::Result::Ok;

use crate::tokens::store_tokens;
use crate::message::{Message, MessageType};

#[derive(Deserialize)]
struct LoginResponse {
    #[serde(rename = "accessToken")]
    access_token: String,

    #[serde(rename = "refreshToken")]
    refresh_token: String,
}

/// Prompt the user for credentials, POST to /v1/auth/login, and store tokens on success.
pub async fn login_command() -> Result<()> {
    // Ensure API URL is configured
    let api_url = env::var("FOREST_API_URL")
        .context("FOREST_API_URL environment variable must be set")?;

    let client = Client::new();
    

    loop {
        // Interactive prompts
        let username: String = Input::new()
            .with_prompt("Username")
            .interact_text()?;

        let password: String = Password::new()
            .with_prompt("Password")
            .interact()?;

        let message = Message::new("Logging in...");
        // Send login request
        let resp = match client
            .post(format!("{}v1/auth/login", api_url))
            .json(&serde_json::json!({
                "username": username,
                "password": password
            }))
            .send()
            .await {
                Ok(r) => r,
                Err(e) => {
                    message.finish(MessageType::Fail, &format!("Failed to send login request: {}", e));
                    continue; // Retry on failure
                }
            };
            //.context("Failed to send login request")?;

        // Parse status and body
        let status = resp.status();
        let data: LoginResponse = resp
            .json()
            .await
            .context("Failed to parse login response JSON")?;

        // Check for success
        if status.is_success() && !data.access_token.is_empty() {
            message.finish(MessageType::Success, "Logged in successfully.");
            store_tokens(&data.access_token, &data.refresh_token)
                .context("Failed to store tokens")?;
            break;
        } else {
            message.finish(MessageType::Fail, "Login failed. Please check your credentials and try again.");
            // Optionally, you could add a retry limit or exit condition here.
        }
    }

    Ok(())
}
