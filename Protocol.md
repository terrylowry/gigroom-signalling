# GigRoom Signalling Protocol

## Overview

This is a protocol for doing multi-participant "room" calls using the GigRoom signalling server.

## Client identification

Clients must identify themselves immediately upon connecting by sending the following plain-text message: **`IDENTIFY <USER_ID>`**

If the identification is accepted, the server will reply with a plain-text message: **`IDENTIFIED <UUID>`** where `<UUID>` is the client ID assigned to your connection.

Note that `<USER_ID>` here can contain any characters, including spaces.

This is the only part of the protocol that utilizes plain-text messages. The remaining messages are required to be JSON-formatted.

The `IDENTIFY` request is sent in plaintext so that the initial processing is cheaper than parsing JSON and invalid clients (such as bots that spam websocket servers on the internet) can be more quickly and cheaply rejected.

## Requests and Responses

* All messages in the protocol other than identification are required to be classified as requests or responses, and must be formatted in JSON.

* Requests may be sent from a client to the server, or may be from the server to the client (to notify it of an event)

* Each message can contain multiple requests or multiple responses, but not both.
* The grouping of messages is preserved: if a client sends multiple requests in a single message, it will receive a single message reply with one response for each request. Note: Server to client requests do not currently require a response from clients.

Example request and response:

```js
[
    {
        "type": "request",
        // Different for each request
        "request_id": "1",
        // Server request goes here
        // in this case, request a list of all
        // rooms that the user is allowed to join
        "args": ["room", "list"]
    }
]
```

```js
[
    {
        "type": "response",
        // The request this is a response to
        "request_id": "1",
        // HTTP status codes
        "status_code": 200,
        // Contains the error response if status_code != 200
        "args": [
            {
                "room_id": "0ec17517-31e8-4bcb-a020-fe865c210555",
                "room_name": "My Studio",
                "creator": "tlowry",
                // There is a call active in this room right now
                "active": true
            },
            {
                "room_id": "006b5499-0464-49b2-8bcb-999ace0fe605",
                "room_name": "Evening Flute Practice",
                "creator": "thaytan",
                "active": false
            }
        ]
    }
]
```

The outermost JSON container is an array, and each element inside it is a request or response. Requests are processed in order by the server, and the responses are batched up and sent in one message once all the requests have been processed.

The string error in `args` in the response is free-form text for debugging purposes that is subject to change at any time. You should only match against the `status_code` to check success/failure.

## Client → Server requests

Clients may send a number of requests to the server and receive responses.

### `room list`

List all rooms that the user is allowed to join.

