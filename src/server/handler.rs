use anyhow::Error;
use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use http::status::StatusCode as HttpCode;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use crate::server::{Peers, Room, State};

type Requests = Vec<HashMap<String, Value>>;
// List of requests and the peer channel to send to
type RequestList = Vec<(String, mpsc::Sender<String>)>;
type Responses = Vec<Value>;

pub struct Handler {}

impl Handler {
    pub async fn cmd_loop(
        mut rx: mpsc::Receiver<String>,
        mut tx: mpsc::Sender<String>,
        ident: oneshot::Receiver<String>,
        mut server_state: State,
    ) -> Result<(), Error> {
        let peer_id = ident.await?;
        while let Some(msg) = rx.next().await {
            let v: Result<Requests, serde_json::Error> = serde_json::from_str(&msg);
            let rsp = match v {
                Ok(v) => {
                    debug!("{}", format!("{server_state:#?}"));
                    {
                        let rooms = server_state.rooms.lock().unwrap();
                        debug!("{}", format!("{rooms:#?}"));
                    }
                    Self::handle_request(v, &peer_id, &mut server_state).await
                }
                Err(_) => {
                    Self::all_responses(&[Self::error_response(None, None, None)]).to_string()
                }
            };
            tx.send(rsp).await?;
        }
        info!("Exiting cmd_loop for {}", peer_id);
        info!("{}", format!("{server_state:#?}"));
        {
            let rooms = server_state.rooms.lock().unwrap();
            info!("{}", format!("{rooms:#?}"));
        }
        // Notify all rooms that this peer has left. Do not destroy rooms that this user created
        // because it will disrupt the call, and they can always rejoin.
        let requests = {
            let mut destroy_rooms = vec![];
            let mut remove_from_rooms = vec![];

            let rooms = server_state.rooms.lock().unwrap();
            for (room_id, room) in rooms.iter() {
                if room.current_members.contains(&peer_id) {
                    remove_from_rooms.push(*room_id);
                }
                if room.creator == peer_id {
                    destroy_rooms.push(*room_id);
                }
            }
            drop(rooms);

            let mut requests = vec![];
            let mut rooms = server_state.rooms.lock().unwrap();
            for room_id in remove_from_rooms.iter() {
                requests.extend(Self::room_remove_member(
                    peer_id.as_str(),
                    room_id,
                    &mut rooms,
                    &server_state.peers,
                ));
            }
            for room_id in destroy_rooms.iter() {
                requests.extend(Self::room_destroy(room_id, &mut rooms, &server_state.peers));
            }
            requests
        };
        Self::send_requests(requests).await;
        Ok(())
    }

    fn make_request(
        room_id: Option<&Uuid>,
        args: &[Value],
    ) -> Value {
        match room_id {
            Some(room_id) => {
                json!([{
                    "type": "request",
                    "room_id": room_id,
                    "args": args,
                    // This is unused right now, but we might need this for requests that need
                    // a response from the client (none right now).
                    "request_id": "",
                }])
            }
            None => {
                json!([{
                    "type": "request",
                    "args": args,
                    "request_id": "",
                }])
            }
        }
    }

    fn make_response(
        code: HttpCode,
        args: Option<Vec<Value>>,
        request_id: Option<&str>,
    ) -> Value {
        json!({
            "type": "response",
            "status_code": code.as_u16(),
            "args": args.unwrap_or(vec![]),
            "request_id": request_id.unwrap_or(""),
        })
    }

    fn error_response(
        request_id: Option<&str>,
        error_code: Option<HttpCode>,
        error_str: Option<&str>,
    ) -> Value {
        let (arg, code) = match (error_str, error_code) {
            (Some(s), Some(code)) => (s.to_string(), code),
            (Some(s), None) => (s.to_string(), HttpCode::BAD_REQUEST),
            (None, Some(code)) => {
                let reason = match code.canonical_reason() {
                    Some(reason) => reason.to_string(),
                    None => {
                        error!("Somehow got an unknown code: {:?}", code);
                        "Bad request".to_string()
                    }
                };
                (reason, code)
            }
            (None, None) => {
                let reason = HttpCode::BAD_REQUEST
                    .canonical_reason()
                    .unwrap()
                    .to_string();
                (reason, HttpCode::BAD_REQUEST)
            }
        };
        Self::make_response(code, Some(vec![serde_json::Value::String(arg)]), request_id)
    }

    fn all_responses(responses: &[Value]) -> Value {
        json!(responses)
    }

    async fn send_requests(mut requests: RequestList) {
        let mut futs = vec![];
        // Inform all other participants of the departure
        for (req, tx) in requests.iter_mut() {
            futs.push(tx.send(req.clone()));
        }
        futures::future::join_all(futs).await;
    }

