//! FOREST_TOKEN: an API token for CI. It wins over the stored login, is
//! never written to disk and never refreshed.

use crate::tokens::get_stored_tokens;

pub const ENV_VAR: &str = "FOREST_TOKEN";

pub fn env_api_token() -> Option<String> {
    std::env::var(ENV_VAR)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// FOREST_TOKEN or a stored login.
pub fn has_credential() -> bool {
    env_api_token().is_some()
        || get_stored_tokens().map(|t| !t.access_token.is_empty()).unwrap_or(false)
}
