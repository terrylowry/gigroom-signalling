use anyhow::Error;
use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use http::status::StatusCode as HttpCode;
use log::{log_enabled, Level::Debug};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use crate::server::{Clients, Room, State};

type Requests = Vec<HashMap<String, Value>>;
// List of requests and the peer channel to send to
type RequestList = Vec<(String, mpsc::Sender<Message>)>;
type Responses = Vec<Value>;

pub struct Handler {}

impl Handler {
    pub async fn cmd_loop(
        mut rx: mpsc::Receiver<String>,
        mut tx: mpsc::Sender<Message>,
        ident: oneshot::Receiver<(Uuid, String)>,
        mut server_state: State,
    ) -> Result<(), Error> {
        let (client_id, user_id) = ident.await?;
        while let Some(msg) = rx.next().await {
            let v: Result<Requests, serde_json::Error> = serde_json::from_str(&msg);
            let rsp = match v {
                Ok(v) => {
                    if log_enabled!(Debug) {
                        debug!("{}", format!("{server_state:#?}"));
                        let rooms = server_state.rooms.lock().unwrap();
                        debug!("{}", format!("{rooms:#?}"));
                    }
                    Self::handle_request(v, &client_id, &user_id, &mut server_state).await
                }
                Err(_) => {
                    Self::all_responses(&[Self::error_response(None, None, None)]).to_string()
                }
            };
            tx.send(Message::Text(rsp)).await?;
        }
        info!("Exiting cmd_loop for {}", user_id);
        info!("{}", format!("{server_state:#?}"));
        {
            let rooms = server_state.rooms.lock().unwrap();
            info!("{}", format!("{rooms:#?}"));
        }
        // Notify all rooms that this peer has left. Do not destroy rooms that this user created
        // because it will disrupt the call, and they can always rejoin.
        let requests = {
            let mut remove_from_rooms = Vec::new();

            let mut rooms = server_state.rooms.lock().unwrap();
            for (room_id, room) in rooms.iter() {
                if room.current_clients.contains(&client_id) {
                    remove_from_rooms.push(*room_id);
                }
            }

            let mut requests = Vec::new();
            let mut destroy_rooms = Vec::new();
            for room_id in remove_from_rooms.iter() {
                let room = rooms.get_mut(room_id).unwrap();
                requests.extend(Self::room_remove_client(
                    &client_id,
                    &user_id,
                    room_id,
                    room,
                    &server_state.clients,
                ));
                if room.current_clients.is_empty() {
                    destroy_rooms.push(*room_id);
                }
            }
            for room_id in destroy_rooms.iter() {
                requests.extend(Self::room_destroy(
                    room_id,
                    &mut rooms,
                    &server_state.clients,
                ));
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
            "args": args.unwrap_or_default(),
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
            futs.push(tx.send(Message::Text(req.clone())));
        }
        futures::future::join_all(futs).await;
    }

    // Build a vector of requests that will be sent to the iterator of client IDs
    fn clients_request_builder<'a, I>(
        client_ids: I,
        room_id: Option<&Uuid>,
        args: &[Value],
        clients: &Arc<Mutex<Clients>>,
    ) -> RequestList
    where
        I: Iterator<Item = &'a Uuid>,
    {
        let mut reqs = Vec::new();
        let senders = Self::client_ids_to_senders(client_ids, clients);
        for sender in senders.into_iter() {
            let msg = Self::make_request(room_id, args).to_string();
            reqs.push((msg, sender));
        }
        reqs
    }

