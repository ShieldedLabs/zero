#!/usr/bin/env bash
#
# Detached worker for .githooks/pre-push. Builds the diff for the pushed
# range, runs it past an AI reviewer with an adversarial prompt, and writes
# REVIEW.md at the repo root.
#
# Runs with nobody watching, so it is written to always terminate and always
# leave REVIEW.md in a readable state: a placeholder goes down first, the real
# result replaces it atomically, and every error path writes an explanation
# into the file rather than dying silently. A worker that fails invisibly is
# indistinguishable from a clean review, which is the same failure mode that
# made path-filtered CI dangerous here (see .github/workflows/z3-smoke.yml).
#
# Everything below reads git OBJECTS, never the worktree, so the review
# describes exactly what was pushed even if you carry on editing, switch
# branches, or start the next change while it runs. The reviewer is allowed to
# read files for context, and those reads do see the live worktree -- so
# surrounding-code context can be newer than the diff, though the diff itself
# cannot drift.
#
# Usage: review-worker.sh <remote-name> <spec-file>
#   spec-file: one line per ref, "<local ref> <local sha> <remote ref> <remote sha>"

set -uo pipefail

REMOTE_NAME="${1:-origin}"
SPEC="${2:-}"
[ -n "$SPEC" ] && [ -f "$SPEC" ] || exit 0

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
cd "$REPO_ROOT" || exit 0

STATE_DIR="$REPO_ROOT/.githooks/.review-state"
LOCK_DIR="$STATE_DIR/lock"
# REVIEW_FILE is chosen once the range is known -- see "output file" below.
REVIEW_FILE=""
REVIEW_URL=""
mkdir -p "$STATE_DIR" 2>/dev/null

# ---------------------------------------------------------------- config ---
# Precedence, highest first: environment -> .githooks/review.conf -> the
# defaults below.
#
# The order of these two blocks is what implements that, so do not swap them.
# review.conf is sourced FIRST and its entries use the `${VAR:-value}` form, so
# a value already exported for this push survives; the defaults then fill in
# whatever neither set. Sourcing the file after the defaults would make every
# default look "already set" and the file could never take effect -- and with
# plain `VAR=value` entries the file would instead silently outrank the
# environment, so `REVIEW_BACKEND=codex git push` would quietly run claude.
#
# .githooks/review.conf is tracked and ships with the repo. The defaults below
# are the backstop if it is ever deleted, and they match what it sets.

# shellcheck source=/dev/null
[ -f "$REPO_ROOT/.githooks/review.conf" ] && . "$REPO_ROOT/.githooks/review.conf"

REVIEW_BACKEND="${REVIEW_BACKEND:-claude}"
REVIEW_MODEL="${REVIEW_MODEL:-}"
REVIEW_EFFORT="${REVIEW_EFFORT:-}"
REVIEW_TIMEOUT="${REVIEW_TIMEOUT:-900}"
REVIEW_MAX_DIFF_LINES="${REVIEW_MAX_DIFF_LINES:-6000}"
REVIEW_EXCLUDE_PATHS="${REVIEW_EXCLUDE_PATHS:-}"
REVIEW_NOTIFY="${REVIEW_NOTIFY:-1}"
REVIEW_OPEN_CMD="${REVIEW_OPEN_CMD:-}"
REVIEW_CMD="${REVIEW_CMD:-}"

# The terminal that ran the push, captured by the hook while it still had one.
# A tty is just a file, so this detached worker can write into that shell
# minutes later without having a controlling terminal itself -- provided the
# terminal is still open. If it has since closed, the write fails and is
# swallowed; REVIEW.md is the delivery that always happens.
REVIEW_TTY="${REVIEW_TTY:-}"

# One line back to the pushing terminal. It arrives asynchronously, so it can
# land over a prompt or mid-command; the leading newline keeps it from running
# into whatever is already on that line.
tty_report() {
    [ -n "$REVIEW_TTY" ] || return 0
    { printf '\n%s\n  %s\n' "$1" "$REVIEW_URL" >"$REVIEW_TTY"; } 2>/dev/null || true
}

