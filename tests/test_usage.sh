#!/usr/bin/env bash
# usage: every section rendered from canned API responses.
#
# A stub `curl` on PATH answers each endpoint from a fixture file and logs the
# arguments it was called with, so the tests see exactly what the script sends
# and never touch the network. USAGE_NOW pins the clock for the reset times and
# the start of the month.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"
SCRIPT="$HERE/../scripts/usage"

if ! command -v jq >/dev/null 2>&1; then
    # CI sets USAGE_NEED_JQ, so a runner that lost jq fails instead of passing
    # with every check here skipped.
    if [ -n "${USAGE_NEED_JQ:-}" ]; then
        printf '  FAIL: jq not installed (USAGE_NEED_JQ is set)\n'
        exit 1
    fi
    printf '  skip: jq not installed\n'
    finish
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/usage-test.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
STUB="$WORK/stub"
mkdir -p "$STUB"

# 2026-10-25T23:00:00Z
NOW=1792969200

cat > "$STUB/curl" <<'STUB'
#!/usr/bin/env bash
# Headers and bodies arrive as `@file` (so secrets stay out of `ps`); log
# their contents. A response is $key.json / $key.code, or $key.<token>.json /
# .code when the request's bearer token has its own; $key.sleep delays it.
printf '%s\n' "$*" >> "$FIXTURES/curl.args"
touch "$FIXTURES/curl-started"
tok=""
for a in "$@"; do
    case "$a" in
        @*) content=$(cat "${a#@}")
            printf '%s\n' "$content" >> "$FIXTURES/curl.log"
            t=$(printf '%s\n' "$content" | sed -n 's/^Authorization: Bearer //p')
            [ -n "$t" ] && tok=$t ;;
        *)  printf '%s\n' "$a" >> "$FIXTURES/curl.log" ;;
    esac
done
url=""
for a in "$@"; do case "$a" in https://*) url="$a" ;; esac; done
case "$url" in
    https://api.anthropic.com/api/oauth/usage*)             key=claude ;;
    https://chatgpt.com/backend-api/wham/usage*)            key=codex ;;
    https://api.anthropic.com/v1/organizations/cost_report*) key=anthropic ;;
    https://api.openai.com/v1/organization/costs*)          key=openai ;;
    https://platform.claude.com/v1/oauth/token*)            key=claude-token ;;
    https://auth.openai.com/oauth/token*)                   key=codex-token ;;
    *) echo "stub curl: unexpected url $url" >&2; exit 7 ;;
esac
[ -e "$FIXTURES/$key.sleep" ] && sleep "$(cat "$FIXTURES/$key.sleep")"
# $key.broken: the transfer dies partway, after some of the body arrived.
if [ -e "$FIXTURES/$key.broken" ]; then
    head -c 60 "$FIXTURES/$key.json"
    echo "curl: (56) Failure when receiving data from the peer" >&2
    exit 56
fi
body="$FIXTURES/$key.json" code="$FIXTURES/$key.code"
[ -n "$tok" ] && [ -e "$FIXTURES/$key.$tok.json" ] && body="$FIXTURES/$key.$tok.json"
[ -n "$tok" ] && [ -e "$FIXTURES/$key.$tok.code" ] && code="$FIXTURES/$key.$tok.code"
cat "$body"
printf '\n%s' "$(cat "$code" 2>/dev/null || echo 200)"
STUB
chmod +x "$STUB/curl"

# The tests run on macOS too: uname says Linux unless a test writes
# $FIXTURES/uname, and file modes are read either platform's way.
cat > "$STUB/uname" <<'STUB'
#!/usr/bin/env bash
cat "$FIXTURES/uname" 2>/dev/null || echo Linux
STUB
chmod +x "$STUB/uname"
mode() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1" 2>&1; }

