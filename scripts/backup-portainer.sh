#!/usr/bin/env bash
# backup-portainer.sh - FlyGo portainer.db scheduled backup.
#
# Copies C:\FlyGoLegacy\portainer\portainer.db (bbolt DB) out to a
# non-FlyGoLegacy backup root with a timestamp, keeps the newest N copies,
# verifies sha256(src) == sha256(copy), and reports the portainer
# container running state. Pure bash, runs in git-bash (Windows) and
# Linux CI.
#
# HARD RED LINE (AGENTS.md rule #1): C:\FlyGoLegacy is READ-ONLY. We only
# READ the source portainer.db and write the backup OUTSIDE FlyGoLegacy
# (default: repo root .wt-backups/portainer/, which is git-ignored via
# the .wt-*/ rule). Never create the backup tree under C:\FlyGoLegacy.
#
# Usage:
#   scripts/backup-portainer.sh                     # run with defaults
#   scripts/backup-portainer.sh --help
#   scripts/backup-portainer.sh --keep 10           # keep newest 10
#   scripts/backup-portainer.sh --out /d/backups    # custom backup root
#
# Env overrides:
#   PORTAINER_DB    source db path (default from --src or /c/FlyGoLegacy/...)
#   BACKUP_ROOT     backup root dir (default repo root .wt-backups/portainer)
#   KEEP            number of copies to keep (default 5)
#
# Windows scheduled task (schtasks) example - NOT registered by this
# script; paste/schedule manually if you want a daily run. This creates a
# task "portainer-backup" that runs the script every day at 02:30.
#
#   schtasks /Create /TN "portainer-backup" /SC DAILY /ST 02:30 \
#       /TR "\"<Git>\bin\bash.exe\" -lc \
#       \"$(pwd)/scripts/backup-portainer.sh --src <portainer.db> >> $(pwd)/.wt-backups/portainer/backup.log 2>&1\""
#
# The log redirect appends to a plain file so cron/schtasks captures each
# run's JSON summary. Adjust the bash.exe path to your own Git install.
set -u

# ---------------------------------------------------------------------------
# defaults / config
# ---------------------------------------------------------------------------
DEFAULT_SRC="${PORTAINER_DB:-}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULT_OUT="$REPO_ROOT/.wt-backups/portainer"
DEFAULT_KEEP=5

SRC="$DEFAULT_SRC"
OUT="$DEFAULT_OUT"
KEEP="$DEFAULT_KEEP"

usage() {
    cat <<'EOF'
backup-portainer.sh - backup portainer.db out of FlyGoLegacy (read-only source).

Usage:
  backup-portainer.sh [--help] [--src PATH] [--out PATH] [--keep N]

Options:
  --help        Show this help and exit.
  --src PATH    Source portainer.db (required; or PORTAINER_DB env)
  --out PATH    Backup root dir (default <repo>/.wt-backups/portainer)
  --keep N      Number of timestamped copies to keep (default 5)

Env overrides (same effect): PORTAINER_DB, BACKUP_ROOT, KEEP.

Behavior:
  - Copies the db to <out>/portainer-YYYYmmdd-HHMMSS.db
  - Verifies sha256 of source vs copy (aborts if mismatch)
  - Prunes old copies, keeping the newest N (default 5)
  - Prints the portainer container running state (informational only)
  - Fails non-zero if the source db is missing/unreadable (never writes an
    empty placeholder backup)

Exit codes:
  0  success
  1  usage/config/source error or sha mismatch
EOF
}

# ---------------------------------------------------------------------------
# arg parsing (no "unknown option" surprises: unknown args are ignored-less,
# we fail loudly on them but still support --help / --src / --out / --keep)
# ---------------------------------------------------------------------------
parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --help|-h)
                usage
                exit 0
                ;;
            --src)
                [ $# -ge 2 ] || { echo "error: --src needs a value" >&2; exit 1; }
                SRC="$2"; shift 2
                ;;
            --out)
                [ $# -ge 2 ] || { echo "error: --out needs a value" >&2; exit 1; }
                OUT="$2"; shift 2
                ;;
            --keep)
                [ $# -ge 2 ] || { echo "error: --keep needs a value" >&2; exit 1; }
                KEEP="$2"; shift 2
                ;;
            *)
                echo "error: unknown option: $1 (use --help)" >&2
                exit 1
                ;;
        esac
    done
}