cleanup() {
    rm -f "$SPEC"
    rmdir "$LOCK_DIR" 2>/dev/null
}
trap cleanup EXIT INT TERM

# Single writer for REVIEW.md. Two quick pushes would otherwise interleave
# their output; the second push waits briefly, then yields rather than
# corrupting the first result.
waited=0
while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    waited=$((waited + 5))
    if [ "$waited" -gt 60 ]; then
        exit 0
    fi
    sleep 5
done

log() { printf '[%s] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"; }

# ------------------------------------------------------------ range calc ---
ZERO=0000000000000000000000000000000000000000
EMPTY_TREE=$(git hash-object -t tree /dev/null)

# Resolve the base for one ref: the commit the remote already has, or -- for a
# branch the remote has never seen -- the parent of the oldest commit that no
# remote ref can reach.
#
# The empty-tree fallback is reserved for the one case that actually means it:
# a new branch whose oldest commit is a root commit. It must NOT catch the
# "rev-list came back empty" case, which means every commit here is already on
# the remote and the honest answer is "nothing new to review" -- returning the
# empty tree there diffs the whole repository instead (observed: 1.3M lines,
# 61MB on disk, for a push with no new commits at all).
resolve_base() {
    local head="$1" remote_sha="$2" oldest base

    if [ "$remote_sha" != "$ZERO" ] && git cat-file -e "${remote_sha}^{commit}" 2>/dev/null; then
        printf '%s' "$remote_sha"
        return
    fi

    oldest=$(git rev-list "$head" --not --remotes="$REMOTE_NAME" 2>/dev/null | tail -1)
    if [ -z "$oldest" ]; then
        # Nothing on this ref is absent from the remote. Base == head yields an
        # empty diff, which the caller reports as "nothing to review".
        printf '%s' "$head"
        return
    fi

    base=$(git rev-parse --verify --quiet "${oldest}^" 2>/dev/null)
    if [ -n "$base" ]; then
        printf '%s' "$base"
    else
        printf '%s' "$EMPTY_TREE"   # oldest is a root commit
    fi
}

REFS=()
BASES=()
HEADS=()
while read -r local_ref local_sha remote_ref remote_sha; do
    [ -n "${local_sha:-}" ] || continue
    base=$(resolve_base "$local_sha" "${remote_sha:-$ZERO}")
    REFS+=("${local_ref#refs/heads/}")
    BASES+=("$base")
    HEADS+=("$local_sha")
done <"$SPEC"

[ "${#HEADS[@]}" -gt 0 ] || exit 0

BRANCH="${REFS[0]}"
BASE="${BASES[0]}"
HEAD="${HEADS[0]}"
SHORT_BASE=$(git rev-parse --short "$BASE" 2>/dev/null || printf '%s' "${BASE:0:9}")
SHORT_HEAD=$(git rev-parse --short "$HEAD" 2>/dev/null || printf '%s' "${HEAD:0:9}")
STARTED=$(date -u '+%Y-%m-%d %H:%M:%SZ')

# ----------------------------------------------------------- output file ---
# A previous review is never clobbered. The first review of a working tree
# lands in REVIEW.md; once that exists, later reviews get REVIEW-<base>.md,
# named for the commit the range starts from, so each file says which range it
# describes and they accumulate rather than overwrite.
#
# The one case that does overwrite is re-reviewing a range whose file already
# exists -- same base, same content under review, so the fresher verdict wins
# instead of piling up REVIEW-<base>.1.md.
#
# Decided once, here, and before the placeholder is written: testing this after
# the placeholder landed would see the file this run just created and rename
# every review after the first.
if [ -e "$REPO_ROOT/REVIEW.md" ]; then
    REVIEW_FILE="$REPO_ROOT/REVIEW-${SHORT_BASE}.md"
else
    REVIEW_FILE="$REPO_ROOT/REVIEW.md"
fi
REVIEW_URL="file://${REVIEW_FILE}"
REVIEW_URL="${REVIEW_URL// /%20}"

# Pathspec for excludes, if configured.
PATHSPEC=()
if [ -n "$REVIEW_EXCLUDE_PATHS" ]; then
    PATHSPEC+=(--)
    PATHSPEC+=(".")
    for p in $REVIEW_EXCLUDE_PATHS; do
        PATHSPEC+=(":(exclude)$p")
    done
fi

COMMITS=$(git log --no-merges --format='- %h %s (%an)' "$BASE".."$HEAD" 2>/dev/null)
[ -n "$COMMITS" ] || COMMITS="(no non-merge commits in range)"
STAT=$(git diff --stat "$BASE" "$HEAD" ${PATHSPEC[@]+"${PATHSPEC[@]}"} 2>/dev/null)

# Size the change from --numstat (a few hundred bytes) rather than from the
# patch itself, then stream the patch through `head`. A first push of a
# long-lived branch can legitimately be a million lines; measuring it must not
# require writing it to disk first.
NUMSTAT=$(git diff --numstat "$BASE" "$HEAD" ${PATHSPEC[@]+"${PATHSPEC[@]}"} 2>/dev/null)
FILE_COUNT=$(printf '%s\n' "$NUMSTAT" | grep -c '[^[:space:]]' || true)
# Binary files report "-" for both counts; awk reads those as 0, which is right.
DIFF_LINES=$(printf '%s\n' "$NUMSTAT" | awk '{a+=$1; d+=$2} END {printf "%d", a+d}')

DIFF_FILE="$STATE_DIR/diff.patch"
git diff --no-color "$BASE" "$HEAD" ${PATHSPEC[@]+"${PATHSPEC[@]}"} 2>/dev/null \
    | head -n "$REVIEW_MAX_DIFF_LINES" >"$DIFF_FILE" || true

# ------------------------------------------------------------ placeholder ---
{
    printf '# Review in progress\n\n'
    printf '**Branch:** `%s`  \n' "$BRANCH"
    printf '**Range:** `%s..%s` (%s files, %s changed lines)  \n' \
        "$SHORT_BASE" "$SHORT_HEAD" "$FILE_COUNT" "$DIFF_LINES"
    printf '**Backend:** `%s`  \n' "$REVIEW_BACKEND"
    printf '**Started:** %s\n\n' "$STARTED"
    printf 'The push already completed. This file is rewritten when the review finishes.\n'
    printf 'If it still says this in a few minutes, check `.githooks/.review-state/worker.log`.\n\n'
    printf '## Commits\n\n%s\n' "$COMMITS"
} >"$REVIEW_FILE"

if [ "$DIFF_LINES" -eq 0 ]; then
    {
        printf '# Review: nothing to do\n\n'
        printf '**Branch:** `%s`  \n**Range:** `%s..%s`\n\n' "$BRANCH" "$SHORT_BASE" "$SHORT_HEAD"
        if [ "$BASE" = "$HEAD" ]; then
            printf 'Every commit on this ref is already present on `%s`, so the push\n' "$REMOTE_NAME"
            printf 'carried nothing new to review.\n'
        else
            printf 'The pushed range produced an empty diff'
            [ -n "$REVIEW_EXCLUDE_PATHS" ] && printf ' (after applying REVIEW_EXCLUDE_PATHS)'
            printf '.\n'
        fi
    } >"$REVIEW_FILE"
    log "empty diff for $SHORT_BASE..$SHORT_HEAD; nothing to review"
    tty_report "Local review: nothing to review in $BRANCH $SHORT_BASE..$SHORT_HEAD"
    exit 0
fi

# ------------------------------------------------------------ size guard ---
# A vendored subtree pull can carry >100 files of upstream code that nobody
# here wrote. Reviewing it whole is expensive and low signal, so oversized
# diffs are cut down -- but the cut is stated in REVIEW.md rather than applied
# quietly, because a silent truncation reads as "all clear" when it isn't.
PAYLOAD="$DIFF_FILE"
PATCH_LINES=$(wc -l <"$DIFF_FILE" | tr -d ' ')
TRUNCATED=""
if [ "$PATCH_LINES" -ge "$REVIEW_MAX_DIFF_LINES" ]; then
    TRUNCATED="yes"
    log "diff truncated at $REVIEW_MAX_DIFF_LINES patch lines ($FILE_COUNT files, $DIFF_LINES changed lines total)"
fi

# ---------------------------------------------------------------- prompt ---
PROMPT_FILE="$STATE_DIR/prompt.txt"
{
    cat <<'PROMPT_HEADER'
You are an adversarial code reviewer. Your default assumption is that the
change below is WRONG and your job is to prove it. Do not summarize the
change, do not compliment it, and do not accept the commit messages'
account of what it does -- verify that against the actual code. Some or all
of this code may itself have been written by an AI, so the reasoning that
produced it may be confidently wrong in ways that read fluently.

Scope: review ONLY the diff provided. Do not report issues in code that the
diff does not touch, except where the diff breaks that code.

You may read files in the repository for context -- callers, type
definitions, tests, the surrounding function -- and you should, because a
hunk in isolation hides most real bugs. Note that the worktree you read may
be slightly newer than the diff.

You should test against the artifact this diff produces rather than just the
code. Testing a binary directly or importing the library is better than
reading the code in isolation.

Rank findings by severity, most severe first, and prioritize in this order:
  1. Correctness bugs: wrong results, off-by-one, inverted conditions,
     unhandled error paths, resource leaks, panics/unwraps on reachable input.
  2. Data loss, corruption, or consensus/protocol divergence.
  3. Security: injection, secret exposure, missing authentication or bounds
     checks, unsafe deserialization, unsafe memory usage.
  4. Concurrency: races, deadlocks, lock-ordering inversions, TOCTOU.
  5. Compatibility breaks in APIs, wire formats, or on-disk state.
  6. Test gaps that would have caught any of the above.

For EACH finding, give:
  - A one-line claim of what is broken.
  - `path/to/file.rs:LINE` for where it is.
  - A concrete failure scenario: specific inputs or state, and the wrong
    output, panic, or corruption that results. If you cannot construct one,
    say so and lower your confidence accordingly.
  - Confidence: HIGH / MEDIUM / LOW.

Be precise about uncertainty. A speculative finding marked LOW is useful; a
speculative finding dressed as certain is worse than silence. If you find
nothing that meets the bar, say "No findings" and explain in two sentences
what you checked and where you would look next. Do not pad the list.

Output GitHub-flavored markdown. Start directly with the findings under a
`## Findings` heading -- no preamble, no restatement of these instructions.

PROMPT_HEADER

    printf 'Repository: %s\n' "$(basename "$REPO_ROOT")"
    printf 'Branch: %s\n' "$BRANCH"
    printf 'Range under review: %s..%s\n\n' "$SHORT_BASE" "$SHORT_HEAD"

    printf 'Commits being pushed:\n%s\n\n' "$COMMITS"
    printf 'Diffstat:\n%s\n\n' "$STAT"

    if [ -n "$TRUNCATED" ]; then
        printf 'NOTE: this diff was truncated to the first %s patch lines. The full\n' \
            "$REVIEW_MAX_DIFF_LINES"
        printf 'range is %s files and %s changed lines.\n' "$FILE_COUNT" "$DIFF_LINES"
        printf 'Review what is present, and say plainly in your output that the\n'
        printf 'tail of the diff was not reviewed.\n\n'
    fi

    printf 'Diff:\n\n```diff\n'
    cat "$PAYLOAD"
    printf '\n```\n'
} >"$PROMPT_FILE"

# --------------------------------------------------------------- backend ---
# Reads the prompt on stdin, writes markdown on stdout. Backends are kept
# read-only: an unattended reviewer has no business editing the tree it is
# reviewing or running arbitrary commands.
backend_run() {
    case "$REVIEW_BACKEND" in
        claude)
            set -- claude -p \
                --allowed-tools "Read Grep Glob" \
                --disallowed-tools "Edit Write NotebookEdit Bash" \
                --permission-mode dontAsk
            [ -n "$REVIEW_MODEL" ] && set -- "$@" --model "$REVIEW_MODEL"
            [ -n "$REVIEW_EFFORT" ] && set -- "$@" --effort "$REVIEW_EFFORT"
            "$@"
            ;;
        codex)
            # Codex CLI headless. Untested here: no `codex` on this machine at
            # the time of writing, so treat the flags as a starting point.
            set -- codex exec --skip-git-repo-check
            [ -n "$REVIEW_MODEL" ] && set -- "$@" --model "$REVIEW_MODEL"
            "$@"
            ;;
        custom)
            [ -n "$REVIEW_CMD" ] || { echo "REVIEW_BACKEND=custom needs REVIEW_CMD" >&2; return 127; }
            eval "$REVIEW_CMD"
            ;;
        *)
            echo "unknown REVIEW_BACKEND: $REVIEW_BACKEND" >&2
            return 127
            ;;
    esac
}

