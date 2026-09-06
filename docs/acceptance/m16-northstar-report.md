# FlyGo M16 North-Star Acceptance Report

- timestamp: 1786972107
- mode: mock
- seed: 20260817
- version: M16-unknown-version
- result: **pass**

## Journey steps

| # | step | status | checks passed |
|---|------|--------|---------------|
| 1 | activation | pass | 6/6 |
| 2 | pairing | pass | 3/3 |
| 3 | one-file-sync | pass | 4/4 |
| 4 | two-replica-verify | pass | 3/3 |

## Step 1: activation (pass)

- [PASS] register node A returns 8-char activation code 8PV8WSZU
- [PASS] register node B returns a distinct 8-char code Q2RWKDKR
- [PASS] activate node A -> JWT + trial subscription subscription=trial expires_at=1789564107
- [PASS] activate node B -> JWT + trial subscription subscription=trial
- [PASS] GET /api/status with node A JWT returns the device name=m16-node-a hw=HW-M16-NODEA-10502
- [PASS] POST /api/heartbeat accepted (disk + version reported) trial

## Step 2: pairing (pass)

- [PASS] node A creates 6-digit invite code=458935
- [PASS] node B accepts with its own Syncthing ID inviter=6NT6UQXS-OYPUJDJP-KCONUZHD-MSLODCZB-LCGYPC6N-EIWEEDWS-A2OSXMQM
- [PASS] node A polls -> matched with node B status=matched peer=234WZZ27-SYQGGXCL-53RF6GVS-6BQUCE4Q-VEZZYJZL-CQH6RQR3-A43N7TCQ

## Step 3: one-file-sync (pass)

- [PASS] both nodes add each other as Syncthing devices peers=['234WZZ27-SYQGGXCL-53RF6GVS-6BQUCE4Q-VEZZYJZL-CQH6RQR3-A43N7TCQ']
- [PASS] provision sendreceive folder on node A (staggered versioning) found=True
- [PASS] write one file into node A folder and scan {'folder': 'm16-10502', 'state': 'idle', 'globalBytes': 307, 'localBytes': 307, 'needBytes': 0, 'needItems': 0}
- [PASS] file converged to node B replica (delete propagation works) replica=scripts\tmp_m16\vault-b\m16-10502\northstar-a-20260817.txt

## Step 4: two-replica-verify (pass)

- [PASS] replica A exists with matching sha256 path=scripts\tmp_m16\vault-a\northstar-a-20260817.txt
- [PASS] replica B exists with matching sha256 path=scripts\tmp_m16\vault-b\m16-10502\northstar-a-20260817.txt
- [PASS] exactly 2 identical replicas (count + size + hash) count=2 sizes_equal=True

## Machine-readable evidence

