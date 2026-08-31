#!/usr/bin/env python3
"""Bridge JSONL on stdio to one exact loopback WebSocket app-server."""

from __future__ import annotations

import base64
import hashlib
import os
import socket
import struct
import sys
import threading
from urllib.parse import urlsplit


GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_HEADER_BYTES = 16 * 1024
MAX_MESSAGE_BYTES = 16 * 1024 * 1024


class BridgeError(Exception):
    pass


class BufferedSocket:
    def __init__(self, sock: socket.socket):
        self.sock = sock
        self.buffer = bytearray()

    def read_exact(self, size: int) -> bytes:
        while len(self.buffer) < size:
            chunk = self.sock.recv(max(4096, size - len(self.buffer)))
            if not chunk:
                raise EOFError("websocket closed without a close frame")
            self.buffer.extend(chunk)
        result = bytes(self.buffer[:size])
        del self.buffer[:size]
        return result

    def read_headers(self) -> bytes:
        marker = b"\r\n\r\n"
        while marker not in self.buffer:
            if len(self.buffer) >= MAX_HEADER_BYTES:
                raise BridgeError("websocket handshake headers too large")
            chunk = self.sock.recv(4096)
            if not chunk:
                raise BridgeError("websocket closed during handshake")
            self.buffer.extend(chunk)
        end = self.buffer.index(marker) + len(marker)
        result = bytes(self.buffer[:end])
        del self.buffer[:end]
        return result


class WebSocketClient:
    def __init__(self, url: str):
        parsed = urlsplit(url)
        try:
            port = parsed.port
        except ValueError as exc:
            raise BridgeError("invalid app-server URL") from exc
        if (
            parsed.scheme != "ws"
            or parsed.hostname != "127.0.0.1"
            or parsed.username is not None
            or parsed.password is not None
            or parsed.path
            or parsed.query
            or parsed.fragment
            or port is None
            or port < 1
            or parsed.netloc != f"127.0.0.1:{port}"
        ):
            raise BridgeError("invalid app-server URL")
        self.host = "127.0.0.1"
        self.port = port
        self.sock = socket.create_connection((self.host, self.port), timeout=10)
        self.sock.settimeout(None)
        self.reader = BufferedSocket(self.sock)
        self.write_lock = threading.Lock()
        self.close_sent = False
        self._handshake()

    def _handshake(self) -> None:
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            "GET / HTTP/1.1\r\n"
            f"Host: {self.host}:{self.port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(request.encode("ascii"))
        raw = self.reader.read_headers()
        lines = raw.decode("ascii", errors="strict").split("\r\n")
        if not lines or " 101 " not in f" {lines[0]} ":
            raise BridgeError("websocket handshake rejected")
        headers: dict[str, str] = {}
        for line in lines[1:]:
            if ":" not in line:
                continue
            name, value = line.split(":", 1)
            headers[name.lower()] = value.strip()
        expected = base64.b64encode(hashlib.sha1((key + GUID).encode("ascii")).digest()).decode(
            "ascii"
        )
        connection_tokens = {
            token.strip().lower() for token in headers.get("connection", "").split(",")
        }
        if (
            headers.get("upgrade", "").lower() != "websocket"
            or "upgrade" not in connection_tokens
            or headers.get("sec-websocket-accept") != expected
        ):
            raise BridgeError("websocket handshake validation failed")

    def send_frame(self, opcode: int, payload: bytes = b"", *, final: bool = True) -> None:
        if opcode >= 0x8 and (not final or len(payload) > 125):
            raise BridgeError("invalid websocket control frame")
        first = (0x80 if final else 0) | opcode
        length = len(payload)
        if length < 126:
            header = bytes((first, 0x80 | length))
        elif length < 65536:
            header = bytes((first, 0x80 | 126)) + struct.pack("!H", length)
        else:
            header = bytes((first, 0x80 | 127)) + struct.pack("!Q", length)
        mask = os.urandom(4)
        masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        with self.write_lock:
            self.sock.sendall(header + mask + masked)
        if opcode == 0x8:
            self.close_sent = True

    def receive_frame(self) -> tuple[bool, int, bytes]:
        first, second = self.reader.read_exact(2)
        final = bool(first & 0x80)
        if first & 0x70:
            raise BridgeError("websocket RSV bits are unsupported")
        opcode = first & 0x0F
        if second & 0x80:
            raise BridgeError("server websocket frames must not be masked")
        length = second & 0x7F
        if length == 126:
            length = struct.unpack("!H", self.reader.read_exact(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self.reader.read_exact(8))[0]
        if length > MAX_MESSAGE_BYTES:
            raise BridgeError("websocket frame too large")
        if opcode >= 0x8 and (not final or length > 125):
            raise BridgeError("invalid websocket control frame")
        return final, opcode, self.reader.read_exact(length)

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def run(url: str) -> None:
    client = WebSocketClient(url)
    sender_error: list[BaseException] = []

    def send_stdin() -> None:
        try:
            for line in sys.stdin:
                client.send_frame(0x1, line.rstrip("\n").encode("utf-8"))
        except BaseException as exc:
            sender_error.append(exc)
            try:
                client.sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    sender = threading.Thread(target=send_stdin, daemon=True)
    sender.start()
    fragmented_opcode: int | None = None
    fragments = bytearray()
    try:
        while True:
            final, opcode, payload = client.receive_frame()
            if opcode == 0x8:
                if not client.close_sent:
                    client.send_frame(0x8, payload)
                return
            if opcode == 0x9:
                client.send_frame(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            if opcode == 0x2:
                raise BridgeError("binary websocket messages are unsupported")
            if opcode == 0x1:
                if fragmented_opcode is not None:
                    raise BridgeError("new websocket message during fragmentation")
                if final:
                    print(payload.decode("utf-8", errors="strict"), flush=True)
                else:
                    fragmented_opcode = opcode
                    fragments.extend(payload)
                continue
            if opcode == 0x0:
                if fragmented_opcode is None:
                    raise BridgeError("unexpected websocket continuation frame")
                fragments.extend(payload)
                if len(fragments) > MAX_MESSAGE_BYTES:
                    raise BridgeError("websocket message too large")
                if final:
                    print(bytes(fragments).decode("utf-8", errors="strict"), flush=True)
                    fragments.clear()
                    fragmented_opcode = None
                continue
            raise BridgeError("unsupported websocket opcode")
    finally:
        client.close()
        sender.join(timeout=1)
        if sender_error:
            raise BridgeError("stdin bridge failed") from sender_error[0]


def main() -> None:
    if len(sys.argv) != 2:
        print("usage: codex_ws_bridge.py ws://127.0.0.1:PORT", file=sys.stderr)
        raise SystemExit(2)
    try:
        run(sys.argv[1])
    except (BridgeError, EOFError, OSError, UnicodeError) as exc:
        print(f"codex_ws_bridge_error: {exc}", file=sys.stderr)
        raise SystemExit(2)


if __name__ == "__main__":
    main()
