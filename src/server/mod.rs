use anyhow::Error;
use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use futures::stream::SplitSink;
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
    user_clients: HashMap<UserID, HashSet<ClientID>>,
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
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("error during handshake {0}")]
    Handshake(#[from] tokio_tungstenite::tungstenite::Error),
}

#[derive(Clone, Debug)]
pub enum WsMessage {
    Outgoing(String),
    Incoming(Message),
}

impl Server {
    pub fn new() -> Self {
        let state = State {
            clients: Arc::new(Mutex::new(Clients {
                connected: HashMap::new(),
                identified: HashMap::new(),
                user_clients: HashMap::new(),
            })),
            rooms: Arc::new(Mutex::new(HashMap::new())),
        };

        Self { state }
    }

    // TODO: This should do authentication using a token that is verified against the auth server.
    async fn identify_client<S: 'static>(
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
        match client {
            Some(client) => match msg.split_once(' ') {
                // XXX: Add peer version reporting here
                Some(("IDENTIFY", got_peer_id)) => {
                    {
                        let mut clients = state.clients.lock().unwrap();
                        clients
                            .identified
                            .insert(client_id, (got_peer_id.to_string(), client));
                        match clients.user_clients.get_mut(got_peer_id) {
                            Some(client_ids) => {
                                client_ids.insert(client_id);
                            }
                            None => {
                                clients
                                    .user_clients
                                    .insert(got_peer_id.to_string(), HashSet::from([client_id]));
                            }
                        };
                    }
                    ws_sender
                        .send(Message::Text(format!("IDENTIFIED {}", client_id)))
                        .await
                        .ok()?;
                    Some(got_peer_id.to_string())
                }
                _ => {
                    error!("No identification, disconnect");
                    None
                }
            },
            None => {
                error!("Invalid client state {:?}, disconnecting", client_id);
                None
            }
        }
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
                return Err(ServerError::Handshake(err));
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
        let _conn_handle = task::spawn(async move {
            let (mut ws_sender, ws_receiver) = ws_conn.split();
            let mut incoming = Box::pin(ws_receiver.map(|msg| msg.map(WsMessage::Incoming)));
            let identified = match incoming.next().await {
                Some(Ok(WsMessage::Incoming(Message::Text(msg)))) => {
                    let user_id =
                        Self::identify_client(&msg, &state_clone, client_id, &mut ws_sender).await;
                    if let Some(user_id) = user_id {
                        match ident_sink.send((client_id, user_id.clone())) {
                            Ok(()) => {}
                            Err(_) => {
                                error!("Failed to complete ident: cmd_loop already dropped!");
                            }
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

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}
