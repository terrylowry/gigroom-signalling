use anyhow::Error;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use futures::channel::{mpsc, oneshot};
use futures::stream::SplitSink;
use futures::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task;
use uuid::Uuid;

#[allow(unused_imports)]
use log::{error, warn, info, debug, trace};

mod handler;
use handler::Handler;

type PeerID = String;
#[derive(Debug)]
pub struct Peer {
    _conn_handle: task::JoinHandle<Result<(), Error>>,
    _cmdloop_handle: task::JoinHandle<Result<(), Error>>,
    tx: mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct Peers {
    connected: HashMap<Uuid, Peer>,
    identified: HashMap<PeerID, Peer>,
}

type RoomID = Uuid;
#[derive(Debug)]
pub struct Room {
    creator: PeerID,
    name: String,
    allowed_members: HashSet<PeerID>,
    current_members: HashSet<PeerID>,
}

#[derive(Clone,Debug)]
pub struct State {
    peers: Arc<Mutex<Peers>>,
    rooms: Arc<Mutex<HashMap<RoomID, Room>>>,
}

#[derive(Clone,Debug)]
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
    pub fn new() -> Self
    {
        let state = State {
            peers: Arc::new(Mutex::new(Peers {
                connected: HashMap::new(),
                identified: HashMap::new(),
            })),
            rooms: Arc::new(Mutex::new(HashMap::new())),
        };

        Self { state }
    }

    // TODO: This should do authentication using a token that is verified against the auth server.
    async fn identify_peer<S: 'static>(
        msg: &String,
        state: &State,
        temp_id: &Uuid,
        ws_sender: &mut SplitSink<WebSocketStream<S>, Message>,
    ) -> Option<PeerID>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        // If the peer does not successfully identify, we will drop the connection
        let peer = {
            state.peers.lock().unwrap().connected.remove(&temp_id)
        };
        match peer {
            Some(peer) => match msg.split_once(" ") {
                // XXX: Add peer version reporting here
                Some(("IDENTIFY", got_peer_id)) => {
                    {
                        let mut peers = state.peers.lock().unwrap();
                        if peers.identified.contains_key(got_peer_id) {
                            error!("Server already has a peer named {}", got_peer_id);
                            // Drop connection
                            // TODO: Once we add authorization, we should kick the previous user on
                            // identify
                            return None;
                        }
                        peers.identified.insert(
                            got_peer_id.to_string(),
                            peer,
                        );
                    }
                    ws_sender.send(Message::Text("IDENTIFIED".to_string())).await.ok()?;
                    Some(got_peer_id.to_string())
                }
                _ => {
                    error!("No identification, disconnect");
                    None
                }
            }
            None => {
                error!("Invalid peer state {:?}, disconnecting", temp_id);
                None
            }
        }
    }

    async fn remove_peer<S: 'static>(
        state: State,
        mut ws_sender: SplitSink<WebSocketStream<S>, Message>,
        temp_id: Uuid,
        peer_id: Option<String>,
    ) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        // XXX: Do we need to explicitly abort the JoinHandles?
        if let Some(peer_id) = peer_id {
            info!("Removing from identified peers list");
            state.peers.lock().unwrap().identified.remove(&peer_id);
        } else {
            info!("Removing from connected peers list");
            state.peers.lock().unwrap().connected.remove(&temp_id);
        }

        info!("Closing websocket client connection");
        ws_sender.send(Message::Close(None)).await?;
        info!("Closing websocket connection");
        ws_sender.close().await?;

        Ok::<(), Error>(())
    }

    pub async fn accept_async<S: 'static>(
        &mut self,
        stream: S
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

        let temp_id = Uuid::new_v4();
        let temp_id_clone = temp_id.clone();

        let (tx, tx_src) = mpsc::channel::<String>(1000);
        let (mut rx_sink, rx) = mpsc::channel::<String>(1000);
        let (ident_sink, ident_src) = oneshot::channel::<String>();
        let _cmdloop_handle = task::spawn(Handler::cmd_loop(rx, tx.clone(), ident_src,
                                                           self.state.clone()));

        let state_clone = self.state.clone();
        let _conn_handle = task::spawn(async move {
            let mut peer_id = None;
            let (mut ws_sender, ws_receiver) = ws_conn.split();
            let mut incoming = Box::pin(ws_receiver.map(|msg| msg.map(|m| WsMessage::Incoming(m))));
            let identified = match incoming.next().await {
                Some(Ok(WsMessage::Incoming(Message::Text(msg)))) => {
                    let id = Self::identify_peer(&msg, &state_clone, &temp_id,
                                                 &mut ws_sender).await;
                    if let Some(id) = id {
                        match ident_sink.send(id.clone()) {
                            Ok(()) => {}
                            Err(_) => {
                                error!("Failed to complete ident: cmd_loop already dropped!");
                            }
                        }
                        peer_id.replace(id);
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
                return Self::remove_peer(state_clone, ws_sender, temp_id, peer_id).await;
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

            Self::remove_peer(state_clone, ws_sender, temp_id, peer_id).await
        });

        self.state.peers.lock().unwrap().connected.insert(
            temp_id_clone,
            Peer {
                _conn_handle,
                _cmdloop_handle,
                tx,
            },
        );

        Ok(())
    }
}