[See above](#Requests-and-Responses) for a complete example.

### `room create` | `destroy`

Create or destroy a new room with the specified name (unique ID).

The user that creates the room is designated as the "Creator", and is the only one who can modify the properties of the room such as who is allowed to join it.

If you try to create a room using a name that you already have a room for, a JSON 409 CONFLICT response will be returned.

Rooms are destroyed when they have no participants. Rooms can be destroyed at any time by the creator. When a room is destroyed, all members of the room at the time receive the message [`room destroyed`](#room-destroyed-server).

Client request:

```js
[
    {
        "type": "request",
        // Different for each request
        "request_id": "9dcab9bd",
        // Client must generate a random UUID4 room id
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        // Server request goes here
        // "Violin Ensemble" is the user-facing room name
        "args": ["room", "create", "Violin Ensemble"]
    },
    {
        "type": "request",
        "request_id": "b4fd1dcz",
        "room_id": "5bc7eb2a-3d66-4634-ba3c-4e4b145aa2ca",
        // OOPS: We're trying to create the same room twice!
        "args": ["room", "create", "Violin Ensemble"]
    },
    {
        "type": "request",
        "request_id": "83fd015c",
        // OOPS: we tried to use the same room_id twice!
        // also applies if we try to use a room ID used by someone else
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": ["room", "create", "Violin Ensemble 2"]
    }
]
```

Server response (one success, two failures):


```js
[
    {
        "type": "response",
        // The request this is a response to
        "request_id": "9dcab9bd",
        // HTTP status code, success
        "status_code": 200,
        // Contains the error string if status_code != 200
        "args": []
    },
    {
        "type": "response",
        "request_id": "83fd015c",
        // HTTP status code, client error: CONFLICT
        "status_code": 409,
        "args": ["Room already exists"]
    },
    {
        "type": "response",
        "request_id": "b4fd1dcz",
        // HTTP status code, client error: CONFLICT
        "status_code": 409,
        "args": ["Room ID already in use"]
    }
]
```

### `room join` | `leave`

Join a room or leave it. You may only join rooms that you have permission to join.

The response will contain a list of all clients currently in the room in the `args` attribute.

Immediately after joining a room, you must begin negotiation will all other clients in the room by sending them SDP offers and ICE candidates. They will reply with answers and ICE candidates of their own.

When a client joins a room, all other clients receive a Server → Client request [`room joined`](#room-joined--left-server), so they can expect to receive an SDP offer from you.

When a client leaves a room, all other clients receive a Server → Client request [`room left`](#room-joined--left-server), so they are expected to stop sending to this client and stop receiving media from it.

Clients may join, leave, and rejoin a room at any point during a call.

```js
[
    {
        "type": "request",
        "request_id": "3",
        // Some room, let's say "Oboe Practice"
        // Clients must maintain a hash table from
        // room ID to room name + owner to figure out
        // what to show in the UI
        "room_id": "f3e70839-d64c-4adb-9183-a487a79aac5e",
        "args": ["room", "join"]
    }
]
```

Success response:


```js
[
    {
        "type": "response",
        "request_id": "3",
        "status_code": 200,
        // Current members of the room
        "args": [
            {
                "user_id": "MahimaV",
                "client_id": "2e73ab52-b490-4cf0-a93a-6729645f13c4"
            },
            {
                "user_id": "ScottPilgrim",
                "client_id": "e0b3595c-822a-49e6-8620-fbc9b776d4e8"
            }
        ]
    }
]
```

Failure response:

```js
[
    {
        "type": "response",
        "request_id": "3",
        "status_code": 403,
        "args": ["Forbidden"]
    }
]
```

### `room set allowed-users`

Set the current list of allowed users. The creator is always implicitly in the list of allowed members, so you can never remove it from the list. 

Users do not have to be connected via a client to be added to (or removed from) the allow-list of a room. If they are connected, all their clients will receive a `room created` message when they are added to the allow-list.

If you *disallow* a user who is already in the room, the user will be kicked out as if they had sent a **`room leave`** request.

```js
[
    {
        "type": "request",
        "request_id": "5",
        // Room "Violin Ensemble"
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": ["room", "set", "allowed-users", "Lao88", "Taiki97", "DambisaM"]
    }
]
```

Success response:


```js
[
    {
        "type": "response",
        "request_id": "5",
        "status_code": 200,
        "args": []
    }
]
```

### `room get allowed-users`

Return a list of users that are in the allow-list for the specified room.

```js
[
    {
        "type": "request",
        "request_id": "6",
        // Room "Violin Ensemble"
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": ["room", "get", "allowed-users"]
    }
]
```

Success response:


```js
[
    {
        "type": "response",
        "request_id": "6",
        "status_code": 200,
        "args": ["Lao88", "Taiki97", "DambisaM"]
    }
]
```

### `room message`

Send a JSON message to a list of participants.

Only current members of a room are allowed to send this request.

The JSON message must be the third element in the `args` list for this request, and it can be ***any*** valid JSON value. The fourth element must be an array of client IDs to send this message to.

On receipt of this request, the server will send the specified message to all specified clients using the server → client _request_ [`room message`](#room-message-server).

You are expected to use this to send SDP, ICE, or any other message to another client, as part of the negotiation to start a call.

```js
[
    {
        "type": "request",
        "request_id": "8",
        // Room "Violin Ensemble"
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": [
            "room",
            "message",
            {
                "sdp": {
                    "type": "offer",
                    "sdp": "o=- ..."
                }
            }, 
            [
                "3bbb38a8-ccd8-42d1-81a6-262776c98146", // client ID for user "Lao88"
                "b4b26799-3851-478d-ac4d-5f56aed43d95" // client ID for user "Taiki97"
            ]
        ]
    }
]
```

Success response:


```js
[
    {
        "type": "response",
        "request_id": "8",
        "status_code": 200,
        "args": []
    }
]
```

## Server → Client requests

In some cases, the server will send _requests_ to the client instead of the usual type of message (a response). This will usually be due to a request by another client.

A response from the client is **not** expected for any requests at the moment.

### `room created` (server) (🖅 allowed)

A room that you are allowed to join has been created. This serves as an update notification for the list you received in `room list`.

In practice, you will get this message when you are added to the allow-list for a room, since a newly-created room has no one in its allow-list.

You will receive a message like this:

```js
[
    {
        "type": "request",
        "request_id": "",
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": [
            "room",
            "created",
            {
                // Duplicated for convenience
                "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
                "room_name": "Violin Ensemble",
                "creator": "tlowry",
                // This will be `true` if you're added to the allow-list
                // of a room with an active call
                "active": false
            }
        ]
    },
]
```

### `room active` (server) (🖅 allowed)

The specified room has become active because someone joined it for the first time, meaning that a call has begun. You will not receive this message if you are already in the room, because the room is implied to be active already if someone is in it.

You will receive a message like this:

```js
[
    {
        "type": "request",
        // Currently empty
        "request_id": "",
        // The Room ID that this command refers to
        // in this case, this is "Violin Ensemble"
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        // Server message goes here
        "args": ["room", "active"]
    },
]
```

### `room joined` | `left` (server) (🖅 members)

The specified client has joined or left the specified room that you are a member of. You will not receive these messages if you are not in the room.

You will receive a message like this:

```js
[
    {
        "type": "request",
        "request_id": "",
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": ["room", "joined", "3bbb38a8-ccd8-42d1-81a6-262776c98146", "Lao88"] // client ID and user ID
    },
]
```

### `room destroyed` (server) (🖅 allowed)

This means the group call has ended. You will receive this message in three cases:

* The room has been destroyed by the Creator while you were in it or you were in the allowed list. This serves as an update notification for the list you received in `room list`.
* You have been removed from the allow-list of the room while you were in it or you were in the allowed list. This serves as an update notification for the list you received in `room list`.
* The room was destroyed because everyone left the room (which automatically destroys it) while you were in the allowed list but *not* in the room itself (since there is no one remaining in the room to receive a `destroyed` in that case).

You will receive a message like this:

```js
[
    {
        "type": "request",
        "request_id": "",
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": ["room", "destroyed"]
    },
]
```

### `room message` (server) (🖅 members)

A member of the room has sent you a JSON message, usually for negotiation (SDP or ICE). You can reply to it with your own [`room message`](#room-message) request.

You will receive a message like this:

```js
[
    {
        "type": "request",
        "request_id": "",
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": [
            "room",
            "message",
            // The message, may be any valid JSON value
            {
                "sdp": {
                    "type": "offer",
                    "sdp": "o=- ..."
                }
            },
            // The client that sent the message
            "3bbb38a8-ccd8-42d1-81a6-262776c98146" // client ID for user "Lao88"
        ]
    },
]
```

## Multiple Request/Response Example

```js
[
    {
        "type": "request",
        // Different for each request
        "request_id": "9dcab9bd",
        // Client must generate a random UUID4 room id
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        // Server request goes here
        // "Violin Ensemble" is the user-facing room name
        "args": ["room", "create", "Violin Ensemble"]
    },
    {
        "type": "request",
        "request_id": "488a8184",
        "room_id": "dca2c32c-caa6-4ed6-8b86-1c0cd7339a4c",
        "args": ["room", "set", "allowed-users", "Lao88", "DambisaM"]
    },
    {
        "type": "request",
        "request_id": "117805b5",
        "room_id": "eb302e64-c76e-4fc3-b4c1-34f3effdb841",
        "args": ["room", "create", "Flute Practice"]
    },
    {
        "type": "request",
        "request_id": "d2790710",
        "room_id": "eb302e64-c76e-4fc3-b4c1-34f3effdb841",
        "args": ["room", "join"]
    }
]
```


```js
[
    {
        "type": "response",
        // The request this is a response to
        "request_id": "9dcab9bd",
        // HTTP status codes
        "status_code": 200,
        // The list of Room IDs
        // Contains the error string if status_code != 200
        "args": []
    },
    {
        "type": "response",
        "request_id": "488a8184",
        "status_code": 200,
        "args": []
    },
    {
        "type": "response",
        "request_id": "117805b5",
        "status_code": 200,
        "args": []
    },
    {
        "type": "response",
        "request_id": "d2790710",
        "status_code": 200,
        // No existing members because we just created the room
        "args": []
    },
]
```