# setup: fresh fixture dir, logged-in Claude and Codex, no admin keys.
setup() {
    FIXTURES="$WORK/fx.$1"
    mkdir -p "$FIXTURES/claude" "$FIXTURES/codex"
    : > "$FIXTURES/curl.log"
    : > "$FIXTURES/curl.args"
    cat > "$FIXTURES/claude/.credentials.json" <<EOF
{"claudeAiOauth":{"accessToken":"claude-tok","expiresAt":$(( (NOW + 3600) * 1000 )),"subscriptionType":"max"}}
EOF
    cat > "$FIXTURES/codex/auth.json" <<'EOF'
{"auth_mode":"chatgpt","tokens":{"access_token":"codex-tok","account_id":"acct-1"}}
EOF
    cat > "$FIXTURES/claude.json" <<'EOF'
{"five_hour":{"utilization":8.0,"resets_at":"2026-10-26T03:08:00.437882+00:00"},
 "seven_day":{"utilization":3.0,"resets_at":"2026-11-01T22:00:00.4379+00:00"},
 "extra_usage":{"is_enabled":false},
 "limits":[
  {"kind":"session","group":"session","percent":8,"resets_at":"2026-10-26T03:08:00.437882+00:00","scope":null},
  {"kind":"weekly_all","group":"weekly","percent":3,"resets_at":"2026-11-01T22:00:00.4379+00:00","scope":null},
  {"kind":"weekly_scoped","group":"weekly","percent":40,"resets_at":"2026-11-01T22:00:00+00:00",
   "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null}}]}
EOF
    cat > "$FIXTURES/codex.json" <<EOF
{"plan_type":"plus",
 "rate_limit":{"primary_window":{"used_percent":19,"limit_window_seconds":18000,"reset_at":$((NOW + 4*3600 + 56*60))},
               "secondary_window":{"used_percent":24,"limit_window_seconds":604800,"reset_at":$((NOW + 6*86400 + 9*3600))}},
 "credits":{"has_credits":false,"unlimited":false,"balance":"0"}}
EOF
}

# run_usage [args...] -> stdout+stderr in $OUT, status in $RC
run_usage() {
    OUT=$(env -u ANTHROPIC_ADMIN_API_KEY -u OPENAI_ADMIN_KEY \
        PATH="$STUB:${BASE_PATH:-$PATH}" FIXTURES="$FIXTURES" USAGE_NOW="$NOW" \
        XDG_DATA_HOME="$FIXTURES/data" TZ=America/Los_Angeles \
        CLAUDE_CONFIG_DIR="$FIXTURES/claude" CODEX_HOME="$FIXTURES/codex" \
        "${EXTRA_ENV[@]}" bash "$SCRIPT" "$@" 2>&1)
    RC=$?
}

has() { case "$OUT" in *"$1"*) echo yes ;; *) echo no ;; esac; }
line() { printf '%s\n' "$OUT" | grep -F -- "$1" | head -1 | tr -s ' '; }
logged() { grep -qF -- "$1" "$FIXTURES/curl.log" && echo yes || echo no; }

# --- Claude subscription ------------------------------------------------------
setup claude
EXTRA_ENV=()
run_usage
check "claude: header names the plan" "yes" "$(has 'Claude · current login (max)')"
check "claude: session row" " session ██░░░░░░░░░░░░░░░░░░ 8% used resets in 4h08m" \
    "$(line 'session')"
check "claude: weekly row" " weekly █░░░░░░░░░░░░░░░░░░░ 3% used resets in 6d23h" \
    "$(line ' weekly ')"
check "claude: model-scoped weekly row is named" " weekly Fable ████████░░░░░░░░░░░░ 40% used resets in 6d23h" \
    "$(line 'Fable')"
check "claude: sends the login token" "yes" "$(logged 'Authorization: Bearer claude-tok')"
check "claude: sends the oauth beta header" "yes" "$(logged 'anthropic-beta: oauth-2025-04-20')"
check "claude: token kept off curl's command line" "no" \
    "$(grep -qF claude-tok "$FIXTURES/curl.args" && echo yes || echo no)"

# Older responses have only the per-window fields, no `limits` list.
setup claude-legacy
jq 'del(.limits) | .seven_day_opus = {"utilization": 50, "resets_at": "2026-11-01T22:00:00Z"}' \
    "$FIXTURES/claude.json" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$FIXTURES/claude.json"
run_usage
check "claude legacy: 5-hour row" " 5-hour ██░░░░░░░░░░░░░░░░░░ 8% used resets in 4h08m" "$(line '5-hour')"
check "claude legacy: opus row" " weekly opus ██████████░░░░░░░░░░ 50% used resets in 6d23h" "$(line 'opus')"

# Paid extra usage is shown when it is turned on.
setup claude-extra
jq '.extra_usage = {"is_enabled": true, "monthly_limit": 5000, "used_credits": 1234}' \
    "$FIXTURES/claude.json" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$FIXTURES/claude.json"
run_usage
check "claude: extra usage spend vs limit" " extra usage \$12.34 of \$50.00 this month" "$(line 'extra usage')"

# An expired login is reported, not refreshed: refreshing rotates the refresh
# token out from under Claude Code itself.
setup claude-expired
jq --argjson t $(( (NOW - 60) * 1000 )) '.claudeAiOauth.expiresAt = $t' \
    "$FIXTURES/claude/.credentials.json" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$FIXTURES/claude/.credentials.json"
run_usage
check "claude expired: says to run claude" "yes" "$(has 'login expired; run claude to refresh it')"
check "claude expired: no request made" "no" "$(logged 'api/oauth/usage')"
check "claude expired: never refreshed" "no" "$(logged 'oauth/token')"
check "claude expired: exits non-zero" "1" "$RC"

setup claude-none
rm "$FIXTURES/claude/.credentials.json"
run_usage
check "claude missing: says not logged in" "yes" "$(has 'Claude · current login: not logged in')"
check "claude missing: not a failure" "0" "$RC"

# macOS keeps the login in the keychain, not in a file.
setup claude-keychain
mv "$FIXTURES/claude/.credentials.json" "$FIXTURES/keychain.json"
echo Darwin > "$FIXTURES/uname"
cat > "$STUB/security" <<'EOF'
#!/usr/bin/env bash
[ "$1 $2 $3" = 'find-generic-password -s Claude Code-credentials' ] || exit 44
cat "$FIXTURES/keychain.json"
EOF
chmod +x "$STUB/security"
run_usage
rm "$STUB/security"
check "claude keychain: token read from the keychain" "yes" "$(logged 'Authorization: Bearer claude-tok')"

setup claude-500
echo 500 > "$FIXTURES/claude.code"
echo '{"error":{"message":"boom"}}' > "$FIXTURES/claude.json"
run_usage
check "claude http error: reported with status" "yes" "$(has 'Claude · current login: HTTP 500')"
check "claude http error: exits non-zero" "1" "$RC"

# --- Codex subscription -------------------------------------------------------
setup codex
run_usage
check "codex: header names the plan" "yes" "$(has 'Codex · current login (plus)')"
check "codex: 5-hour row" " 5-hour ████░░░░░░░░░░░░░░░░ 19% used resets in 4h56m" "$(line '5-hour')"
check "codex: weekly row" " weekly █████░░░░░░░░░░░░░░░ 24% used resets in 6d09h" \
    "$(printf '%s\n' "$OUT" | sed -n '/^Codex/,$p' | grep -F ' weekly ' | head -1 | tr -s ' ')"
check "codex: no credits line without credits" "no" "$(has 'credits')"
check "codex: sends the access token" "yes" "$(logged 'Authorization: Bearer codex-tok')"
check "codex: sends the account id" "yes" "$(logged 'ChatGPT-Account-Id: acct-1')"

setup codex-credits
jq '.credits = {"has_credits": true, "unlimited": false, "balance": "42.5"}' \
    "$FIXTURES/codex.json" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$FIXTURES/codex.json"
run_usage
check "codex: credit balance shown" " credits 42.5 left" "$(line 'credits')"

setup codex-401
echo 401 > "$FIXTURES/codex.code"
run_usage
check "codex 401: says to run codex" "yes" "$(has 'Codex · current login: login expired; run codex to refresh it')"
check "codex 401: current login never refreshed" "no" "$(logged 'auth.openai.com')"

setup codex-apikey
echo '{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}' > "$FIXTURES/codex/auth.json"
run_usage
check "codex api-key login: no plan to show" "yes" "$(has 'Codex · current login: signed in with an API key, not a ChatGPT plan')"
check "codex api-key login: no request made" "no" "$(logged 'wham/usage')"

# --- API spend ----------------------------------------------------------------
setup api-unset
run_usage
check "api: no section without keys" "no" "$(has 'API')"
check "api: no hint without keys" "no" "$(has 'ADMIN')"
check "api: last block is the last account" "  weekly" "$(printf '%s\n' "$OUT" | tail -1 | sed -E 's/^(  [a-z]+).*/\1/')"
check "api: no section is not a failure" "0" "$RC"

setup api-one-kind
api_fixtures
EXTRA_ENV=(ANTHROPIC_ADMIN_API_KEY=sk-ant-admin-x)
run_usage
EXTRA_ENV=()
check "api: one kind set, section shown" "yes" "$(has 'API spend')"
check "api: its line shown" "yes" "$(has 'Anthropic API · env')"
check "api: no hint for the other kind" "no" "$(has 'OpenAI')"

api_fixtures() {
    cat > "$FIXTURES/anthropic.json" <<'EOF'
{"data":[{"starting_at":"2026-10-01T00:00:00Z","results":[{"amount":"123.45","currency":"USD"}]},
         {"starting_at":"2026-10-02T00:00:00Z","results":[]},
         {"starting_at":"2026-10-03T00:00:00Z","results":[{"amount":"100","currency":"USD"},{"amount":"0.55","currency":"USD"}]}],
 "has_more":false,"next_page":null}
EOF
    cat > "$FIXTURES/openai.json" <<'EOF'
{"object":"page","data":[{"start_time":1790812800,"results":[{"amount":{"value":0.06,"currency":"usd"}}]},
                         {"start_time":1790899200,"results":[{"amount":{"value":3.5,"currency":"usd"}}]}],
 "has_more":false,"next_page":null}
EOF
}

setup api
api_fixtures
EXTRA_ENV=(ANTHROPIC_ADMIN_API_KEY=sk-ant-admin-x OPENAI_ADMIN_KEY=sk-admin-y)
run_usage
EXTRA_ENV=()
check "anthropic api: month-to-date spend" " Anthropic API · env \$2.24 spent this month" "$(line 'Anthropic API')"
check "openai api: month-to-date spend" " OpenAI API · env \$3.56 spent this month" "$(line 'OpenAI API')"
check "anthropic api: asks from the first of the month" "yes" "$(logged 'starting_at=2026-10-01T00:00:00Z')"
check "anthropic api: sends the admin key" "yes" "$(logged 'x-api-key: sk-ant-admin-x')"
check "openai api: asks from the first of the month" "yes" "$(logged 'start_time=1790812800')"
check "openai api: sends the admin key" "yes" "$(logged 'Authorization: Bearer sk-admin-y')"
check "all sources fine: exits 0" "0" "$RC"

# --- saved Admin keys ---------------------------------------------------------
setup keys
KEYS="$FIXTURES/data/usage/keys"
api_fixtures
run_usage add anthropic-api work <<<'sk-ant-admin-a'
check "add key: exits 0" "0" "$RC"
check "add key: stored 0600" "600" "$(mode "$KEYS/anthropic-api/work")"
check "add key: dir 0700" "700" "$(mode "$KEYS")"
run_usage add openai-api personal <<<'sk-admin-b'
EXTRA_ENV=(ANTHROPIC_ADMIN_API_KEY=sk-ant-admin-env)
run_usage
EXTRA_ENV=()
check "keys: env key labelled env" " Anthropic API · env \$2.24 spent this month" "$(line 'Anthropic API · env')"
check "keys: saved key labelled by name" " Anthropic API · work \$2.24 spent this month" "$(line 'Anthropic API · work')"
check "keys: saved openai key" " OpenAI API · personal \$3.56 spent this month" "$(line 'OpenAI API · personal')"
check "keys: no hint once a key exists" "no" "$(has 'set OPENAI_ADMIN_KEY')"
check "keys: saved key sent" "yes" "$(logged 'x-api-key: sk-ant-admin-a')"
check "keys: key never on argv" "no" "$(grep -qF sk-ant-admin-a "$FIXTURES/curl.args" && echo yes || echo no)"
run_usage add anthropic-api work <<<'sk-ant-admin-c'
check "add key again: replaces it" "sk-ant-admin-c" "$(cat "$KEYS/anthropic-api/work")"
run_usage add anthropic-api empty <<<''
check "add empty key: refused" "2" "$RC"
check "add empty key: nothing stored" "no" "$([ -e "$KEYS/anthropic-api/empty" ] && echo yes || echo no)"
run_usage remove anthropic-api work
check "remove key: exits 0" "0" "$RC"
check "remove key: file gone" "no" "$([ -e "$KEYS/anthropic-api/work" ] && echo yes || echo no)"
run_usage remove anthropic-api nosuch
check "remove unknown: exits 1" "1" "$RC"

setup names
for bad in '../x' 'a/b' '' '.' '..' '.hidden' 'has space'; do
    run_usage add anthropic-api "$bad" <<<'k'
    check "bad name [$bad] refused" "2" "$RC"
    run_usage remove anthropic-api "$bad"
    check "bad name [$bad] refused by remove" "2" "$RC"
done
check "bad names: nothing written" "no" "$([ -e "$FIXTURES/data" ] && echo yes || echo no)"
run_usage add bogus x
check "unknown kind: exits 2" "2" "$RC"
run_usage add anthropic-api
check "add without a name: exits 2" "2" "$RC"
run_usage frobnicate
check "unknown subcommand: exits 2" "2" "$RC"

# --- saved Claude and Codex logins ---------------------------------------------
# Stub CLIs: log the config dir they were given and their arguments; a login
# writes a fresh credentials file there (a new token each time), unless
# $FIXTURES/login-fails exists.
cat > "$STUB/claude" <<'EOF'
#!/usr/bin/env bash
printf '%s|%s\n' "$CLAUDE_CONFIG_DIR" "$*" >> "$FIXTURES/cli.log"
[ "$1 $2" = "auth login" ] || exit 0
[ -e "$FIXTURES/login-fails" ] && exit 1
n=$(( $(cat "$FIXTURES/logins" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$FIXTURES/logins"
mkdir -p "$CLAUDE_CONFIG_DIR"
printf '{"claudeAiOauth":{"accessToken":"tok-%s","refreshToken":"ref-%s","expiresAt":%s,"scopes":["user:inference","user:profile"],"subscriptionType":"max"}}\n' \
    "$n" "$n" "$(( (USAGE_NOW + 3600) * 1000 ))" > "$CLAUDE_CONFIG_DIR/.credentials.json"
printf '{"oauthAccount":{"accountUuid":"u-%s","emailAddress":"c%s@example.com"}}\n' "$n" "$n" > "$CLAUDE_CONFIG_DIR/.claude.json"
EOF
cat > "$STUB/codex" <<'EOF'
#!/usr/bin/env bash
printf '%s|%s\n' "$CODEX_HOME" "$*" >> "$FIXTURES/cli.log"
[ "$1" = "login" ] || exit 0
[ -e "$FIXTURES/login-fails" ] && exit 1
n=$(( $(cat "$FIXTURES/logins" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$FIXTURES/logins"
mkdir -p "$CODEX_HOME"
printf '{"auth_mode":"chatgpt","tokens":{"id_token":"x.e30.y","access_token":"codex-%s","refresh_token":"ref-%s","account_id":"acct-%s"}}\n' \
    "$n" "$n" "$n" > "$CODEX_HOME/auth.json"
EOF
chmod +x "$STUB/claude" "$STUB/codex"
cli_logged() { grep -qxF -- "$1" "$FIXTURES/cli.log" && echo yes || echo no; }
exists() { [ -e "$1" ] && echo yes || echo no; }
# await <file>: wait up to 10s for it, so a broken setup fails rather than hangs.
await() { local _; for _ in $(seq 200); do [ -e "$1" ] && return 0; sleep 0.05; done; return 1; }
HAVE_FLOCK=$(command -v flock >/dev/null 2>&1 && echo yes || echo no)

# Saving a login needs flock, which macOS lacks.
if [ "$HAVE_FLOCK" = yes ]; then
setup add
ACC="$FIXTURES/data/usage/accounts"
D="$ACC/claude/work"
run_usage add claude work
check "add claude: exits 0" "0" "$RC"
check "add claude: login ran in the account dir" "yes" "$(cli_logged "$D|auth login --claudeai")"
check "add claude: credentials saved there" "tok-1" "$(jq -r .claudeAiOauth.accessToken "$D/.credentials.json" 2>&1)"
check "add claude: dir 0700" "700" "$(mode "$D")"
check "add claude: accounts dir 0700" "700" "$(mode "$ACC")"
run_usage add codex alt --device-auth
check "add codex: exits 0" "0" "$RC"
check "add codex: file store forced, args passed through" "yes" \
    "$(cli_logged "$ACC/codex/alt|login -c cli_auth_credentials_store=\"file\" --device-auth")"
check "add codex: credentials saved there" "codex-2" "$(jq -r .tokens.access_token "$ACC/codex/alt/auth.json" 2>&1)"
check "add: the CLIs' own logins untouched" "no" \
    "$(grep -qE "^($FIXTURES/claude|$FIXTURES/codex)\|" "$FIXTURES/cli.log" && echo yes || echo no)"

touch "$FIXTURES/login-fails"
run_usage add claude broken
check "failed add: exits 1" "1" "$RC"
check "failed add: no account left behind" "no" "$(exists "$ACC/claude/broken")"
run_usage add codex broken
check "failed codex add: no account left behind" "no" "$(exists "$ACC/codex/broken")"
cp "$D/.credentials.json" "$FIXTURES/before.json"
run_usage add claude work
check "failed re-add: exits 1" "1" "$RC"
check "failed re-add: old login kept" "yes" "$(cmp -s "$D/.credentials.json" "$FIXTURES/before.json" && echo yes || echo no)"
check "failed re-add: no staging dir left" "" "$(find "$ACC/claude" -mindepth 1 -maxdepth 1 -name '.*')"
rm "$FIXTURES/login-fails"
run_usage add claude work
check "re-add: new login replaces the old" "tok-3" "$(jq -r .claudeAiOauth.accessToken "$D/.credentials.json" 2>&1)"
check "re-add: no staging dir left" "" "$(find "$ACC/claude" -mindepth 1 -maxdepth 1 -name '.*')"

# A login that exits 0 without writing credentials is not a saved account.
cat > "$STUB/claude.quiet" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$STUB/claude.quiet"
mv "$STUB/claude" "$STUB/claude.real" && mv "$STUB/claude.quiet" "$STUB/claude"
run_usage add claude empty
mv "$STUB/claude" "$STUB/claude.quiet" && mv "$STUB/claude.real" "$STUB/claude"
check "add with no credentials written: exits 1" "1" "$RC"
check "add with no credentials written: nothing kept" "no" "$(exists "$ACC/claude/empty")"

run_usage remove claude work
check "remove claude: exits 0" "0" "$RC"
check "remove claude: logged the session out" "yes" "$(cli_logged "$D|auth logout")"
check "remove claude: dir gone" "no" "$(exists "$D")"
run_usage remove codex alt
check "remove codex: logged the session out" "yes" "$(cli_logged "$ACC/codex/alt|logout -c cli_auth_credentials_store=\"file\"")"
check "remove codex: dir gone" "no" "$(exists "$ACC/codex/alt")"
run_usage remove codex nosuch
check "remove unknown login: exits 1" "1" "$RC"
else
    printf '  skip: saving logins (no flock)\n'
fi

# Saved Claude accounts are Linux-only: the macOS keychain entry for a custom
# config dir is unverified.
setup add-macos
echo Darwin > "$FIXTURES/uname"
run_usage add claude mac
check "add claude on macOS: refused" "yes" "$(has 'not supported on macOS')"
check "add claude on macOS: exits 1" "1" "$RC"
check "add claude on macOS: no login run" "no" "$(exists "$FIXTURES/cli.log")"

# --- saved Claude accounts, renewed by usage ------------------------------------
# saved_claude <name> <access> <refresh> <expires (epoch s)> <email>
saved_claude() {
    local d="$FIXTURES/data/usage/accounts/claude/$1"
    mkdir -p "$d" && chmod 700 "$FIXTURES/data/usage/accounts" "$d"
    jq -n --arg a "$2" --arg r "$3" --argjson e "$(( $4 * 1000 ))" \
        '{claudeAiOauth: {accessToken: $a, refreshToken: $r, expiresAt: $e,
          scopes: ["user:inference", "user:profile"], subscriptionType: "max"}}' > "$d/.credentials.json"
    chmod 600 "$d/.credentials.json"
    jq -n --arg m "$5" --arg u "u-$1" '{oauthAccount: {accountUuid: $u, emailAddress: $m}}' > "$d/.claude.json"
}
token_calls() { grep -c "$1" "$FIXTURES/curl.args"; }

setup claude-saved
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
saved_claude home tok-h ref-h $((NOW + 3600)) h@example.com
run_usage
check "saved: header with name, plan, email" "yes" "$(has 'Claude · work (max) · w@example.com')"
check "saved: the other account too" "yes" "$(has 'Claude · home (max) · h@example.com')"
check "saved: after the current login, by name" "Claude · current login (max)|Claude · home (max) · h@example.com|Claude · work (max) · w@example.com" \
    "$(printf '%s\n' "$OUT" | grep '^Claude' | paste -sd'|' -)"
check "saved: rows under each account" "3" "$(printf '%s\n' "$OUT" | grep -c ' session ')"
check "saved: work's token sent" "yes" "$(logged 'Bearer tok-w')"
check "saved: home's token sent" "yes" "$(logged 'Bearer tok-h')"
check "saved: no refresh while fresh" "0" "$(token_calls platform.claude.com)"
check "saved: exits 0" "0" "$RC"

setup claude-unreadable
saved_claude work tok-old ref-old $((NOW + 3600)) w@example.com
rm "$FIXTURES/data/usage/accounts/claude/work/.credentials.json"
run_usage
check "saved account without a login: says to sign in again" "yes" "$(has 'Claude · work: signed out; run usage add claude work to sign in again')"

# Renewing takes a lock with flock (util-linux), which macOS lacks.
if command -v flock >/dev/null 2>&1; then

setup claude-renew
saved_claude work tok-old ref-old $((NOW + 60)) w@example.com
C="$FIXTURES/data/usage/accounts/claude/work/.credentials.json"
echo '{"access_token":"tok-new","refresh_token":"ref-new","expires_in":28800}' > "$FIXTURES/claude-token.json"
run_usage
check "renew: one refresh" "1" "$(token_calls platform.claude.com/v1/oauth/token)"
check "renew: request carries the refresh token" "yes" "$(logged '"refresh_token":"ref-old"')"
check "renew: request carries the client id" "yes" "$(logged '"client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e"')"
check "renew: request carries the saved scopes" "yes" "$(logged '"scope":"user:inference user:profile"')"
check "renew: refresh token off argv" "no" "$(grep -qF ref-old "$FIXTURES/curl.args" && echo yes || echo no)"
check "renew: new tokens written back" "tok-new ref-new $(( (NOW + 28800) * 1000 ))" \
    "$(jq -r '.claudeAiOauth | "\(.accessToken) \(.refreshToken) \(.expiresAt)"' "$C")"
check "renew: other fields kept" "max" "$(jq -r .claudeAiOauth.subscriptionType "$C")"
check "renew: still 0600" "600" "$(mode "$C")"
check "renew: usage read with the new token" "yes" "$(logged 'Bearer tok-new')"
check "renew: exits 0" "0" "$RC"

setup claude-renew-keeps-refresh
saved_claude work tok-old ref-old $((NOW - 10)) w@example.com
C="$FIXTURES/data/usage/accounts/claude/work/.credentials.json"
echo '{"access_token":"tok-new","expires_in":28800}' > "$FIXTURES/claude-token.json"
run_usage
check "renew without a new refresh token: old one kept" "tok-new ref-old" \
    "$(jq -r '.claudeAiOauth | "\(.accessToken) \(.refreshToken)"' "$C")"

setup claude-401-retry
saved_claude work tok-old ref-old $((NOW + 3600)) w@example.com
echo 401 > "$FIXTURES/claude.tok-old.code"
echo '{"error":{"message":"expired"}}' > "$FIXTURES/claude.tok-old.json"
echo '{"access_token":"tok-new","refresh_token":"ref-new","expires_in":28800}' > "$FIXTURES/claude-token.json"
run_usage
check "401: renewed once" "1" "$(token_calls platform.claude.com/v1/oauth/token)"
check "401: retried with the new token" "yes" "$(logged 'Bearer tok-new')"
check "401: rows shown" "2" "$(printf '%s\n' "$OUT" | grep -c ' session ')"
check "401: exits 0" "0" "$RC"

setup claude-401-twice
saved_claude work tok-old ref-old $((NOW + 3600)) w@example.com
echo 401 > "$FIXTURES/claude.code"
echo '{"access_token":"tok-new","refresh_token":"ref-new","expires_in":28800}' > "$FIXTURES/claude-token.json"
run_usage
check "401 after renewing: no second renewal" "1" "$(token_calls platform.claude.com/v1/oauth/token)"
check "401 after renewing: says to sign in again" "yes" "$(has 'Claude · work: signed out; run usage add claude work to sign in again')"

setup claude-rejected
saved_claude work tok-old ref-old $((NOW - 10)) w@example.com
C="$FIXTURES/data/usage/accounts/claude/work/.credentials.json"
echo 400 > "$FIXTURES/claude-token.code"
echo '{"error":"invalid_grant","error_description":"Refresh token not found or invalid"}' > "$FIXTURES/claude-token.json"
cp "$C" "$FIXTURES/before.json"
run_usage
check "rejected: says how to sign in again" "yes" "$(has 'Claude · work: signed out; run usage add claude work to sign in again')"
check "rejected: no raw HTTP error" "no" "$(has 'HTTP 400')"
check "rejected: credentials untouched" "yes" "$(cmp -s "$C" "$FIXTURES/before.json" && echo yes || echo no)"
check "rejected: exits 1" "1" "$RC"

setup claude-transient
saved_claude home tok-h ref-h $((NOW + 3600)) h@example.com
saved_claude work tok-old ref-old $((NOW - 10)) w@example.com
C="$FIXTURES/data/usage/accounts/claude/work/.credentials.json"
echo 503 > "$FIXTURES/claude-token.code"
echo '{"error":{"message":"overloaded"}}' > "$FIXTURES/claude-token.json"
cp "$C" "$FIXTURES/before.json"
run_usage
check "transient: says it could not renew" "yes" "$(has 'Claude · work: could not renew sign-in: HTTP 503')"
check "transient: credentials untouched" "yes" "$(cmp -s "$C" "$FIXTURES/before.json" && echo yes || echo no)"
check "transient: the other account still shown" "yes" "$(has 'Claude · home (max) · h@example.com')"
check "transient: exits 1" "1" "$RC"

# A renewal whose answer breaks off mid-transfer may already hold the new
# tokens; the error must say what curl said, never echo what arrived.
setup claude-broken-transfer
saved_claude work tok-old ref-old $((NOW - 10)) w@example.com
C="$FIXTURES/data/usage/accounts/claude/work/.credentials.json"
echo '{"access_token":"tok-LEAKED-secret","refresh_token":"ref-LEAKED-secret","expires_in":28800}' > "$FIXTURES/claude-token.json"
touch "$FIXTURES/claude-token.broken"
cp "$C" "$FIXTURES/before.json"
run_usage
check "broken transfer: says what curl said" "yes" "$(has 'Claude · work: could not renew sign-in: request failed: curl: (56) Failure when receiving data from the peer')"
check "broken transfer: no token printed" "no" "$(has 'LEAKED')"
check "broken transfer: credentials untouched" "yes" "$(cmp -s "$C" "$FIXTURES/before.json" && echo yes || echo no)"

setup usage-broken-transfer
touch "$FIXTURES/codex.broken"
run_usage
check "broken usage transfer: says what curl said" "yes" "$(has 'Codex · current login: request failed: curl: (56) Failure when receiving data from the peer')"
check "broken usage transfer: no body printed" "no" "$(has 'rate_limit')"

# Two runs at once must renew once: the second waits for the lock, re-reads the
# file and finds it fresh. A second renewal would spend a rotated refresh token.
setup claude-lock
saved_claude work tok-old ref-old $((NOW - 10)) w@example.com
echo '{"access_token":"tok-new","refresh_token":"ref-new","expires_in":28800}' > "$FIXTURES/claude-token.json"
echo 1 > "$FIXTURES/claude-token.sleep"
run_usage & run_usage; wait
check "lock: one renewal across two runs" "1" "$(token_calls platform.claude.com/v1/oauth/token)"
check "lock: the waiting run used the new token" "2" "$(grep -c 'Bearer tok-new' "$FIXTURES/curl.log")"

else
    printf '  skip: Claude renewal (no flock)\n'
fi

# --- saved Codex accounts, renewed by usage -------------------------------------
b64url() { base64 | tr -d '\n=' | tr '+/' '-_'; }
mkjwt() { printf '%s.%s.sig' "$(printf '{"alg":"none"}' | b64url)" "$(printf '%s' "$1" | b64url)"; }
# codex_at <name> <exp>: the access token saved_codex gives that account.
codex_at() { mkjwt "{\"exp\":$2,\"who\":\"$1\"}"; }
# saved_codex <name> <access exp (epoch s)> <refresh> <email>
saved_codex() {
    local d="$FIXTURES/data/usage/accounts/codex/$1"
    mkdir -p "$d" && chmod 700 "$FIXTURES/data/usage/accounts" "$d"
    jq -n --arg at "$(codex_at "$1" "$2")" --arg r "$3" --arg acct "acct-$1" \
        --arg it "$(mkjwt "{\"email\":\"$4\",\"https://api.openai.com/auth\":{\"chatgpt_account_id\":\"acct-$1\"}}")" \
        '{auth_mode: "chatgpt", last_refresh: "2026-10-20T00:00:00Z",
          tokens: {id_token: $it, access_token: $at, refresh_token: $r, account_id: $acct}}' > "$d/auth.json"
    chmod 600 "$d/auth.json"
}

setup codex-saved
saved_codex a $((NOW + 864000)) ref-a a@example.com
saved_codex b $((NOW + 864000)) ref-b b@example.com
run_usage
check "codex saved: header with name, plan, email" "yes" "$(has 'Codex · a (plus) · a@example.com')"
check "codex saved: the other account too" "yes" "$(has 'Codex · b (plus) · b@example.com')"
check "codex saved: after the current login, by name" "Codex · current login (plus)|Codex · a (plus) · a@example.com|Codex · b (plus) · b@example.com" \
    "$(printf '%s\n' "$OUT" | grep '^Codex' | paste -sd'|' -)"
check "codex saved: a's token sent" "yes" "$(logged "Bearer $(codex_at a $((NOW + 864000)))")"
check "codex saved: a's account id sent" "yes" "$(logged 'ChatGPT-Account-Id: acct-a')"
check "codex saved: no refresh while fresh" "0" "$(token_calls auth.openai.com)"
check "codex saved: exits 0" "0" "$RC"

claims() { bash -c 'source "$1"; jwt_claims "$2"' _ "$SCRIPT" "$1"; }
check "jwt_claims: a token's payload" '{"exp":5}' "$(claims "$(mkjwt '{"exp":5}')" | jq -c .)"
check "jwt_claims: empty is {}" "{}" "$(claims '')"
check "jwt_claims: not a jwt is {}" "{}" "$(claims 'not-a-jwt')"
check "jwt_claims: bad base64 is {}" "{}" "$(claims 'x.!!!.y')"

setup codex-unreadable
saved_codex a $((NOW + 864000)) ref-a a@example.com
rm "$FIXTURES/data/usage/accounts/codex/a/auth.json"
run_usage
check "codex saved account without a login: says to sign in again" "yes" "$(has 'Codex · a: signed out; run usage add codex a to sign in again')"

if command -v flock >/dev/null 2>&1; then

NEW_AT=$(mkjwt "{\"exp\":$((NOW + 864000)),\"who\":\"new\"}")
NEW_IT=$(mkjwt '{"email":"n@example.com"}')
codex_token_ok() {
    jq -n --arg at "$NEW_AT" --arg it "$NEW_IT" '{id_token: $it, access_token: $at, refresh_token: "ref-new"}' \
        > "$FIXTURES/codex-token.json"
}

setup codex-renew
saved_codex a $((NOW + 60)) ref-old a@example.com
A="$FIXTURES/data/usage/accounts/codex/a/auth.json"
codex_token_ok
run_usage
check "codex renew: one refresh" "1" "$(token_calls auth.openai.com/oauth/token)"
check "codex renew: request carries the client id" "yes" "$(logged '"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"')"
check "codex renew: request is a refresh grant" "yes" "$(logged '"grant_type":"refresh_token"')"
check "codex renew: request carries the refresh token" "yes" "$(logged '"refresh_token":"ref-old"')"
check "codex renew: refresh token off argv" "no" "$(grep -qF ref-old "$FIXTURES/curl.args" && echo yes || echo no)"
check "codex renew: tokens written back" "yes" \
    "$(jq -e --arg at "$NEW_AT" --arg it "$NEW_IT" '.tokens.access_token == $at and .tokens.id_token == $it and .tokens.refresh_token == "ref-new"' "$A" >/dev/null && echo yes || echo no)"
check "codex renew: last_refresh updated" "2026-10-25T23:00:00Z" "$(jq -r .last_refresh "$A")"
check "codex renew: account id kept" "acct-a" "$(jq -r .tokens.account_id "$A")"
check "codex renew: still 0600" "600" "$(mode "$A")"
check "codex renew: usage read with the new token" "yes" "$(logged "Bearer $NEW_AT")"
check "codex renew: email from the new id token" "yes" "$(has 'Codex · a (plus) · n@example.com')"
check "codex renew: exits 0" "0" "$RC"

setup codex-401-retry
saved_codex a $((NOW + 864000)) ref-old a@example.com
echo 401 > "$FIXTURES/codex.$(codex_at a $((NOW + 864000))).code"
codex_token_ok
run_usage
check "codex 401: renewed once" "1" "$(token_calls auth.openai.com/oauth/token)"
check "codex 401: retried with the new token" "yes" "$(logged "Bearer $NEW_AT")"
check "codex 401: exits 0" "0" "$RC"

setup codex-rejected
saved_codex a $((NOW - 10)) ref-old a@example.com
A="$FIXTURES/data/usage/accounts/codex/a/auth.json"
echo 401 > "$FIXTURES/codex-token.code"
echo '{"error":{"code":"refresh_token_reused","message":"already used"}}' > "$FIXTURES/codex-token.json"
cp "$A" "$FIXTURES/before.json"
run_usage
check "codex rejected: says how to sign in again" "yes" "$(has 'Codex · a: signed out; run usage add codex a to sign in again')"
check "codex rejected: credentials untouched" "yes" "$(cmp -s "$A" "$FIXTURES/before.json" && echo yes || echo no)"
check "codex rejected: exits 1" "1" "$RC"

setup codex-rejected-400
saved_codex a $((NOW - 10)) ref-old a@example.com
echo 400 > "$FIXTURES/codex-token.code"
echo '{"error":"invalid_grant"}' > "$FIXTURES/codex-token.json"
run_usage
check "codex invalid_grant: says how to sign in again" "yes" "$(has 'Codex · a: signed out; run usage add codex a to sign in again')"

setup codex-transient
saved_codex a $((NOW - 10)) ref-old a@example.com
A="$FIXTURES/data/usage/accounts/codex/a/auth.json"
echo 502 > "$FIXTURES/codex-token.code"
echo 'bad gateway' > "$FIXTURES/codex-token.json"
cp "$A" "$FIXTURES/before.json"
run_usage
check "codex transient: says it could not renew" "yes" "$(has 'Codex · a: could not renew sign-in: HTTP 502')"
check "codex transient: credentials untouched" "yes" "$(cmp -s "$A" "$FIXTURES/before.json" && echo yes || echo no)"

setup codex-badjwt
saved_codex a $((NOW + 864000)) ref-old a@example.com
A="$FIXTURES/data/usage/accounts/codex/a/auth.json"
jq '.tokens.access_token = "not-a-jwt"' "$A" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$A"
codex_token_ok
run_usage
check "codex unreadable token: treated as expired and renewed" "1" "$(token_calls auth.openai.com/oauth/token)"
check "codex unreadable token: rows shown" "yes" "$(has 'Codex · a (plus)')"

setup codex-lock
saved_codex a $((NOW - 10)) ref-old a@example.com
codex_token_ok
echo 1 > "$FIXTURES/codex-token.sleep"
run_usage & run_usage; wait
check "codex lock: one renewal across two runs" "1" "$(token_calls auth.openai.com/oauth/token)"

else
    printf '  skip: Codex renewal (no flock)\n'
fi

# --- without flock (macOS has none) ------------------------------------------------
# path_without <command> <dir>: fill dir with every command on PATH but one.
path_without() {
    local d f n
    mkdir -p "$2"
    IFS=: read -ra path_dirs <<<"$PATH"
    for d in "${path_dirs[@]}"; do
        for f in "$d"/*; do
            n=${f##*/}
            [ "$n" = "$1" ] || [ -e "$2/$n" ] || [ ! -x "$f" ] || ln -s "$f" "$2/$n"
        done
    done
}
NOFLOCK="$WORK/noflock"
path_without flock "$NOFLOCK"

setup noflock-add
BASE_PATH=$NOFLOCK run_usage add codex a
check "no flock: add codex refused" "1" "$RC"
check "no flock: add says flock is needed" "yes" "$(has 'flock')"
check "no flock: no login run" "no" "$(exists "$FIXTURES/cli.log")"
check "no flock: no account made" "no" "$(exists "$FIXTURES/data/usage/accounts/codex/a")"

setup noflock-renew
saved_codex a $((NOW - 10)) ref-a a@example.com
saved_codex b $((NOW + 864000)) ref-b b@example.com
api_fixtures
EXTRA_ENV=(ANTHROPIC_ADMIN_API_KEY=sk-ant-admin-x)
BASE_PATH=$NOFLOCK run_usage
EXTRA_ENV=()
check "no flock: the stale account says why" "yes" "$(has 'Codex · a: could not renew sign-in: flock is required')"
check "no flock: the next account still shown" "yes" "$(has 'Codex · b (plus) · b@example.com')"
check "no flock: API section still shown" "yes" "$(has 'API spend')"
check "no flock: exits 1" "1" "$RC"

# --- names that look like options ------------------------------------------------
setup dash-name
run_usage add codex --device-auth alt
check "dash name: refused" "2" "$RC"
check "dash name: no login run" "no" "$(exists "$FIXTURES/cli.log")"
run_usage add anthropic-api -k <<<'k'
check "dash name for a key: refused" "2" "$RC"

# --- removing a Codex login revokes it from the same store it was saved to -------
setup codex-remove-store
saved_codex a $((NOW + 864000)) ref-a a@example.com
run_usage remove codex a
check "remove codex: logout told the file store" "yes" \
    "$(cli_logged "$FIXTURES/data/usage/accounts/codex/a|logout -c cli_auth_credentials_store=\"file\"")"