# Apply env overrides after CLI args (CLI wins when both? we let env be the
# fallback; parse first, then apply env only if CLI did not set it).
parse_args "$@"
[ -n "${PORTAINER_DB:-}" ] && SRC="$PORTAINER_DB"
[ -n "${BACKUP_ROOT:-}" ] && OUT="$BACKUP_ROOT"
[ -n "${KEEP:-}" ] && KEEP="$KEEP"

# sanity: KEEP must be a positive integer
if ! [[ "$KEEP" =~ ^[1-9][0-9]*$ ]]; then
    echo "error: --keep must be a positive integer (got: $KEEP)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# source checks: must exist and be readable, else fail (never write a fake)
# ---------------------------------------------------------------------------
if [ ! -f "$SRC" ]; then
    echo "error: source db not found: $SRC" >&2
    exit 1
fi
if [ ! -r "$SRC" ]; then
    echo "error: source db not readable: $SRC" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# container running state (informational only, never intervene)
# ---------------------------------------------------------------------------
CONTAINER_STATE="unknown"
if command -v docker >/dev/null 2>&1; then
    STATE="$(docker inspect portainer --format '{{.State.Status}} running={{.State.Running}}' 2>/dev/null)"
    if [ -n "$STATE" ]; then
        CONTAINER_STATE="$STATE"
    else
        CONTAINER_STATE="not-found-or-inspect-failed"
    fi
else
    CONTAINER_STATE="docker-cli-unavailable"
fi

# ---------------------------------------------------------------------------
# make backup root (OUT is guaranteed outside FlyGoLegacy by caller; we only
# create it under whatever the user supplied, never under /c/FlyGoLegacy)
# ---------------------------------------------------------------------------
mkdir -p "$OUT" || { echo "error: cannot create backup root: $OUT" >&2; exit 1; }

TS="$(date +%Y%m%d-%H%M%S)"
DEST="$OUT/portainer-$TS.db"

# ---------------------------------------------------------------------------
# copy + verify (cp from FlyGoLegacy read-only source; read only)
# ---------------------------------------------------------------------------
cp "$SRC" "$DEST"
if [ $? -ne 0 ]; then
    echo "error: cp failed: $SRC -> $DEST" >&2
    rm -f "$DEST"
    exit 1
fi

SRC_HASH="$(sha256sum "$SRC" | awk '{print $1}')"
DEST_HASH="$(sha256sum "$DEST" | awk '{print $1}')"

if [ -z "$SRC_HASH" ] || [ -z "$DEST_HASH" ]; then
    echo "error: sha256 unavailable" >&2
    rm -f "$DEST"
    exit 1
fi
if [ "$SRC_HASH" != "$DEST_HASH" ]; then
    echo "error: sha256 mismatch between source and copy" >&2
    echo "  src : $SRC_HASH" >&2
    echo "  dest: $DEST_HASH" >&2
    rm -f "$DEST"
    exit 1
fi

# ---------------------------------------------------------------------------
# prune: keep the newest KEEP portainer-*.db files (delete older ones).
# Collect names of pruned files into PRUNED (space-safe when names have no
# spaces; db filenames here never contain spaces by construction).
# ---------------------------------------------------------------------------
PRUNED=()
while IFS= read -r f; do
    [ -z "$f" ] && continue
    case "$f" in
        "$DEST") : ;; # never delete the one we just wrote
        *)
            rm -f "$f" && PRUNED+=("$f") && echo "pruned: $f"
            ;;
    esac
done < <(find "$OUT" -maxdepth 1 -type f -name 'portainer-*.db' -printf '%T@ %p\n' \
    | sort -rn \
    | tail -n +$((KEEP + 1)) \
    | sed 's/^[^ ]* //')

# ---------------------------------------------------------------------------
# report (JSON to stdout for cron/schtasks logging)
# ---------------------------------------------------------------------------
json_list() {
    if [ ${#PRUNED[@]} -eq 0 ]; then
        printf '[]'
        return
    fi
    local i
    printf '['
    for i in "${!PRUNED[@]}"; do
        [ $i -gt 0 ] && printf ','
        printf '"%s"' "${PRUNED[$i]}"
    done
    printf ']'
}

cat <<EOF
{
  "tool": "backup-portainer.sh",
  "timestamp": "$(date '+%Y-%m-%d %H:%M:%S')",
  "source": "$SRC",
  "backup": "$DEST",
  "sha256_source": "$SRC_HASH",
  "sha256_copy": "$DEST_HASH",
  "match": true,
  "container_state": "$CONTAINER_STATE",
  "keep": "$KEEP",
  "pruned": $(json_list)
}
EOF
