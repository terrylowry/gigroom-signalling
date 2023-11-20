use clap::Parser;
use tokio::task;

use anyhow::{anyhow, bail, Error};
use std::io::BufReader;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::rustls::{Certificate, PrivateKey, ServerConfig};
use tokio_rustls::TlsAcceptor;

use gigroom_signalling::server::Server;

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

#[derive(Parser, Debug)]
#[clap(about, version, author)]
/// Program arguments
struct Args {
    /// Address to listen on
    #[clap(long, default_value = "0.0.0.0")]
    host: String,
    /// Port to listen on
    #[clap(short, long, default_value_t = 8443)]
    port: u16,
    /// TLS certificate chain to use
    #[clap(long)]
    chain: Option<String>,
    /// TLS certificate private key to use
    #[clap(long)]
    priv_key: Option<String>,
    /// password to TLS certificate
    #[clap(long)]
    cert_password: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::init();

    let args = Args::parse();
    let server = Server::new();

    let addr = format!("{}:{}", args.host, args.port);

    // Create the event loop and TCP listener we'll accept connections on.
    let listener = TcpListener::bind(&addr).await?;

    let acceptor = match (args.chain, args.priv_key) {
        (Some(chain), Some(key)) => {
            let chain_file = std::fs::File::open(chain)?;
            let mut reader = BufReader::new(chain_file);
            let certs = rustls_pemfile::certs(&mut reader)?;

            let key_file = std::fs::File::open(key)?;
            let mut reader = BufReader::new(key_file);
            let mut keys = rustls_pemfile::pkcs8_private_keys(&mut reader)?;

            let private_key = match keys.len() {
                0 => Err(anyhow!("No keys found in priv key")),
                1 => Ok(keys.remove(0)),
                _ => Err(anyhow!("More than one key found in priv key")),
            }?;
            let config = ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(
                    certs.into_iter().map(Certificate).collect(),
                    PrivateKey(private_key),
                )
                .expect("bad cert/key");
            Some(TlsAcceptor::from(Arc::new(config)))
        }
        // TODO: Use Arg::requires() in the clap builder API instead of clap::_derive which can't
        // describe args that require other args.
        (Some(_), None) | (None, Some(_)) => {
            bail!("Both --chain and --key should be passed or neither");
        }
        (None, None) => None,
    };

    info!("Listening on: {}", addr);

    while let Ok((stream, _)) = listener.accept().await {
        let mut server_clone = server.clone();

        let address = match stream.peer_addr() {
            Ok(address) => address,
            Err(err) => {
                warn!("Connected peer with no address: {}", err);
                continue;
            }
        };

        info!("Accepting connection from {}", address);

        if let Some(ref acceptor) = acceptor {
            let stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("Failed to accept TLS connection from {}: {}", address, err);
                    continue;
                }
            };
            task::spawn(async move { server_clone.accept_async(stream).await });
        } else {
            task::spawn(async move { server_clone.accept_async(stream).await });
        }
    }

    Ok(())
}