# --- the CLIs' own login alongside saved accounts --------------------------------
# The current login is shown once: hidden when the same account is also saved.
setup dedupe
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
saved_claude home tok-h ref-h $((NOW + 3600)) h@example.com
echo '{"oauthAccount":{"accountUuid":"u-work","emailAddress":"w@example.com"}}' > "$FIXTURES/claude/.claude.json"
saved_codex a $((NOW + 864000)) ref-a a@example.com
jq '.tokens.account_id = "acct-a"' "$FIXTURES/codex/auth.json" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$FIXTURES/codex/auth.json"
run_usage
check "dedupe: claude current login hidden when saved" "no" "$(has 'Claude · current login')"
check "dedupe: its saved copy shown" "yes" "$(has 'Claude · work (max) · w@example.com')"
check "dedupe: the other saved account shown" "yes" "$(has 'Claude · home (max) · h@example.com')"
check "dedupe: current login not queried" "no" "$(logged 'Bearer claude-tok')"
check "dedupe: codex current login hidden when saved" "no" "$(has 'Codex · current login')"
check "dedupe: codex saved copy shown" "yes" "$(has 'Codex · a (plus) · a@example.com')"
check "dedupe: exits 0" "0" "$RC"

# Codex's account id may only be in the id token.
setup dedupe-idtoken
saved_codex a $((NOW + 864000)) ref-a a@example.com
jq --arg it "$(mkjwt '{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-a"}}')" \
    '.tokens.id_token = $it | del(.tokens.account_id)' "$FIXTURES/codex/auth.json" > "$FIXTURES/c.json" &&
    mv "$FIXTURES/c.json" "$FIXTURES/codex/auth.json"
