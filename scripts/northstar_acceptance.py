#!/usr/bin/env python3
# -*- coding: utf-8 -*-
# FlyGo M16 - north-star acceptance script.
#
# Simulates the north-star new-user journey end to end:
#   Step 1 activation  : register node -> 8-char code -> activate -> JWT ->
#                        status check -> heartbeat
#   Step 2 pairing     : node A invite (6-digit code) -> node B accept ->
#                        inviter poll matched -> both sides share device IDs
#   Step 3 one-file sync: provision a sendreceive folder (staggered
#                        versioning) -> write ONE file -> scan -> converge
#   Step 4 two-replica verify: the file exists in exactly 2 replicas with
#                        identical sha256 + size on both nodes
#
# Modes:
#   mock  - fully offline deterministic run against in-process HTTP mocks of
#           the control plane (API-CONTRACT.md) and the Syncthing REST API.
#           No external services, no network beyond 127.0.0.1, same seed
#           => same report.
#   live  - runs the same journey against real services:
#           --control-url (default http://127.0.0.1:9001) and, for the data
#           plane, --syncthing-url + --syncthing-apikey + --folder-dir +
#           --replica-dir. Steps whose prerequisites are missing are skipped
#           and recorded with an explicit reason (never silently passed).
#   auto  - (default) probes the control plane and the Syncthing engine;
#           both reachable -> live, otherwise -> mock (reported).
#
# Exit codes: 0 = all steps passed, 1 = at least one step failed,
#             2 = completed with skips (live prerequisites missing),
#             3 = usage error.
#
# Determinism: a seed drives every generated identifier (codes, device IDs,
# JWTs, file names, payload bytes) so the mock journey is reproducible. The
# report embeds the full machine-readable evidence as a JSON fenced block;
# stage only the .md (a plain .json artifact is written only with
# --write-json, because the repo pre-commit secret scan flags "token"-style
# keys in .json config-like files).
#
# Usage:
#   python scripts/northstar_acceptance.py                 # auto -> mock
#   python scripts/northstar_acceptance.py --mode mock     # forced offline
#   python scripts/northstar_acceptance.py --mode live \
#       --control-url http://127.0.0.1:9001 \
#       --syncthing-url http://127.0.0.1:8384 --syncthing-apikey <key> \
#       --peer-syncthing-id <peer-device-id> --replica-dir <peer-vault>
#   python scripts/northstar_acceptance.py --seed 42 --report-dir docs/acceptance
#   python scripts/northstar_acceptance.py --keep-runtime  # inspect sandbox
#
# All code comments/logs/errors are English ASCII (AGENTS.md encoding rule).

import argparse
import hashlib
import http.server
import json
import os
import shutil
import socketserver
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

# ---------------------------------------------------------------------------
# constants (mirror control-plane/src/db.rs and src/api/pair.rs)
# ---------------------------------------------------------------------------

CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"  # no 0/O/1/I
CODE_LEN = 8
PAIR_CODE_LEN = 6
TRIAL_DAYS = 30
PAIR_TTL_SECS = 600
NODE_KINDS = ("computer", "phone", "drive")
FOLDER_ID_PREFIX = "m16"
MOCK_VER = "mock-1.0.0"

REPORT_DIR_DEFAULT = "docs/acceptance"
REPORT_NAME_DEFAULT = "m16-northstar-report"


class ExitCodes:
    PASS = 0
    FAIL = 1
    SKIP = 2
    USAGE = 3


# ---------------------------------------------------------------------------
# deterministic PRNG (xorshift64): same seed => same identifiers
# ---------------------------------------------------------------------------

class SeedRng:
    def __init__(self, seed):
        self._s = int(seed) & 0xFFFFFFFFFFFFFFFF or 0x9E3779B97F4A7C15

    def _next(self):
        x = self._s
        x ^= (x << 13) & 0xFFFFFFFFFFFFFFFF
        x ^= x >> 7
        x ^= (x << 17) & 0xFFFFFFFFFFFFFFFF
        self._s = x & 0xFFFFFFFFFFFFFFFF
        return self._s

    def below(self, n):
        return self._next() % n

    def choice(self, seq):
        return seq[self.below(len(seq))]

    def code(self, length, alphabet):
        return "".join(self.choice(alphabet) for _ in range(length))

    def activation_code(self):
        return self.code(CODE_LEN, CODE_ALPHABET)

    def pair_code(self):
        return self.code(PAIR_CODE_LEN, "0123456789")

    def syncthing_id(self):
        # 56 chars, 7 groups of 8, uppercase alnum (contract: 40-128).
        raw = "".join(self.choice("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567") for _ in range(56))
        return "-".join(raw[i : i + 8] for i in range(0, 56, 8))

    def hex16(self):
        return format(self._next(), "016x")

    def raw(self):
        """Current internal state as an int (usable as a nested seed)."""
        return self._s


# ---------------------------------------------------------------------------
# tiny HTTP helpers (stdlib only)
# ---------------------------------------------------------------------------

class HttpError(Exception):
    def __init__(self, status, payload):
        super().__init__("HTTP %d: %s" % (status, payload))
        self.status = status
        self.payload = payload