    fn client_ids_to_senders<'a, I>(
        client_ids: I,
        clients: &Arc<Mutex<Clients>>,
    ) -> Vec<mpsc::Sender<Message>>
    where
        I: Iterator<Item = &'a Uuid>,
    {
        let mut senders = Vec::new();
        let user_clients = &clients.lock().unwrap().identified;
        for client_id in client_ids {
            if let Some((_, client)) = user_clients.get(client_id) {
                senders.push(client.tx.clone());
            }
        }
        senders
    }

    fn room_destroy(
        room_id: &Uuid,
        rooms: &mut std::sync::MutexGuard<'_, HashMap<Uuid, Room>>,
        clients: &Arc<Mutex<Clients>>,
    ) -> RequestList {
        match rooms.remove(room_id) {
            Some(room) => Self::users_craft_messages(
                room.allowed_users.iter(),
                room_id,
                &[
                    Value::String("room".to_string()),
                    Value::String("destroyed".to_string()),
                ],
                clients,
            ),
            None => {
                debug!("Room already destroyed");
                vec![]
            }
        }
    }

    fn room_add_client(
        client_id: &Uuid,
        user_id: &str,
        room_id: &Uuid,
        room: &mut Room,
        clients: &Arc<Mutex<Clients>>,
    ) -> Option<(RequestList, Vec<Value>)> {
        let (reqs, resp) = if !room.current_clients.is_empty() {
            let reqs = Self::clients_request_builder(
                room.current_clients.iter(),
                Some(room_id),
                &[
                    Value::String("room".to_string()),
                    Value::String("joined".to_string()),
                    Value::String(client_id.to_string()),
                    Value::String(user_id.to_string()),
                ],
                clients,
            );
            let mut other_members = Vec::new();
            let clients = clients.lock().unwrap();
            for id in room.current_clients.iter() {
                if let Some((user_id, _)) = clients.identified.get(id) {
                    let mut o = serde_json::Map::new();
                    o.insert("user_id".to_string(), Value::String(user_id.clone()));
                    o.insert("client_id".to_string(), Value::String(id.to_string()));
                    other_members.push(Value::Object(o));
                } else {
                    warn!(
                        "Unidentified client ID {} in room {} {}",
                        id, room.name, room_id
                    );
                }
            }
            (reqs, other_members)
        } else {
            // There were no members in the room, which means the room went from inactive to active
            // and we need to notify allowed members
            (
                Self::users_craft_messages(
                    room.allowed_users.iter(),
                    room_id,
                    &[
                        Value::String("room".to_string()),
                        Value::String("active".to_string()),
                    ],
                    clients,
                ),
                Vec::new(),
            )
        };
        // Add peer to the room
        room.current_clients.insert(*client_id);
        Some((reqs, resp))
    }

    fn room_remove_client(
        client_id: &Uuid,
        user_id: &str,
        room_id: &Uuid,
        room: &mut Room,
        clients: &Arc<Mutex<Clients>>,
    ) -> RequestList {
        if room.current_clients.remove(client_id) {
            if !room.current_clients.is_empty() {
                Self::clients_request_builder(
                    room.current_clients.iter(),
                    Some(room_id),
                    &[
                        Value::String("room".to_string()),
                        Value::String("left".to_string()),
                        Value::String(client_id.to_string()),
                        Value::String(user_id.to_string()),
                    ],
                    clients,
                )
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    fn room_remove_user(
        user_id: &str,
        room_id: &Uuid,
        room: &mut Room,
        clients: &Arc<Mutex<Clients>>,
    ) -> RequestList {
        let removed_clients = {
            let client_list = clients.lock().unwrap();
            if let Some(client_ids) = client_list.user_clients.get(user_id) {
                client_ids
                    .iter()
                    .filter(|client_id| room.current_clients.remove(client_id.0))
                    .map(|client_id| (user_id, *client_id.0))
                    .collect()
            } else {
                Vec::new()
            }
        };

        let mut reqs: RequestList = RequestList::new();
        if !room.current_clients.is_empty() {
            for (user_id, client_id) in removed_clients.iter() {
                reqs.extend(Self::clients_request_builder(
                    room.current_clients.iter(),
                    Some(room_id),
                    &[
                        Value::String("room".to_string()),
                        Value::String("left".to_string()),
                        Value::String(client_id.to_string()),
                        Value::String(user_id.to_string()),
                    ],
                    clients,
                ))
            }
        }
        reqs
    }

    fn users_craft_messages<'a, I>(
        users: I,
        room_id: &Uuid,
        args: &[Value],
        clients: &Arc<Mutex<Clients>>,
    ) -> RequestList
    where
        I: Iterator<Item = &'a String>,
    {
        let mut connected: Vec<Uuid> = Vec::new();
        {
            let clients = clients.lock().unwrap();
            for user in users {
                if let Some(c) = clients.user_clients.get(user) {
                    connected.extend(c.keys())
                }
            }
        }
        Self::clients_request_builder(connected.iter(), Some(room_id), args, clients)
    }

    fn users_craft_created_messages<'a, I>(
        users: I,
        room_id: &Uuid,
        room: &Room,
        clients: &Arc<Mutex<Clients>>,
    ) -> RequestList
    where
        I: Iterator<Item = &'a String>,
    {
        let mut room_details = serde_json::Map::new();
        room_details.insert("room_id".to_string(), Value::String(room_id.to_string()));
        room_details.insert(
            "room_name".to_string(),
            Value::String(room.name.to_string()),
        );
        room_details.insert("creator".to_string(), Value::String(room.creator.clone()));
        room_details.insert(
            "active".to_string(),
            Value::Bool(!room.current_clients.is_empty()),
        );
        Self::users_craft_messages(
            users,
            room_id,
            &[
                Value::String("room".to_string()),
                Value::String("created".to_string()),
                Value::Object(room_details),
            ],
            clients,
        )
    }

    async fn handle_room<'a, I>(
        mut args: I,
        room_id: Option<&str>,
        request_id: &str,
        client_id: &Uuid,
        user_id: &str,
        server_state: State,
    ) -> Value
    where
        I: Iterator<Item = &'a Value>,
    {
        let arg1 = args.next().and_then(|v| v.as_str());
        let room_id = match room_id {
            Some(room_id) => {
                let Ok(room_id) = Uuid::try_parse(room_id) else {
                    let args = vec![Value::String("Bad room_id, must be UUID4".to_string())];
                    return Self::make_response(
                        HttpCode::BAD_REQUEST,
                        Some(args),
                        Some(request_id),
                    );
                };
                room_id
            }
            None => {
                let Some(args) = arg1 else {
                    return Self::make_response(HttpCode::BAD_REQUEST, None, Some(request_id));
                };
                if args != "list" {
                    let args = vec![Value::String("room_id not specified".to_string())];
                    return Self::make_response(
                        HttpCode::BAD_REQUEST,
                        Some(args),
                        Some(request_id),
                    );
                }
                let rooms = server_state.rooms.lock().unwrap();
                let args: Vec<Value> = rooms
                    .iter()
                    .filter(|(_, room)| room.allowed_users.contains(user_id))
                    .map(|(id, room)| {
                        let mut v = serde_json::Map::new();
                        v.insert("room_id".to_string(), Value::String(id.to_string()));
                        v.insert(
                            "room_name".to_string(),
                            Value::String(room.name.to_string()),
                        );
                        v.insert("creator".to_string(), Value::String(room.creator.clone()));
                        let active = !room.current_clients.is_empty();
                        v.insert("active".to_string(), Value::Bool(active));
                        Value::Object(v)
                    })
                    .collect();
                return Self::make_response(HttpCode::OK, Some(args), Some(request_id));
            }
        };
        let response_args = match arg1 {
            Some("create") => {
                debug!("{}", format!("{server_state:#?}"));
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    if rooms.contains_key(&room_id) {
                        Err((HttpCode::CONFLICT, "Room ID already in use"))
                    } else {
                        debug!("{}", format!("{rooms:#?}"));
                        if let Some(name) = args.next().and_then(|v| v.as_str()) {
                            if rooms
                                .values()
                                .any(|room| room.creator == user_id && room.name == name)
                            {
                                Err((HttpCode::CONFLICT, "Room already exists"))
                            } else {
                                let room = Room {
                                    creator: user_id.to_string(),
                                    name: name.to_string(),
                                    allowed_users: HashSet::from([user_id.to_string()]),
                                    current_clients: HashSet::new(),
                                };
                                let reqs = Self::users_craft_created_messages(
                                    [&room.creator].into_iter(),
                                    &room_id,
                                    &room,
                                    &server_state.clients,
                                );
                                rooms.insert(room_id, room);
                                Ok(Some(reqs))
                            }
                        } else {
                            Err((HttpCode::BAD_REQUEST, "Room name missing"))
                        }
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
            Some("destroy") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    match rooms.get(&room_id) {
                        Some(room) => {
                            if room.creator == user_id {
                                Ok(Self::room_destroy(
                                    &room_id,
                                    &mut rooms,
                                    &server_state.clients,
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
            Some("get") => match args.next().and_then(|v| v.as_str()) {
                Some("allowed-users") => {
                    let rooms = server_state.rooms.lock().unwrap();
                    match rooms.get(&room_id) {
                        Some(room) => {
                            if room.creator == user_id {
                                Ok(Some(
                                    room.allowed_users
                                        .iter()
                                        .map(|s| Value::String(s.to_string()))
                                        .collect(),
                                ))
                            } else {
                                Err((HttpCode::FORBIDDEN, "room get not allowed"))
                            }
                        }
                        None => Err((HttpCode::NOT_FOUND, "No such room")),
                    }
                }
                Some(_) => Err((HttpCode::BAD_REQUEST, "Unknown room get argument")),
                None => Err((HttpCode::BAD_REQUEST, "Missing room get argument")),
            },
            Some("set") => match args.next().and_then(|v| v.as_str()) {
                Some("allowed-users") => {
                    let requests = {
                        let mut rooms = server_state.rooms.lock().unwrap();
                        match rooms.get_mut(&room_id) {
                            Some(room) => {
                                if room.creator == user_id {
                                    let mut reqs = Vec::new();
                                    let old_allowed: HashSet<String> =
                                        HashSet::from_iter(room.allowed_users.iter().cloned());
                                    let mut allowed: HashSet<String> = HashSet::from_iter(
                                        args.filter_map(|v| v.as_str()).map(|s| s.to_string()),
                                    );
                                    allowed.insert(room.creator.clone());

                                    let had_clients = !room.current_clients.is_empty();

                                    // Construct "room created" messages; must not be sent to the
                                    // creator's clients
                                    reqs.extend(Self::users_craft_created_messages(
                                        allowed.iter().filter(|&uid| !old_allowed.contains(uid)),
                                        &room_id,
                                        room,
                                        &server_state.clients,
                                    ));

                                    // Construct "room left" and "room destroyed" messages for
                                    // removed users
                                    let removed_users: Vec<_> = old_allowed
                                        .iter()
                                        .filter(|&uid| !allowed.contains(uid))
                                        .collect();

                                    for uid in removed_users.iter() {
                                        reqs.extend(Self::room_remove_user(
                                            uid,
                                            &room_id,
                                            room,
                                            &server_state.clients,
                                        ))
                                    }

                                    reqs.extend(Self::users_craft_messages(
                                        removed_users.into_iter(),
                                        &room_id,
                                        &[
                                            Value::String("room".to_string()),
                                            Value::String("destroyed".to_string()),
                                        ],
                                        &server_state.clients,
                                    ));

                                    room.allowed_users = allowed;

                                    // If this removed all the room's users destroy the room
                                    if had_clients && room.current_clients.is_empty() {
                                        reqs.extend(Self::room_destroy(
                                            &room_id,
                                            &mut rooms,
                                            &server_state.clients,
                                        ));
                                    }

                                    Ok(Some(reqs))
                                } else {
                                    Err((HttpCode::FORBIDDEN, "room set not allowed"))
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
                Some(_) => Err((HttpCode::BAD_REQUEST, "Unknown room set argument")),
                None => Err((HttpCode::BAD_REQUEST, "Missing room set argument")),
            },
            Some("join") => {
                let requests = {
                    let mut rooms = server_state.rooms.lock().unwrap();
                    match rooms.get_mut(&room_id) {
                        Some(room) => {
                            if room.allowed_users.contains(user_id) {
                                if room.current_clients.contains(client_id) {
                                    Err((HttpCode::CONFLICT, "Already in the room"))
                                } else {
                                    info!("Joining room {}", room_id);
                                    Ok(Self::room_add_client(
                                        client_id,
                                        user_id,
                                        &room_id,
                                        room,
                                        &server_state.clients,
                                    ))
                                }
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
                        if !room.current_clients.contains(client_id) {
                            Err((HttpCode::BAD_REQUEST, "Not a member of room"))
                        } else {
                            let mut reqs = Self::room_remove_client(
                                client_id,
                                user_id,
                                &room_id,
                                room,
                                &server_state.clients,
                            );
                            if room.current_clients.is_empty() {
                                reqs.extend(Self::room_destroy(
                                    &room_id,
                                    &mut rooms,
                                    &server_state.clients,
                                ));
                            }
                            Ok(reqs)
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
            // ["message", <message>, ["client_id1", ...]]
            Some("message") => {
                let msg = args.next(); // Could be any type, but probably just an object (dict)
                let to = args.next().and_then(|v| v.as_array());
                let messages = match (msg, to) {
                    (Some(msg), Some(to)) => {
                        info!("Sending message {:?} to {:?}", msg, to);
                        let mut rooms = server_state.rooms.lock().unwrap();
                        match rooms.get_mut(&room_id) {
                            Some(room) => {
                                if room.current_clients.contains(client_id) {
                                    // Collect a list of room members that need to be notified that
                                    // this peer has been kicked out of the room
                                    Ok(Some(Self::clients_request_builder(
                                        room.current_clients.iter().filter(|id| {
                                            to.contains(&Value::String(id.to_string()))
                                        }),
                                        Some(&room_id),
                                        &[
                                            Value::String("room".to_string()),
                                            Value::String("message".to_string()),
                                            msg.clone(),
                                            Value::String(client_id.to_string()),
                                        ],
                                        &server_state.clients,
                                    )))
                                } else {
                                    Err((HttpCode::BAD_REQUEST, "Not a room member"))
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
        let args =
            response_args.unwrap_or_else(|(error, _)| Some(vec![Value::String(error.to_string())]));
        Self::make_response(code, args, Some(request_id))
    }

    async fn handle_request(
        reqs: Requests,
        client_id: &Uuid,
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
                                    Self::handle_room(
                                        args, room_id, request_id, client_id, peer_id, state,
                                    )
                                    .await
                                }
                                Some(_) | None => Self::error_response(
                                    Some(request_id),
                                    Some(HttpCode::BAD_REQUEST),
                                    Some("Incorrect args"),
                                ),
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