run_usage
check "dedupe: codex account id from the id token" "no" "$(has 'Codex · current login')"

setup dedupe-other
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
echo '{"oauthAccount":{"accountUuid":"u-other","emailAddress":"o@example.com"}}' > "$FIXTURES/claude/.claude.json"
run_usage
check "current login not saved: shown first, with its email" "Claude · current login (max) · o@example.com" \
    "$(printf '%s\n' "$OUT" | grep -m1 '^Claude')"
check "current login not saved: saved one after it" "yes" "$(has 'Claude · work (max) · w@example.com')"

# Without CLAUDE_CONFIG_DIR, Claude Code keeps .claude.json in $HOME.
setup dedupe-home
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
mkdir -p "$FIXTURES/home/.claude"
cp "$FIXTURES/claude/.credentials.json" "$FIXTURES/home/.claude/"
echo '{"oauthAccount":{"accountUuid":"u-work","emailAddress":"w@example.com"}}' > "$FIXTURES/home/.claude.json"
OUT=$(env -u ANTHROPIC_ADMIN_API_KEY -u OPENAI_ADMIN_KEY -u CLAUDE_CONFIG_DIR \
    PATH="$STUB:$PATH" FIXTURES="$FIXTURES" USAGE_NOW="$NOW" HOME="$FIXTURES/home" \
    XDG_DATA_HOME="$FIXTURES/data" CODEX_HOME="$FIXTURES/codex" bash "$SCRIPT" 2>&1)
