#!/usr/bin/env bash
# check-secrets.sh - FlyGo pre-submit sensitive-file check.
#
# Enforces AGENTS.md rule #2 ("敏感文件绝不入库": term_token.env /
# alist_pass.txt / docker-compose.yml.bak / *.env - always check before
# committing; no secrets in git). Pure bash, runs in git-bash (Windows) and
# Linux CI. Fails with a non-zero exit and a list of offenders when any
# listed file looks sensitive.
#
# Usage:
#   scripts/check-secrets.sh            # check staged changes (default)
#   scripts/check-secrets.sh --staged   # same
#   scripts/check-secrets.sh --all      # whole tree incl. untracked files
#   scripts/check-secrets.sh --self-test
#
# Install as the repo pre-commit hook (per-repo, shared by all worktrees):
#   git config core.hooksPath .githooks
set -u

# ---------------------------------------------------------------------------
# detection rules (keep pure ASCII)
# ---------------------------------------------------------------------------

# Path rules: applied to the lower-cased path (basename or full path).
#  - the concrete AGENTS.md offenders
#  - secret-bearing extensions / well-known secret filenames
#  - backup artifacts (the real secret often lurks in .bak copies)
PATH_RULES=(
    'term_token\.env'
    'alist_pass\.txt'
    'docker-compose\.yml\.bak'
    '\.env$'
    '\.pem$'
    '\.p12$'
    '\.pfx$'
    '\.key$'
    'id_rsa$'
    'id_dsa$'
    'id_ed25519$'
    '\.netrc$'
    'credentials\.json$'
    'client_secret[^/]*\.json$'
    'service[-_]account[^/]*\.json$'
    '\.bak$'
)

# Content rules: secret-looking KEY=value / KEY: value / "KEY": "value"
# assignments. Only applied to config-like files (see content_scan_file);
# the value must be non-empty and at least 6 chars to skip booleans.
CONTENT_RE='(TOKEN|SECRET|PASSWORD|PASSWD|API[_-]?KEY|ACCESS[_-]?KEY|PRIVATE[_-]?KEY|CLIENT[_-]?SECRET|AUTH[_-]?TOKEN|CF_TOKEN|FENGDOG_SECRET)[A-Z0-9_]*["'"'"']?[[:space:]]*[:=][[:space:]]*["'"'"']?[^[:space:]"'"'"']{6,}'

# PEM private-key blocks are definitive anywhere, even inside code files.
PEM_RE='-----BEGIN (RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----'

# Config-like files get the KEY=value content scan; Rust/JS source is only
# scanned for PEM blocks, so ordinary code is never false-flagged on
# variable names.
content_scan_file() {
    local f="$1" base
    base="$(basename -- "$f")"
    case "$base" in
        *.env|*.env.*|*.toml|*.yaml|*.yml|*.json|*.ini|*.conf|*.cfg|*.properties|*.txt|*.bat|*.cmd|*.ps1|*.sh)
            return 0 ;;
    esac
    case "$f" in
        *config*|*secret*|*credential*|*cred*|*auth*|*token*|*passwd*|*password*)
            return 0 ;;
    esac
    return 1
}

# ---------------------------------------------------------------------------
# engine
# ---------------------------------------------------------------------------

OFFENDERS=0

report() { # file pattern detail
    printf 'SECRET-CHECK FAIL: %s  [%s] %s\n' "$1" "$2" "$3"
    OFFENDERS=$((OFFENDERS + 1))
}

check_path() { # file
    local f="$1" lower rule
    lower=$(printf '%s' "$f" | tr '[:upper:]' '[:lower:]')
    for rule in "${PATH_RULES[@]}"; do
        if printf '%s' "$lower" | grep -qE -- "$rule"; then
            report "$f" "path rule: $rule" "sensitive filename pattern"
            return 1
        fi
    done
    return 0
}

