use anyhow::{anyhow, Error, Result};
use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use futures::stream::SplitSink;
use jsonwebtoken as jwt;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

mod handler;
use handler::Handler;

type UserID = String;
type ClientID = Uuid;
type ClientVersion = String;
#[derive(Eq, Hash, PartialEq, PartialOrd, Clone, Debug)]
struct ClientInfo {
    version: ClientVersion,
}

#[derive(Debug)]
pub struct Client {
    _conn_handle: task::JoinHandle<Result<(), Error>>,
    _cmdloop_handle: task::JoinHandle<Result<(), Error>>,
    tx: mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct Clients {
    // Connected but not identified yet
    connected: HashMap<ClientID, Client>,
    // Connected and identified
    identified: HashMap<ClientID, (UserID, Client)>,
    // All connected clients identified as UserID, reverse mapping of the above for convenience
    user_clients: HashMap<UserID, HashMap<ClientID, ClientInfo>>,
}

type RoomID = Uuid;
#[derive(Debug)]
pub struct Room {
    creator: UserID,
    name: String,
    allowed_users: HashSet<UserID>,
    current_clients: HashSet<ClientID>,
}

#[derive(Clone, Debug)]
pub struct State {
    clients: Arc<Mutex<Clients>>,
    rooms: Arc<Mutex<HashMap<RoomID, Room>>>,
}

#[derive(Clone, Debug)]
pub struct Server {
    state: State,
    jwt_key: String,
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("error during TLS handshake {0}")]
    TLSHandshake(#[from] std::io::Error),
    #[error("timeout during TLS handshake {0}")]
    TLSHandshakeTimeout(#[from] tokio::time::error::Elapsed),
    #[error("error during WS handshake {0}")]
    WSHandshake(#[from] tokio_tungstenite::tungstenite::Error),
}

#[derive(Clone, Debug)]
pub enum WsMessage {
    Outgoing(String),
    Incoming(Message),
}

#[derive(Clone, Debug, Deserialize)]
struct IdentifyPayload {
    client_version: ClientVersion,
    token: String,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct TokenClaims {
    username: String,
    email: String,
    exp: u64,
}

impl Server {
    pub fn new(jwt_key: String) -> Self {
        let state = State {
            clients: Arc::new(Mutex::new(Clients {
                connected: HashMap::new(),
                identified: HashMap::new(),
                user_clients: HashMap::new(),
            })),
            rooms: Arc::new(Mutex::new(HashMap::new())),
        };

        Self { state, jwt_key }
    }

    fn parse_token(
        &self,
        token: &str,
    ) -> Result<TokenClaims> {
        let token_data = jwt::decode::<TokenClaims>(
            &token,
            &jwt::DecodingKey::from_secret(self.jwt_key.as_bytes()),
            &jwt::Validation::new(jwt::Algorithm::HS256),
        )?;

        if token_data.claims.exp < jwt::get_current_timestamp() {
            error!("auth token from {} expired", token_data.claims.username);
            return Err(anyhow!("auth token expired"));
        }

        Ok(token_data.claims)
    }

    // TODO: This should do authentication using a token that is verified against the auth server.
    async fn identify_client<S: 'static>(
        &self,
        msg: &str,
        state: &State,
        client_id: Uuid,
        ws_sender: &mut SplitSink<WebSocketStream<S>, Message>,
    ) -> Option<UserID>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        // If the peer does not successfully identify, we will drop the connection
        let client = { state.clients.lock().unwrap().connected.remove(&client_id) };
        let Some(client) = client else {
            error!("Invalid client state {:?}, disconnecting", client_id);
            return None;
        };

        let Some(("IDENTIFY", ident)) = msg.split_once(' ') else {
            error!("No identification, disconnect");
            return None;
        };

        let (got_peer_id, client_version) = match serde_json::from_str::<IdentifyPayload>(ident) {
            Ok(payload) => {
                let Ok(claims) = self.parse_token(&payload.token) else {
                    error!("Invalid auth token, disconnect");
                    return None;
                };
                (claims.username.to_string(), payload.client_version)
            }
            Err(_) => {
                if !ident.is_ascii() || !ident.chars().all(char::is_alphanumeric) {
                    error!("Invalid legacy peer_id: {}, disconnect", ident);
                    return None;
                }
                // TODO: Eliminate this codepath when we have a new "known-good" tagged release of
                // GigRoom. It's a security vulnerability.
                warn!(
                    "Legacy client connected, assuming payload `{}` is peer_id",
                    ident
                );
                (ident.to_string(), "0.2.0".to_string())
            }
        };

        {
            let mut clients = state.clients.lock().unwrap();

            clients
                .identified
                .insert(client_id, (got_peer_id.clone(), client));

            let client_info = ClientInfo {
                version: client_version,
            };
            if let Some(client_ids) = clients.user_clients.get_mut(&got_peer_id) {
                client_ids.insert(client_id, client_info);
            } else {
                clients.user_clients.insert(
                    got_peer_id.clone(),
                    HashMap::from([(client_id, client_info)]),
                );
            }
        }

        ws_sender
            .send(Message::Text(format!("IDENTIFIED {}", client_id)))
            .await
            .ok()?;
        Some(got_peer_id)
    }

    async fn remove_client<S: 'static>(
        state: State,
        mut ws_sender: SplitSink<WebSocketStream<S>, Message>,
        client_id: Uuid,
    ) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        {
            let mut clients = state.clients.lock().unwrap();
            // XXX: Do we need to explicitly abort the JoinHandles?
            info!("Removing from connected (but not identified) clients list");
            clients.connected.remove(&client_id);
            info!("Removing from identified clients list");
            if let Some((user_id, _)) = clients.identified.remove(&client_id) {
                if let Some(client_ids) = clients.user_clients.get_mut(&user_id) {
                    client_ids.remove(&client_id);
                    if client_ids.is_empty() {
                        clients.user_clients.remove(&user_id);
                    }
                }
            }
        }

        info!("Closing websocket client connection");
        ws_sender.send(Message::Close(None)).await?;
        info!("Closing websocket connection");
        ws_sender.close().await?;

        Ok::<(), Error>(())
    }

