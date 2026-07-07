#!/usr/bin/env python3
"""Forward a local serial port as a transparent TCP byte stream."""

from __future__ import annotations

import argparse
import signal
import socket
import sys
import threading
import time
from typing import Dict, List, Optional, Tuple


PARITY_NAMES = {
    "none": "N",
    "even": "E",
    "odd": "O",
    "mark": "M",
    "space": "S",
}

STOPBITS_NAMES = {
    "1": 1,
    "1.5": 1.5,
    "2": 2,
}


def parse_listen_addr(value: str) -> Tuple[str, int]:
    value = value.strip()
    if not value:
        raise ValueError("listen address is empty")

    if value.startswith("["):
        end = value.find("]")
        if end < 0 or end + 2 > len(value) or value[end + 1] != ":":
            raise ValueError("IPv6 listen address must look like [::1]:5657")
        host = value[1:end]
        port_text = value[end + 2 :]
    else:
        if ":" not in value:
            raise ValueError("listen address must include host:port")
        host, port_text = value.rsplit(":", 1)

    host = host or "0.0.0.0"
    try:
        port = int(port_text, 10)
    except ValueError as exc:
        raise ValueError(f"invalid TCP port: {port_text}") from exc
    if not 0 < port < 65536:
        raise ValueError(f"TCP port out of range: {port}")
    return host, port


def open_serial(args: argparse.Namespace):
    try:
        import serial
    except ImportError:
        print(
            "pyserial is required: python3 -m pip install pyserial",
            file=sys.stderr,
        )
        return None

    try:
        port = serial.Serial(
            port=args.serial,
            baudrate=args.baud,
            bytesize=args.bytesize,
            parity=PARITY_NAMES[args.parity],
            stopbits=STOPBITS_NAMES[args.stopbits],
            timeout=0.05,
            write_timeout=args.write_timeout,
            rtscts=args.rtscts,
            xonxoff=args.xonxoff,
        )
        port.dtr = not args.no_dtr
        port.rts = not args.no_rts
        if args.clear:
            port.reset_input_buffer()
            port.reset_output_buffer()
        return port
    except Exception as exc:
        print(f"failed to open serial {args.serial}: {exc}", file=sys.stderr)
        return None


class SerialTcpForwarder:
    def __init__(
        self,
        serial_port,
        listen_host: str,
        listen_port: int,
        chunk_size: int,
        multi_client: bool,
        replace_client: bool,
        quiet: bool,
    ) -> None:
        self.serial_port = serial_port
        self.listen_host = listen_host
        self.listen_port = listen_port
        self.chunk_size = max(1, chunk_size)
        self.multi_client = multi_client
        self.replace_client = replace_client
        self.quiet = quiet
        self.clients: Dict[socket.socket, str] = {}
        self.clients_lock = threading.Lock()
        self.serial_write_lock = threading.Lock()
        self.stop_event = threading.Event()
        self.server_sock: Optional[socket.socket] = None

    def log(self, message: str) -> None:
        if not self.quiet:
            print(message, file=sys.stderr, flush=True)

    def run(self) -> int:
        family = socket.AF_INET6 if ":" in self.listen_host else socket.AF_INET
        self.server_sock = socket.socket(family, socket.SOCK_STREAM)
        self.server_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            self.server_sock.bind((self.listen_host, self.listen_port))
            self.server_sock.listen()
            self.server_sock.settimeout(0.2)
        except OSError as exc:
            print(
                f"failed to listen on {self.listen_host}:{self.listen_port}: {exc}",
                file=sys.stderr,
            )
            self.close()
            return 1

        self.log(
            f"serial {self.serial_port.port} @ {self.serial_port.baudrate} "
            f"-> tcp {self.listen_host}:{self.listen_port}"
        )
        if self.multi_client:
            self.log("multi-client mode enabled; concurrent writes may interleave")
        else:
            policy = "replace" if self.replace_client else "reject"
            self.log(f"single-client mode; extra clients will {policy}")

        signal.signal(signal.SIGINT, self._on_signal)
        signal.signal(signal.SIGTERM, self._on_signal)

        threads = [
            threading.Thread(target=self._accept_loop, name="tcp-accept", daemon=True),
            threading.Thread(target=self._serial_to_tcp_loop, name="serial-rx", daemon=True),
        ]
        for thread in threads:
            thread.start()

        try:
            while not self.stop_event.is_set():
                time.sleep(0.2)
        finally:
            self.close()
        return 0

    def _on_signal(self, _sig, _frame) -> None:
        self.stop_event.set()
        if self.server_sock is not None:
            try:
                self.server_sock.close()
            except OSError:
                pass

    def _accept_loop(self) -> None:
        while not self.stop_event.is_set():
            try:
                assert self.server_sock is not None
                client, addr = self.server_sock.accept()
                client.settimeout(0.2)
                peer = format_peer(addr)
                if not self._register_client(client, peer):
                    self._close_socket(client)
                    continue
                self.log(f"client connected: {peer}")
                thread = threading.Thread(
                    target=self._tcp_to_serial_loop,
                    args=(client, peer),
                    name=f"tcp-rx-{peer}",
                    daemon=True,
                )
                thread.start()
            except socket.timeout:
                continue
            except OSError:
                break

    def _register_client(self, client: socket.socket, peer: str) -> bool:
        clients_to_close: List[socket.socket] = []
        with self.clients_lock:
            if not self.multi_client and self.clients:
                if not self.replace_client:
                    self.log(f"client rejected: {peer}; existing client is active")
                    return False
                clients_to_close = list(self.clients.keys())
                self.clients.clear()
            self.clients[client] = peer

        for existing in clients_to_close:
            self._close_socket(existing)
        return True

    def _serial_to_tcp_loop(self) -> None:
        while not self.stop_event.is_set():
            try:
                waiting = getattr(self.serial_port, "in_waiting", 0) or 1
                size = min(self.chunk_size, max(1, waiting))
                data = self.serial_port.read(size)
                if data:
                    self._broadcast(data)
            except Exception as exc:
                print(f"serial read failed: {exc}", file=sys.stderr)
                self.stop_event.set()
                break

    def _tcp_to_serial_loop(self, client: socket.socket, peer: str) -> None:
        while not self.stop_event.is_set():
            try:
                data = client.recv(self.chunk_size)
                if not data:
                    break
                with self.serial_write_lock:
                    self.serial_port.write(data)
                    self.serial_port.flush()
            except socket.timeout:
                continue
            except OSError:
                break
            except Exception as exc:
                print(f"serial write failed for {peer}: {exc}", file=sys.stderr)
                self.stop_event.set()
                break
        self._remove_client(client, peer)

    def _broadcast(self, data: bytes) -> None:
        with self.clients_lock:
            clients = list(self.clients.items())

        dead: List[Tuple[socket.socket, str]] = []
        for client, peer in clients:
            try:
                client.sendall(data)
            except OSError:
                dead.append((client, peer))

        for client, peer in dead:
            self._remove_client(client, peer)

    def _remove_client(self, client: socket.socket, peer: str) -> None:
        removed = False
        with self.clients_lock:
            if client in self.clients:
                self.clients.pop(client, None)
                removed = True
        self._close_socket(client)
        if removed:
            self.log(f"client disconnected: {peer}")

    def close(self) -> None:
        self.stop_event.set()
        if self.server_sock is not None:
            self._close_socket(self.server_sock)
            self.server_sock = None

        with self.clients_lock:
            clients = list(self.clients.keys())
            self.clients.clear()
        for client in clients:
            self._close_socket(client)

        try:
            self.serial_port.close()
        except Exception:
            pass

    @staticmethod
    def _close_socket(sock: socket.socket) -> None:
        try:
            sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            sock.close()
        except OSError:
            pass


