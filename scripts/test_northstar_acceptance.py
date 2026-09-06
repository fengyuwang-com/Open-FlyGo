# -*- coding: utf-8 -*-
"""Pytest suite for the FlyGo M16 north-star acceptance script.

Runs the acceptance journey and its mock stack entirely offline: the mock
control plane and mock Syncthing engines are real HTTP servers bound to
ephemeral 127.0.0.1 ports, so these tests exercise the exact same HTTP
client code as a live run - deterministically, with no external services.

Run from the repo root:
    python -m pytest scripts/test_northstar_acceptance.py -q
"""
import hashlib
import json
import re
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import northstar_acceptance as na  # noqa: E402

FIXED_NOW = 1_800_000_000  # deterministic fake clock


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

def make_env(tmp_path, seed="20260817", runtime="rt", **kw):
    base = tmp_path / runtime
    base.mkdir(parents=True, exist_ok=True)
    env = na.Env(
        seed=int(seed),
        clock=na.FakeClock(FIXED_NOW),
        version="test-version",
        timeout=5,
        control_url=kw.get("control_url", "http://127.0.0.1:9"),
        syncthing_url=kw.get("syncthing_url", None),
        syncthing_apikey=kw.get("syncthing_apikey", None),
        peer_syncthing_id=kw.get("peer_syncthing_id", None),
        poll_secs=kw.get("poll_secs", 5),
        folder_dir=str(base / "vault-a"),
        replica_dir=str(base / "vault-b"),
        vault_a=base / "vault-a",
        vault_b=base / "vault-b",
        report_name=kw.get("report_name", "m16-northstar-report"),
    )
    env.vault_a.mkdir(parents=True, exist_ok=True)
    env.vault_b.mkdir(parents=True, exist_ok=True)
    return env


def serve(handler):
    """Start an HTTP handler (MockControlPlane / MockSyncthing); returns
    (server, url) with teardown via request fixture reversal."""
    server, url = na.serve_app(handler)
    return server, url


@pytest.fixture
def plane():
    app = na.MockControlPlane(12345, clock=na.FakeClock(FIXED_NOW))
    server, url = na.serve_app(app)
    yield na.ControlPlaneClient(url, timeout=5)
    server.shutdown()


def make_syncthing_pair(tmp_path, seed, my_a, my_b):
    root_a = tmp_path / "st-a"
    root_b = tmp_path / "st-b"
    root_a.mkdir(exist_ok=True)
    root_b.mkdir(exist_ok=True)
    st_a = na.MockSyncthing(seed + 1, my_a, root_a, {my_b: root_b})
    st_b = na.MockSyncthing(seed + 2, my_b, root_b, {my_a: root_a})
    srv_a, url_a = na.serve_app(st_a)
    srv_b, url_b = na.serve_app(st_b)
    return st_a, st_b, na.SyncthingClient(url_a, timeout=5), na.SyncthingClient(url_b, timeout=5), srv_a, srv_b


def ids(seed):
    rng = na.SeedRng(seed)
    return rng.syncthing_id(), rng.syncthing_id()


# ---------------------------------------------------------------------------
# control plane contract
# ---------------------------------------------------------------------------

def test_activation_roundtrip(plane):
    hw = "HW-TEST-AAA-0001"
    r = plane.register("alpha", "computer", hw)
    assert r["ok"] is True
    code = r["device"]["activation_code"]
    assert len(code) == na.CODE_LEN
    assert all(c in na.CODE_ALPHABET for c in code)

    reg2 = plane.register("alpha", "computer", hw)
    assert reg2["device"]["activation_code"] == code  # idempotent pre-activation

    act = plane.activate(code, hw)
    assert act["ok"] is True
    assert len(act["token"]) > 20
    assert act["subscription_state"] == "trial"
    assert act["expires_at"] == FIXED_NOW + na.TRIAL_DAYS * 86400

    st = plane.status(act["token"])
    assert st["device"]["name"] == "alpha"
    assert st["device"]["hardware_id"] == hw

    hb = plane.heartbeat(act["token"], disk_free=999, version="0.1.0")
    assert hb["ok"] is True and hb["subscription_state"] == "trial"


