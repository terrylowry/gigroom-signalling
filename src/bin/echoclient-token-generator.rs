use anyhow::Result;
use clap::Parser;
use gigroom_signalling::{parse_secrets, TokenClaims};
use jsonwebtoken::{self as jwt, EncodingKey};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// JSON file containing necessary secrets: jwt_key
    #[arg(long, value_name = "FILE")]
    secrets: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let jwt_key = parse_secrets(&args.secrets)?;

    let echo_claims = TokenClaims {
        username: "echo-client".to_string(),
        email: "info@gigroom.com".to_string(),
        // Set expiry as one year from now
        exp: jwt::get_current_timestamp() + (365 * 24 * 3600),
    };

    let token = jwt::encode(
        &jwt::Header::default(),
        &echo_claims,
        &EncodingKey::from_secret(jwt_key.as_bytes()),
    )?;
    println!("Echo client auth token, expires in 1 year: {token}");

    Ok(())
}
