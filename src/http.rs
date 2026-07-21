use crate::tokens::{get_stored_tokens, store_tokens};
use anyhow::{Context, Result};
use reqwest::{header, multipart::Form, Client, Method, StatusCode};
use serde_json::Value;
use std::{env, sync::Arc, sync::OnceLock, time::Duration};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Publish uploads can legitimately take minutes on a slow uplink, so
/// multipart requests override the total timeout with this instead.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Shared async client: an install makes ~2 small registry requests per
/// package, so connection keep-alive across them (instead of a fresh
/// TCP+TLS handshake each time) dominates resolution latency.
fn async_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build HTTP client")
    })
}

/// Shared blocking client for tarball downloads. Only call this from the
/// download worker threads — reqwest's blocking client must not be created
/// on an async runtime thread.
///
/// gzip is explicitly OFF: tarball bytes are hashed against the lockfile
/// integrity, and transparent decompression would change the hashed bytes
/// if the CDN ever added Content-Encoding.
pub fn blocking_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(Duration::from_secs(120))
            .gzip(false)
            .build()
            .expect("failed to build download client")
    })
}

/// Body for API requests: either JSON or a builder for multipart form.
#[derive(Clone)]
pub enum RequestBody {
    #[allow(dead_code)]
    Json(Value),
    Multipart(Arc<dyn Fn() -> Form + Send + Sync>),
}

/// Request WITHOUT auth or refresh handling — for pre-auth endpoints (login,
/// 2FA verify) where a 401 is a real answer (wrong password / wrong code),
/// not a stale-token signal to retry on.
pub async fn api_request_public(
    endpoint: &str,
    method: Method,
    body: Option<RequestBody>,
) -> Result<(Value, StatusCode)> {
    let api_url = env::var("FOREST_API_URL").context("FOREST_API_URL must be set")?;
    let client = async_client();

    let mut req = client.request(method, format!("{}{}", api_url, endpoint))
        .header("Accept", "application/json");

    if let Some(b) = body {
        match b {
            RequestBody::Json(json) => {
                req = req.json(&json);
            }
            RequestBody::Multipart(builder) => {
                req = req.multipart((builder)());
            }
        }
    }

    let resp = req.send().await.context("Network error")?;
    let status = resp.status();
    let body_json: Value = resp.json().await.context("Failed to parse JSON response")?;

    Ok((body_json, status))
}

/// Generic API request against the main API (FOREST_API_URL): auth, account,
/// package listings — everything except upload/download.
pub async fn api_request(
    endpoint: &str,
    method: Method,
    body: Option<RequestBody>,
    headers: Option<header::HeaderMap>,
) -> Result<(Value, StatusCode)> {
    api_request_with_base("FOREST_API_URL", endpoint, method, body, headers).await
}

/// Request against the package gateway (FOREST_PACKAGES_URL) — the public,
/// independently auditable service that owns package upload and download.
/// Same auth/refresh behavior; the session refresh itself always goes to the
/// main API, which is the only service that handles credentials.
pub async fn packages_api_request(
    endpoint: &str,
    method: Method,
    body: Option<RequestBody>,
    headers: Option<header::HeaderMap>,
) -> Result<(Value, StatusCode)> {
    api_request_with_base("FOREST_PACKAGES_URL", endpoint, method, body, headers).await
}

/// Shared implementation. Supports JSON or multipart bodies with auth + auto-refresh.
///
/// - `base_env`: env var holding the base URL for this request
/// - `endpoint`: API path, appended to the base URL
/// - `method`: HTTP method
/// - `body`: optional `RequestBody`, determines whether to send JSON or multipart.
///
/// Returns the parsed JSON response on success or an error.
async fn api_request_with_base(
    base_env: &str,
    endpoint: &str,
    method: Method,
    body: Option<RequestBody>,
    headers: Option<header::HeaderMap>,
) -> Result<(Value, StatusCode)> {
    let api_url = env::var(base_env).with_context(|| format!("{} must be set", base_env))?;
    let mut tokens = get_stored_tokens()?;
    let client = async_client();

    // Builder function for a new request with the given token
    let build_req = |token: &str| {
        let url = format!("{}{}", api_url, endpoint);
        //println!("Requesting {} {}", method, url);
        let mut req = client.request(method.clone(), &url)
            .bearer_auth(token)
            .header("Accept", "application/json");


        // Add custom headers if provided
        if let Some(ref hdrs) = headers {
            for (key, value) in hdrs.iter() {
                req = req.header(key, value);
            }
        }

        if let Some(ref b) = body {
            match b {
                RequestBody::Json(json) => {
                    req = req.json(json);
                }
                RequestBody::Multipart(builder) => {
                    let form = (builder)();
                    // Uploads get a longer leash than the client-wide timeout.
                    req = req.multipart(form).timeout(UPLOAD_TIMEOUT);
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

    // On 401, refresh token and retry once. The refresh always goes to the
    // main API regardless of which base this request used — auth is
    // centralized there and nowhere else.
    if resp.status() == StatusCode::UNAUTHORIZED {
        let auth_url = env::var("FOREST_API_URL").context("FOREST_API_URL must be set")?;
        let refresh_resp = client
            .post(format!("{}v1/auth/refresh", auth_url))
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
            return Ok((
                serde_json::json!({ "error": "Failed to refresh token" }),
                StatusCode::UNAUTHORIZED
            ));
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
        //let err = body_json.get("error").and_then(Value::as_str).unwrap_or("Unknown error");
       
    Ok((body_json, status))
}