check "dedupe: default config dir reads ~/.claude.json" "no" "$(has 'Claude · current login')"

# --- a blank line between accounts ---------------------------------------------
setup gaps
saved_claude home tok-h ref-h $((NOW + 3600)) h@example.com
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
run_usage
# Blocks: Claude current, home, work; Codex current (no keys, so no API spend).
check "gaps: one blank line between each of 4 blocks" "3" "$(printf '%s\n' "$OUT" | grep -c '^$')"
check "gaps: no blank line first" "Claude · current login (max)" "$(printf '%s\n' "$OUT" | head -1)"
check "gaps: no blank line last" "no" "$(printf '%s\n' "$OUT" | tail -1 | grep -q '^$' && echo yes || echo no)"
check "gaps: blank before each of the 3 later blocks" "||" \
    "$(printf '%s\n' "$OUT" | grep -B1 -E '^(Claude|Codex) · [a-z]' | grep -vE '^(Claude|Codex|--)' | paste -sd'|' -)"
check "gaps: none inside a block" "Claude · home (max) · h@example.com|  session" \
    "$(printf '%s\n' "$OUT" | grep -A1 '^Claude · home' | sed -E 's/^(  [a-z]+).*/\1/' | paste -sd'|' -)"

# A one-line error is a block of its own too.
setup gaps-error
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
rm "$FIXTURES/data/usage/accounts/claude/work/.credentials.json"
run_usage
check "gaps: error line set apart" "" "$(printf '%s\n' "$OUT" | grep -B1 '^Claude · work: signed out' | head -1)"

# --- sorted by weekly reset, all queried before anything prints ---------------------
# claude_weekly <token> <iso|null> [percent]: that token's usage answer, with
# its weekly reset moved (and its weekly use set).
claude_weekly() {
    jq --argjson r "$( [ "$2" = null ] && echo null || printf '"%s"' "$2")" --argjson p "${3:-3}" \
        '.seven_day.resets_at = $r | .seven_day.utilization = $p
         | .limits |= map(if .group == "weekly" and .scope == null then .resets_at = $r | .percent = $p else . end)' \
        "$FIXTURES/claude.json" > "$FIXTURES/claude.$1.json"
}
# codex_weekly <token> <epoch> [percent]
codex_weekly() {
    jq --argjson r "$2" --argjson p "${3:-24}" \
        '.rate_limit.secondary_window.reset_at = $r | .rate_limit.secondary_window.used_percent = $p' \
        "$FIXTURES/codex.json" > "$FIXTURES/codex.$1.json"
}
headers() { printf '%s\n' "$OUT" | grep -E '^(Claude|Codex|API)' | sed -E 's/ \(.*//; s/:.*//' | paste -sd'|' -; }

setup sorted
saved_claude soon tok-s ref-s $((NOW + 3600)) s@example.com
saved_claude later tok-l ref-l $((NOW + 3600)) l@example.com
saved_claude fresh tok-f ref-f $((NOW + 3600)) f@example.com
saved_claude broken tok-b ref-b $((NOW + 3600)) b@example.com
saved_claude full tok-x ref-x $((NOW + 3600)) x@example.com
claude_weekly tok-s 2026-10-26T12:00:00Z     # in 13h
claude_weekly tok-l 2026-10-29T00:00:00Z     # in 3d
claude_weekly tok-f null                     # weekly window not started
claude_weekly tok-x 2026-10-26T06:00:00Z 100 # full, though it resets soonest
echo 500 > "$FIXTURES/claude.tok-b.code"     # current login resets in 6d23h
saved_codex a $((NOW + 864000)) ref-a a@example.com
saved_codex b $((NOW + 864000)) ref-b b@example.com
codex_weekly "$(codex_at a $((NOW + 864000)))" $((NOW + 5 * 86400))
codex_weekly "$(codex_at b $((NOW + 864000)))" $((NOW + 2 * 86400)) 100   # full; current login: 6d09h
run_usage
check "sorted: room left first, then full, each by weekly reset; errors last" \
    "Claude · soon|Claude · later|Claude · current login|Claude · fresh|Claude · full|Claude · broken|Codex · a|Codex · current login|Codex · b" \
    "$(headers)"
