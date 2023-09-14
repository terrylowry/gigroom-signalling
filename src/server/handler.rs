use anyhow::Error;
use http::status::StatusCode as HttpCode;
use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[allow(unused_imports)]
use log::{error, warn, info, debug, trace};

use crate::server::{State, Room};

type Requests = Vec<HashMap<String, Value>>;
type Responses = Vec<Value>;

pub struct Handler {
}

impl Handler {
    pub async fn cmd_loop(
        mut rx: mpsc::Receiver<String>,
        mut tx: mpsc::Sender<String>,
        ident: oneshot::Receiver<String>,
        mut server_state: State,
    ) -> Result<(), Error>
    {
        let peer_id = ident.await?;
        loop {
            match rx.next().await {
                Some(msg) => {
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
                            Self::all_responses(&[Self::error_response(None, None, None)])
                                .to_string()
                        }
                    };
                    tx.send(rsp).await?;
                }
                None => break
            };
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
                    remove_from_rooms.push(room_id.clone());
                }
                if room.creator == peer_id {
                    destroy_rooms.push(room_id.clone());
                }
            }
            drop(rooms);

            let mut requests = vec![];
            let mut rooms = server_state.rooms.lock().unwrap();
            for room_id in remove_from_rooms.iter() {
                requests.extend(Self::room_remove_member(peer_id.as_str(), room_id,
                                                         &mut rooms, server_state.clone()));
            }
            for room_id in destroy_rooms.iter() {
                requests.extend(Self::room_destroy(room_id, &mut rooms, server_state.clone()));
            }
            requests
        };
        Self::send_requests(requests).await;
        Ok(())
    }

    fn make_request(
        req_type: &str,
        room_id: Option<&str>,
        args: &[&str],
    ) -> Value
    {
        match room_id {
            Some(room_id) => {
                json!([{
                    "type": req_type,
                    "room_id": room_id,
                    "args": args,
                    // This is unused right now, but we might need this for requests that need
                    // a response from the client (none right now).
                    "request_id": "",
                }])
            }
            None => {
                json!([{
                    "type": req_type,
                    "args": args,
                    "request_id": "",
                }])
            }
        }
    }

    fn make_response(
        code: HttpCode,
        args: Option<Vec<String>>,
        request_id: Option<&str>,
    ) -> Value
    {
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
    ) -> Value
    {
        let (arg, code) = match (error_str, error_code) {
            (Some(s), Some(code)) => (s.to_string(), code),
            (Some(s), None) => (s.to_string(), HttpCode::BAD_REQUEST),
            (None, Some(code)) => {
                let reason = match code.canonical_reason() {
                    Some(reason) => reason.to_string(),
                    None => {
                        error!("Somehow got an unknown code: {:?}", code);
                        format!("Bad request")
                    }
                };
                (reason, code)
            },
            (None, None) => {
                let reason = HttpCode::BAD_REQUEST
                    .canonical_reason()
                    .unwrap()
                    .to_string();
                (reason, HttpCode::BAD_REQUEST)
            },
        };
        Self::make_response(code, Some(vec![arg]), request_id)
    }

    fn all_responses(
        responses: &[Value],
    ) -> Value
    {
        json!(responses)
    }

    async fn send_requests(
        mut requests: Vec<(String, mpsc::Sender<String>)>,
    ) {
        let mut futs = vec![];
        // Inform all other participants of the departure
        for (req, tx) in requests.iter_mut() {
            futs.push(tx.send(req.clone()));
        };
        futures::future::join_all(futs).await;
    }

    fn room_request_builder(
        args: &[&str],
        room_id: &str,
        room: &mut Room,
        server_state: State,
    ) -> Vec<(String, mpsc::Sender<String>)>
    {
        let peers = server_state.peers.lock().unwrap();
        let requests = room.current_members
            .iter()
            .filter_map(|id| {
                let maybe_peer = peers
                    .identified
                    .get(id.as_str());
                if let Some(peer) = maybe_peer {
                    let msg = Self::make_request("room", Some(room_id), args).to_string();
                    Some((msg, peer.tx.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<(String, mpsc::Sender<String>)>>();
        requests
    }

    fn room_destroy(
        room_id: &str,
        rooms: &mut std::sync::MutexGuard<'_, HashMap<String, Room>>,
        server_state: State,
    ) -> Vec<(String, mpsc::Sender<String>)>
    {
        match rooms.remove(room_id) {
            Some(mut room) => {
                Self::room_request_builder(&["room", "destroyed", room_id], room_id, &mut room,
                                           server_state.clone())
            },
            None => {
                debug!("Room already destroyed");
                vec![]
            }
        }
    }

    fn room_add_member(
        member: &str,
        room_id: &str,
        room: &mut Room,
        server_state: State,
    ) -> Option<(Vec<(String, mpsc::Sender<String>)>, Vec<String>)>
    {
        debug_assert!(room.allowed_members.contains(member));
        let ret = Self::room_request_builder(&["room", "joined", member], room_id, room,
                                             server_state);
        let other_members = room.current_members
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Add peer to the room
        room.current_members.insert(member.to_string());
        Some((ret, other_members))
    }

    fn room_remove_member(
        member: &str,
        room_id: &str,
        rooms: &mut std::sync::MutexGuard<'_, HashMap<String, Room>>,
        server_state: State,
    ) -> Vec<(String, mpsc::Sender<String>)>
    {
        let room = rooms.get_mut(room_id).unwrap();
        room.current_members.remove(member);
        if room.current_members.len() == 0 {
            Self::room_destroy(room_id, rooms, server_state.clone())
        } else {
            Self::room_request_builder(&["room", "left", member], room_id, room,
                                       server_state.clone())
        }
    }

    fn room_craft_message(
        from_id: &str,
        room_id: &str,
        room: &mut Room,
        msg: &Value,
        server_state: State,
    ) -> Option<Vec<(String, mpsc::Sender<String>)>>
    {
        let peers = server_state.peers.lock().unwrap();
        // Collect a list of room members that need to be notified that this peer has been kicked
        // out of the room
        let messages = room.current_members
            .iter()
            .filter(|id| *id == from_id)
            .filter_map(|id| {
                let maybe_peer = peers
                    .identified
                    .get(id.as_str());
                if let Some(peer) = maybe_peer {
                    // We can't use room_request_builder() here because `msg` is not a string
                    let msg = json!([{
                        "type": "request",
                        "room_id": room_id,
                        "args": ["room", "message", msg, from_id],
                        "request_id": "",
                    }]);
                    Some((msg.to_string(), peer.tx.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<(String, mpsc::Sender<String>)>>();
        Some(messages)
    }

    async fn handle_room(
        argv: &Vec<Value>,
        room_id: Option<&str>,
        request_id: &str,
        peer_id: &str,
        server_state: State,
    ) -> Value
    {
        let mut args = argv.iter();
        let arg0 = args.next().map(|v| v.as_str()).flatten();
        let room_id = match room_id {
            Some(room_id) => room_id,
            None => {
                let args = match arg0 {
                    Some("list") => {
                        let rooms = server_state.rooms.lock().unwrap();
                        let args: Vec<String> = rooms
                            .iter()
                            .filter(|(_, room)| room.allowed_members.contains(peer_id))
                            .map(|(id, _)| id.to_owned())
                            .collect();
                        args
                    }
                    Some(_) => {
                        return Self::make_response(HttpCode::BAD_REQUEST,
                                                   Some(vec!["room_id not specified".to_string()]),
                                                   Some(request_id));
                    }
                    None => {
                        return Self::make_response(HttpCode::BAD_REQUEST, None, Some(request_id));
                    }
                };
                return Self::make_response(HttpCode::OK, Some(args), Some(request_id));
            }
        };
        let response_args = match arg0 {
            Some("create") => {
                debug!("{}", format!("{server_state:#?}"));
                // TODO: Destroy any other room created by this peer
                // TODO: Check that we're not overwriting an existing room
                let mut rooms = server_state.rooms.lock().unwrap();
                if rooms.contains_key(room_id) {
                    Err((HttpCode::FORBIDDEN, "Forbidden"))
                } else {
                    debug!("{}", format!("{rooms:#?}"));
                    rooms.insert(room_id.to_string(), Room {
                        creator: peer_id.to_string(),
                        allowed_members: HashSet::from([peer_id.to_string()]),
                        current_members: HashSet::new(),
                    });
                    Ok(None)
                }
            }
            Some("destroy") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    match rooms.get_mut(room_id) {
                        Some(room) => {
                            if room.creator == peer_id {
                                Ok(Self::room_destroy(room_id, &mut rooms, server_state.clone()))
                            } else {
                                Err((HttpCode::FORBIDDEN, "Forbidden"))
                            }
                        },
                        None => Err((HttpCode::NOT_FOUND, "No such room")),
                    }
                };
                match requests {
                    Ok(reqs) => {
                        Self::send_requests(reqs).await;
                        Ok(None)
                    },
                    Err(e) => Err(e),
                }
            }
            Some("edit") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    match rooms.get_mut(room_id) {
                        Some(room) => {
                            if room.creator == peer_id {
                                match args.next().and_then(|v| v.as_str()) {
                                    Some("allow") => {
                                        for member in args.map(|v| v.as_str()) {
                                            // Trust that the creator knows that these PeerID
                                            // values are valid
                                            if let Some(member) = member {
                                                if !room.allowed_members.contains(member) {
                                                    room.allowed_members.insert(member.to_string());
                                                }
                                            }
                                        }
                                        Ok(None)
                                    }
                                    Some("disallow") => {
                                        Ok(Some(args
                                            .filter_map(|arg| arg.as_str())
                                            .filter_map(|arg| {
                                                let state = server_state.clone();
                                                Some(Self::room_remove_member(arg, room_id, &mut
                                                                              rooms, state))
                                            })
                                            .into_iter()
                                            .flatten()
                                            .collect::<Vec<(String, mpsc::Sender<String>)>>()
                                        ))
                                    }
                                    _ => {
                                        Err((HttpCode::BAD_REQUEST, "Unknown room edit argument"))
                                    }
                                }
                            } else {
                                Err((HttpCode::FORBIDDEN, "Not allowed to edit room"))
                            }
                        }
                        None => Err((HttpCode::NOT_FOUND, "No such room"))
                    }
                };
                match requests {
                    Ok(Some(reqs)) => {
                        Self::send_requests(reqs).await;
                        Ok(None)
                    },
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            }
            Some("join") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    match rooms.get_mut(room_id) {
                        Some(room) => {
                            if room.allowed_members.contains(peer_id) {
                                info!("Joining room {}", room_id);
                                Ok(Self::room_add_member(peer_id, room_id, room, server_state.clone()))
                            } else {
                                Err((HttpCode::FORBIDDEN, "Not allowed to join room"))
                            }
                        }
                        None => Err((HttpCode::NOT_FOUND, "No such room"))
                    }
                };
                match requests {
                    Ok(Some((reqs, response))) => {
                        Self::send_requests(reqs).await;
                        Ok(Some(response))
                    },
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            }
            Some("leave") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    if !rooms.contains_key(room_id) {
                        Err((HttpCode::NOT_FOUND, "No such room"))
                    } else {
                        let room = rooms.get_mut(room_id).unwrap();
                        if !room.current_members.contains(peer_id) {
                            Err((HttpCode::BAD_REQUEST, "Not a member of room"))
                        } else {
                            Ok(Self::room_remove_member(peer_id, room_id, &mut rooms,
                                                        server_state.clone()))
                        }
                    }
                };
                match requests {
                    Ok(reqs) => {
                        Self::send_requests(reqs).await;
                        Ok(None)
                    },
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
                        match rooms.get_mut(room_id) {
                            Some(room) => {
                                if room.current_members.contains(peer_id) {
                                    Ok(Self::room_craft_message(peer_id, room_id, room, msg,
                                                                server_state.clone()))
                                } else {
                                    Err((HttpCode::BAD_REQUEST, "Not a member of room"))
                                }
                            }
                            None => Err((HttpCode::NOT_FOUND, "No such room"))
                        }
                    },
                    _ => Err((HttpCode::BAD_REQUEST, "Bad request")),
                };
                match messages {
                    Ok(Some(msgs)) => {
                        Self::send_requests(msgs).await;
                        Ok(None)
                    },
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            }
            _ => Err((HttpCode::BAD_REQUEST, "Invalid room argument"))
        };
        debug!("{}", format!("{server_state:#?}"));
        {
            let rooms = server_state.rooms.lock().unwrap();
            debug!("{}", format!("{rooms:#?}"));
        }
        let code = response_args
            .clone()
            .err()
            .unzip().0
            .unwrap_or(HttpCode::OK);
        let args = response_args
            .map_or_else(|(error, _)| Some(vec![error.to_string()]),
                         |v| v);
        Self::make_response(code, args, Some(request_id))
    }

    async fn handle_request(
        reqs: Requests,
        peer_id: &str,
        server_state: &mut State,
    ) -> String {
        let responses: Responses = futures::future::join_all(
            reqs
            .into_iter()
            .map(|req| {
                let state = server_state.clone();
                async move {
                    let m = (
                        req.get("type").and_then(|s| s.as_str()),
                        req.get("request_id").and_then(|s| s.as_str()),
                        req.get("args").and_then(|v| v.as_array()),
                    );
                    info!("{:?}", req);
                    match m {
                        (Some("room"), Some(request_id), Some(args)) => {
                            let room_id = req.get("room_id").and_then(|s| s.as_str());
                            if args.len() == 0 {
                                Self::error_response(Some(request_id),
                                                     Some(HttpCode::BAD_REQUEST),
                                                     Some("Empty args"))
                            } else {
                                Self::handle_room(args, room_id, request_id, peer_id,
                                                  state).await
                            }
                        },
                        (room, request_id, args) => {
                            error!("{:?} {:?}", room, args);
                            Self::error_response(
                                request_id,
                                Some(HttpCode::BAD_REQUEST),
                                None,
                            )
                        }
                    }
                }
            })).await;

        Self::all_responses(responses.as_slice()).to_string()
    }
}
