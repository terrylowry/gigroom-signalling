use clap::Parser;
use tokio::task;

use anyhow::{anyhow, bail, Error};
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::rustls::{Certificate, PrivateKey, ServerConfig};
use tokio_rustls::TlsAcceptor;

use gigroom_signalling::server::{Server, ServerError};

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

fn tls_config(
    chain: &str,
    key: &str,
) -> Result<ServerConfig, Error> {
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
    ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(
            certs.into_iter().map(Certificate).collect(),
            PrivateKey(private_key),
        )
        .map_err(|x| anyhow!("ServerConfig build failed: {:?}", x))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::init();

    let args = Args::parse();
    let server = Server::new();

    let addr = format!("{}:{}", args.host, args.port);

    let acceptor = match (args.chain, args.priv_key) {
        (Some(chain), Some(key)) => {
            let mut acceptor = Arc::new(TlsAcceptor::from(Arc::new(tls_config(&chain, &key)?)));
            let acceptor_clone = acceptor.clone();
            let chain = chain.clone();
            let key = key.clone();
            task::spawn(async move {
                // This should not fail, since tls_config() succeeded previously
                let mut chain_mtime = std::fs::metadata(&chain)
                    .unwrap_or_else(|_| panic!("stat failed: {}", chain))
                    .modified()
                    .unwrap_or_else(|_| panic!("mtime failed: {}", chain));
                let mut key_mtime = std::fs::metadata(&key)
                    .unwrap_or_else(|_| panic!("stat failed: {}", key))
                    .modified()
                    .unwrap_or_else(|_| panic!("mtime failed: {}", key));
                let mut interval = tokio::time::interval(Duration::from_secs(3600));
                let changed = |path: &str, last_mtime: &mut std::time::SystemTime| -> bool {
                    if let Ok(m) = std::fs::metadata(path) {
                        if let Ok(mtime) = m.modified() {
                            if mtime > *last_mtime {
                                *last_mtime = mtime;
                                return true;
                            }
                        }
                    }
                    false
                };
                loop {
                    interval.tick().await;
                    if changed(&chain, &mut chain_mtime) || changed(&key, &mut key_mtime) {
                        match tls_config(&chain, &key) {
                            Ok(config) => {
                                info!("New certificate, resetting TlsAcceptor");
                                let new_acceptor = TlsAcceptor::from(Arc::new(config));
                                (*Arc::make_mut(&mut acceptor)) = new_acceptor;
                            }
                            Err(error) => error!(
                                "Detected cert change, but failed to load certs: {:?}",
                                error
                            ),
                        }
                    }
                }
            });
            Some(acceptor_clone)
        }
        // TODO: Use Arg::requires() in the clap builder API instead of clap::_derive which can't
        // describe args that require other args.
        (Some(_), None) | (None, Some(_)) => {
            bail!("Both --chain and --key should be passed or neither");
        }
        (None, None) => None,
    };

    // Create the event loop and TCP listener we'll accept connections on.
    let listener = TcpListener::bind(&addr).await?;

    info!("Listening on: {}", addr);

    loop {
        let (stream, address) = match listener.accept().await {
            Ok((s, a)) => (s, a),
            Err(err) => {
                warn!("Failed to accept TCP connection {:?}", err);
                continue;
            }
        };

        let mut server = server.clone();

        if let Some(acceptor) = acceptor.clone() {
            info!("Accepting TLS connection from {}", address);
            task::spawn(async move {
                match tokio::time::timeout(Duration::from_secs(5), acceptor.accept(stream)).await {
                    Ok(Ok(stream)) => server.accept_async(stream).await,
                    Ok(Err(err)) => {
                        warn!("Failed to accept TLS connection: {:?}", err);
                        Err(ServerError::TLSHandshake(err))
                    }
                    Err(elapsed) => {
                        warn!("TLS connection timed out {} after {}", address, elapsed);
                        Err(ServerError::TLSHandshakeTimeout(elapsed))
                    }
                }
            });
        } else {
            info!("Accepting connection from {}", address);
            task::spawn(async move { server.accept_async(stream).await });
        }
    }
}