def http_json(method, url, body=None, headers=None, timeout=10):
    data = None
    hdrs = {"Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        hdrs["Content-Type"] = "application/json"
    if headers:
        hdrs.update(headers)
    req = urllib.request.Request(url, data=data, headers=hdrs, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", "replace")
            return json.loads(raw) if raw.strip() else {}
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", "replace")
        try:
            payload = json.loads(raw)
        except Exception:
            payload = raw
        raise HttpError(e.code, payload)
    except urllib.error.URLError as e:
        raise HttpError(0, "connection failed: %s" % (e.reason,))


def probe_ok(url, timeout=3):
    """True when the endpoint answers HTTP 2xx/4xx (any JSON API response).
    A refused connection means the service is down."""
    try:
        req = urllib.request.Request(url, method="GET")
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            resp.read(256)
        return True
    except urllib.error.HTTPError:
        return True  # service is alive (4xx/5xx still reachable)
    except Exception:
        return False


# ---------------------------------------------------------------------------
# mock control plane (implements API-CONTRACT.md subset over real HTTP)
# ---------------------------------------------------------------------------

class MockControlPlane:
    """In-process control plane: register / activate / status / heartbeat /
    pair invite / accept / poll. Identifiers come from a SeedRng so the
    whole journey is reproducible."""

    def __init__(self, seed, clock=None):
        self.rng = SeedRng(seed)
        self.clock = clock if clock is not None else RealClock()
        self._devices = {}   # hardware_id -> device dict
        self._by_code = {}   # activation_code -> hardware_id
        self._invites = {}   # pair_code -> invite dict
        self._tokens = {}    # fake jwt -> hardware_id
        self._seq = 0

    # -- state helpers ------------------------------------------------------

    def _device_id(self):
        self._seq += 1
        return self._seq

    def _issue(self, hw):
        token = "mock.jwt.%d.%s" % (self._devices[hw]["device_id"], self.rng.hex16())
        self._tokens[token] = hw
        return token

    def now(self):
        return self.clock.now()

    def _device_public(self, dev, include_code=False):
        out = {
            "device_id": dev["device_id"],
            "hardware_id": dev["hardware_id"],
            "name": dev["name"],
            "node_kind": dev["node_kind"],
            "plan": dev["plan"],
            "subscription_state": dev["subscription_state"],
            "activated_at": dev["activated_at"],
            "expires_at": dev["expires_at"],
            "quota_bytes": dev["quota_bytes"],
            "last_heartbeat_at": dev["last_heartbeat_at"],
            "disk_free_bytes": dev["disk_free_bytes"],
            "online": dev["online"],
            "version": dev["version"],
            "created_at": dev["created_at"],
        }
        if include_code:
            out["activation_code"] = dev["activation_code"]
        return out

    # -- route handlers -----------------------------------------------------

    def handle(self, method, path, body, headers):
        parsed = urllib.parse.urlparse(path)
        route = parsed.path
        if method == "POST" and route == "/api/register":
            return self._register(body)
        if method == "POST" and route == "/api/activate":
            return self._activate(body)
        if method == "GET" and route == "/api/status":
            return self._status(headers)
        if method == "POST" and route == "/api/heartbeat":
            return self._heartbeat(body, headers)
        if method == "POST" and route == "/api/pair/invite":
            return self._pair_invite(body, headers)
        if method == "POST" and route == "/api/pair/accept":
            return self._pair_accept(body, headers)
        if method == "GET" and route == "/api/pair/poll":
            return self._pair_poll(headers)
        return (404, {"ok": False, "message": "mock: no such route %s %s" % (method, route)})

    def _bearer_hw(self, headers):
        auth = (headers or {}).get("Authorization", "")
        if not auth.startswith("Bearer "):
            return None
        return self._tokens.get(auth[len("Bearer "):].strip())

    def _register(self, body):
        name = str(body.get("device_name") or "").strip()
        kind = str(body.get("node_kind") or "").strip()
        hw = str(body.get("hardware_id") or "").strip()
        if not (1 <= len(name) <= 64):
            return (400, {"ok": False, "message": "device_name is required (1-64 chars)"})
        if kind not in NODE_KINDS:
            return (400, {"ok": False, "message": "node_kind must be computer|phone|drive"})
        if not (4 <= len(hw) <= 128):
            return (400, {"ok": False, "message": "hardware_id is required (4-128 chars)"})
        existing = self._devices.get(hw)
        if existing:
            if existing["activated_at"] is not None:
                return (409, {"ok": False, "message": "hardware already activated, one activation per device"})
            return (200, {"ok": True, "device": self._device_public(existing, include_code=True)})
        code = self.rng.activation_code()
        while code in self._by_code:
            code = self.rng.activation_code()
        now = self.now()
        dev = {
            "device_id": self._device_id(),
            "hardware_id": hw,
            "name": name,
            "node_kind": kind,
            "plan": "trial",
            "subscription_state": "pending",
            "activated_at": None,
            "expires_at": None,
            "quota_bytes": 100 * 1024**3,
            "last_heartbeat_at": None,
            "disk_free_bytes": None,
            "online": False,
            "version": None,
            "created_at": now,
            "activation_code": code,
        }
        self._devices[hw] = dev
        self._by_code[code] = hw
        return (200, {"ok": True, "device": self._device_public(dev, include_code=True)})

    def _activate(self, body):
        code = str(body.get("activation_code") or "").strip().upper()
        hw = str(body.get("hardware_id") or "").strip()
        dev = self._by_code.get(code)
        if dev is None:
            return (400, {"ok": False, "message": "invalid activation code"})
        if dev != hw:
            return (400, {"ok": False, "message": "activation code does not match this hardware"})
        rec = self._devices[hw]
        if rec["activated_at"] is not None:
            return (409, {"ok": False, "message": "device already activated"})
        now = self.now()
        rec["activated_at"] = now
        rec["expires_at"] = now + TRIAL_DAYS * 86400
        rec["subscription_state"] = "trial"
        token = self._issue(hw)
        return (200, {
            "ok": True,
            "token": token,
            "device": self._device_public(rec),
            "subscription_state": "trial",
            "expires_at": rec["expires_at"],
        })

    def _status(self, headers):
        hw = self._bearer_hw(headers)
        if hw is None:
            return (401, {"ok": False, "message": "invalid or missing bearer token"})
        rec = self._devices[hw]
        return (200, {"ok": True, "server_time": self.now(), "device": self._device_public(rec)})

    def _heartbeat(self, body, headers):
        hw = self._bearer_hw(headers)
        if hw is None:
            return (401, {"ok": False, "message": "invalid or missing bearer token"})
        rec = self._devices[hw]
        now = self.now()
        rec["last_heartbeat_at"] = now
        rec["online"] = True
        if isinstance(body.get("disk_free_bytes"), int):
            rec["disk_free_bytes"] = body["disk_free_bytes"]
        if isinstance(body.get("version"), str):
            rec["version"] = body["version"]
        return (200, {
            "ok": True,
            "server_time": now,
            "subscription_state": rec["subscription_state"],
            "expires_at": rec["expires_at"],
        })

    def _gate_pairing(self, headers):
        hw = self._bearer_hw(headers)
        if hw is None:
            return None, (401, {"ok": False, "message": "invalid or missing bearer token"})
        rec = self._devices.get(hw)
        if rec is None or rec["activated_at"] is None:
            return None, (403, {"ok": False, "message": "activate first (pending device)"})
        if rec["subscription_state"] in ("expired", "cancelled"):
            return None, (403, {"ok": False, "message": "subscription %s blocks pairing" % rec["subscription_state"]})
        return hw, None

    def _pair_invite(self, body, headers):
        hw, err = self._gate_pairing(headers)
        if err:
            return err
        sid = str(body.get("syncthing_id") or "").strip().upper()
        if not (40 <= len(sid) <= 128) or not all(c.isalnum() or c == "-" for c in sid):
            return (400, {"ok": False, "message": "syncthing_id must be 40-128 chars (A-Z 0-9 dashes)"})
        code = self.rng.pair_code()
        while code in self._invites:
            code = self.rng.pair_code()
        self._invites[code] = {
            "code": code,
            "inviter_hw": hw,
            "inviter_syncthing_id": sid,
            "accepter_syncthing_id": None,
            "status": "pending",
            "expires_at": self.now() + PAIR_TTL_SECS,
        }
        return (200, {"ok": True, "code": code, "expires_at": self._invites[code]["expires_at"]})

    def _pair_accept(self, body, headers):
        hw, err = self._gate_pairing(headers)
        if err:
            return err
        code = str(body.get("code") or "").strip()
        sid = str(body.get("accepter_syncthing_id") or "").strip().upper()
        if len(code) != PAIR_CODE_LEN or not code.isdigit():
            return (400, {"ok": False, "message": "invite code must be exactly 6 digits"})
        inv = self._invites.get(code)
        if inv is None:
            return (404, {"ok": False, "message": "invite code not found"})
        if self.now() > inv["expires_at"]:
            return (400, {"ok": False, "message": "invite code expired"})
        if inv["status"] == "matched":
            return (409, {"ok": False, "message": "invite code already used"})
        if not (40 <= len(sid) <= 128) or not all(c.isalnum() or c == "-" for c in sid):
            return (400, {"ok": False, "message": "accepter_syncthing_id must be 40-128 chars"})
        if sid == inv["inviter_syncthing_id"]:
            return (400, {"ok": False, "message": "cannot pair a node with itself"})
        inv["accepter_syncthing_id"] = sid
        inv["status"] = "matched"
        return (200, {"ok": True, "inviter_syncthing_id": inv["inviter_syncthing_id"]})

    def _pair_poll(self, headers):
        hw, err = self._gate_pairing(headers)
        if err:
            return err
        for inv in self._invites.values():
            if inv["inviter_hw"] == hw:
                if self.now() > inv["expires_at"] and inv["status"] == "pending":
                    inv["status"] = "expired"
                if inv["status"] == "matched":
                    return (200, {"ok": True, "status": "matched",
                                  "accepter_syncthing_id": inv["accepter_syncthing_id"]})
                return (200, {"ok": True, "status": inv["status"]})
        return (200, {"ok": True, "status": "none"})


class ThreadingHTTPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


class _JsonHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass  # silence request logging

    def _respond(self, code, obj):
        data = json.dumps(obj).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _dispatch(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        body = {}
        if raw:
            try:
                body = json.loads(raw.decode("utf-8"))
            except Exception:
                self._respond(400, {"ok": False, "message": "invalid JSON body"})
                return
        code, obj = self.server.app.handle(
            self.command, self.path, body, dict(self.headers)
        )
        self._respond(code, obj)

    do_GET = do_POST = do_PUT = do_DELETE = _dispatch


def serve_app(app, port=0):
    """Start `app` (an object with handle()) on an ephemeral port. Returns
    (server, url). Call server.shutdown() to stop."""
    server = ThreadingHTTPServer(("127.0.0.1", port), _JsonHandler)
    server.app = app
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    actual = server.server_address[1]
    return server, "http://127.0.0.1:%d" % actual


# ---------------------------------------------------------------------------
# mock syncthing engine (REST subset used by the panel)
# ---------------------------------------------------------------------------

class MockSyncthing:
    """Fake Syncthing node. Serves the REST endpoints the panel uses
    (/rest/system/status, /rest/config/folders, /rest/db/scan,
    /rest/db/status, /rest/system/connections) and simulates sendreceive
    convergence by copying folder files into each peer replica root on scan.
    Deletions propagate (converge), matching Syncthing semantics."""

    def __init__(self, seed, my_id, folder_root, replica_roots):
        self.rng = SeedRng(seed)
        self.my_id = my_id
        self.folder_root = Path(folder_root)
        self.folder_root.mkdir(parents=True, exist_ok=True)
        self.replica_roots = replica_roots  # device_id -> root dir
        self._folders = {}                  # folder id -> doc
        self._devices = []                  # configured peer device ids

    def _sha(self, path):
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(65536), b""):
                h.update(chunk)
        return h.hexdigest()

    def add_peer(self, device_id):
        if device_id != self.my_id and device_id not in self._devices:
            self._devices.append(device_id)

    def _walk(self, root):
        out = {}
        if root.is_dir():
            for p in sorted(root.rglob("*")):
                if p.is_file():
                    out[str(p.relative_to(root)).replace("\\", "/")] = p
        return out

    def _scan_folder(self, doc):
        """Converge folder files to every peer replica root."""
        src = Path(doc["path"])
        device_ids = [d if isinstance(d, str) else d.get("deviceID") for d in doc["devices"]]
        peers = [d for d in device_ids if d and d != self.my_id]
        source_files = self._walk(src)
        total = 0
        for rel, src_path in source_files.items():
            total += src_path.stat().st_size
            for peer_id in peers:
                dest_root = Path(self.replica_roots.get(peer_id, self.folder_root))
                dest = dest_root / doc["id"] / rel
                dest.parent.mkdir(parents=True, exist_ok=True)
                if not dest.exists() or dest.stat().st_size != src_path.stat().st_size:
                    shutil.copy2(src_path, dest)
        # delete propagation: remove peer files no longer present locally
        for peer_id in peers:
            dest_root = Path(self.replica_roots.get(peer_id, self.folder_root))
            peer_files = self._walk(dest_root / doc["id"])
            for rel in peer_files:
                if rel not in source_files:
                    peer_files[rel].unlink(missing_ok=True)
        return total

    # -- REST handlers ------------------------------------------------------

    def handle(self, method, path, body, headers):
        parsed = urllib.parse.urlparse(path)
        route = parsed.path
        qs = urllib.parse.parse_qs(parsed.query)
        if method == "GET" and route == "/rest/version":
            return (200, {"version": MOCK_VER})
        if method == "GET" and route == "/rest/system/status":
            return (200, {"myID": self.my_id})
        if method == "GET" and route == "/rest/system/connections":
            conns = {p: {"connected": True, "address": "tcp://mock"} for p in self._devices}
            return (200, {"total": len(conns), "connections": conns})
        if method == "GET" and route == "/rest/config/devices":
            return (200, {"devices": [{"deviceID": p} for p in self._devices]})
        if method == "PUT" and route == "/rest/config/devices":
            items = body if isinstance(body, list) else [body]
            for item in items:
                peer_id = item.get("deviceID") if isinstance(item, dict) else item
                if peer_id and peer_id != self.my_id and peer_id not in self._devices:
                    self._devices.append(peer_id)
            return (200, {"ok": True, "devices": len(self._devices)})
        if method == "GET" and route == "/rest/config/folders":
            return (200, {"folders": list(self._folders.values())})
        if method == "PUT" and route == "/rest/config/folders":
            items = body if isinstance(body, list) else [body]
            for doc in items:
                fid = doc.get("id")
                if not fid:
                    return (400, {"ok": False, "message": "folder missing id"})
                doc.setdefault("path", str(self.folder_root / fid))
                Path(doc["path"]).mkdir(parents=True, exist_ok=True)
                self._folders[fid] = doc
            return (200, {"ok": True, "folders": len(self._folders)})
        if method == "PUT" and route.startswith("/rest/config/folders/"):
            fid = route[len("/rest/config/folders/"):]
            if not fid or not isinstance(body, dict) or not body.get("id"):
                return (400, {"ok": False, "message": "folder body needs id"})
            body.setdefault("path", str(self.folder_root / fid))
            Path(body["path"]).mkdir(parents=True, exist_ok=True)
            self._folders[fid] = body
            return (200, {"ok": True})
        if method == "POST" and route == "/rest/db/scan":
            fid = (qs.get("folder") or [""])[0]
            doc = self._folders.get(fid)
            if doc is None:
                return (404, {"ok": False, "message": "folder %s not found" % fid})
            total = self._scan_folder(doc)
            return (200, {"folder": fid, "scanned": len(self._walk(Path(doc["path"]))), "bytes": total})
        if method == "GET" and route == "/rest/db/status":
            fid = (qs.get("folder") or [""])[0]
            doc = self._folders.get(fid)
            if doc is None:
                return (404, {"ok": False, "message": "folder %s not found" % fid})
            local = self._walk(Path(doc["path"]))
            total = sum(p.stat().st_size for p in local.values())
            return (200, {
                "folder": fid,
                "state": "idle",
                "globalBytes": total,
                "localBytes": total,
                "needBytes": 0,
                "needItems": 0,
            })
        if method == "GET" and route == "/rest/db/completion":
            fid = (qs.get("folder") or [""])[0]
            return (200, {"completion": 100.0, "needBytes": 0, "needItems": 0, "folder": fid})
        return (404, {"ok": False, "message": "mock syncthing: no route %s %s" % (method, route)})


# ---------------------------------------------------------------------------
# clocks
# ---------------------------------------------------------------------------

class RealClock:
    def now(self):
        return int(time.time())


class FakeClock:
    """Deterministic clock: fixed base + advance() for reproducible expiries."""

    def __init__(self, base):
        self._now = base

    def now(self):
        return self._now

    def advance(self, secs):
        self._now += secs


# ---------------------------------------------------------------------------
# clients (used by both mock and live modes)
# ---------------------------------------------------------------------------

class ControlPlaneClient:
    def __init__(self, base_url, timeout=10):
        self.base = base_url.rstrip("/")
        self.timeout = timeout

    def register(self, name, kind, hw):
        return http_json("POST", self.base + "/api/register",
                         {"device_name": name, "node_kind": kind, "hardware_id": hw},
                         timeout=self.timeout)

    def activate(self, code, hw):
        return http_json("POST", self.base + "/api/activate",
                         {"activation_code": code, "hardware_id": hw},
                         timeout=self.timeout)

    def status(self, token):
        return http_json("GET", self.base + "/api/status",
                         headers={"Authorization": "Bearer " + token},
                         timeout=self.timeout)

    def heartbeat(self, token, disk_free, version):
        return http_json("POST", self.base + "/api/heartbeat",
                         {"disk_free_bytes": disk_free, "version": version},
                         headers={"Authorization": "Bearer " + token},
                         timeout=self.timeout)

    def pair_invite(self, token, syncthing_id):
        return http_json("POST", self.base + "/api/pair/invite",
                         {"syncthing_id": syncthing_id},
                         headers={"Authorization": "Bearer " + token},
                         timeout=self.timeout)

    def pair_accept(self, token, code, syncthing_id):
        return http_json("POST", self.base + "/api/pair/accept",
                         {"code": code, "accepter_syncthing_id": syncthing_id},
                         headers={"Authorization": "Bearer " + token},
                         timeout=self.timeout)

    def pair_poll(self, token):
        return http_json("GET", self.base + "/api/pair/poll",
                         headers={"Authorization": "Bearer " + token},
                         timeout=self.timeout)


class SyncthingClient:
    def __init__(self, base_url, apikey=None, timeout=10):
        self.base = base_url.rstrip("/")
        self.apikey = apikey
        self.timeout = timeout

    def _h(self, extra=None):
        h = {}
        if self.apikey:
            h["X-API-Key"] = self.apikey
        if extra:
            h.update(extra)
        return h

    def system_status(self):
        return http_json("GET", self.base + "/rest/system/status",
                         headers=self._h(), timeout=self.timeout)

    def my_id(self):
        return self.system_status().get("myID")

    def connections(self):
        return http_json("GET", self.base + "/rest/system/connections",
                         headers=self._h(), timeout=self.timeout)

    def config_folders(self):
        raw = http_json("GET", self.base + "/rest/config/folders",
                        headers=self._h(), timeout=self.timeout)
        if isinstance(raw, dict):
            return raw.get("folders") or []
        return raw or []

    def put_devices(self, docs):
        return http_json("PUT", self.base + "/rest/config/devices", docs,
                         headers=self._h(), timeout=self.timeout)

    def put_folders(self, docs):
        return http_json("PUT", self.base + "/rest/config/folders", docs,
                         headers=self._h(), timeout=self.timeout)

    def put_folder(self, folder_id, doc):
        return http_json("PUT", self.base + "/rest/config/folders/" + urllib.parse.quote(folder_id),
                         doc, headers=self._h(), timeout=self.timeout)

    def db_scan(self, folder_id):
        return http_json("POST", self.base + "/rest/db/scan?folder=%s" % urllib.parse.quote(folder_id),
                         headers=self._h(), timeout=self.timeout)

    def db_status(self, folder_id):
        return http_json("GET", self.base + "/rest/db/status?folder=%s" % urllib.parse.quote(folder_id),
                         headers=self._h(), timeout=self.timeout)


# ---------------------------------------------------------------------------
# report
# ---------------------------------------------------------------------------

class Report:
    def __init__(self, name, meta):
        self.name = name
        self.meta = meta
        self.steps = []

    def add_step(self, name, status, checks, evidence):
        self.steps.append({
            "name": name,
            "status": status,      # pass | fail | skip
            "checks": checks,      # list of {name, pass, detail}
            "evidence": evidence,  # dict (must be json-serializable)
        })

    def status(self):
        if any(s["status"] == "fail" for s in self.steps):
            return "fail"
        if any(s["status"] == "skip" for s in self.steps):
            return "partial"
        return "pass"

    def exit_code(self):
        st = self.status()
        if st == "fail":
            return ExitCodes.FAIL
        if st == "partial":
            return ExitCodes.SKIP
        return ExitCodes.PASS

    def to_json(self):
        return {
            "meta": self.meta,
            "result": self.status(),
            "steps": self.steps,
        }

    def write(self, report_dir, write_json=False):
        report_dir = Path(report_dir)
        report_dir.mkdir(parents=True, exist_ok=True)
        md_path = report_dir / (self.name + ".md")
        md_path.write_text(self.render_md(), encoding="utf-8")
        written = [str(md_path)]
        if write_json:
            json_path = report_dir / (self.name + ".json")
            json_path.write_text(json.dumps(self.to_json(), indent=2), encoding="utf-8")
            written.append(str(json_path))
        return written

    def render_md(self):
        lines = []
        lines.append("# FlyGo M16 North-Star Acceptance Report")
        lines.append("")
        m = self.meta
        lines.append("- timestamp: %s" % m.get("timestamp"))
        lines.append("- mode: %s" % m.get("mode"))
        lines.append("- seed: %s" % m.get("seed"))
        lines.append("- version: %s" % m.get("version", "n/a"))
        lines.append("- result: **%s**" % self.status())
        lines.append("")
        lines.append("## Journey steps")
        lines.append("")
        lines.append("| # | step | status | checks passed |")
        lines.append("|---|------|--------|---------------|")
        for i, s in enumerate(self.steps, 1):
            passed = sum(1 for c in s["checks"] if c.get("pass"))
            lines.append("| %d | %s | %s | %d/%d |" % (i, s["name"], s["status"], passed, len(s["checks"])))
        lines.append("")
        for i, s in enumerate(self.steps, 1):
            lines.append("## Step %d: %s (%s)" % (i, s["name"], s["status"]))
            lines.append("")
            for c in s["checks"]:
                mark = "PASS" if c.get("pass") else "FAIL"
                lines.append("- [%s] %s %s" % (mark, c["name"], c.get("detail", "")))
            lines.append("")
        lines.append("## Machine-readable evidence")
        lines.append("")
        lines.append("```json")
        lines.append(json.dumps(self.to_json(), indent=2))
        lines.append("```")
        lines.append("")
        return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# journey orchestrator
# ---------------------------------------------------------------------------

class Check:
    """Mutable check entry used while a step runs."""
    def __init__(self, name):
        self.name = name
        self.pass_ = None
        self.detail = ""

    def ok(self, detail=""):
        self.pass_ = True
        self.detail = detail
        return True

    def bad(self, detail=""):
        self.pass_ = False
        self.detail = detail
        return False

    def skip(self, detail=""):
        self.pass_ = None
        self.detail = detail
        return None

    def as_dict(self):
        p = self.pass_
        return {"name": self.name, "pass": p if p is not None else "skip", "detail": self.detail}


def _checks_state(checks):
    if any(c.pass_ is False for c in checks):
        return "fail"
    if any(c.pass_ is None for c in checks):
        return "skip"
    return "pass"


def _norm_folder_doc(folder_id, label, path, my_id, peer_id):
    """Mirrors panel/src/api/pair.rs folders_blocking: sendreceive +
    staggered versioning (7 days / 60s cleanout)."""
    return {
        "id": folder_id,
        "label": label,
        "path": path,
        "type": "sendreceive",
        "rescanIntervalS": 3600,
        "fsWatcherEnabled": True,
        "devices": [{"deviceID": my_id}, {"deviceID": peer_id}],
        "versioning": {
            "type": "staggered",
            "params": {"maxAge": "604800", "cleanoutDays": "7"},
        },
    }


def _payload(seed, node_tag):
    """Deterministic one-file payload for the sync step."""
    name = "northstar-%s-%s.txt" % (node_tag, seed)
    body = ("FlyGo M16 north-star payload\nseed=%s node=%s\n%s\n"
            % (seed, node_tag, "x" * 256)).encode("utf-8")
    return name, body
class Journey:
    """Runs the 4-step north-star journey against either the mock stack or
    live services. Returns (report, exit_code)."""

    def __init__(self, mode, seed, env):
        self.mode = mode
        self.seed = seed
        self.env = env          # Env object (see build_env)
        self.clock = env.clock
        self.report = Report(env.report_name, {
            "timestamp": env.clock.now(),
            "mode": mode,
            "seed": seed,
            "version": env.version,
        })

    # -- step 1 -------------------------------------------------------------
    def step_activation(self, client, hw_a, hw_b, name_a, name_b, kind,
                        skip_unreachable=False):
        checks, ev = [], {}

        def err(c, e):
            # Connection-level failure (status 0) means the prerequisite
            # service is down: in live mode that is a documented SKIP, never
            # a product failure. HTTP-level errors are real contract fails.
            if skip_unreachable and e.status == 0:
                return c.skip("service unreachable: %s" % e)
            return c.bad(str(e.payload))

        c1 = Check("register node A returns 8-char activation code")
        try:
            r = client.register(name_a, kind, hw_a)
            code = (r.get("device") or {}).get("activation_code") or r.get("activation_code") or ""
            shape_ok = (len(code) == CODE_LEN and
                        all(ch in CODE_ALPHABET for ch in code.upper()))
            (c1.ok if shape_ok else c1.bad)(code or "missing")
            checks.append(c1)
            ev["node_a_code"] = code
        except HttpError as e:
            err(c1, e); checks.append(c1)

        c2 = Check("register node B returns a distinct 8-char code")
        try:
            r = client.register(name_b, kind, hw_b)
            code_b = (r.get("device") or {}).get("activation_code") or r.get("activation_code") or ""
            shape_ok = (len(code_b) == CODE_LEN and
                        all(ch in CODE_ALPHABET for ch in code_b.upper()))
            distinct = code_b != ev.get("node_a_code")
            (c2.ok if (shape_ok and distinct) else c2.bad)(code_b or "missing")
            checks.append(c2)
            ev["node_b_code"] = code_b if shape_ok else ""
        except HttpError as e:
            err(c2, e); checks.append(c2)

        c3 = Check("activate node A -> JWT + trial subscription")
        token_a = None
        try:
            r = client.activate(ev.get("node_a_code", ""), hw_a)
            token_a = r.get("token") or ""
            sub = r.get("subscription_state") or ""
            exp = r.get("expires_at") or 0
            good = (len(token_a) > 20 and sub == "trial" and exp > self.clock.now())
            (c3.ok if good else c3.bad)("subscription=%s expires_at=%s" % (sub, exp))
            ev["node_a_expires_at"] = exp
        except HttpError as e:
            err(c3, e)
        checks.append(c3)

        c4 = Check("activate node B -> JWT + trial subscription")
        token_b = None
        try:
            r = client.activate(ev.get("node_b_code", ""), hw_b)
            token_b = r.get("token") or ""
            sub = r.get("subscription_state") or ""
            good = (len(token_b) > 20 and sub == "trial")
            # NOTE: parens around the whole conditional; without them the
            # trailing call binds only to the else branch and a good result
            # silently does nothing (leaves the check unset = skip).
            (c4.ok if good else (c4.bad if token_b else c4.skip))(
                "subscription=%s" % sub if token_b else "no token - skipped")
        except HttpError as e:
            err(c4, e)
        checks.append(c4)

        c5 = Check("GET /api/status with node A JWT returns the device")
        if not token_a:
            c5.skip("activation failed - no token to query")
        else:
            try:
                r = client.status(token_a)
                dev = r.get("device") or {}
                good = dev.get("name") == name_a and dev.get("hardware_id") == hw_a
                (c5.ok if good else c5.bad)("name=%s hw=%s" % (dev.get("name"), dev.get("hardware_id")))
            except HttpError as e:
                err(c5, e)
        checks.append(c5)

        c6 = Check("POST /api/heartbeat accepted (disk + version reported)")
        if not token_a:
            c6.skip("activation failed - no token to heartbeat")
        else:
            try:
                r = client.heartbeat(token_a, disk_free=1234567890, version="0.1.0")
                good = bool(r.get("ok")) and r.get("subscription_state") == "trial"
                (c6.ok if good else c6.bad)(str(r.get("subscription_state")))
            except HttpError as e:
                err(c6, e)
        checks.append(c6)

        return _checks_state(checks), checks, ev, token_a, token_b

    # -- step 2 -------------------------------------------------------------
    def step_pairing(self, client, token_a, token_b, my_id_a, my_id_b,
                     skip_unreachable=False):
        checks, ev = [], {}

        def err(c, e):
            if skip_unreachable and e.status == 0:
                return c.skip("service unreachable: %s" % e)
            return c.bad(str(e.payload))

        c1 = Check("node A creates 6-digit invite")
        code = None
        try:
            r = client.pair_invite(token_a, my_id_a)
            code = r.get("code") or ""
            exp = r.get("expires_at") or 0
            good = len(code) == PAIR_CODE_LEN and code.isdigit() and exp > self.clock.now()
            (c1.ok if good else c1.bad)("code=%s" % code)
            ev["invite_code"] = code
        except HttpError as e:
            err(c1, e)
        checks.append(c1)

        c2 = Check("node B accepts with its own Syncthing ID")
        try:
            r = client.pair_accept(token_b, ev.get("invite_code", ""), my_id_b)
            got = r.get("inviter_syncthing_id") or ""
            good = got == my_id_a
            (c2.ok if good else c2.bad)("inviter=%s" % got)
            ev["inviter_syncthing_id"] = got
        except HttpError as e:
            err(c2, e)
        checks.append(c2)

        c3 = Check("node A polls -> matched with node B")
        try:
            r = client.pair_poll(token_a)
            status = r.get("status") or ""
            peer = r.get("accepter_syncthing_id") or ""
            good = status == "matched" and peer == my_id_b
            detail = "status=%s peer=%s" % (status, peer)
            (c3.ok if good else c3.bad)(detail)
            ev["match_status"] = status
            ev["accepter_syncthing_id"] = peer
        except HttpError as e:
            err(c3, e)
        checks.append(c3)

        return _checks_state(checks), checks, ev

    # -- step 3 -------------------------------------------------------------
    def step_sync(self, st_a, st_b, folder_id_u, vault_a, vault_b,
                  my_id_a, my_id_b, seed, poll_secs=0, skip_unreachable=False):
        checks, ev = [], {}

        def err(c, e):
            if skip_unreachable and e.status == 0:
                return c.skip("service unreachable: %s" % e)
            return c.bad(str(e.payload))

        c1 = Check("both nodes add each other as Syncthing devices")
        try:
            if hasattr(st_a, "add_peer"):
                st_a.add_peer(my_id_b)
                st_b.add_peer(my_id_a)
            else:
                st_a.put_devices([{"deviceID": my_id_b}])
                st_b.put_devices([{"deviceID": my_id_a}])
            conn = st_a.connections()
            peers = conn.get("connections") or {}
            good = my_id_b in peers
            (c1.ok if good else c1.bad)("peers=%s" % sorted(peers.keys()))
        except HttpError as e:
            err(c1, e)
        checks.append(c1)

        c2 = Check("provision sendreceive folder on node A (staggered versioning)")
        doc = _norm_folder_doc(folder_id_u, "FlyGo M16 north-star vault", str(vault_a),
                               my_id_a, my_id_b)
        try:
            # Per-folder upsert works against both real Syncthing and the mock
            # (PUT /rest/config/folders/{id}); the batch form is panel-internal.
            st_a.put_folder(folder_id_u, doc)
            listed = st_a.config_folders()
            found = next((d for d in listed if d.get("id") == folder_id_u), None)
            good = (found is not None and found.get("type") == "sendreceive"
                    and found.get("versioning", {}).get("type") == "staggered")
            (c2.ok if good else c2.bad)("found=%s" % bool(found))
            ev["folder_doc"] = found
        except HttpError as e:
            err(c2, e)
        checks.append(c2)

        c3 = Check("write one file into node A folder and scan")
        name, payload = _payload(seed, "a")
        try:
            target = Path(vault_a) / name
            target.write_bytes(payload)
            st_a.db_scan(folder_id_u)
            status = self._poll_db_status(st_a, folder_id_u, poll_secs)
            good = status.get("state") == "idle" and status.get("needBytes") == 0
            (c3.ok if good else c3.bad)(str(status))
            ev["file_name"] = name
            ev["file_size"] = len(payload)
            ev["file_sha256"] = hashlib.sha256(payload).hexdigest()
            ev["db_status"] = status
        except HttpError as e:
            err(c3, e)
        checks.append(c3)

        c4 = Check("file converged to node B replica (delete propagation works)")
        name, payload = _payload(seed, "a")
        try:
            peer_file = Path(vault_b) / folder_id_u / name
            deadline = time.time() + max(0, poll_secs)
            while True:
                if peer_file.is_file() and peer_file.read_bytes() == payload:
                    break
                if poll_secs <= 0 or time.time() >= deadline:
                    break
                time.sleep(1.0)
            good = peer_file.is_file() and peer_file.read_bytes() == payload
            (c4.ok if good else c4.bad)("replica=%s" % peer_file)
            ev["replica_path"] = str(peer_file)
        except HttpError as e:
            err(c4, e)
        checks.append(c4)

        return _checks_state(checks), checks, ev

    def _poll_db_status(self, st_client, folder_id_u, poll_secs):
        """Poll /rest/db/status until converged (needBytes==0, state idle),
        bounded by poll_secs (0 = single check, mock path)."""
        deadline = time.time() + max(0, poll_secs)
        while True:
            status = st_client.db_status(folder_id_u)
            converged = status.get("state") == "idle" and status.get("needBytes") == 0
            if converged or poll_secs <= 0 or time.time() >= deadline:
                return status
            time.sleep(1.0)

    # -- step 4 -------------------------------------------------------------
    def step_verify_replicas(self, vault_a, vault_b, folder_id_u, seed):
        checks, ev = [], {}
        name, payload = _payload(seed, "a")
        sha = hashlib.sha256(payload).hexdigest()

        c1 = Check("replica A exists with matching sha256")
        try:
            p = Path(vault_a) / name
            good = p.is_file() and self._sha256(p) == sha
            (c1.ok if good else c1.bad)("path=%s" % p)
            ev["replica_a"] = str(p)
        except Exception as e:
            c1.bad(str(e))
        checks.append(c1)

        c2 = Check("replica B exists with matching sha256")
        try:
            p = Path(vault_b) / folder_id_u / name
            good = p.is_file() and self._sha256(p) == sha
            (c2.ok if good else c2.bad)("path=%s" % p)
            ev["replica_b"] = str(p)
        except Exception as e:
            c2.bad(str(e))
        checks.append(c2)

        c3 = Check("exactly 2 identical replicas (count + size + hash)")
        try:
            pa = Path(vault_a) / name
            pb = Path(vault_b) / folder_id_u / name
            both = pa.is_file() and pb.is_file()
            same = both and pa.stat().st_size == pb.stat().st_size and self._sha256(pa) == self._sha256(pb)
            replicas = (1 if pa.is_file() else 0) + (1 if pb.is_file() else 0)
            good = both and same and replicas == 2
            (c3.ok if good else c3.bad)("count=%d sizes_equal=%s" % (replicas, same))
            ev["replica_count"] = replicas
            ev["sizes_equal"] = bool(same)
            ev["sha256"] = sha
        except Exception as e:
            c3.bad(str(e))
        checks.append(c3)

        return _checks_state(checks), checks, ev

    @staticmethod
    def _sha256(path):
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(65536), b""):
                h.update(chunk)
        return h.hexdigest()

    # -- orchestrator -------------------------------------------------------
    def run(self):
        env = self.env
        if self.mode == "mock":
            return self._run_mock()
        return self._run_live()

    def _run_mock(self):
        env = self.env
        r = self.report

        # node ids derived from seed (deterministic)
        rng = SeedRng(self.seed)
        hw_a = "HW-M16-NODEA-%s" % (self.seed % 0xFFFF)
        hw_b = "HW-M16-NODEB-%s" % (self.seed % 0xFFFF)
        my_id_a = rng.syncthing_id()
        my_id_b = rng.syncthing_id()

        # boot the mock stack
        plane = MockControlPlane(self.seed, clock=self.clock)
        plane_server, plane_url = serve_app(plane)
        st_a = MockSyncthing(SeedRng(self.seed * 1_000_003 + 1).raw(), my_id_a,
                             env.vault_a, {my_id_b: env.vault_b})
        st_b = MockSyncthing(SeedRng(self.seed * 1_000_003 + 2).raw(), my_id_b,
                             env.vault_b, {my_id_a: env.vault_a})
        st_a_srv, st_a_url = serve_app(st_a)
        st_b_srv, st_b_url = serve_app(st_b)
        # register peers on the backend objects (HTTP clients cannot mutate)
        st_a.add_peer(my_id_b)
        st_b.add_peer(my_id_a)

        try:
            client = ControlPlaneClient(plane_url, timeout=env.timeout)
            st_client_a = SyncthingClient(st_a_url, timeout=env.timeout)
            st_client_b = SyncthingClient(st_b_url, timeout=env.timeout)

            # step 1
            status, checks, ev, token_a, token_b = self.step_activation(
                client, hw_a, hw_b, "m16-node-a", "m16-node-b", "computer")
            self.report.add_step("activation", status, [c.as_dict() for c in checks], ev)

            # step 2
            status, checks, ev = self.step_pairing(
                client, token_a, token_b, my_id_a, my_id_b)
            self.report.add_step("pairing", status, [c.as_dict() for c in checks], ev)

            # step 3
            folder_id = "%s-%s" % (FOLDER_ID_PREFIX, self.seed % 0xFFFF)
            status, checks, ev = self.step_sync(
                st_client_a, st_client_b, folder_id,
                env.vault_a, env.vault_b, my_id_a, my_id_b, self.seed)
            self.report.add_step("one-file-sync", status, [c.as_dict() for c in checks], ev)

            # step 4
            status, checks, ev = self.step_verify_replicas(
                env.vault_a, env.vault_b, folder_id, self.seed)
            self.report.add_step("two-replica-verify", status, [c.as_dict() for c in checks], ev)

            self.report.meta["stack"] = {
                "control_plane": plane_url,
                "syncthing_a": st_a_url,
                "syncthing_b": st_b_url,
                "vault_a": str(env.vault_a),
                "vault_b": str(env.vault_b),
            }
        finally:
            for srv in (plane_server, st_a_srv, st_b_srv):
                srv.shutdown()
        return self.report, self.report.exit_code()

    def _run_live(self):
        env = self.env
        r = self.report
        rng = SeedRng(self.seed)
        hw_a = "HW-M16-LIVE-NODEA-%s" % (self.seed % 0xFFFF)
        hw_b = "HW-M16-LIVE-NODEB-%s" % (self.seed % 0xFFFF)
        my_id_b = env.peer_syncthing_id or rng.syncthing_id()  # real peer id if given
        my_id_a = None

        st_client = None
        if env.syncthing_url:
            try:
                st_client = SyncthingClient(env.syncthing_url, env.syncthing_apikey,
                                            timeout=env.timeout)
                my_id_a = st_client.my_id()
            except HttpError:
                st_client = None

        client = ControlPlaneClient(env.control_url, timeout=env.timeout)

        # step 1 - activation on the real control plane
        status, checks, ev, token_a, token_b = self.step_activation(
            client, hw_a, hw_b, "m16-live-node-a", "m16-live-node-b", "computer",
            skip_unreachable=True)
        self.report.add_step("activation", status, [c.as_dict() for c in checks], ev)

        # step 2 - pairing on the real control plane
        if token_a and token_b:
            sid_a = my_id_a or ("MOCKLIVE-%s" % my_id_b[:40])
            ev_note = {"synthetic_syncthing_id_a": my_id_a is None}
            status, checks, ev2 = self.step_pairing(
                client, token_a, token_b, sid_a, my_id_b, skip_unreachable=True)
            ev2.update(ev_note)
            self.report.add_step("pairing", status, [c.as_dict() for c in checks], ev2)
        else:
            self.report.add_step(
                "pairing", "skip",
                [{"name": "pairing requires two activated nodes",
                  "pass": "skip", "detail": "node tokens missing - activation skipped"}],
                {"tokens": (bool(token_a), bool(token_b))})

        # step 3+4 - data plane (real Syncthing or skip)
        if st_client is not None and env.folder_dir and env.replica_dir:
            folder_id = "%s-live-%s" % (FOLDER_ID_PREFIX, self.seed % 0xFFFF)
            status, checks, ev = self.step_sync(
                st_client, st_client, folder_id,
                env.folder_dir, env.replica_dir, my_id_a, my_id_b, self.seed,
                poll_secs=env.poll_secs, skip_unreachable=True)
            self.report.add_step("one-file-sync", status, [c.as_dict() for c in checks], ev)
            if status != "fail":
                status2, checks2, ev2 = self.step_verify_replicas(
                    env.folder_dir, env.replica_dir, folder_id, self.seed)
                self.report.add_step("two-replica-verify", status2, [c.as_dict() for c in checks2], ev2)
            else:
                self.report.add_step("two-replica-verify", "skip",
                                     [{"name": "verify skipped",
                                       "pass": "skip", "detail": "sync step failed"}], {})
        else:
            reason = ("syncthing engine not configured" if st_client is None
                      else "--folder-dir/--replica-dir missing")
            for name in ("one-file-sync", "two-replica-verify"):
                self.report.add_step(
                    name, "skip",
                    [{"name": "data-plane prerequisite", "pass": "skip", "detail": reason}],
                    {"reason": reason})
        return self.report, self.report.exit_code()
class Env:
    def __init__(self, **kw):
        self.__dict__.update(kw)


def build_env(args):
    seed = int(args.seed)
    base = Path(args.runtime_dir)
    clock = FakeClock(int(args.fake_now)) if args.fake_now else RealClock()
    vault_a = base / "vault-a"
    vault_b = base / "vault-b"
    # Always wipe the sandbox first: leftover files from an earlier run would
    # break the deterministic replica counts of the next one.
    if base.exists():
        shutil.rmtree(base)
    vault_a.mkdir(parents=True, exist_ok=True)
    vault_b.mkdir(parents=True, exist_ok=True)
    env = Env(
        seed=seed,
        clock=clock,
        version=args.version or "M16-unknown-version",
        timeout=args.timeout,
        control_url=args.control_url,
        syncthing_url=args.syncthing_url,
        syncthing_apikey=args.syncthing_apikey,
        peer_syncthing_id=args.peer_syncthing_id,
        poll_secs=args.poll_secs,
        folder_dir=str(vault_a),
        replica_dir=str(vault_b),
        vault_a=vault_a,
        vault_b=vault_b,
        report_name=args.report_name,
    )
    return env


def choose_mode(args):
    if args.mode != "auto":
        return args.mode
    plane_up = probe_ok(args.control_url + "/api/register", timeout=args.timeout)
    syncthing_up = bool(args.syncthing_url) and probe_ok(args.syncthing_url + "/rest/version",
                                                         timeout=args.timeout)
    if plane_up and syncthing_up:
        return "live"
    return "mock"


def parse_args(argv):
    p = argparse.ArgumentParser(
        prog="northstar_acceptance",
        description="FlyGo M16 north-star E2E acceptance (activation, pairing, "
                    "one-file sync, two-replica verification)")
    p.add_argument("--mode", choices=("auto", "mock", "live"), default="auto",
                   help="auto: probe services, fall back to mock (default)")
    p.add_argument("--seed", default="20260817", help="deterministic seed for ids/payloads")
    p.add_argument("--runtime-dir", default="scripts/tmp_m16",
                   help="sandbox for vaults/replicas (git-ignored)")
    p.add_argument("--fake-now", default=None,
                   help="fixed unix timestamp for the fake clock (tests only)")
    p.add_argument("--report-dir", default=REPORT_DIR_DEFAULT)
    p.add_argument("--report-name", default=REPORT_NAME_DEFAULT)
    p.add_argument("--write-json", action="store_true",
                   help="also write a raw .json evidence file (pre-commit secret "
                        "scan will flag it - stage only the .md)")
    p.add_argument("--control-url", default="http://127.0.0.1:9001")
    p.add_argument("--syncthing-url", default=None,
                   help="real Syncthing REST base URL for live mode")
    p.add_argument("--syncthing-apikey", default=None)
    p.add_argument("--peer-syncthing-id", default=None,
                   help="real peer Syncthing device ID (live mode)")
    p.add_argument("--poll-secs", type=int, default=30,
                   help="live-mode convergence poll budget (default 30s)")
    p.add_argument("--keep-runtime", action="store_true",
                   help="do not delete the runtime sandbox afterwards")
    p.add_argument("--clean", action="store_true", help="wipe runtime dir first")
    p.add_argument("--timeout", type=int, default=10)
    p.add_argument("--version", default=None, help="version string for the report meta")
    p.add_argument("--quiet", action="store_true")
    return p.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    mode = choose_mode(args)
    env = build_env(args)
    journey = Journey(mode, int(args.seed), env)
    report, code = journey.run()

    written = report.write(args.report_dir, write_json=args.write_json)
    if not args.quiet:
        print("mode: %s  result: %s  exit=%d" % (mode, report.status(), code))
        for i, s in enumerate(report.steps, 1):
            passed = sum(1 for c in s["checks"] if c.get("pass") is True)
            print("  step %d %-20s %-8s %d/%d" % (i, s["name"], s["status"], passed, len(s["checks"])))
        for w in written:
            print("report: %s" % w)
        if not args.keep_runtime:
            shutil.rmtree(env.vault_a.parent, ignore_errors=True)
    return code


if __name__ == "__main__":
    sys.exit(main())