def test_activation_contract_errors(plane):
    hw = "HW-TEST-BBB-0002"
    code = plane.register("beta", "phone", hw)["device"]["activation_code"]

    with pytest.raises(na.HttpError) as e:
        plane.activate("BADCODE", hw)  # wrong code shape / unknown
    assert e.value.status in (400, 404)

    with pytest.raises(na.HttpError) as e:
        plane.activate(code, "HW-TEST-OTHER-9999")  # other hardware
    assert e.value.status == 400

    act = plane.activate(code, hw)
    assert act["ok"] is True

    with pytest.raises(na.HttpError) as e:
        plane.activate(code, hw)  # re-activation
    assert e.value.status == 409


def test_pairing_flow(plane):
    hw_a, hw_b = "HW-TEST-PAIR-A", "HW-TEST-PAIR-B"
    ca = plane.register("pair-a", "computer", hw_a)["device"]["activation_code"]
    cb = plane.register("pair-b", "computer", hw_b)["device"]["activation_code"]
    ta = plane.activate(ca, hw_a)["token"]
    tb = plane.activate(cb, hw_b)["token"]

    my_a, my_b = na.SeedRng(7).syncthing_id(), na.SeedRng(8).syncthing_id()

    inv = plane.pair_invite(ta, my_a)
    assert len(inv["code"]) == na.PAIR_CODE_LEN and inv["code"].isdigit()

    poll = plane.pair_poll(ta)  # not accepted yet -> pending
    assert poll["status"] == "pending"

    acc = plane.pair_accept(tb, inv["code"], my_b)
    assert acc["inviter_syncthing_id"] == my_a

    poll = plane.pair_poll(ta)
    assert poll["status"] == "matched"
    assert poll["accepter_syncthing_id"] == my_b

    with pytest.raises(na.HttpError) as e:  # one-time code
        plane.pair_accept(tb, inv["code"], my_b)
    assert e.value.status == 409


def test_pairing_requires_activation(plane):
    hw = "HW-TEST-NOACT"
    plane.register("ga", "computer", hw)
    # garbage / missing token is rejected (401) before any eligibility check
    with pytest.raises(na.HttpError) as e:
        plane.pair_invite("garbage-token", "A" * 56)
    assert e.value.status == 401
    with pytest.raises(na.HttpError) as e:
        plane.pair_accept("garbage-token", "123456", "B" * 56)
    assert e.value.status == 401


# ---------------------------------------------------------------------------
# mock syncthing engine
# ---------------------------------------------------------------------------

def test_syncthing_replication_and_delete(tmp_path):
    my_a, my_b = ids(99)
    st_a, st_b, cli_a, cli_b, srv_a, srv_b = make_syncthing_pair(tmp_path, 99, my_a, my_b)
    try:
        st_a.add_peer(my_b)
        st_b.add_peer(my_a)
        assert my_b in (cli_a.connections()["connections"] or {})

        doc = na._norm_folder_doc("f-test", "vault", str(tmp_path / "vault-a"), my_a, my_b)
        cli_a.put_folder("f-test", doc)
        assert any(f["id"] == "f-test" for f in cli_a.config_folders())

        payload = b"hello replica"
        (tmp_path / "vault-a" / "note.txt").write_bytes(payload)
        sc = cli_a.db_scan("f-test")
        assert sc["bytes"] == len(payload)

        st = cli_a.db_status("f-test")
        assert st["needBytes"] == 0 and st["state"] == "idle"

        # st_a replicates into st_b's root (the peer replica root)
        replica = tmp_path / "st-b" / "f-test" / "note.txt"
        assert replica.read_bytes() == payload
        assert hashlib.sha256(replica.read_bytes()).hexdigest() == \
            hashlib.sha256(payload).hexdigest()

        # delete propagation
        (tmp_path / "vault-a" / "note.txt").unlink()
        cli_a.db_scan("f-test")
        assert not replica.exists()
    finally:
        srv_a.shutdown()
        srv_b.shutdown()


# ---------------------------------------------------------------------------
# full journey (mock mode)
# ---------------------------------------------------------------------------

def run_mock(tmp_path, seed, runtime="rt"):
    env = make_env(tmp_path, seed=seed, runtime=runtime)
    journey = na.Journey("mock", int(seed), env)
    report, code = journey.run()
    return report, code, env


def test_journey_mock_full_pass(tmp_path):
    report, code, env = run_mock(tmp_path, "20260817")
    assert code == na.ExitCodes.PASS
    assert report.status() == "pass"
    names = [s["name"] for s in report.steps]
    assert names == ["activation", "pairing", "one-file-sync", "two-replica-verify"]
    for s in report.steps:
        assert s["status"] == "pass", s
    # evidence of the two replicas
    verify = report.steps[3]["evidence"]
    assert verify["replica_count"] == 2
    assert verify["sha256"] and verify["sizes_equal"] is True