    // Build a vector of requests that will be sent to the iterator of member IDs
    fn members_request_builder<'a, I>(
        members: I,
        room_id: &Uuid,
        args: &[Value],
        peers: &Arc<Mutex<Peers>>,
    ) -> RequestList
    where
        I: Iterator<Item = &'a String>,
    {
        members
            .filter_map(|id| {
                let peers = peers.lock().unwrap();
                let maybe_peer = peers.identified.get(id.as_str());
                if let Some(peer) = maybe_peer {
                    let msg = Self::make_request(Some(room_id), args).to_string();
                    Some((msg, peer.tx.clone()))
                } else {
                    None
                }
            })
            .collect::<RequestList>()
    }

    fn room_members_request_builder(
        room: &Room,
        room_id: &Uuid,
        args: &[Value],
        peers: &Arc<Mutex<Peers>>,
    ) -> RequestList {
        Self::members_request_builder(room.current_members.iter(), room_id, args, peers)
    }

    fn room_destroy(
        room_id: &Uuid,
        rooms: &mut std::sync::MutexGuard<'_, HashMap<Uuid, Room>>,
        peers: &Arc<Mutex<Peers>>,
    ) -> RequestList {
        match rooms.remove(room_id) {
            Some(room) => {
                Self::members_request_builder(
                    // Send the message to both current and allowed members
                    room.current_members
                        .iter()
                        .chain(room.allowed_members.iter()),
                    room_id,
                    &[
                        Value::String("room".to_string()),
                        Value::String("destroyed".to_string()),
                    ],
                    peers,
                )
            }
            None => {
                debug!("Room already destroyed");
                vec![]
            }
        }
    }

    fn room_add_member(
        member: &str,
        room_id: &Uuid,
        room: &mut Room,
        peers: &Arc<Mutex<Peers>>,
    ) -> Option<(RequestList, Vec<Value>)> {
        debug_assert!(room.allowed_members.contains(member));
        let (ret, args) = if !room.current_members.is_empty() {
            let ret = Self::room_members_request_builder(
                room,
                room_id,
                &[
                    Value::String("room".to_string()),
                    Value::String("joined".to_string()),
                    Value::String(member.to_string()),
                ],
                peers,
            );
            let other_members = room
                .current_members
                .iter()
                .map(|s| Value::String(s.to_string()))
                .collect();
            (ret, other_members)
        } else {
            // There were no members in the room, which means the room went from inactive to active
            // and we need to notify allowed members
            let ret = Self::allowed_members_craft_active_messages(room_id, room, peers);
            (ret, vec![])
        };
        // Add peer to the room
        room.current_members.insert(member.to_string());
        Some((ret, args))
    }

    fn room_remove_member(
        member: &str,
        room_id: &Uuid,
        rooms: &mut std::sync::MutexGuard<'_, HashMap<Uuid, Room>>,
        peers: &Arc<Mutex<Peers>>,
    ) -> RequestList {
        let room = rooms.get_mut(room_id).unwrap();
        room.allowed_members.remove(member);
        if room.current_members.remove(member) {
            if room.current_members.is_empty() {
                Self::room_destroy(room_id, rooms, peers)
            } else {
                Self::room_members_request_builder(
                    room,
                    room_id,
                    &[
                        Value::String("room".to_string()),
                        Value::String("left".to_string()),
                        Value::String(member.to_string()),
                    ],
                    peers,
                )
            }
        } else {
            vec![]
        }
    }

    fn room_craft_messages(
        from_id: &str,
        room_id: &Uuid,
        room: &Room,
        msg: &Value,
        peers: &Arc<Mutex<Peers>>,
    ) -> RequestList {
        // Collect a list of room members that need to be notified that this peer has been kicked
        // out of the room
        Self::members_request_builder(
            room.current_members.iter().filter(|id| *id == from_id),
            room_id,
            &[
                Value::String("room".to_string()),
                Value::String("message".to_string()),
                msg.clone(),
                Value::String(from_id.to_string()),
            ],
            peers,
        )
    }

    fn allowed_members_craft_active_messages(
        room_id: &Uuid,
        room: &Room,
        peers: &Arc<Mutex<Peers>>,
    ) -> RequestList {
        Self::members_request_builder(
            room.allowed_members.iter(),
            room_id,
            &[
                Value::String("room".to_string()),
                Value::String("active".to_string()),
            ],
            peers,
        )
    }

    async fn handle_room<'a, I>(
        mut args: I,
        room_id: Option<&str>,
        request_id: &str,
        peer_id: &str,
        server_state: State,
    ) -> Value
    where
        I: Iterator<Item = &'a Value>,
    {
        let arg1 = args.next().and_then(|v| v.as_str());
        let room_id = match room_id {
            Some(room_id) => {
                if let Ok(room_id) = Uuid::try_parse(room_id) {
                    room_id
                } else {
                    let args = vec![Value::String("Bad room_id, must be UUID4".to_string())];
                    return Self::make_response(
                        HttpCode::BAD_REQUEST,
                        Some(args),
                        Some(request_id),
                    );
                }
            }
            None => {
                let args = match arg1 {
                    Some("list") => {
                        let rooms = server_state.rooms.lock().unwrap();
                        let args: Vec<Value> = rooms
                            .iter()
                            .filter(|(_, room)| room.allowed_members.contains(peer_id))
                            .map(|(id, room)| {
                                let mut v = serde_json::Map::new();
                                v.insert("room_id".to_string(), Value::String(id.to_string()));
                                v.insert(
                                    "room_name".to_string(),
                                    Value::String(room.name.to_string()),
                                );
                                v.insert(
                                    "creator".to_string(),
                                    Value::String(room.creator.clone()),
                                );
                                let active = !room.current_members.is_empty();
                                v.insert("active".to_string(), Value::Bool(active));
                                Value::Object(v)
                            })
                            .collect();
                        args
                    }
                    Some(_) => {
                        let args = vec![Value::String("room_id not specified".to_string())];
                        return Self::make_response(
                            HttpCode::BAD_REQUEST,
                            Some(args),
                            Some(request_id),
                        );
                    }
                    None => {
                        return Self::make_response(HttpCode::BAD_REQUEST, None, Some(request_id));
                    }
                };
                return Self::make_response(HttpCode::OK, Some(args), Some(request_id));
            }
        };
        let response_args = match arg1 {
            Some("create") => {
                debug!("{}", format!("{server_state:#?}"));
                let mut rooms = server_state.rooms.lock().unwrap();
                if rooms.contains_key(&room_id) {
                    Err((HttpCode::CONFLICT, "Room ID already in use"))
                } else {
                    debug!("{}", format!("{rooms:#?}"));
                    if let Some(name) = args.next().and_then(|v| v.as_str()) {
                        if rooms
                            .values()
                            .any(|room| room.creator == peer_id && room.name == name)
                        {
                            Err((HttpCode::CONFLICT, "Room already exists"))
                        } else {
                            rooms.insert(
                                room_id,
                                Room {
                                    creator: peer_id.to_string(),
                                    name: name.to_string(),
                                    allowed_members: HashSet::from([peer_id.to_string()]),
                                    current_members: HashSet::new(),
                                },
                            );
                            Ok(None)
                        }
                    } else {
                        Err((HttpCode::BAD_REQUEST, "Room name missing"))
                    }
                }
            }
            Some("destroy") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    match rooms.get_mut(&room_id) {
                        Some(room) => {
                            if room.creator == peer_id {
                                Ok(Self::room_destroy(
                                    &room_id,
                                    &mut rooms,
                                    &server_state.peers,
                                ))
                            } else {
                                Err((HttpCode::FORBIDDEN, "Forbidden"))
                            }
                        }
                        None => Err((HttpCode::NOT_FOUND, "No such room")),
                    }
                };
                match requests {
                    Ok(reqs) => {
                        Self::send_requests(reqs).await;
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            }
            Some("edit") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    match rooms.get_mut(&room_id) {
                        Some(room) => {
                            if room.creator == peer_id {
                                match args.next().and_then(|v| v.as_str()) {
                                    Some("allow") => {
                                        for member in args.filter_map(|v| v.as_str()) {
                                            // Trust that the creator knows that these PeerID
                                            // values are valid
                                            if !room.allowed_members.contains(member) {
                                                room.allowed_members.insert(member.to_string());
                                            }
                                        }
                                        Ok(None)
                                    }
                                    Some("disallow") => Ok(Some(
                                        args.filter_map(|arg| arg.as_str())
                                            .flat_map(|arg| {
                                                Self::room_remove_member(
                                                    arg,
                                                    &room_id,
                                                    &mut rooms,
                                                    &server_state.peers,
                                                )
                                            })
                                            .collect::<RequestList>(),
                                    )),
                                    _ => Err((HttpCode::BAD_REQUEST, "Unknown room edit argument")),
                                }
                            } else {
                                Err((HttpCode::FORBIDDEN, "Not allowed to edit room"))
                            }
                        }
                        None => Err((HttpCode::NOT_FOUND, "No such room")),
                    }
                };
                match requests {
                    Ok(Some(reqs)) => {
                        Self::send_requests(reqs).await;
                        Ok(None)
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            }
            Some("join") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    match rooms.get_mut(&room_id) {
                        Some(room) => {
                            if room.allowed_members.contains(peer_id) {
                                info!("Joining room {}", room_id);
                                Ok(Self::room_add_member(
                                    peer_id,
                                    &room_id,
                                    room,
                                    &server_state.peers,
                                ))
                            } else {
                                Err((HttpCode::FORBIDDEN, "Not allowed to join room"))
                            }
                        }
                        None => Err((HttpCode::NOT_FOUND, "No such room")),
                    }
                };
                match requests {
                    Ok(Some((reqs, response))) => {
                        Self::send_requests(reqs).await;
                        Ok(Some(response))
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            }
            Some("leave") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    if !rooms.contains_key(&room_id) {
                        Err((HttpCode::NOT_FOUND, "No such room"))
                    } else {
                        let room = rooms.get_mut(&room_id).unwrap();
                        if !room.current_members.contains(peer_id) {
                            Err((HttpCode::BAD_REQUEST, "Not a member of room"))
                        } else {
                            Ok(Self::room_remove_member(
                                peer_id,
                                &room_id,
                                &mut rooms,
                                &server_state.peers,
                            ))
                        }
                    }
                };
                match requests {
                    Ok(reqs) => {
                        Self::send_requests(reqs).await;
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            }
            // ["message", <message>, ["peer_id1", ...]]
            Some("message") => {
                let msg = args.next(); // Could be any type, but probably just an object (dict)
                let to = args.next().and_then(|v| v.as_array());
                let messages = match (msg, to) {
                    (Some(msg), Some(to)) => {
                        info!("Sending message {:?} to {:?}", msg, to);
                        let mut rooms = server_state.rooms.lock().unwrap();
                        match rooms.get_mut(&room_id) {
                            Some(room) => {
                                if room.current_members.contains(peer_id) {
                                    Ok(Some(Self::room_craft_messages(
                                        peer_id,
                                        &room_id,
                                        room,
                                        msg,
                                        &server_state.peers,
                                    )))
                                } else {
                                    Err((HttpCode::BAD_REQUEST, "Not a member of room"))
                                }
                            }
                            None => Err((HttpCode::NOT_FOUND, "No such room")),
                        }
                    }
                    _ => Err((HttpCode::BAD_REQUEST, "Bad request")),
                };
                match messages {
                    Ok(Some(msgs)) => {
                        Self::send_requests(msgs).await;
                        Ok(None)
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            }
            _ => Err((HttpCode::BAD_REQUEST, "Invalid room argument")),
        };
        debug!("{}", format!("{server_state:#?}"));
        {
            let rooms = server_state.rooms.lock().unwrap();
            debug!("{}", format!("{rooms:#?}"));
        }
        let code = response_args
            .clone()
            .err()
            .unzip()
            .0
            .unwrap_or(HttpCode::OK);
        let args = response_args.map_or_else(
            |(error, _)| Some(vec![Value::String(error.to_string())]),
            |v| v,
        );
        Self::make_response(code, args, Some(request_id))
    }

    async fn handle_request(
        reqs: Requests,
        peer_id: &str,
        server_state: &mut State,
    ) -> String {
        let responses: Responses = futures::future::join_all(reqs.into_iter().map(|req| {
            let state = server_state.clone();
            async move {
                let m = (
                    req.get("type").and_then(|s| s.as_str()),
                    req.get("request_id").and_then(|s| s.as_str()),
                    req.get("args").and_then(|v| v.as_array()),
                );
                info!("{:?}", req);
                match m {
                    (Some("request"), Some(request_id), Some(args)) => {
                        let room_id = req.get("room_id").and_then(|s| s.as_str());
                        if args.is_empty() {
                            Self::error_response(
                                Some(request_id),
                                Some(HttpCode::BAD_REQUEST),
                                Some("Empty args"),
                            )
                        } else {
                            let mut args = args.iter();
                            match args.next().and_then(|v| v.as_str()) {
                                Some("room") => {
                                    Self::handle_room(args, room_id, request_id, peer_id,
                                        state).await
                                },
                                Some(_) | None => {
                                    Self::error_response(Some(request_id),
                                        Some(HttpCode::BAD_REQUEST), Some("Incorrect args"))
                                },
                            }
                        }
                    }
                    (type_, request_id, args) => {
                        error!("{:?} {:?}", type_, args);
                        Self::error_response(request_id, Some(HttpCode::BAD_REQUEST), None)
                    }
                }
            }
        }))
        .await;

        Self::all_responses(responses.as_slice()).to_string()
    }
}
