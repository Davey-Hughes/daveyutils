#!/usr/bin/env bash
# Land a feature branch on main as ONE commit that passes the gate on its own.
#
#   dev/land.sh                      # land the current branch (opens an editor for the subject)
#   dev/land.sh my-branch            # land a named branch
#   dev/land.sh -- --flag            # pass --flag through to dev/check-all.sh
#   dev/land.sh -m "feat(tm): ..."   # skip the editor, use this subject
#   dev/land.sh --no-gate            # land without running the gate (loud, discouraged)
#   dev/land.sh --keep-branch        # do not delete the branch after landing
#
# WHY A SCRIPT AND NOT `git merge --squash`:
#
#   1. The gate runs on the MERGED tree, BEFORE the commit exists. Every commit on main is meant to
#      be an atomic unit that builds and passes CI by itself; the only way to know that is to check
#      the merged result and refuse to commit when it fails. `git merge --squash && git commit` gets
#      this backwards — it creates the commit first and finds out afterwards.
#
#   2. `git merge --squash` DISCARDS every commit message on the branch. The message here is
#      prefilled with all of them, so the reasoning survives the squash instead of being traded for
#      a tidy graph. Delete what you do not want; what is left is kept verbatim.
#
# The remote enforces the same shape from the other direction: allow_merge_commits is off and
# default_merge_style is squash, so a PR merged in the web UI cannot produce a merge commit either.
# CI's `linear-history` job remains the actual gate — this is convenience, like setup-dev.sh.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

MAIN=main
# `#LAND#` rather than a bare `#`, and --cleanup=whitespace rather than =strip, because =strip
# removes EVERY line beginning with the comment char — including one inside a preserved commit body
# that happens to start with `#`. The whole point is that those bodies survive verbatim.
MARK='#LAND#'

gate_args=()
skip_gate=0
keep_branch=0
subject=""
branch=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-gate)     skip_gate=1; shift ;;
    --keep-branch) keep_branch=1; shift ;;
    --)            shift; while [ "$#" -gt 0 ]; do gate_args+=("$1"); shift; done ;;
    -m)        subject="${2:?-m needs a subject}"; shift 2 ;;
    -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*)        echo "error: unknown argument: $1" >&2; exit 2 ;;
    *)         [ -z "$branch" ] || { echo "error: more than one branch given" >&2; exit 2; }
               branch="$1"; shift ;;
  esac
done

[ -n "$branch" ] || branch="$(git rev-parse --abbrev-ref HEAD)"

# --- refusals, cheapest first -------------------------------------------------------------------

[ "$branch" != "$MAIN" ] || { echo "error: $branch IS $MAIN — nothing to land" >&2; exit 1; }
git rev-parse --verify --quiet "refs/heads/$branch" >/dev/null \
  || { echo "error: no such branch: $branch" >&2; exit 1; }

[ -z "$(git status --porcelain)" ] \
  || { echo "error: working tree is dirty — commit or stash first" >&2; git status --short >&2; exit 1; }

# main must match the remote. Landing onto a stale main produces a commit whose parent is not what
# anyone else has, and the push that follows either fails or force-updates someone's work away.
git fetch --quiet origin "$MAIN"
if [ "$(git rev-parse "$MAIN")" != "$(git rev-parse "origin/$MAIN")" ]; then
  echo "error: $MAIN differs from origin/$MAIN — pull (fast-forward) before landing" >&2
  git --no-pager log --oneline --left-right "$MAIN...origin/$MAIN" >&2
  exit 1
fi

n=$(git rev-list --count "$MAIN..$branch")
[ "$n" -gt 0 ] || { echo "error: $branch has no commits not already on $MAIN" >&2; exit 1; }

behind=$(git rev-list --count "$branch..$MAIN")
if [ "$behind" -gt 0 ]; then
  echo "error: $branch is $behind commit(s) behind $MAIN — rebase it first:" >&2
  echo "         git rebase $MAIN $branch" >&2
  exit 1
fi

echo "==> landing $branch onto $MAIN ($n commit(s))"

# --- build the message, with every squashed commit preserved ------------------------------------

msg="$(mktemp -t land-msg-XXXXXX)"
trap 'rm -f "${msg:?}"' EXIT