    pub async fn accept_async<S: 'static>(
        &mut self,
        stream: S,
    ) -> Result<(), ServerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let ws_conn = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(err) => {
                warn!("Error during the websocket handshake: {}", err);
                return Err(ServerError::WSHandshake(err));
            }
        };

        let client_id = Uuid::new_v4();
        let client_id_clone = client_id;

        let (tx, tx_src) = mpsc::channel::<String>(1000);
        let (mut rx_sink, rx) = mpsc::channel::<String>(1000);
        let (ident_sink, ident_src) = oneshot::channel::<(Uuid, String)>();
        let _cmdloop_handle = task::spawn(Handler::cmd_loop(
            rx,
            tx.clone(),
            ident_src,
            self.state.clone(),
        ));

        let state_clone = self.state.clone();
        let self_clone = self.clone();
        let _conn_handle = task::spawn(async move {
            let (mut ws_sender, ws_receiver) = ws_conn.split();
            let mut incoming = Box::pin(ws_receiver.map(|msg| msg.map(WsMessage::Incoming)));
            let identified = match incoming.next().await {
                Some(Ok(WsMessage::Incoming(Message::Text(msg)))) => {
                    let user_id = self_clone
                        .identify_client(&msg, &state_clone, client_id, &mut ws_sender)
                        .await;
                    if let Some(user_id) = user_id {
                        if let Err(_) = ident_sink.send((client_id, user_id.clone())) {
                            error!("Failed to complete ident: cmd_loop already dropped!");
                        }
                        true
                    } else {
                        false
                    }
                }
                _ => {
                    error!("Identification failed");
                    false
                }
            };
            if !identified {
                return Self::remove_client(state_clone, ws_sender, client_id).await;
            }

            let outgoing = Box::pin(tx_src.map(|msg| Ok(WsMessage::Outgoing(msg))));
            let s = vec![incoming.boxed(), outgoing.boxed()];
            let mut streams = futures::stream::select_all(s);
            loop {
                match streams.next().await {
                    Some(Ok(WsMessage::Outgoing(msg))) => {
                        ws_sender.send(Message::Text(msg)).await?;
                    }
                    Some(Ok(WsMessage::Incoming(Message::Text(msg)))) => {
                        info!("Received {}", msg);
                        rx_sink.send(msg.clone()).await?;
                    }
                    Some(Ok(WsMessage::Incoming(Message::Close(reason)))) => {
                        info!("connection closed: {:?}", reason);
                        break;
                    }
                    Some(Ok(WsMessage::Incoming(Message::Ping(payload)))) => {
                        info!("Received ping");
                        ws_sender.send(Message::Pong(payload)).await?;
                    }
                    Some(Ok(WsMessage::Incoming(Message::Pong(_)))) => {
                        trace!("Ignoring incoming pong, we do not send pings");
                    }
                    Some(Ok(WsMessage::Incoming(msg))) => {
                        warn!("Unsupported incoming message: {:?}", msg);
                    }
                    Some(Err(error)) => {
                        warn!("Connection error: {:?}", error);
                        break;
                    }
                    None => {
                        info!("Task destroyed, exiting loop");
                        break;
                    }
                }
            }

            Self::remove_client(state_clone, ws_sender, client_id).await
        });

        self.state.clients.lock().unwrap().connected.insert(
            client_id_clone,
            Client {
                _conn_handle,
                _cmdloop_handle,
                tx,
            },
        );

        Ok(())
    }
}
