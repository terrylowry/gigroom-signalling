use clap::Parser;
use tokio::io::AsyncReadExt;
use tokio::task;

use anyhow::Error;
use tokio::fs;
use tokio::net::TcpListener;
use tokio_native_tls::native_tls::TlsAcceptor;

use gigroom_signalling::server::Server;

#[allow(unused_imports)]
use log::{error, warn, info, debug, trace};

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
    /// TLS certificate to use
    #[clap(short, long)]
    cert: Option<String>,
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

    let acceptor = match args.cert {
        Some(cert) => {
            let mut file = fs::File::open(cert).await?;
            let mut identity = vec![];
            file.read_to_end(&mut identity).await?;
            let identity = tokio_native_tls::native_tls::Identity::from_pkcs12(
                &identity,
                args.cert_password.as_deref().unwrap_or(""),
            )
            .unwrap();
            Some(tokio_native_tls::TlsAcceptor::from(
                TlsAcceptor::new(identity).unwrap(),
            ))
        }
        None => None,
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
