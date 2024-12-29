use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

pub mod server;

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize)]
pub struct TokenClaims {
    pub username: String,
    pub email: String,
    pub exp: u64,
}

pub fn parse_secrets(secrets_file: &std::path::Path) -> Result<String> {
    let file = std::fs::File::open(secrets_file)?;
    let reader = std::io::BufReader::new(file);
    let v: serde_json::Map<String, serde_json::Value> = serde_json::from_reader(reader)?;
    v.get("jwt_key")
        .map(|v| v.as_str().map(|s| s.to_string()))
        .flatten()
        .ok_or_else(|| anyhow!("jwt_key is missing"))
}