OUT_FILE="$STATE_DIR/review.out"
ERR_FILE="$STATE_DIR/review.err"
: >"$OUT_FILE"
: >"$ERR_FILE"

log "reviewing $BRANCH $SHORT_BASE..$SHORT_HEAD ($DIFF_LINES lines) via $REVIEW_BACKEND"
START_EPOCH=$(date +%s)

# Portable watchdog: macOS ships no coreutils `timeout`, and a reviewer that
# hangs would otherwise hold the lock and leave REVIEW.md on the placeholder
# forever.
backend_run <"$PROMPT_FILE" >"$OUT_FILE" 2>"$ERR_FILE" &
BACKEND_PID=$!
( sleep "$REVIEW_TIMEOUT"; kill -TERM "$BACKEND_PID" 2>/dev/null ) &
WATCHDOG_PID=$!
wait "$BACKEND_PID"
RC=$?
kill -TERM "$WATCHDOG_PID" 2>/dev/null
wait "$WATCHDOG_PID" 2>/dev/null

ELAPSED=$(( $(date +%s) - START_EPOCH ))

# ---------------------------------------------------------------- output ---
TMP_OUT="$STATE_DIR/REVIEW.md.tmp"
{
    if [ "$RC" -eq 0 ] && [ -s "$OUT_FILE" ]; then
        printf '# Adversarial review\n\n'
    else
        printf '# Adversarial review FAILED\n\n'
    fi

    printf '**Branch:** `%s`  \n' "$BRANCH"
    printf '**Range:** `%s..%s`  \n' "$SHORT_BASE" "$SHORT_HEAD"
    printf '**Reviewer:** `%s`' "$REVIEW_BACKEND"
    [ -n "$REVIEW_MODEL" ] && printf ' (`%s`)' "$REVIEW_MODEL"
    [ -n "$REVIEW_EFFORT" ] && printf ' effort `%s`' "$REVIEW_EFFORT"
    printf '  \n'
    printf '**Diff:** %s files, %s changed lines' "$FILE_COUNT" "$DIFF_LINES"
    [ -n "$TRUNCATED" ] && printf ' — **TRUNCATED at %s patch lines; the tail was NOT reviewed**' "$REVIEW_MAX_DIFF_LINES"
    printf '  \n'
    [ -n "$REVIEW_EXCLUDE_PATHS" ] && printf '**Excluded paths:** `%s`  \n' "$REVIEW_EXCLUDE_PATHS"
    printf '**Finished:** %s (%ss)\n\n' "$(date -u '+%Y-%m-%d %H:%M:%SZ')" "$ELAPSED"

    printf '<details><summary>Commits reviewed</summary>\n\n%s\n\n</details>\n\n' "$COMMITS"
    printf -- '---\n\n'

    if [ "$RC" -eq 0 ] && [ -s "$OUT_FILE" ]; then
        cat "$OUT_FILE"
    else
        printf 'The reviewer exited %s' "$RC"
        [ "$ELAPSED" -ge "$REVIEW_TIMEOUT" ] && printf ' (hit the %ss REVIEW_TIMEOUT)' "$REVIEW_TIMEOUT"
        printf ' and produced no usable output.\n\n'
        printf 'This is a reviewer failure, **not** a clean review. Nothing was checked.\n\n'
        printf 'stderr:\n\n```\n'
        tail -n 40 "$ERR_FILE"
        printf '\n```\n\nRe-run by hand:\n\n```sh\n'
        printf '.githooks/review-worker.sh %s <(echo "refs/heads/%s %s refs/heads/%s %s")\n' \
            "$REMOTE_NAME" "$BRANCH" "$HEAD" "$BRANCH" "$BASE"
        printf '```\n'
    fi
} >"$TMP_OUT"

