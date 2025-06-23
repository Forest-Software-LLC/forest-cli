use crate::tokens::{get_stored_tokens, store_tokens};
use anyhow::{anyhow, Context, Result};
use reqwest::{Client, Method, StatusCode, multipart::Form};
use serde_json::Value;
use std::{env, sync::Arc};

/// Body for API requests: either JSON or a builder for multipart form.
#[derive(Clone)]
pub enum RequestBody {
    #[allow(dead_code)]
    Json(Value),
    Multipart(Arc<dyn Fn() -> Form + Send + Sync>),
}

/// Generic API request helper. Supports JSON or multipart bodies with auth + auto-refresh.
///
/// - `endpoint`: API path, appended to FOREST_API_URL
/// - `method`: HTTP method
/// - `body`: optional `RequestBody`, determines whether to send JSON or multipart.
///
/// Returns the parsed JSON response on success or an error.
pub async fn api_request(
    endpoint: &str,
    method: Method,
    body: Option<RequestBody>,
) -> Result<Value> {
    let api_url = env::var("FOREST_API_URL").context("FOREST_API_URL must be set")?;
    let mut tokens = get_stored_tokens()?;
    let client = Client::new();

    // Builder function for a new request with the given token
    let build_req = |token: &str| {
        let url = format!("{}{}", api_url, endpoint);
        println!("Requesting {} {}", method, url);
        let mut req = client.request(method.clone(), &url)
            .bearer_auth(token)
            .header("Accept", "application/json");

        if let Some(ref b) = body {
            match b {
                RequestBody::Json(json) => {
                    req = req.json(json);
                }
                RequestBody::Multipart(builder) => {
                    let form = (builder)();
                    req = req.multipart(form);
                }
            }
        }
        req
    };

    // First attempt
    let mut resp = build_req(&tokens.access_token)
        .send()
        .await
        .context("Network error on first attempt")?;

    // On 401, refresh token and retry once
    if resp.status() == StatusCode::UNAUTHORIZED {
        // Refresh
        let refresh_resp = client
            .post(format!("{}v1/auth/refresh", api_url))
            .json(&serde_json::json!({ "refreshToken": get_stored_tokens()?.refresh_token }))
            .send()
            .await
            .context("Failed to send refresh request")?;
        let data: Value = refresh_resp.json().await.unwrap_or_default();
        if let (Some(at), Some(rt)) = (
            data.get("accessToken").and_then(Value::as_str),
            data.get("refreshToken").and_then(Value::as_str)
        ) {
            store_tokens(at, rt)?;
            tokens = get_stored_tokens()?;
        } else {
            return Err(anyhow!("Token refresh failed, please login again"));
        }
        // Retry with new token
        resp = build_req(&tokens.access_token)
            .send()
            .await
            .context("Network error on retry")?;
    }

    // Parse response body
    let status = resp.status();
    let body_json: Value = resp.json().await.context("Failed to parse JSON response")?;
    if !status.is_success() {
        let err = body_json.get("error").and_then(Value::as_str).unwrap_or("Unknown error");
        Err(anyhow!("Request failed {}: {}", status, err))
    } else {
        Ok(body_json)
    }
}