check "sorted: the sort key never shows" "no" "$(printf '%s' "$OUT" | grep -q '#sort' && echo yes || echo no)"
check "sorted: rows stay under their account" "Claude · later (max) · l@example.com|  session" \
    "$(printf '%s\n' "$OUT" | grep -A1 '^Claude · later' | sed -E 's/^(  [a-z]+).*/\1/' | paste -sd'|' -)"
check "sorted: still a gap between each" "8" "$(printf '%s\n' "$OUT" | grep -c '^$')"
check "sorted: a failed account still fails the run" "1" "$RC"

# Queried together: four answers that each take a second come back in about one.
setup parallel
saved_claude a tok-a ref-a $((NOW + 3600)) a@example.com
saved_claude b tok-b ref-b $((NOW + 3600)) b@example.com
saved_claude c tok-c ref-c $((NOW + 3600)) c@example.com
echo 1 > "$FIXTURES/claude.sleep"
start=$SECONDS
run_usage
check "parallel: four 1s queries take under 3s" "yes" "$( (( SECONDS - start < 3 )) && echo yes || echo no)"
check "parallel: all four shown" "4" "$(printf '%s\n' "$OUT" | grep -c '^Claude')"

# --- the bar: 20 whole cells on a faint track -------------------------------------
setup bars
jq '.limits = [
      {"group":"session","percent":0,"resets_at":null},
      {"group":"weekly","percent":1,"resets_at":null,"scope":{"model":{"display_name":"one"}}},
      {"group":"weekly","percent":99,"resets_at":null,"scope":{"model":{"display_name":"almost"}}},
      {"group":"weekly","percent":100,"resets_at":null,"scope":{"model":{"display_name":"full"}}},
      {"group":"weekly","percent":130,"resets_at":null,"scope":{"model":{"display_name":"over"}}}]' \
    "$FIXTURES/claude.json" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$FIXTURES/claude.json"
run_usage
# bar_of <label> <percent>: the bar between the padded label and the percentage,
# cut out by those ASCII ends (offsets would count bytes in a C locale).
bar_of() {
    local line rest
    line=$(printf '%s\n' "$OUT" | grep -F -- "  $(printf '%-14s' "$1") " | head -1)
    rest=${line#"  $(printf '%-14s' "$1") "}
    printf '[%s]' "${rest%%" $(printf '%3s' "$2")% used"*}"
}
check "bar: empty is all track"  "[░░░░░░░░░░░░░░░░░░░░]" "$(bar_of session 0)"
check "bar: 1% shows a cell"      "[█░░░░░░░░░░░░░░░░░░░]" "$(bar_of 'weekly one' 1)"
check "bar: 99% stops a cell short" "[███████████████████░]" "$(bar_of 'weekly almost' 99)"
check "bar: 100% is full"        "[████████████████████]" "$(bar_of 'weekly full' 100)"
check "bar: over 100% stays full" "[████████████████████]" "$(bar_of 'weekly over' 130)"
check "bar: no partial blocks" "no" "$(printf '%s' "$OUT" | grep -qE '▏|▎|▍|▌|▋|▊|▉' && echo yes || echo no)"
check "bar: no brackets"         "no" "$(printf '%s' "$OUT" | grep -qF '[' && echo yes || echo no)"

# --- colors ------------------------------------------------------------------------
# Plain up to half used, yellow past half, red when full. Only in a terminal
# (the tests force it with CLICOLOR_FORCE), and never with NO_COLOR.
setup colors
jq '.limits = [
      {"group":"session","percent":0,"resets_at":null},
      {"group":"weekly","percent":50,"resets_at":null},
      {"group":"weekly","percent":51,"resets_at":null,"scope":{"model":{"display_name":"A"}}},
      {"group":"weekly","percent":100,"resets_at":null,"scope":{"model":{"display_name":"B"}}}]' \
    "$FIXTURES/claude.json" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$FIXTURES/claude.json"
jq '.rate_limit.primary_window.used_percent = 100' "$FIXTURES/codex.json" > "$FIXTURES/c.json" &&
    mv "$FIXTURES/c.json" "$FIXTURES/codex.json"