mv -f "$TMP_OUT" "$REVIEW_FILE"
log "wrote $REVIEW_FILE (rc=$RC, ${ELAPSED}s)"

# ------------------------------------------------------------- reporting ---
# Two channels, both best-effort; REVIEW.md on disk is the delivery that always
# happens.
#
#   1. A line back to the terminal that pushed, carrying a file:// URL that
#      most terminals make clickable (cmd-click in iTerm2 / Terminal.app,
#      ctrl-click in VS Code).
#   2. A desktop notification.
#
# REVIEW_NOTIFY: 0 = silent, 1 = notify, open = notify and open the file.
#
# Clicking a *notification* can only open REVIEW.md when terminal-notifier is
# installed. macOS attributes a notification to the process that posted it and
# gives `display notification` no click action of its own, so an osascript
# notification activates *osascript* -- which is why clicking one opens an
# empty Script Editor window. Nothing in the argument list fixes that; the
# sender is the whole story. Without terminal-notifier the fallback posts as
# Finder, so a stray click raises Finder rather than a blank editor. The
# file:// line above is the reliable way in either case.
if [ "$RC" -eq 0 ] && [ -s "$OUT_FILE" ]; then
    title="Adversarial review"
    tty_report "Local review ready — $BRANCH $SHORT_BASE..$SHORT_HEAD"
