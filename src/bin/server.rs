use clap::Parser;
use tokio::io::AsyncReadExt;
use tokio::task;

use anyhow::Error;
use tokio::fs;
use tokio::net::TcpListener;
use tokio_native_tls::native_tls::TlsAcceptor;

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
            let mut chain_file = fs::File::open(chain).await?;
            let mut chain_slice = Vec::new();
            chain_file.read_to_end(&mut chain_slice).await?;
            let mut key_file = fs::File::open(key).await?;
            let mut key_slice = Vec::new();
            key_file.read_to_end(&mut key_slice).await?;
            let identity = tokio_native_tls::native_tls::Identity::from_pkcs8(
                &chain_slice,
                &key_slice,
            )
            .unwrap();
            Some(tokio_native_tls::TlsAcceptor::from(
                TlsAcceptor::new(identity).unwrap(),
            ))
        }
        // TODO: Use Arg::requires() in the clap builder API instead of clap::_derive which can't
        // describe args that require other args.
        (Some(_), None) | (None, Some(_)) => {
            error!("Both --chain and --key should be passed or neither");
            std::process::exit(1);
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