```json
{
  "meta": {
    "timestamp": 1786972107,
    "mode": "mock",
    "seed": 20260817,
    "version": "M16-unknown-version",
    "stack": {
      "control_plane": "http://127.0.0.1:57824",
      "syncthing_a": "http://127.0.0.1:57825",
      "syncthing_b": "http://127.0.0.1:57826",
      "vault_a": "scripts\\tmp_m16\\vault-a",
      "vault_b": "scripts\\tmp_m16\\vault-b"
    }
  },
  "result": "pass",
  "steps": [
    {
      "name": "activation",
      "status": "pass",
      "checks": [
        {
          "name": "register node A returns 8-char activation code",
          "pass": true,
          "detail": "8PV8WSZU"
        },
        {
          "name": "register node B returns a distinct 8-char code",
          "pass": true,
          "detail": "Q2RWKDKR"
        },
        {
          "name": "activate node A -> JWT + trial subscription",
          "pass": true,
          "detail": "subscription=trial expires_at=1789564107"
        },
        {
          "name": "activate node B -> JWT + trial subscription",
          "pass": true,
          "detail": "subscription=trial"
        },
        {
          "name": "GET /api/status with node A JWT returns the device",
          "pass": true,
          "detail": "name=m16-node-a hw=HW-M16-NODEA-10502"
        },
        {
          "name": "POST /api/heartbeat accepted (disk + version reported)",
          "pass": true,
          "detail": "trial"
        }
      ],
      "evidence": {
        "node_a_code": "8PV8WSZU",
        "node_b_code": "Q2RWKDKR",
        "node_a_expires_at": 1789564107
      }
    },
    {
      "name": "pairing",
      "status": "pass",
      "checks": [
        {
          "name": "node A creates 6-digit invite",
          "pass": true,
          "detail": "code=458935"
        },
        {
          "name": "node B accepts with its own Syncthing ID",
          "pass": true,
          "detail": "inviter=6NT6UQXS-OYPUJDJP-KCONUZHD-MSLODCZB-LCGYPC6N-EIWEEDWS-A2OSXMQM"
        },
        {
          "name": "node A polls -> matched with node B",
          "pass": true,
          "detail": "status=matched peer=234WZZ27-SYQGGXCL-53RF6GVS-6BQUCE4Q-VEZZYJZL-CQH6RQR3-A43N7TCQ"
        }
      ],
      "evidence": {
        "invite_code": "458935",
        "inviter_syncthing_id": "6NT6UQXS-OYPUJDJP-KCONUZHD-MSLODCZB-LCGYPC6N-EIWEEDWS-A2OSXMQM",
        "match_status": "matched",
        "accepter_syncthing_id": "234WZZ27-SYQGGXCL-53RF6GVS-6BQUCE4Q-VEZZYJZL-CQH6RQR3-A43N7TCQ"
      }
    },
    {
      "name": "one-file-sync",
      "status": "pass",
      "checks": [
        {
          "name": "both nodes add each other as Syncthing devices",
          "pass": true,
          "detail": "peers=['234WZZ27-SYQGGXCL-53RF6GVS-6BQUCE4Q-VEZZYJZL-CQH6RQR3-A43N7TCQ']"
        },
        {
          "name": "provision sendreceive folder on node A (staggered versioning)",
          "pass": true,
          "detail": "found=True"
        },
        {
          "name": "write one file into node A folder and scan",
          "pass": true,
          "detail": "{'folder': 'm16-10502', 'state': 'idle', 'globalBytes': 307, 'localBytes': 307, 'needBytes': 0, 'needItems': 0}"
        },
        {
          "name": "file converged to node B replica (delete propagation works)",
          "pass": true,
          "detail": "replica=scripts\\tmp_m16\\vault-b\\m16-10502\\northstar-a-20260817.txt"
        }
      ],
      "evidence": {
        "folder_doc": {
          "id": "m16-10502",
          "label": "FlyGo M16 north-star vault",
          "path": "scripts\\tmp_m16\\vault-a",
          "type": "sendreceive",
          "rescanIntervalS": 3600,
          "fsWatcherEnabled": true,
          "devices": [
            {
              "deviceID": "6NT6UQXS-OYPUJDJP-KCONUZHD-MSLODCZB-LCGYPC6N-EIWEEDWS-A2OSXMQM"
            },
            {
              "deviceID": "234WZZ27-SYQGGXCL-53RF6GVS-6BQUCE4Q-VEZZYJZL-CQH6RQR3-A43N7TCQ"
            }
          ],
          "versioning": {
            "type": "staggered",
            "params": {
              "maxAge": "604800",
              "cleanoutDays": "7"
            }
          }
        },
        "file_name": "northstar-a-20260817.txt",
        "file_size": 307,
        "file_sha256": "9333647f998623ea4d920d82a202742e11883ccce763b3c2811c22a354117eb8",
        "db_status": {
          "folder": "m16-10502",
          "state": "idle",
          "globalBytes": 307,
          "localBytes": 307,
          "needBytes": 0,
          "needItems": 0
        },
        "replica_path": "scripts\\tmp_m16\\vault-b\\m16-10502\\northstar-a-20260817.txt"
      }
    },
    {
      "name": "two-replica-verify",
      "status": "pass",
      "checks": [
        {
          "name": "replica A exists with matching sha256",
          "pass": true,
          "detail": "path=scripts\\tmp_m16\\vault-a\\northstar-a-20260817.txt"
        },
        {
          "name": "replica B exists with matching sha256",
          "pass": true,
          "detail": "path=scripts\\tmp_m16\\vault-b\\m16-10502\\northstar-a-20260817.txt"
        },
        {
          "name": "exactly 2 identical replicas (count + size + hash)",
          "pass": true,
          "detail": "count=2 sizes_equal=True"
        }
      ],
      "evidence": {
        "replica_a": "scripts\\tmp_m16\\vault-a\\northstar-a-20260817.txt",
        "replica_b": "scripts\\tmp_m16\\vault-b\\m16-10502\\northstar-a-20260817.txt",
        "replica_count": 2,
        "sizes_equal": true,
        "sha256": "9333647f998623ea4d920d82a202742e11883ccce763b3c2811c22a354117eb8"
      }
    }
  ]
}
```