else
    title="Adversarial review FAILED"
    tty_report "Local review FAILED — $BRANCH $SHORT_BASE..$SHORT_HEAD (nothing was checked)"
fi

if [ "$REVIEW_NOTIFY" != "0" ]; then
    msg="$BRANCH $SHORT_BASE..$SHORT_HEAD"

    # How to open REVIEW.md on this platform, unless configured.
    opener="$REVIEW_OPEN_CMD"
    if [ -z "$opener" ]; then
        if command -v open >/dev/null 2>&1; then
            opener="open"
        elif command -v xdg-open >/dev/null 2>&1; then
            opener="xdg-open"
        fi
    fi

    if command -v terminal-notifier >/dev/null 2>&1; then
        # -execute runs on click. Quote the path: it may contain spaces.
        terminal-notifier \
            -title "$title" \
            -message "$msg — click to open REVIEW.md" \
            -execute "$opener '$REVIEW_FILE'" >/dev/null 2>&1
    elif [ "$(uname -s)" = "Darwin" ] && command -v osascript >/dev/null 2>&1; then
        osascript -e "tell application \"Finder\" to display notification \"$msg\" with title \"$title\"" \
            >/dev/null 2>&1
    elif command -v notify-send >/dev/null 2>&1; then
        notify-send "$title" "$msg" >/dev/null 2>&1
    fi

    if [ "$REVIEW_NOTIFY" = "open" ] && [ -n "$opener" ]; then
        $opener "$REVIEW_FILE" >/dev/null 2>&1 &
    fi
fi

exit 0
