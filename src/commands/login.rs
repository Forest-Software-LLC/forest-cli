use anyhow::{Context, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password, Select};
use reqwest::Client;
use serde::Deserialize;
use std::env;
use std::result::Result::Ok;

use crate::http::api_request;
use crate::tokens::store_tokens;
use crate::message::{Message, MessageType};

#[derive(Deserialize)]
struct LoginResponse {
    #[serde(rename = "accessToken")]
    access_token: String,

    #[serde(rename = "refreshToken")]
    refresh_token: String,
}

fn open_url(url: &str) -> anyhow::Result<()> {
    open::that(url)?;
    Ok(())
}

/// Prompt the user for credentials, POST to /v1/auth/login, and store tokens on success.
pub async fn login_command() -> Result<()> {
    // Ensure API URL is configured
    let api_url = env::var("FOREST_API_URL")
        .context("FOREST_API_URL environment variable must be set")?;

    let frontend_url = env::var("FRONTEND_URL")
        .context("FRONTEND_URL environment variable must be set")?;

    let client = Client::new();
    
    let login_method = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("How would you like to log in?")
        .items(&["Browser (Recommended)", "Username/Password"])
        .default(0)
        .interact()?;

    if login_method == 1 {
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
    } else {
        // Browser login flow
        let message = Message::new("Waiting for browser login...");

        let (resp, _) = api_request("v1/auth/browser", reqwest::Method::POST, None)
            .await
            .context("Failed to initiate browser login")?;

        let device_code = resp.get("deviceCode")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing deviceCode in response"))?;

        open_url(&format!("{}/auth/verify/cli?deviceCode={}", frontend_url, device_code))
            .context("Failed to open browser for login")?;

        tokio::time::sleep(tokio::time::Duration::from_secs(4)).await; // Wait before checking status

        let mut retry_count : i8 = 0;
        loop {
            let (status_data, status_code) = api_request(&format!("v1/auth/browser?deviceCode={}", device_code), reqwest::Method::GET, None)
                .await
                .context("Failed to check browser login status")?;

            let status = status_data.get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Invalid status format in response"))?;

            if status_code.is_success() {
                if status == "authenticated" {
                    let session_data = status_data.get("data")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| anyhow::anyhow!("Missing session data in response"))?;

                    // Get username from session_data.user.username
                    let username = session_data.get("user")
                        .and_then(|v| v.get("username"))
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("Missing username in session data"))?;


                    // Get tokens from session_data.tokens

                    let access_token = session_data.get("tokens")
                        .and_then(|v| v.get("accessToken"))
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("Missing accessToken in response"))?;
                    let refresh_token = session_data.get("tokens")
                        .and_then(|v| v.get("refreshToken"))
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("Missing refreshToken in response"))?;
                    store_tokens(access_token, refresh_token)
                        .context("Failed to store tokens")?;

                    message.finish(MessageType::Success, &format!("Welcome back, {}!", username));
                    break;
                }
            } else {
                message.finish(MessageType::Fail, "Login failed, please try again.");
                break;
                // Optionally, you could add a retry limit or exit condition here.
            }

            if retry_count >= 10 {
                message.finish(MessageType::Fail, "Login timed out. Please try again.");
                break;
            }

            // Wait before checking again
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

            retry_count += 1;
        }
        
    }

    Ok(())
}
