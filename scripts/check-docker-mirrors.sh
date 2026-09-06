#!/usr/bin/env bash
# check-docker-mirrors.sh - one-click health probe for Docker registry mirrors.
#
# For FlyGo setup / pre-launch checks: makes sure the local Docker daemon is
# reachable and that every configured registry-mirror is alive, so image
# pulls will not hang on a dead mirror. Read-only: never edits daemon.json,
# never restarts dockerd, never pulls large images. (hello-world is tiny and
# only pulled with --pull.)
#
# Usage:
#   scripts/check-docker-mirrors.sh              # probe daemon + all mirrors (default)
#   scripts/check-docker-mirrors.sh --pull       # also pull hello-world end-to-end
#   scripts/check-docker-mirrors.sh --mirrors "m1 m2"   # probe explicit mirror list
#   scripts/check-docker-mirrors.sh --help
#
# Exit codes:
#   0  all mirrors online (or no mirrors configured)
#   1  at least one mirror unreachable, or the --pull end-to-end check failed
#   2  docker command not found on PATH
#   3  docker daemon unreachable (cannot read server version or mirrors)
#
# Targets the active docker context. To check the WSL self-built daemon from
# this host, run inside WSL (so `docker` resolves to the WSL unix socket).
set -u

CURL_TIMEOUT=8          # seconds, per mirror probe
IP_MODE=""              # empty = let curl choose; set "-4" if a mirror is IPv6-flaky
PULL=0
EXPLICIT_MIRRORS=""

# ---------------------------------------------------------------------------
# CLI (mirrors check-secrets.sh arg style)
# ---------------------------------------------------------------------------
case "${1:-}" in
    ''|china|--check) ;;
    --pull) PULL=1 ;;
    --mirrors)
        if [ $# -lt 2 ] || [ -z "$2" ]; then
            printf 'check-docker-mirrors: --mirrors requires a list\n'
            exit 2
        fi
        EXPLICIT_MIRRORS="$2"
        ;;
    -h|--help)
        sed -n '2,20p' "$0"
        exit 0
        ;;
    *)
        printf 'unknown option: %s (see --help)\n' "${1:-}"
        exit 2
        ;;
esac

# ---------------------------------------------------------------------------
# 1. docker command present?
# ---------------------------------------------------------------------------
if ! command -v docker >/dev/null 2>&1; then
    printf 'check-docker-mirrors: docker not found on PATH (nothing to check)\n'
    exit 2
fi

# ---------------------------------------------------------------------------
# 2. daemon reachable + read configured registry-mirrors
# ---------------------------------------------------------------------------
SERVER_VERSION=""
SERVER_VERSION=$(docker version --format '{{.Server.Version}}' 2>/dev/null) \
    || SERVER_VERSION=""
if [ -z "$SERVER_VERSION" ]; then
    printf 'check-docker-mirrors: docker daemon unreachable (server version not returned)\n'
    printf '  hint: is dockerd running? is DOCKER_HOST/DOCKER_CONTEXT pointing at a live daemon?\n'
    exit 3
fi
printf 'docker daemon reachable (server %s)\n' "$SERVER_VERSION"

MIRRORS="$EXPLICIT_MIRRORS"
# Normalize to one mirror per line (explicit lists may be space-separated).
MIRRORS=$(printf '%s\n' "$MIRRORS" | tr ' ' '\n' | sed '/^[[:space:]]*$/d')
if [ -z "$MIRRORS" ]; then
    # Parse the JSON array returned by docker info. Fall back to daemon.json
    # grep if the active daemon reports no mirrors but a config file has them.
    MIRRORS=$(docker info --format '{{json .RegistryConfig.Mirrors}}' 2>/dev/null \
        | tr -d '[]"' | tr ',' '\n' | sed '/^[[:space:]]*$/d')
    if [ -z "$MIRRORS" ]; then
        MIRRORS=$(grep -oE 'https?://[^" ,]+' /etc/docker/daemon.json 2>/dev/null | sed '/^$/d')
    fi
fi

if [ -z "$MIRRORS" ]; then
    printf 'check-docker-mirrors: no registry-mirrors configured (pulls go direct to Docker Hub)\n'
    printf '  info only: nothing to probe, exiting 0\n'
    exit 0
fi

printf 'configured registry-mirrors:\n'
printf '%s\n' "$MIRRORS" | sed 's/^/  /'

# ---------------------------------------------------------------------------
# 3. probe each mirror (curl to https://<host>/v2/; 401/405/200 = alive,
#    000/timeout/connection-refused = dead)
# ---------------------------------------------------------------------------
ALIVE=0
DEAD=0
FAILURES=""

probe_mirror() { # url
    local url="$1" host code ms
    host=$(printf '%s' "$url" | sed -E 's#^[a-zA-Z]+://##; s#/.*$##; s#.*@##')
    # shellcheck disable=SC2086
    t0=$(date +%s%N 2>/dev/null || date +%s)
    code=$(curl $IP_MODE -s -o /dev/null -w '%{http_code}' -m "$CURL_TIMEOUT" \
        "https://$host/v2/" 2>/dev/null)
    t1=$(date +%s%N 2>/dev/null || date +%s)
    case "$code" in
        200|401|403|405)
            ALIVE=$((ALIVE + 1))
            printf '  [OK]   %-30s HTTP %s\n' "$host" "$code"
            ;;
        *)
            # 000 = curl timeout / connection refused; anything else = odd
            # status. Both count as a dead mirror so we never falsely report
            # "all online" when a mirror silently times out.
            DEAD=$((DEAD + 1))
            printf '  [FAIL] %-30s HTTP %s\n' "$host" "${code:-timeout/refused}"
            FAILURES="$FAILURES $url"
            ;;
    esac
}

printf 'mirror connectivity probe (timeout %ss/mirror):\n' "$CURL_TIMEOUT"
while IFS= read -r m; do
    [ -n "$m" ] || continue
    probe_mirror "$m"
done <<< "$MIRRORS"

# ---------------------------------------------------------------------------
# 4. optional end-to-end pull of hello-world
# ---------------------------------------------------------------------------
if [ "$PULL" -eq 1 ]; then
    printf 'end-to-end check: docker pull hello-world ...\n'
    OUT=$(docker pull hello-world 2>&1); RC=$?
    if [ "$RC" -eq 0 ]; then
        printf '  [OK] hello-world pulled via mirror chain\n'
    else
        DEAD=$((DEAD + 1))
        printf '  [FAIL] hello-world pull failed (rc=%s):\n%s\n' "$RC" "$OUT"
        FAILURES="$FAILURES pull:hello-world"
    fi
fi

# ---------------------------------------------------------------------------
# 5. summary + exit code
# ---------------------------------------------------------------------------
printf 'summary: %s mirror(s) alive, %s failed\n' "$ALIVE" "$DEAD"
if [ "$DEAD" -gt 0 ]; then
    printf 'unreachable: %s\n' "$FAILURES" | sed 's/^/  /'
    printf 'check-docker-mirrors: %s mirror(s) NOT reachable - fix before proceeding\n' "$DEAD"
    exit 1
fi
printf 'check-docker-mirrors: all mirrors online - OK\n'
exit 0