def strip_paths(obj, roots):
    """Replace runtime sandbox paths with <RT> so evidence from different
    tmp dirs compares equal (determinism check)."""
    if isinstance(obj, dict):
        return {k: strip_paths(v, roots) for k, v in obj.items()}
    if isinstance(obj, list):
        return [strip_paths(v, roots) for v in obj]
    if isinstance(obj, str):
        for r in roots:
            obj = obj.replace(str(r), "<RT>")
        return obj
    return obj


def test_journey_determinism(tmp_path):
    r1, c1, e1 = run_mock(tmp_path, "20260817", runtime="r1")
    r2, c2, e2 = run_mock(tmp_path, "20260817", runtime="r2")
    assert c1 == c2 == na.ExitCodes.PASS
    steps1 = [(s["name"], s["status"]) for s in r1.steps]
    steps2 = [(s["name"], s["status"]) for s in r2.steps]
    assert steps1 == steps2
    roots1 = [e1.vault_a.parent, e1.vault_b.parent]
    roots2 = [e2.vault_a.parent, e2.vault_b.parent]
    ev1 = [strip_paths(s["evidence"], roots1) for s in r1.steps]
    ev2 = [strip_paths(s["evidence"], roots2) for s in r2.steps]
    assert ev1 == ev2  # fully reproducible evidence


def test_journey_different_seed_differs(tmp_path):
    r1, _, _ = run_mock(tmp_path, "111", runtime="s1")
    r2, _, _ = run_mock(tmp_path, "222", runtime="s2")
    f1 = r1.steps[2]["evidence"]["file_name"]
    f2 = r2.steps[2]["evidence"]["file_name"]
    assert f1 != f2


def test_journey_writes_report_with_embedded_json(tmp_path):
    env = make_env(tmp_path, seed="777", runtime="wr")
    report, code = na.Journey("mock", 777, env).run()
    out = report.write(str(tmp_path / "reports"), write_json=False)
    md = Path(out[0]).read_text(encoding="utf-8")
    block = re.search(r"```json\n(.*?)\n```", md, re.S).group(1)
    parsed = json.loads(block)
    assert parsed["result"] == "pass"
    assert len(parsed["steps"]) == 4
    assert out[0].endswith(".md")  # no .json artifact by default


def test_live_unreachable_skips_with_exit_code(tmp_path):
    env = make_env(tmp_path, seed="424242",
                   control_url="http://127.0.0.1:1",  # nothing listens there
                   syncthing_url="http://127.0.0.1:1")
    report, code = na.Journey("live", 42, env).run()
    assert code == na.ExitCodes.SKIP  # 2: completed with skips
    assert report.status() == "partial"
    names = [s["name"] for s in report.steps]
    assert names == ["activation", "pairing", "one-file-sync", "two-replica-verify"]
    assert all(s["status"] == "skip" for s in report.steps)
    # reasons recorded (connection-level failures are documented skips)
    assert any("unreachable" in c["detail"]
               for s in report.steps for c in s["checks"] if isinstance(c.get("detail"), str))


def test_cli_auto_falls_back_to_mock(tmp_path, monkeypatch):
    """auto mode probes dead services and runs the mock journey to PASS."""
    monkeypatch.chdir(tmp_path)
    args = na.parse_args([
        "--mode", "auto",
        "--control-url", "http://127.0.0.1:1",
        "--syncthing-url", "http://127.0.0.1:1",
        "--runtime-dir", str(tmp_path / "auto-rt"),
        "--report-dir", str(tmp_path / "auto-report"),
        "--quiet",
    ])
    assert na.choose_mode(args) == "mock"
    code = na.main(["--mode", "auto",
                    "--control-url", "http://127.0.0.1:1",
                    "--syncthing-url", "http://127.0.0.1:1",
                    "--runtime-dir", str(tmp_path / "auto-rt"),
                    "--report-dir", str(tmp_path / "auto-report"),
                    "--quiet"])
    assert code == na.ExitCodes.PASS
    md = (tmp_path / "auto-report") / (na.REPORT_NAME_DEFAULT + ".md")
    assert md.exists()
    assert "**pass**" in md.read_text(encoding="utf-8")