check_content() { # file content (already read; files with NUL skipped)
    local f="$1" input="$2"
    if printf '%s' "$input" | grep -aqE -- "$PEM_RE"; then
        report "$f" "content: private key block" "PEM private key embedded in file"
        return 1
    fi
    if content_scan_file "$f"; then
        if printf '%s' "$input" | grep -aqiE -- "$CONTENT_RE"; then
            report "$f" "content: secret assignment" "KEY=VALUE with a non-empty value"
            return 1
        fi
    fi
    return 0
}

check_blob() { # file (staged index blob in --staged mode, working file in --all)
    local f="$1" src
    if [ "$MODE" = staged ]; then
        src=$(git show ":$f" 2>/dev/null) || return 0
    else
        src=$(cat "$f" 2>/dev/null) || return 0
    fi
    check_path "$f" && {
        # Cheap text/binary gate: files with NUL bytes are never scanned for
        # content patterns (binary blobs can false-positive on random bytes).
        if printf '%s' "$src" | tr -d '\000' | cmp -s - <(printf '%s' "$src"); then
            check_content "$f" "$src"
        fi
    }
    return 0
}

# ---------------------------------------------------------------------------
# modes
# ---------------------------------------------------------------------------

run_staged() {
    local f
    while IFS= read -r -d '' f; do
        check_blob "$f"
    done < <(git diff --cached --name-only -z)
}

run_all() {
    local f
    while IFS= read -r -d '' f; do
        check_blob "$f"
    done < <(git ls-files -z; git ls-files --others --exclude-standard -z)
}

self_test() {
    local tmp rc f
    tmp=$(mktemp -d) || { printf 'self-test: mktemp failed\n'; exit 1; }
    # Path-rule offenders
    touch "$tmp/term_token.env" "$tmp/alist_pass.txt" "$tmp/docker-compose.yml.bak" "$tmp/x.env" "$tmp/id_rsa" "$tmp/notes.bak"
    # Content-rule offenders (config-like files). Fixtures are pieced
    # together at runtime so the checker's own source never carries a
    # contiguous secret-shaped literal (it scans every file, itself included).
    tok="FENGDOG_""SECRET"; v="deadbeef""cafe"
    printf '%s=%s\n' "$tok" "$v" > "$tmp/s1.toml"
    tok="TOK""EN"; v="abc123""secret"
    printf '%s = "%s"\n' "$tok" "$v" > "$tmp/s2.conf"
    k="pass""word"; v="hunter2""hunter2"
    printf '%s: %s\n' "$k" "$v" > "$tmp/s3.yml"
    pk="PRIVATE"
    printf '%s\n' "-----BEGIN $pk KEY-----" > "$tmp/pk.txt"
    # Allowed files: code with a "token" variable, boolean values, docs
    printf 'let %s = "%s"\n' "tok""en" "resolve" > "$tmp/ok.rs"
    printf 'enabled = true\n' > "$tmp/ok.toml"
    printf '# normal comment\n' > "$tmp/ok2.md"
    for f in "$tmp"/*; do
        [ -f "$f" ] || continue
        MODE=all check_blob "$f"
    done
    rc=0
    [ "$OFFENDERS" -eq 10 ] || { printf 'SELF-TEST FAIL: expected 10 offenders, got %s\n' "$OFFENDERS"; rc=1; }
    rm -rf "$tmp"
    if [ "$rc" -eq 0 ]; then
        printf 'SELF-TEST OK (10 offenders detected, 3 allowed files passed)\n'
    fi
    exit "$rc"
}

# ---------------------------------------------------------------------------

MODE=staged
case "${1:-}" in
    ''|--staged) MODE=staged ;;
    --all) MODE=all ;;
    --self-test) self_test ;;
    -h|--help)
        sed -n '2,22p' "$0"
        exit 0
        ;;
    *)
        printf 'unknown option: %s (see --help)\n' "${1:-}"
        exit 2
        ;;
esac

run_staged
if [ "$OFFENDERS" -eq 0 ]; then
    printf 'check-secrets: clean - no sensitive files in the staged changes\n'
else
    printf 'check-secrets: %s sensitive file(s) blocked - fix before committing\n' "$OFFENDERS"
fi
exit "$OFFENDERS"