def format_peer(addr) -> str:
    if isinstance(addr, tuple) and len(addr) >= 2:
        return f"{addr[0]}:{addr[1]}"
    return str(addr)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Forward a local serial device as a transparent TCP byte stream.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("serial_path", nargs="?", help="serial device, e.g. /dev/ttyACM0 or COM3")
    parser.add_argument("listen_addr", nargs="?", help="listen address, e.g. 127.0.0.1:5657")
    parser.add_argument("--serial", dest="serial_opt", help="serial device path")
    parser.add_argument("--listen", dest="listen_opt", help="TCP listen host:port")
    parser.add_argument("--baud", type=int, default=115200, help="serial baud rate")
    parser.add_argument("--bytesize", type=int, choices=(5, 6, 7, 8), default=8)
    parser.add_argument("--parity", choices=tuple(PARITY_NAMES.keys()), default="none")
    parser.add_argument("--stopbits", choices=tuple(STOPBITS_NAMES.keys()), default="1")
    parser.add_argument("--write-timeout", type=float, default=1.0)
    parser.add_argument("--chunk-size", type=int, default=4096)
    parser.add_argument("--rtscts", action="store_true", help="enable RTS/CTS flow control")
    parser.add_argument("--xonxoff", action="store_true", help="enable software flow control")
    parser.add_argument("--no-dtr", action="store_true", help="leave DTR low after opening")
    parser.add_argument("--no-rts", action="store_true", help="leave RTS low after opening")
    parser.add_argument("--clear", action="store_true", help="clear serial input/output buffers on start")
    parser.add_argument("--multi-client", action="store_true", help="broadcast serial RX to all TCP clients")
    parser.add_argument(
        "--replace-client",
        action="store_true",
        help="in single-client mode, close the old client when a new client connects",
    )
    parser.add_argument("-q", "--quiet", action="store_true", help="only print errors")
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    args.serial = args.serial_opt or args.serial_path
    listen = args.listen_opt or args.listen_addr

    if not args.serial:
        parser.error("serial device is required, e.g. --serial /dev/ttyACM0")
    if not listen:
        parser.error("listen address is required, e.g. --listen 127.0.0.1:5657")

    try:
        host, port = parse_listen_addr(listen)
    except ValueError as exc:
        parser.error(str(exc))

    serial_port = open_serial(args)
    if serial_port is None:
        return 1

    forwarder = SerialTcpForwarder(
        serial_port=serial_port,
        listen_host=host,
        listen_port=port,
        chunk_size=args.chunk_size,
        multi_client=args.multi_client,
        replace_client=args.replace_client,
        quiet=args.quiet,
    )
    return forwarder.run()


if __name__ == "__main__":
    raise SystemExit(main())
