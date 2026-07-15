#!/usr/bin/env bash
# Contract: no test file may leave a real `at` job behind in the user's spool.
#
# The e2e files queue throwaway jobs to exercise a REAL `at`. Every one of those
# probes must be reaped even when the inline `atrm` doesn't run or doesn't work
# -- the id has to be visible to an EXIT trap, not trapped inside the command
# substitution that created it. test_jobs_e2e.sh gets this right (remember_id +
# purge); test_jobs_e2e_skip.sh's probe did not: it created and removed the job
# inside ONE substitution, so `pid` died with the subshell and anything landing
# in that window (a failing atrm, a SIGINT) orphaned a real job in the user's
# queue -- exactly the "never touch the user's spool" rule the e2e file is so
# careful about.
#
# We simulate a TRANSIENT atrm failure (the first atrm of the run fails without
# removing anything; later ones work). That is precisely the case the trap backup
# exists for: the inline atrm is allowed to fail, the trap must still reap the
# job. Pre-fix the id was unreachable from the trap, so the job leaked.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"

for b in at atq atrm; do
    if ! command -v "$b" >/dev/null 2>&1; then
        echo "  SKIP: '$b' not installed"
        exit 0
    fi
done

# Every throwaway queue the e2e files touch (w = their own, v/u = F1/F4 staging).
QUEUES="w v u"
REAL_ATRM=$(command -v atrm)

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/at-hygiene.XXXXXX")
# GUARD_ID is our own probe job (below), hoisted out of its substitution so this
# trap can reap it -- the very pattern this file exists to enforce.
GUARD_ID=""
trap 'rm -rf "$WORKDIR"; [ -n "$GUARD_ID" ] && atrm "$GUARD_ID" 2>/dev/null' EXIT

mkdir -p "$WORKDIR/bin"
cat > "$WORKDIR/bin/atrm" <<FAKE
#!/usr/bin/env bash
# Fail the first atrm of a run (a spool hiccup), then delegate to the real one.
if [ ! -e "\$ATRM_HICCUP_STAMP" ]; then
    : > "\$ATRM_HICCUP_STAMP"
    echo "atrm: transient failure (simulated)" >&2
    exit 1
fi
exec "$REAL_ATRM" "\$@"
FAKE
chmod +x "$WORKDIR/bin/atrm"

# Every pending job across $QUEUES as "<queue>:<id>" tokens, space-separated.
# Tagging with the queue keeps ids from different queues distinguishable.
queue_census() {
    local q
    for q in $QUEUES; do
        atq -q "$q" 2>/dev/null | awk -v q="$q" '{print q ":" $1}'
    done | sort | tr '\n' ' '
}

# census_added <before> <after> -- the tokens in <after> that <before> lacked.
census_added() {
    local before=" $1 " tok out=""
    for tok in $2; do
        case "$before" in *" $tok "*) ;; *) out="$out $tok" ;; esac
    done
    printf '%s' "${out# }"
}

# --- guard: the fake atrm must behave as advertised ---------------------------
# If it delegated on the first call (or never delegated at all) no leak could be
# provoked or reaped, and this whole file would prove nothing. Fail loudly here.
GUARD_ID=$(echo true | at -q w now + 3 hours 2>&1 | grep -oE 'job [0-9]+' | grep -oE '[0-9]+')
if [ -z "$GUARD_ID" ]; then
    echo "  SKIP: environment can't queue an 'at' job"
    exit 0
fi
hiccup_atrm() {
    PATH="$WORKDIR/bin:$PATH" ATRM_HICCUP_STAMP="$WORKDIR/stamp.guard" \
        atrm "$1" >/dev/null 2>&1
}
hiccup_atrm "$GUARD_ID"; rc1=$?
still_there=$(queue_census | grep -qw "w:$GUARD_ID" && echo yes || echo no)
hiccup_atrm "$GUARD_ID"; rc2=$?
check "hiccup atrm: first call fails"                "1"   "$rc1"
check "hiccup atrm: first call removed nothing"      "yes" "$still_there"
check "hiccup atrm: second call delegates (removes)" "0"   "$rc2"
check "hiccup atrm: the job is really gone"          "yes" \
    "$(queue_census | grep -qw "w:$GUARD_ID" && echo no || echo yes)"
GUARD_ID=""

# --- the contract -------------------------------------------------------------
# Run each e2e file with one atrm hiccup in it, then assert the user's queues are
# exactly as we found them. Anything leaked is reaped here, so even a FAILING run
# of this test doesn't itself pollute the spool.
for f in test_jobs_e2e_skip.sh test_jobs_e2e.sh; do
    before=$(queue_census)
    PATH="$WORKDIR/bin:$PATH" ATRM_HICCUP_STAMP="$WORKDIR/stamp.$f" \
        bash "$HERE/$f" >/dev/null 2>&1
    leaked=$(census_added "$before" "$(queue_census)")
    check "$f leaves no 'at' job behind when an atrm hiccups" "" "$leaked"
    for tok in $leaked; do atrm "${tok#*:}" 2>/dev/null; done
done

finish