ESC=$(printf '\033')
EXTRA_ENV=(CLICOLOR_FORCE=1)
run_usage
EXTRA_ENV=()
colored() { printf '%s\n' "$OUT" | grep -F -- "$1" | head -1 | grep -qF "${ESC}[$2m█" && echo yes || echo no; }
plain() { printf '%s\n' "$OUT" | grep -F -- "$1" | head -1 | grep -qF "$ESC" && echo no || echo yes; }
check "colors: empty is plain" "yes" "$(plain ' session ')"
check "colors: half is plain" "yes" "$(plain ' weekly   ')"
check "colors: past half is yellow" "yes" "$(colored 'weekly A' 33)"
check "colors: full is red" "yes" "$(colored 'weekly B' 31)"
check "colors: codex rows too" "yes" "$(colored '5-hour' 31)"
check "colors: reset after the percentage" "yes" \
    "$(printf '%s\n' "$OUT" | grep -F 'weekly B' | grep -qF "100% used${ESC}[0m" && echo yes || echo no)"
check "colors: label uncolored" "yes" "$(printf '%s\n' "$OUT" | grep -qE "^  weekly B +$ESC\[31m█" && echo yes || echo no)"
EXTRA_ENV=(CLICOLOR_FORCE=1 NO_COLOR=1)
run_usage
EXTRA_ENV=()
check "colors: NO_COLOR wins" "no" "$(printf '%s' "$OUT" | grep -qF "$ESC" && echo yes || echo no)"
run_usage
check "colors: none when not a terminal" "no" "$(printf '%s' "$OUT" | grep -qF "$ESC" && echo yes || echo no)"

# --- rename ----------------------------------------------------------------------
setup rename-key
run_usage add anthropic-api work <<<'sk-ant-admin-a'
K="$FIXTURES/data/usage/keys/anthropic-api"
run_usage rename anthropic-api work job
check "rename key: exits 0" "0" "$RC"
check "rename key: under the new name" "sk-ant-admin-a" "$(cat "$K/job" 2>&1)"
check "rename key: still 0600" "600" "$(mode "$K/job")"
check "rename key: old name gone" "no" "$(exists "$K/work")"
run_usage add anthropic-api other <<<'sk-ant-admin-b'
run_usage rename anthropic-api job other
check "rename onto a taken name: exits 1" "1" "$RC"
check "rename onto a taken name: says so" "yes" "$(has 'already')"
check "rename onto a taken name: both kept" "sk-ant-admin-a sk-ant-admin-b" "$(cat "$K/job") $(cat "$K/other")"
run_usage rename anthropic-api nosuch x
check "rename unknown: exits 1" "1" "$RC"
run_usage rename anthropic-api job ../x
check "rename to a bad name: exits 2" "2" "$RC"
check "rename to a bad name: nothing moved" "sk-ant-admin-a" "$(cat "$K/job")"
run_usage rename anthropic-api job
check "rename without a new name: exits 2" "2" "$RC"

if [ "$HAVE_FLOCK" = yes ]; then
setup rename-login
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
ACC="$FIXTURES/data/usage/accounts"
cp "$ACC/claude/work/.credentials.json" "$FIXTURES/before.json"
run_usage rename claude work job
check "rename login: exits 0" "0" "$RC"
check "rename login: credentials moved intact" "yes" "$(cmp -s "$ACC/claude/job/.credentials.json" "$FIXTURES/before.json" && echo yes || echo no)"
check "rename login: old name gone" "no" "$(exists "$ACC/claude/work")"
check "rename login: still 0700" "700" "$(mode "$ACC/claude/job")"
check "rename login: no sign-in or sign-out" "no" "$(exists "$FIXTURES/cli.log")"
run_usage
check "rename login: shown under the new name" "yes" "$(has 'Claude · job (max) · w@example.com')"
saved_codex a $((NOW + 864000)) ref-a a@example.com
run_usage rename codex a b
check "rename codex login: moved" "yes no" "$(exists "$ACC/codex/b/auth.json") $(exists "$ACC/codex/a")"

# A renewal in progress holds the account's lock; rename waits for it, so the
# renewal's write-back never lands in a folder that has moved.
setup rename-lock
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
L="$FIXTURES/data/usage/accounts/claude/work/.usage.lock"
( flock 9; touch "$FIXTURES/locked"; sleep 1; touch "$FIXTURES/released" ) 9>"$L" &
await "$FIXTURES/locked"
run_usage rename claude work job
released_before_rename_returned=$(exists "$FIXTURES/released")
wait
check "rename login: waited for the lock" "yes" "$released_before_rename_returned"
check "rename login: then moved" "yes" "$(exists "$FIXTURES/data/usage/accounts/claude/job/.credentials.json")"

# Re-adding swaps a new login in; a renewal in progress on the old one must
# finish first, or its write-back would land on the new login.
setup readd-lock
run_usage add claude work
L="$FIXTURES/data/usage/accounts/claude/work/.usage.lock"
( flock 9; touch "$FIXTURES/locked"; sleep 1; touch "$FIXTURES/released" ) 9>"$L" &
await "$FIXTURES/locked"
run_usage add claude work
released_before_add_returned=$(exists "$FIXTURES/released")
wait
check "re-add: waited for the old login's lock" "yes" "$released_before_add_returned"
check "re-add: new login in place" "tok-2" "$(jq -r .claudeAiOauth.accessToken "$FIXTURES/data/usage/accounts/claude/work/.credentials.json")"
fi

setup rename-noflock
saved_codex a $((NOW + 864000)) ref-a a@example.com
BASE_PATH=$NOFLOCK run_usage rename codex a b
check "no flock: rename login refused" "1" "$RC"
check "no flock: rename login left it alone" "yes" "$(exists "$FIXTURES/data/usage/accounts/codex/a/auth.json")"
run_usage add openai-api k <<<'sk-x'
BASE_PATH=$NOFLOCK run_usage rename openai-api k k2
check "no flock: rename key still works" "0" "$RC"

setup rename-help
run_usage --help
check "--help mentions rename" "yes" "$(has 'usage rename <kind> <old> <new>')"

# --- what the servers send is never trusted to be printable -------------------------
setup control-chars
jq '.limits[2].scope.model.display_name = "Bad\u001b[2JName"' "$FIXTURES/claude.json" > "$FIXTURES/c.json" &&
    mv "$FIXTURES/c.json" "$FIXTURES/claude.json"
jq '.plan_type = "pl\u001b]0;pwned\u0007us"' "$FIXTURES/codex.json" > "$FIXTURES/c.json" &&
    mv "$FIXTURES/c.json" "$FIXTURES/codex.json"
run_usage
check "control chars: none reach the terminal" "no" "$(printf '%s' "$OUT" | grep -q "$(printf '[\033\007]')" && echo yes || echo no)"
check "control chars: the rest of the name kept" "yes" "$(has 'weekly Bad[2JName')"
check "control chars: plan kept, cleaned" "yes" "$(has 'Codex · current login (pl]0;pwnedus)')"
setup control-chars-error
echo 500 > "$FIXTURES/claude.code"
printf '{"error":{"message":"bo\\u001b[31mom"}}' > "$FIXTURES/claude.json"
run_usage
check "control chars: error text cleaned" "yes" "$(has 'Claude · current login: HTTP 500: bo[31mom')"

# --- answers that aren't what we expect --------------------------------------------
setup malformed
echo '<html>gateway</html>' > "$FIXTURES/claude.json"
run_usage
check "malformed: one clear line" "yes" "$(has 'Claude · current login: unexpected response from the usage endpoint')"
check "malformed: no jq noise" "no" "$(has 'jq: error')"
check "malformed: fails the run" "1" "$RC"
check "malformed: other blocks still shown" "yes" "$(has 'Codex · current login (plus)')"
setup odd-fields
jq '.limits = [{"group":"session","percent":null,"resets_at":"not a date"}]' "$FIXTURES/claude.json" > "$FIXTURES/c.json" &&
    mv "$FIXTURES/c.json" "$FIXTURES/claude.json"
run_usage
check "odd fields: no jq noise" "no" "$(has 'jq: error')"
check "odd fields: row shown without a reset" " session ░░░░░░░░░░░░░░░░░░░░ 0% used" "$(line ' session ')"

if command -v flock >/dev/null 2>&1; then
setup refused-with-reason
saved_claude work tok-old ref-old $((NOW - 10)) w@example.com
echo 400 > "$FIXTURES/claude-token.code"
echo '{"error":"invalid_request","error_description":"scope not allowed"}' > "$FIXTURES/claude-token.json"
run_usage
check "refused renewal: the server's reason shown" "yes" "$(has 'Claude · work: could not renew sign-in: HTTP 400: scope not allowed')"
fi

if [ "$HAVE_FLOCK" = yes ]; then
check "lock on a vanished folder: says so" "1:account folder is gone" \
    "$(bash -c 'source "$1"; REFRESH_ERROR=""; with_lock "$2" true 2>/dev/null; echo "$?:$REFRESH_ERROR"' _ "$SCRIPT" "$WORK/no-such-dir")"
fi

# --- argument edges -----------------------------------------------------------------
setup key-args
run_usage add anthropic-api work extra <<<'sk-ant-admin-a'
check "add key with extra args: refused" "2" "$RC"
check "add key with extra args: nothing saved" "no" "$(exists "$FIXTURES/data/usage/keys/anthropic-api/work")"
run_usage add openai-api env <<<'sk-x'
check "key named env: refused" "2" "$RC"
check "key named env: says why" "yes" "$(has 'reserved')"
run_usage add openai-api k <<<'sk-x'
run_usage rename openai-api k env
check "rename key to env: refused" "2" "$RC"

# --- loose ends ------------------------------------------------------------------------
# A stray file among the account folders is not an account.
setup stray-file
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
touch "$FIXTURES/data/usage/accounts/claude/notes.txt"
run_usage
check "stray file: not shown as an account" "no" "$(has 'notes.txt')"
check "stray file: not a failure" "0" "$RC"

# An Admin key the API refuses says so on its line, and fails the run.
setup api-error
echo 401 > "$FIXTURES/anthropic.code"
echo '{"error":{"type":"authentication_error","message":"invalid x-api-key"}}' > "$FIXTURES/anthropic.json"
EXTRA_ENV=(ANTHROPIC_ADMIN_API_KEY=sk-ant-admin-bad)
run_usage
EXTRA_ENV=()
check "admin key refused: its line says why" "  Anthropic API · env: HTTP 401: invalid x-api-key" \
    "$(printf '%s\n' "$OUT" | grep -F 'Anthropic API · env')"
check "admin key refused: fails the run" "1" "$RC"

# Removing a login whose CLI isn't installed still removes it, with a warning.
setup remove-no-cli
saved_claude work tok-w ref-w $((NOW + 3600)) w@example.com
NOCLAUDE="$WORK/noclaude"
[ -d "$NOCLAUDE" ] || path_without claude "$NOCLAUDE"
mv "$STUB/claude" "$STUB/claude.away"
BASE_PATH=$NOCLAUDE run_usage remove claude work
mv "$STUB/claude.away" "$STUB/claude"
check "remove without the CLI: exits 0" "0" "$RC"
check "remove without the CLI: warns" "yes" "$(has 'warning: could not sign claude work out; removing it anyway')"
check "remove without the CLI: removed" "no" "$(exists "$FIXTURES/data/usage/accounts/claude/work")"

if command -v flock >/dev/null 2>&1; then
# Lock files are private like everything else usage makes.
setup lock-mode
saved_claude work tok-old ref-old $((NOW - 10)) w@example.com
echo '{"access_token":"tok-new","refresh_token":"ref-new","expires_in":28800}' > "$FIXTURES/claude-token.json"
run_usage
check "lock file: 0600" "600" "$(mode "$FIXTURES/data/usage/accounts/claude/work/.usage.lock")"

# A lock held too long gives up with a reason (USAGE_LOCK_WAIT shortens the wait).
setup lock-timeout
saved_claude work tok-old ref-old $((NOW - 10)) w@example.com
L="$FIXTURES/data/usage/accounts/claude/work/.usage.lock"
( flock 9; touch "$FIXTURES/locked"; sleep 3 ) 9>"$L" &
await "$FIXTURES/locked"
EXTRA_ENV=(USAGE_LOCK_WAIT=1)
run_usage
EXTRA_ENV=()
wait
check "lock timeout: says so" "yes" "$(has 'Claude · work: could not renew sign-in: timed out waiting for another usage run')"
check "lock timeout: no renewal attempted" "0" "$(token_calls platform.claude.com/v1/oauth/token)"

# A signal in the middle of re-adding waits for the swap to finish, so the
# account is never left half-moved. A stub mv sends TERM while the old login
# is being moved aside.
setup readd-signal
run_usage add claude work
MVSTUB="$WORK/mvstub.$RANDOM"
mkdir -p "$MVSTUB"
cat > "$MVSTUB/mv" <<'EOF'
#!/usr/bin/env bash
PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -vxF "$MVSTUB" | paste -sd: -)
case "$2" in
    */old) [ -e "$FIXTURES/signalled" ] || { touch "$FIXTURES/signalled"; kill -TERM "$PPID"; } ;;
esac
exec mv "$@"
EOF
chmod +x "$MVSTUB/mv"
EXTRA_ENV=(MVSTUB="$MVSTUB")
BASE_PATH="$MVSTUB:$PATH" run_usage add claude work
EXTRA_ENV=()
A="$FIXTURES/data/usage/accounts/claude"
check "signal mid-swap: it was sent" "yes" "$(exists "$FIXTURES/signalled")"
check "signal mid-swap: exits 143" "143" "$RC"
check "signal mid-swap: the new login is in place" "tok-2" "$(jq -r .claudeAiOauth.accessToken "$A/work/.credentials.json" 2>&1)"
check "signal mid-swap: nothing left hidden" "" "$(find "$A" -mindepth 1 -maxdepth 1 -name '.*')"
check "signal mid-swap: the replaced session still signed out" "yes" \
    "$(grep -q '|auth logout$' "$FIXTURES/cli.log" && echo yes || echo no)"

# mv can be killed after its rename but before it exits; the swap goes by
# where things are, not by mv's status. This stub renames, then fails.
setup readd-mv-fails
run_usage add claude work
MVFAIL="$WORK/mvfail.$RANDOM"
mkdir -p "$MVFAIL"
cat > "$MVFAIL/mv" <<'EOF'
#!/usr/bin/env bash
PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -vxF "$MVFAIL" | paste -sd: -)
mv "$@" || exit
case "$2" in */old) exit 1 ;; esac
EOF
chmod +x "$MVFAIL/mv"
EXTRA_ENV=(MVFAIL="$MVFAIL")
BASE_PATH="$MVFAIL:$PATH" run_usage add claude work
EXTRA_ENV=()
A="$FIXTURES/data/usage/accounts/claude"
check "mv fails after renaming: re-add still succeeds" "0" "$RC"
check "mv fails after renaming: the new login is in place" "tok-2" "$(jq -r .claudeAiOauth.accessToken "$A/work/.credentials.json" 2>&1)"
check "mv fails after renaming: nothing left hidden" "" "$(find "$A" -mindepth 1 -maxdepth 1 -name '.*')"

