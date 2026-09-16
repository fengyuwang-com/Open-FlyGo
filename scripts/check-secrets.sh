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

# High-entropy fallback for source files: a quoted token of >= 32 chars from
# the base64/hex alphabet that mixes upper case, lower case and digits is a
# likely embedded secret (API keys, JWT-ish blobs). Only DOUBLE and SINGLE
# quoted tokens are scanned - unquoted code identifiers never match. Benign
# well-known constants (encoding alphabets) are allowlisted exactly below.
HIGHENT_DOUBLE_RE='"[A-Za-z0-9+/=_-]{32,}"'
HIGHENT_SINGLE_RE="'[A-Za-z0-9+/=_-]{32,}'"
# RFC 4648 base64 alphabets (standard + URL-safe + Crockford-ish variant):
# the only known benign 32+ mixed-case+digit literals in this repo.
HIGHENT_ALLOW=(
    '"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"'
    '"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"'
    '"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"'
)

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
    case "$base" in
        # Source code: keep PEM scan (separate rule, runs on every file)
        # but never KEY=value scan - source identifiers paired with
        # function calls are not secrets (e.g. panel/src/api/auth.rs).
        # The high-entropy fallback (source_scan_file) still applies.
        *.rs|*.js|*.ts|*.jsx|*.tsx|*.py|*.go|*.java|*.kt|*.c|*.cpp|*.h)
            return 1 ;;
    esac
    case "$f" in
        *config*|*secret*|*credential*|*cred*|*auth*|*token*|*passwd*|*password*)
            return 0 ;;
    esac
    return 1
}

# Source-code extensions get the PEM scan (above) plus the high-entropy
# fallback (quoted 32+ char mixed-case+digit tokens).
source_scan_file() {
    local base
    base="$(basename -- "$1")"
    case "$base" in
        *.rs|*.js|*.ts|*.jsx|*.tsx|*.py|*.go|*.java|*.kt|*.c|*.cpp|*.h)
            return 0 ;;
    esac
    return 1
}

check_high_entropy() { # file content
    local f="$1" tok rest allow skip
    while IFS= read -r tok; do
        [ -n "$tok" ] || continue
        skip=0
        for allow in "${HIGHENT_ALLOW[@]}"; do
            if [ "$tok" = "$allow" ]; then skip=1; break; fi
        done
        [ "$skip" = 1 ] && continue
        # Strip the surrounding quote and require upper + lower + digit.
        rest="${tok:1:${#tok}-2}"
        if printf '%s' "$rest" | grep -q '[A-Z]' \
            && printf '%s' "$rest" | grep -q '[a-z]' \
            && printf '%s' "$rest" | grep -q '[0-9]'; then
            report "$f" "content: high-entropy literal" "quoted 32+ char token with upper+lower+digit"
            return 1
        fi
    done < <(printf '%s' "$2" | grep -oE -- "$HIGHENT_DOUBLE_RE"; printf '%s' "$2" | grep -oE -- "$HIGHENT_SINGLE_RE")
    return 0
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
    elif source_scan_file "$f"; then
        check_high_entropy "$f" "$input" && return 0
        return 1
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
    # High-entropy fallback: source file with a 32+ char mixed-case+digit
    # quoted literal is flagged (value pieced together so this script does
    # not flag itself); the base64 alphabet constant is allowlisted.
    h="A1b2C3d4""E5f6G7h8I9j0K1l2M3n4O5p6"
    printf 'const %s = "%s"\n' "key" "$h" > "$tmp/s4.js"
    printf 'const ALPHA = "%s"\n' "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/" > "$tmp/ok3.rs"
    # Allowed files: code with a "token" variable, boolean values, docs
    printf 'let %s = "%s"\n' "tok""en" "resolve" > "$tmp/ok.rs"
    printf 'enabled = true\n' > "$tmp/ok.toml"
    printf '# normal comment\n' > "$tmp/ok2.md"
    for f in "$tmp"/*; do
        [ -f "$f" ] || continue
        MODE=all check_blob "$f"
    done
    rc=0
    [ "$OFFENDERS" -eq 11 ] || { printf 'SELF-TEST FAIL: expected 11 offenders, got %s\n' "$OFFENDERS"; rc=1; }
    rm -rf "$tmp"
    if [ "$rc" -eq 0 ]; then
        printf 'SELF-TEST OK (11 offenders detected, 4 allowed files passed)\n'
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