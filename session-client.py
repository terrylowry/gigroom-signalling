#!/usr/bin/env python3
#
# Copyright (C) 2023 Centricular Ltd.
#
#  Author: Nirbheek Chauhan <nirbheek@centricular.com>
#

import sys
import ssl
import json
import uuid
import asyncio
import websockets
import argparse

parser = argparse.ArgumentParser(formatter_class=argparse.ArgumentDefaultsHelpFormatter)
parser.add_argument('--url', default='wss://localhost:8443', help='URL to connect to')
parser.add_argument('--peer-id', default=str(uuid.uuid4())[:6], help='Peer ID to use')
parser.add_argument('--allow-peer-ids', default=[], type=str.split, help='Peer IDs to allow in the rooms')

options = parser.parse_args(sys.argv[1:])

SERVER_ADDR = options.url
DEFAULT_ROOM_ID = '00000000-0000-0000-0000-000000000000'
DEFAULT_ROOM_NAME = 'Some Room Name'
print(options.allow_peer_ids)

sslctx = None
if SERVER_ADDR.startswith(('wss://', 'https://')):
    sslctx = ssl.create_default_context()
    # FIXME
    sslctx.check_hostname = False
    sslctx.verify_mode = ssl.CERT_NONE

class Context:
    def __init__(self, ws, peer_id):
        self.next_request_id = 0
        self.ws = ws
        self.id = peer_id

    def build_request(self, args, **kwargs):
        req =  {
            'type': 'request',
            'request_id': str(self.next_request_id),
            #'room_id': room_id,
            'args': ['room'] + args
        }
        self.next_request_id += 1
        req.update(**kwargs)
        return req

    def list_rooms(self):
        return self.build_request(['list'])

    def create_room(self, room_id=DEFAULT_ROOM_ID):
        return self.build_request(['create', DEFAULT_ROOM_NAME], room_id=room_id)

    def destroy_room(self, room_id=DEFAULT_ROOM_ID):
        return self.build_request(['destroy'], room_id=room_id)

    def join_room(self, room_id=DEFAULT_ROOM_ID):
        return self.build_request(['join'], room_id=room_id)

    def allow_member(self, peer_ids, room_id=DEFAULT_ROOM_ID):
        return self.build_request(['edit', 'allow'] + peer_ids, room_id=room_id)

    def message_room(self, peer_ids, room_id=DEFAULT_ROOM_ID):
        return self.build_request(['message', 'SOME_MESSAGE', peer_ids], room_id=room_id)

    async def send_requests(self, requests):
        s = json.dumps(requests)
        print(f'>>> {s}')
        await self.ws.send(s)
        return await self.check_responses(requests)

    async def check_responses(self, requests):
        is_response = False
        responses = []
        replies = []
        try:
            while True:
                reply = json.loads(await self.ws.recv())
                for rsp in reply:
                    if rsp['type'] == 'response':
                        is_response = True
                        responses.append(reply)
                    if not is_response:
                        replies.append(reply)
                        break
                    if rsp['status_code'] != 200:
                        for req in requests:
                            if req['request_id'] == rsp['request_id']:
                                print(f"!!! Request {req} failed")
                                print(f">>> {rsp}")
                if is_response or requests is None:
                    break
        except (ValueError, TypeError):
            print(reply)
            raise
        return responses, replies

    async def identify(self):
        await self.ws.send('IDENTIFY ' + self.id)
        assert(await self.ws.recv() == 'IDENTIFIED')
        print("Identified")

    async def loop(self):
        await self.identify()
        responses, replies = await self.send_requests([self.list_rooms()])
        print(f'<<< {responses}')
        if replies:
            print(f'<<< {replies}')
        await self.send_requests([self.create_room(), self.join_room()])
        responses, replies = await self.send_requests([self.list_rooms()])
        print(f'<<< {responses}')
        if replies:
            print(f'<<< {replies}')
        if options.allow_peer_ids:
            await self.send_requests([self.allow_member(options.allow_peer_ids)])
        # Send a message to yourself (easy way to test)
        responses, replies = await self.send_requests([self.message_room([self.id])])
        print(f'<<< {responses}')
        if replies:
            print(f'<<< {replies}')

        print("Waiting for other messages")
        while True:
            responses, replies = await self.check_responses(None)
            print(f'<<< {responses}')
            if replies:
                print(f'<<< {replies}')


def reply_sdp_ice(msg):
    # Here we'd parse the incoming JSON message for ICE and SDP candidates
    print("Got: " + msg)
    reply = json.dumps({'sdp': 'reply sdp'})
    print("Sent: " + reply)
    return reply

def send_sdp_ice():
    reply = json.dumps({'sdp': 'initial sdp'})
    print("Sent: " + reply)
    return reply

print('Our uid is {!r}'.format(options.peer_id))

async def main():
    async with websockets.connect(SERVER_ADDR, ssl=sslctx) as ws:
        ctx = Context(ws, options.peer_id)
        await ctx.loop()

try:
        asyncio.run(main())
except websockets.exceptions.InvalidHandshake:
    print('Invalid handshake: are you sure this is a websockets server?\n')
    raise
except ssl.SSLError:
    print('SSL Error: are you sure the server is using TLS?\n')
    raise