{
  [ -z "$subject" ] || printf '%s\n' "$subject"
  printf '\n'
  printf '%s The first line is the subject: one conventional-commit line describing what this\n' "$MARK"
  printf '%s branch delivered AS A WHOLE. Add a summary paragraph under it if it helps.\n' "$MARK"
  printf '%s\n' "$MARK"
  printf '%s Lines starting with %s are removed. Everything else is kept VERBATIM, including the\n' "$MARK" "$MARK"
  printf '%s preserved commit messages below — delete any you do not want in the body.\n' "$MARK"
  printf '%s\n' "$MARK"
  printf '%s Landing: %s -> %s (%d commits)\n' "$MARK" "$branch" "$MAIN" "$n"
  printf '\n--- Squashed from %d commits (preserved below) ---\n' "$n"
  i=0
  while read -r sha; do
    i=$((i + 1))
    printf '\n[%d/%d] %s\n' "$i" "$n" "$(git show -s --format='%s' "$sha")"
    body="$(git show -s --format='%b' "$sha")"
    [ -z "$(printf '%s' "$body" | tr -d '[:space:]')" ] || printf '%s\n' "$body"
  done < <(git rev-list --reverse "$MAIN..$branch")
} > "$msg"

if [ -z "$subject" ]; then
  "${GIT_EDITOR:-${VISUAL:-${EDITOR:-vi}}}" "$msg"
fi

grep -v "^${MARK}" "$msg" > "$msg.clean" && mv "$msg.clean" "$msg"
# A subject is the one thing that cannot be recovered from the branch, so require it explicitly
# rather than letting git reject an empty message after the gate has already run.
if [ -z "$(head -1 "$msg" | tr -d '[:space:]')" ]; then
  echo "error: no subject on the first line — aborting, $MAIN untouched" >&2
  exit 1
fi

# --- stage the squash, then gate it BEFORE committing --------------------------------------------

git checkout --quiet "$MAIN"

unwind() { git reset --hard --quiet "$MAIN"; git checkout --quiet "$branch"; }

if ! git merge --squash "$branch" >/dev/null 2>&1; then
  echo "error: squash merge conflicted — rebase $branch onto $MAIN and retry" >&2
  git merge --abort 2>/dev/null || true
  unwind
  exit 1
fi

if [ "$skip_gate" -eq 1 ]; then
  # Deliberately loud. The whole point of this script is that main's commits are known-good, and a
  # commit landed without the gate is exactly the one nobody will think to re-check later.
  echo "!!! --no-gate: landing WITHOUT running dev/check-all.sh" >&2
  echo "!!! this commit's claim to build and pass CI is unverified" >&2
else
  echo "==> gate: dev/check-all.sh ${gate_args[*]-}"
  # Guarded expansion: an empty array must expand to NO arguments, not to one empty string —
  # check-all.sh rejects unknown arguments, and "" is an unknown argument.
  if ! ./dev/check-all.sh ${gate_args[@]+"${gate_args[@]}"}; then
    echo >&2
    echo "error: gate FAILED on the merged tree — nothing committed, $MAIN untouched" >&2
    unwind
    exit 1
  fi
fi

if ! git commit --quiet --file "$msg" --cleanup=whitespace; then
  echo "error: commit failed — nothing committed, $MAIN untouched" >&2
  unwind
  exit 1
fi

echo "==> landed: $(git log --oneline -1)"

# `git branch -d` ALWAYS refuses a squash-merged branch: the squash commit has no parent link back to
# it, so git cannot see the work as merged and -d's reachability check is measuring the wrong thing.
# Reaching for -D by reflex is the wrong lesson — it is the flag that also throws away work that was
# genuinely never merged. The check that IS meaningful after a squash is whether the branch's TREE
# matches main; if it does, the work is on main by content and the branch is redundant. So: verify
# that here, delete on the strength of it, and refuse if it does not hold.
if [ "$keep_branch" -eq 1 ]; then
  echo "==> keeping $branch (--keep-branch)"
elif git diff --quiet "$MAIN" "$branch"; then
  git branch -D "$branch" >/dev/null
  echo "==> deleted $branch (its tree matched $MAIN exactly)"
  if git ls-remote --exit-code --heads origin "$branch" >/dev/null 2>&1; then
    echo "    a remote copy remains: git push origin --delete $branch"
  fi
else
  echo "!!! $branch differs from $MAIN after landing — NOT deleting it" >&2
  git --no-pager diff --stat "$MAIN" "$branch" >&2
fi

echo
echo "Next: git push origin $MAIN"
