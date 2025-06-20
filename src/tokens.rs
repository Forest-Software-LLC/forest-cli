// src/tokens.rs
use std::{fs, path::PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::{Context, Result};

#[derive(Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

fn tokens_file() -> PathBuf {
    let mut path = dirs::home_dir().expect("Could not determine home directory");
    path.push(".forest_tokens.json");
    path
}

/// Read the JSON file at `~/.forest_tokens.json` and deserialize it.
pub fn get_stored_tokens() -> Result<Tokens> {
    let path = tokens_file();
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read tokens file at {:?}", path))?;
    let tokens = serde_json::from_str(&contents)?;
    Ok(tokens)
}

/// Serialize `Tokens` and write them to `~/.forest_tokens.json`.
pub fn store_tokens(access: &str, refresh: &str) -> Result<()> {
    let tokens = Tokens {
        access_token: access.to_owned(),
        refresh_token: refresh.to_owned(),
    };
    let json = serde_json::to_string_pretty(&tokens)?;
    fs::write(tokens_file(), json)?;
    Ok(())
}