# Worst case: the new login won't move in and the old one won't move back.
# The old login must not be deleted with the staging dir, and the message
# must say where it is.
setup readd-restore-fails
run_usage add claude work
MVNONE="$WORK/mvnone.$RANDOM"
mkdir -p "$MVNONE"
cat > "$MVNONE/mv" <<'EOF'
#!/usr/bin/env bash
PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -vxF "$MVNONE" | paste -sd: -)
n=$(( $(cat "$FIXTURES/mv-calls" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$FIXTURES/mv-calls"
[ "$n" = 1 ] && exec mv "$@"
exit 1
EOF
chmod +x "$MVNONE/mv"
EXTRA_ENV=(MVNONE="$MVNONE")
BASE_PATH="$MVNONE:$PATH" run_usage add claude work
EXTRA_ENV=()
A="$FIXTURES/data/usage/accounts/claude"
kept=$(find "$A" -path '*/.add-work.*/old/.credentials.json' | head -1)
check "restore fails: exits 1" "1" "$RC"
check "restore fails: the old login is kept" "tok-1" "$(jq -r .claudeAiOauth.accessToken "$kept" 2>&1)"
check "restore fails: the message says where" "yes" "$([ -n "$kept" ] && has "${kept%/.credentials.json}" || echo no)"

# If the staging sub-folder can't be made, the staging dir doesn't linger.
setup readd-stage-fails
run_usage add claude work
MKFAIL="$WORK/mkfail.$RANDOM"
mkdir -p "$MKFAIL"
cat > "$MKFAIL/mkdir" <<'EOF'
#!/usr/bin/env bash
PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -vxF "$MKFAIL" | paste -sd: -)
case "${*: -1}" in */new) exit 1 ;; esac
exec mkdir "$@"
EOF
chmod +x "$MKFAIL/mkdir"
EXTRA_ENV=(MKFAIL="$MKFAIL")
BASE_PATH="$MKFAIL:$PATH" run_usage add claude work
EXTRA_ENV=()
A="$FIXTURES/data/usage/accounts/claude"
check "staging fails: exits 1" "1" "$RC"
check "staging fails: old login kept" "tok-1" "$(jq -r .claudeAiOauth.accessToken "$A/work/.credentials.json" 2>&1)"
check "staging fails: nothing left hidden" "" "$(find "$A" -mindepth 1 -maxdepth 1 -name '.*')"

# Ctrl-C reaches the whole process group, flock and mv included. While a re-add
# waits for the old login's lock, that must stop cleanly: 143, old login kept,
# nothing left behind, and no talk of a timeout.
if command -v setsid >/dev/null 2>&1; then
setup readd-group-signal
run_usage add claude work
A="$FIXTURES/data/usage/accounts/claude"
( flock 9; touch "$FIXTURES/locked"; sleep 3 ) 9>"$A/work/.usage.lock" &
holder=$!
await "$FIXTURES/locked"
setsid env PATH="$STUB:$PATH" FIXTURES="$FIXTURES" USAGE_NOW="$NOW" XDG_DATA_HOME="$FIXTURES/data" \
    bash "$SCRIPT" add claude work > "$FIXTURES/out" 2>&1 &
pid=$!
# Signal only once usage is really waiting in flock for the held lock.
waiting=no
for _ in $(seq 200); do
    pgrep -s "$pid" -x flock >/dev/null 2>&1 && { waiting=yes; break; }
    sleep 0.05
done
kill -TERM -- -"$pid"
wait "$pid"
rc=$?
wait "$holder"
check "group signal while waiting for the lock: exits 143" "143" "$rc"
check "group signal while waiting for the lock: old login kept" "tok-1" "$(jq -r .claudeAiOauth.accessToken "$A/work/.credentials.json" 2>&1)"
check "group signal while waiting for the lock: nothing left behind" "" "$(find "$A" -mindepth 1 -maxdepth 1 -name '.*')"
check "group signal while waiting for the lock: it was waiting in flock" "yes" "$waiting"
check "group signal while waiting for the lock: no timeout claimed" "no" "$(grep -q 'timed out' "$FIXTURES/out" && echo yes || echo no)"
check "group signal while waiting for the lock: no bash noise" "no" "$(grep -q 'Terminated' "$FIXTURES/out" && echo yes || echo no)"
fi
fi

# SIGTERM while a login is running exits 143 (128+15), and the half-made
# account is cleaned up. The stub login waits until told to stop.
if [ "$HAVE_FLOCK" = yes ]; then
setup add-sigterm
cat > "$STUB/claude.slow" <<'EOF'
#!/usr/bin/env bash
touch "$FIXTURES/login-started"
for _ in $(seq 50); do [ -e "$FIXTURES/stop" ] && exit 1; sleep 0.1; done
exit 1
EOF
chmod +x "$STUB/claude.slow"
mv "$STUB/claude" "$STUB/claude.real" && mv "$STUB/claude.slow" "$STUB/claude"
env PATH="$STUB:$PATH" FIXTURES="$FIXTURES" XDG_DATA_HOME="$FIXTURES/data" bash "$SCRIPT" add claude slow >/dev/null 2>&1 &
pid=$!
await "$FIXTURES/login-started"
kill -TERM "$pid"
touch "$FIXTURES/stop"
wait "$pid"
rc=$?
mv "$STUB/claude" "$STUB/claude.slow" && mv "$STUB/claude.real" "$STUB/claude"
check "SIGTERM: exits 143" "143" "$rc"
check "SIGTERM: half-made account cleaned up" "no" "$(exists "$FIXTURES/data/usage/accounts/claude/slow")"
fi

# Killed mid-request, nothing is left in TMPDIR.
if command -v setsid >/dev/null 2>&1; then
setup kill-mid-request
mkdir -p "$FIXTURES/tmp"
echo 3 > "$FIXTURES/claude.sleep"
setsid env PATH="$STUB:$PATH" FIXTURES="$FIXTURES" USAGE_NOW="$NOW" XDG_DATA_HOME="$FIXTURES/data" \
    TMPDIR="$FIXTURES/tmp" CLAUDE_CONFIG_DIR="$FIXTURES/claude" CODEX_HOME="$FIXTURES/codex" \
    bash "$SCRIPT" >/dev/null 2>&1 &
pid=$!
await "$FIXTURES/curl-started"
sleep 0.2
kill -TERM -- -"$pid"
wait "$pid"
rc=$?
check "killed mid-request: exits 143" "143" "$rc"
check "killed mid-request: nothing left in TMPDIR" "" "$(find "$FIXTURES/tmp" -mindepth 1)"
fi

# A lock that can't be opened says why, without bash's own error.
if [ "$HAVE_FLOCK" = yes ]; then
mkdir -p "$WORK/lockdir/.usage.lock"
check "lock that can't be opened: says why, quietly" "1:cannot lock $WORK/lockdir:" \
    "$(bash -c 'source "$1"; REFRESH_ERROR=""; with_lock "$2" true 2>"$3"; echo "$?:$REFRESH_ERROR:$(cat "$3")"' \
        _ "$SCRIPT" "$WORK/lockdir" "$WORK/lockdir.err")"
check "lock that can't be opened: bash in POSIX mode carries on too" "1:cannot lock $WORK/lockdir" \
    "$(POSIXLY_CORRECT=1 bash -c 'source "$1"; REFRESH_ERROR=""; with_lock "$2" true 2>/dev/null; echo "$?:$REFRESH_ERROR"' \
        _ "$SCRIPT" "$WORK/lockdir")"
fi

# flock failing for any reason but a timeout says so, not "timed out".
mkdir -p "$WORK/flockfail" "$WORK/flockfail-bin"
printf '#!/bin/sh\nexit 65\n' > "$WORK/flockfail-bin/flock"
chmod +x "$WORK/flockfail-bin/flock"
check "flock failing: says how" "1:flock failed (65)" \
    "$(PATH="$WORK/flockfail-bin:$PATH" bash -c 'source "$1"; REFRESH_ERROR=""; with_lock "$2" true; echo "$?:$REFRESH_ERROR"' \
        _ "$SCRIPT" "$WORK/flockfail")"

# A stray folder among saved keys is not a key.
setup key-folder
mkdir -p "$FIXTURES/data/usage/keys/anthropic-api/stray"
run_usage
check "key folder: not taken for a key" "no" "$(has 'stray')"
check "key folder: no API section" "no" "$(has 'API spend')"
check "key folder: not a failure" "0" "$RC"

# C1 controls (U+0080-U+009F) are controls too in some terminals.
setup c1-controls
jq '.plan_type = "pl\u009bus"' "$FIXTURES/codex.json" > "$FIXTURES/c.json" && mv "$FIXTURES/c.json" "$FIXTURES/codex.json"
jq '.claudeAiOauth.subscriptionType = "ma\u009bx"' "$FIXTURES/claude/.credentials.json" > "$FIXTURES/c.json" &&
    mv "$FIXTURES/c.json" "$FIXTURES/claude/.credentials.json"
jq --arg it "$(mkjwt '{"email":"a\u009b@example.com"}')" '.tokens.id_token = $it' "$FIXTURES/codex/auth.json" > "$FIXTURES/c.json" &&
    mv "$FIXTURES/c.json" "$FIXTURES/codex/auth.json"
run_usage
check "C1 controls: none reach the terminal" "no" "$(printf '%s' "$OUT" | grep -q "$(printf '\302\233')" && echo yes || echo no)"
check "C1 controls: text kept" "yes" "$(has 'Codex · current login (plus) · a@example.com')"

# --- help -----------------------------------------------------------------------------
setup help-notes
run_usage --help
check "help: saved accounts need flock, macOS has none" "yes" "$(has 'flock')"
check "help: at your own risk" "yes" "$(has 'at your own risk')"
check "help: names Anthropic's terms" "yes" "$(has 'code.claude.com/docs/en/legal-and-compliance')"
check "help: calls it the Claude Code usage policy" "yes" "$(has "Claude Code usage policy")"
check "help: the note is its own paragraph" "" \
    "$(printf '%s\n' "$OUT" | grep -A1 '^way\.$' | tail -1)"

# --- arguments ----------------------------------------------------------------
setup args
run_usage --help
check "--help prints usage" "yes" "$(has 'Usage: usage')"
run_usage --bogus
check "unknown option rejected" "yes" "$(has 'unknown option: --bogus')"
check "unknown option exits 2" "2" "$RC"

# CI sets USAGE_NEED_JQ so a runner without jq fails rather than skipping.
NOJQ="$WORK/nojq"
path_without jq "$NOJQ"
env -u USAGE_NEED_JQ PATH="$NOJQ" bash "$HERE/test_usage.sh" >/dev/null 2>&1
check "no jq: skips" "0" "$?"
USAGE_NEED_JQ=1 PATH="$NOJQ" bash "$HERE/test_usage.sh" >/dev/null 2>&1
check "no jq with USAGE_NEED_JQ: fails" "1" "$?"

finish
