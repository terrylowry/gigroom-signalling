use clap::Parser;
use tokio::task;

use anyhow::{anyhow, bail, Error, Result};
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use gigroom_signalling::parse_secrets;
use gigroom_signalling::server::{Server, ServerError};

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

#[derive(Parser, Debug)]
#[clap(about, version, author)]
/// Program arguments
struct Args {
    /// Address to listen on
    #[clap(long, default_value = "0.0.0.0")]
    addr: IpAddr,
    /// Port to listen on
    #[clap(short, long, default_value_t = 8443, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,

    /// TLS certificate chain to use
    #[clap(long)]
    chain: Option<String>,
    /// TLS certificate private key to use
    #[clap(long)]
    priv_key: Option<String>,
    /// TLS certificate password (optional)
    #[clap(long)]
    cert_password: Option<String>,
    /// If specified and different from @addr (specified above), we will accept TLS connections
    /// only on this address, and @addr will only accept insecure connections
    #[clap(long)]
    tls_addr: Option<IpAddr>,

    /// JSON file containing necessary secrets: jwt_key
    #[arg(long, value_name = "FILE")]
    secrets: PathBuf,
}

fn tls_config(
    chain: &str,
    key: &str,
) -> Result<ServerConfig, Error> {
    let certs = CertificateDer::pem_file_iter(chain)?
        .filter_map(|c| c.ok())
        .collect();
    let key = PrivateKeyDer::from_pem_file(key)?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|x| anyhow!("ServerConfig build failed: {:?}", x))
}

fn make_acceptor(
    chain: &str,
    key: &str,
) -> Result<Arc<TlsAcceptor>> {
    let mut acceptor = Arc::new(TlsAcceptor::from(Arc::new(tls_config(chain, key)?)));
    let acceptor_clone = acceptor.clone();
    let chain = chain.to_string();
    let key = key.to_string();

    // Monitor certs for rotation
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
    Ok(acceptor_clone)
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();
    let server = Server::new(parse_secrets(&args.secrets)?);

    let acceptor = match (args.chain, args.priv_key) {
        (Some(chain), Some(key)) => Some(make_acceptor(&chain, &key)?),
        // TODO: Use Arg::requires() in the clap builder API instead of clap::_derive which can't
        // describe args that require other args.
        (Some(_), None) | (None, Some(_)) => {
            bail!("Both --chain and --key should be passed or neither");
        }
        (None, None) => None,
    };

    // Create the event loop and TCP listener we'll accept connections on.
    let (tls_listener, insecure_listener) = match (&acceptor, args.tls_addr) {
        (Some(_), Some(tls_addr)) => (
            Some(TcpListener::bind((tls_addr, args.port)).await?),
            Some(TcpListener::bind((args.addr, args.port)).await?),
        ),
        (Some(_), None) => (Some(TcpListener::bind((args.addr, args.port)).await?), None),
        (None, None) => (None, Some(TcpListener::bind((args.addr, args.port)).await?)),
        (None, Some(_)) => {
            bail!("--tls-addr option needs --chain and --key");
        }
    };

    let server_clone = server.clone();
    let mut handles = vec![];
    if let Some(acceptor) = acceptor {
        let listener = tls_listener.unwrap();
        let handle = tokio::spawn(async move {
            info!("Listening with TLS on: {:?}", listener.local_addr());
            loop {
                let (stream, address) = match listener.accept().await {
                    Ok((s, a)) => (s, a),
                    Err(err) => {
                        warn!("Failed to accept TCP connection {:?}", err);
                        continue;
                    }
                };

                let mut server = server_clone.clone();
                let acceptor_clone = acceptor.clone();

                info!("Accepting TLS connection from {}", address);
                task::spawn(async move {
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        acceptor_clone.accept(stream),
                    )
                    .await
                    {
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
            }
        });
        handles.push(handle)
    }

    if let Some(listener) = insecure_listener {
        let handle = tokio::spawn(async move {
            info!("Listening without TLS on: {:?}", listener.local_addr());
            loop {
                let (stream, address) = match listener.accept().await {
                    Ok((s, a)) => (s, a),
                    Err(err) => {
                        warn!("Failed to accept TCP connection {:?}", err);
                        continue;
                    }
                };

                let mut server = server.clone();

                info!("Accepting connection from {}", address);
                task::spawn(async move { server.accept_async(stream).await });
            }
        });
        handles.push(handle)
    }

    for handle in handles {
        handle.await?;
    }

    Ok(())
}
