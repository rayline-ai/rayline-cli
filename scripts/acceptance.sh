#!/usr/bin/env bash
# Acceptance matrix: real turns per harness x config profile x case.
#   profiles: config files, each passed as `rayline run <harness> --config
#             <file>`, which runs in the file's `mode`. Default: the
#             committed profiles in scripts/acceptance/profiles/ (cloud;
#             local with the cloud router and ARC Swift decisions; local with
#             the cloud router and ARC Sage decisions; local with the local
#             router). ARC picks the model of every turn, so local cells also
#             report the models that executed.
#   cases:    plain  one turn, a fixed answer
#             tool   the model must read a file and add
#             heavy  an MCP tool set whose schemas carry top-level oneOf/anyOf/
#                    allOf (scripts/acceptance/mcp_tools.py); the model must
#                    call one of those tools
#             multi  two turns in one session: the second answer needs what
#                    the first turn read and reasoned about
# Usage: [PROFILES="<file> ..."] [CASES="plain tool heavy multi"]
#        [TIMEOUT=<secs>] [WORK=<dir>] scripts/acceptance.sh [HARNESS...]
#   (default: every harness; a harness that isn't installed fails)
# For each local profile the script runs `rayline local start --config
# <file>`, that profile's cells, then `rayline local stop`. It writes no config
# outside WORK (a fresh temporary directory by default). If a local process is
# already running, or `rayline local status` fails, it exits 1 before any
# turn. It runs `rayline local stop` on exit, including on Ctrl-C, TERM and
# HUP, while a local process it started may be running. If `rayline local
# start` fails, it stops nothing: a process it didn't start is left alone.
# A cell a harness cannot run (no per-run MCP config) or that `rayline run`
# refuses by design (a harness local ARC decisions don't read) is reported as
# SKIP with the reason and does not count as a failure.
# Exit: 0 when no cell failed, 1 when one did, 2 on a usage or config error.
# See docs/acceptance-testing.md.
set -uo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
PROFILE_DIR=$SCRIPT_DIR/acceptance/profiles
RAYLINE=${RAYLINE:-rayline}
TIMEOUT=${TIMEOUT:-600}
PROFILES=${PROFILES:-}
CASES=${CASES:-"plain tool heavy multi"}
HARNESSES=(claude codex opencode pi omp hermes openclaw)
ALL_CASES=(plain tool heavy multi)
MCP_SERVER=$SCRIPT_DIR/acceptance/mcp_tools.py
RUN_TOKEN=$RANDOM$RANDOM

usage() {
  echo "usage: [PROFILES=...] [CASES=...] $0 [HARNESS...]" >&2
  echo "harnesses: ${HARNESSES[*]}; cases: ${ALL_CASES[*]}" >&2
  exit 2
}

one_of() {
  local needle=$1 word
  shift
  for word in "$@"; do
    [ "$needle" = "$word" ] && return 0
  done
  return 1
}

installed() {
  command -v "$1" >/dev/null 2>&1
}

# ---------------------------------------------------------------- harnesses

# harness_cmd <harness> <prompt> [args...]: sets `cmd` to the harness's own
# arguments for one non-interactive turn with [args...] added.
harness_cmd() {
  local harness=$1 prompt=$2
  shift 2
  case $harness in
    # Prompt first: --mcp-config and --allowedTools take several values.
    claude) cmd=(-p "$prompt" "$@") ;;
    # Flags after the args: a resumed turn's `resume <id>` must follow `exec`
    # directly, where Codex and `rayline run --config` both look for it.
    codex) cmd=(exec "$@" --skip-git-repo-check "$prompt") ;;
    opencode) cmd=(run "$@" "$prompt") ;;
    pi | omp) cmd=(-p "$@" "$prompt") ;;
    # Not -Q: quiet mode hides the "Session:" line the multi case resumes.
    hermes) cmd=(chat "$@" -q "$prompt") ;;
    openclaw) cmd=(agent "$@" -m "$prompt") ;;
  esac
}

# heavy_args <harness> <dir>: sets `args` to the flags that add the MCP tool
# set for one turn (files go in <dir>). Sets `skip` with the reason instead
# when the harness cannot take a per-run MCP server, or never sends such
# schemas.
heavy_args() {
  local harness=$1 dir=$2
  args=()
  skip=
  case $harness in
    claude) skip="Claude Code skips tools with top-level oneOf/anyOf/allOf before any request" ;;
    codex) args=(-c "mcp_servers.acceptance.command=\"python3\"" -c "mcp_servers.acceptance.args=$(mcp_args_json)") ;;
    opencode) skip="OpenCode exposes MCP tools through its Code Mode execute tool; the schemas never leave the harness" ;;
    pi)
      # Pi's default exposure is codemode, which keeps the schemas inside the
      # harness; direct puts them in the model's tool declarations.
      mkdir -p "$dir/.pi"
      printf '{"mcpServers":{"acceptance":{"command":"python3","args":%s,"exposure":"direct"}}}\n' "$(mcp_args_json)" >"$dir/.pi/mcp.json"
      args=(--approve)
      ;;
    omp | hermes | openclaw) skip="$harness takes MCP servers only from its persistent config" ;;
  esac
}

# The harness starts the MCP server itself, not through bash, so on Windows (Git
# Bash, MSYS2, Cygwin) the path must be a native one such as C:/x/y.py, not /c/x/y.py.
mcp_args_json() {
  local server=$MCP_SERVER
  command -v cygpath >/dev/null 2>&1 && server=$(cygpath -m "$server")
  printf '["-I","%s","--value","%s"]' "$server" "$RUN_TOKEN"
}

# session_args <harness> <turn 1|2> <dir>: sets `args` to the flags that put
# the turn in the cell's session. Turn 2 reads `session_out`, turn 1's output,
# for harnesses that only name a session on exit.
session_args() {
  local harness=$1 turn=$2 dir=$3 id
  case $harness in
    claude)
      # A new id per run: Claude Code refuses --session-id for a session that exists.
      [ "$turn" = 1 ] && python3 -c 'import uuid; print(uuid.uuid4())' >"$dir/session-id"
      id=$(cat "$dir/session-id")
      [ "$turn" = 1 ] && args=(--session-id "$id") || args=(--resume "$id")
      ;;
    codex)
      if [ "$turn" = 1 ]; then
        args=()
      else
        id=$(sed -n -E 's/^session id: ([^ ]+).*/\1/p' <<<"$session_out" | head -n 1)
        [ -n "$id" ] || return 1
        args=(resume "$id")
      fi
      ;;
    opencode) args=(-s "ses_acceptance$RUN_TOKEN$turn_tag") ;;
    pi) args=(--session-id "acceptance-$RUN_TOKEN$turn_tag") ;;
    openclaw) args=() ;; # the cell's session, from cell_flags
    omp) [ "$turn" = 1 ] && args=() || args=(-c) ;;
    hermes)
      if [ "$turn" = 1 ]; then
        args=()
      else
        id=$(sed -n -E 's/^Session:[[:space:]]+([^ ]+).*/\1/p' <<<"$session_out" | head -n 1)
        [ -n "$id" ] || return 1
        args=(--resume "$id")
      fi
      ;;
  esac
}

# ----------------------------------------------------------------- running

pass=0
fail=0
skipped=0

# Arrays that may be empty are expanded as ${a[@]+"${a[@]}"}: bash 3.2 (macOS)
# treats "${a[@]}" of an empty array as unbound under set -u.

# with_timeout <secs> <cmd...>: runs cmd in its own process group and kills
# the whole group after <secs> or on Ctrl-C (TERM, then KILL after up to 10s),
# so no harness child outlives the turn.
with_timeout() {
  perl -e '
    use POSIX ":sys_wait_h";
    my $secs = shift;
    my $pid = fork // die "fork: $!\n";
    if (!$pid) { setpgrp(0, 0); exec @ARGV or die "exec: $!\n" }
    setpgrp($pid, $pid);
    sub stop_group {
      my ($code, $message) = @_;
      kill "TERM", -$pid;
      for (1 .. 10) { waitpid($pid, WNOHANG); last unless kill 0, -$pid; sleep 1 }
      kill "KILL", -$pid;
      waitpid($pid, 0);
      print STDERR $message if $message;
      exit $code;
    }
    $SIG{INT} = $SIG{TERM} = sub { stop_group(130) };
    $SIG{ALRM} = sub { stop_group(124, "timed out after ${secs}s\n") };
    alarm $secs;
    waitpid($pid, 0);
    exit($? & 127 ? 128 + ($? & 127) : $? >> 8);
  ' "$@"
}

# run_turn <harness> <cwd> <log> <prompt> [args...]: one harness turn in
# <cwd> with the current profile; output (router keys redacted) goes to <log>
# and `out`, exit status to `status`. In local mode `executed` collects the
# models the local process ran for this turn.
run_turn() {
  local harness=$1 cwd=$2 log=$3 prompt=$4 offset=0
  shift 4
  # Session args first: Codex's `resume <id>` comes right after `exec`.
  harness_cmd "$harness" "$prompt" "$@" ${cell_flags[@]+"${cell_flags[@]}"}
  [ "$mode" = local ] && offset=$(wc -l <"$RUNTIME_LOG")
  out=$(cd "$cwd" && with_timeout "$TIMEOUT" "$RAYLINE" run "$harness" --config "$profile_file" -- "${cmd[@]}" </dev/null 2>&1 |
    sed -E 's/rlk-[A-Za-z0-9_-]+/rlk-<redacted>/g')
  status=${PIPESTATUS[0]}
  printf '%s\n' "$out" >"$log"
  if [ "$mode" = local ]; then
    # Only records with an executed_model: sidecalls complete without one.
    executed=$(tail -n +"$((offset + 1))" "$RUNTIME_LOG" | grep -F '"execution completed"' |
      sed -n -E 's/.*"executed_model":"([^"]*)".*/\1/p' | sort -u | paste -sd, -)
  fi
}

# cause <expected>: one line on why the turn failed, from `status` and `out`:
# the last error line, as a harness prints its fatal error after its warnings.
cause() {
  local line
  if [ "$status" -eq 124 ]; then
    echo "timed out after ${TIMEOUT}s"
  elif [ "$status" -eq 0 ]; then
    echo "answer $1 missing from the output"
  elif line=$(grep -i -E 'error|unsupported|refused|not found|denied|invalid|timed out' <<<"$out" | tail -n 1); then
    echo "exit $status: ${line:0:200}"
  else
    echo "exit $status"
  fi
}

record() {
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$harness" "$profile" "$case" "$1" "${executed:-}" "$2" >>"$RESULTS"
}

report_pass() {
  local detail=
  [ "$mode" = local ] && detail=" (executed ${executed:-none})"
  echo "PASS    $harness $profile $case$detail"
  pass=$((pass + 1))
  record PASS ""
  # A turn that passed but logged an error or a 4xx still has a story. Three
  # digits inside a longer number, such as the token count 7,465, are not a status.
  grep -m 1 -i -E '\berror\b|(^|[^[:alnum:]_.,])4[0-9]{2}($|[^[:alnum:]_.,]|[.,]($|[^0-9]))' <<<"$out" |
    sed 's/^/NOTE    /' | cut -c1-220
}

report_fail() {
  local why=$1 detail=
  [ "$mode" = local ] && detail=" (executed ${executed:-none})"
  echo "FAIL    $harness $profile $case$detail: $why"
  tail -n 15 <<<"$out" | sed 's/^/        /'
  fail=$((fail + 1))
  record FAIL "$why"
}

report_skip() {
  echo "SKIP    $harness $profile $case: $1"
  skipped=$((skipped + 1))
  record SKIP "$1"
}

# fail_or_skip <cause>: a failed turn is a SKIP when `rayline run` itself
# refused it by design before the harness started: local ARC decisions refuse a
# harness they don't read. Anything else is a FAIL.
fail_or_skip() {
  local refusal
  if refusal=$(grep -m 1 -E 'local ARC decisions read only' <<<"$out"); then
    report_skip "$refusal"
  else
    report_fail "$1"
  fi
}

# run_cell: one cell for the current `harness`, `profile`, `case`. The harness
# runs in <cell dir>/work; the turn logs stay in <cell dir>, where the harness
# has no reason to look.
run_cell() {
  local dir=$WORK/$harness-$profile-$case cwd a b log
  local -a args
  cwd=$dir/work
  mkdir -p "$cwd"
  executed=
  cell_flags=()
  # OpenClaw continues its one persistent agent session unless told which to
  # use; a session per cell keeps cells independent.
  [ "$harness" = openclaw ] && cell_flags=(--session-id "acceptance-$RUN_TOKEN-$RANDOM")
  a=$((RANDOM % 90 + 10))
  b=$((RANDOM % 90 + 10))
  printf '%s\n%s\n' "$a" "$b" >"$cwd/answer.txt"
  case $case in
    plain)
      run_turn "$harness" "$cwd" "$dir/turn.log" \
        "Reply with the word ACCEPT immediately followed by the sum of 17 and 25, with no space and nothing else."
      check ACCEPT42
      ;;
    tool)
      run_turn "$harness" "$cwd" "$dir/turn.log" \
        "Read the file $cwd/answer.txt. It holds two integers, one per line. Reply with the word ACCEPT immediately followed by their sum, with no space and nothing else."
      check "ACCEPT$((a + b))"
      ;;
    heavy)
      heavy_args "$harness" "$cwd"
      [ -z "$skip" ] || { report_skip "$skip"; return; }
      run_turn "$harness" "$cwd" "$dir/turn.log" \
        "Call the lookup_record tool with id 7 and reply with the word ACCEPT immediately followed by the value it returns, with no space and nothing else." \
        ${args[@]+"${args[@]}"}
      check "ACCEPT$RUN_TOKEN"
      ;;
    multi)
      turn_tag=$RANDOM
      session_args "$harness" 1 "$dir"
      run_turn "$harness" "$cwd" "$dir/turn-1.log" \
        "Read the file $cwd/answer.txt, which holds two integers, one per line, and remember them. Reply with the word OK and nothing else." \
        ${args[@]+"${args[@]}"}
      [ "$status" -eq 0 ] || { fail_or_skip "turn 1: $(cause OK)"; return; }
      session_out=$out
      session_args "$harness" 2 "$dir" || { report_fail "turn 1 printed no session id"; return; }
      # Without the file, turn 2 can only get the integers from the session.
      rm "$cwd/answer.txt"
      run_turn "$harness" "$cwd" "$dir/turn-2.log" \
        "Reply with the word ACCEPT immediately followed by the sum of the two integers from the file you read earlier, with no space and nothing else." \
        ${args[@]+"${args[@]}"}
      [ "$status" -eq 0 ] || { fail_or_skip "$(cause "ACCEPT$((a + b))")"; return; }
      check "ACCEPT$((a + b))"
      ;;
  esac
}

# check <expected>: PASS when the last turn exited 0 and printed <expected>.
check() {
  if [ "$status" -eq 0 ] && grep -q -F "$1" <<<"$out"; then
    report_pass
  elif [ "$status" -ne 0 ]; then
    fail_or_skip "$(cause "$1")"
  else
    report_fail "$(cause "$1")"
  fi
}

# run_profile: every selected cell with the current `profile`.
run_profile() {
  for harness in "${selected[@]}"; do
    if ! installed "$harness"; then
      for case in "${cases[@]}"; do
        executed=
        out=
        status=127
        report_fail "not installed"
      done
      continue
    fi
    for case in "${cases[@]}"; do
      run_cell
    done
  done
}

# stop_local: runs `rayline local stop` once per started local process; a
# failed stop is a FAIL.
stopped=true
stop_local() {
  $stopped && return
  stopped=true
  if ! "$RAYLINE" local stop; then
    echo "FAIL    rayline local stop ($profile)"
    fail=$((fail + 1))
  fi
}

# ------------------------------------------------------------------- checks

selected=("$@")
[ ${#selected[@]} -gt 0 ] || selected=("${HARNESSES[@]}")
for harness in "${selected[@]}"; do
  one_of "$harness" "${HARNESSES[@]}" || { echo "unknown harness: $harness" >&2; usage; }
done
# The default profiles are an array, so a checkout path with spaces stays one
# file each; only a PROFILES value given by the caller is split on spaces.
if [ -n "$PROFILES" ]; then
  read -r -a profile_files <<<"$PROFILES"
else
  profile_files=("$PROFILE_DIR/cloud.json" "$PROFILE_DIR/local-cloud-arc-swift.json"
    "$PROFILE_DIR/local-cloud-arc-sage.json" "$PROFILE_DIR/local-router-local.json")
fi
read -r -a cases <<<"$CASES"
[ ${#profile_files[@]} -gt 0 ] || { echo "PROFILES is empty" >&2; usage; }
[ ${#cases[@]} -gt 0 ] || { echo "CASES is empty" >&2; usage; }
for case in "${cases[@]}"; do
  one_of "$case" "${ALL_CASES[@]}" || { echo "unknown case: $case" >&2; usage; }
done
installed python3 || { echo "python3 is needed (MCP tool server, session ids)" >&2; exit 2; }
[ -f "$MCP_SERVER" ] || { echo "missing $MCP_SERVER" >&2; exit 2; }

# Turns run in each cell's working directory, so paths given relative to the
# current directory are made absolute first.
case $RAYLINE in
  */*) RAYLINE=$(cd "$(dirname "$RAYLINE")" && pwd)/$(basename "$RAYLINE") || exit 2 ;;
esac
# Each profile's name is its file name without `.json`, and its mode is the
# file's `mode`, as `rayline run --config` reads it.
profile_names=()
profile_modes=()
for i in "${!profile_files[@]}"; do
  file=${profile_files[$i]}
  file=$(cd "$(dirname "$file")" && pwd)/$(basename "$file") || exit 2
  profile_files[$i]=$file
  name=$(basename "$file" .json)
  one_of "$name" ${profile_names[@]+"${profile_names[@]}"} &&
    { echo "two profiles are named $name" >&2; usage; }
  kind=$(perl -MJSON::PP -e '
    my $file = shift;
    open my $fh, "<", $file or die "profile $file: $!\n";
    my $config = eval { JSON::PP->new->decode(do { local $/; <$fh> }) };
    ref $config eq "HASH" or die "profile $file: not a JSON object\n";
    my $mode = $config->{mode} // "";
    $mode eq "cloud" || $mode eq "local"
      or die "profile $file: `mode` must be `cloud` or `local`\n";
    print $mode;
  ' "$file") || exit 2
  profile_names+=("$name")
  profile_modes+=("$kind")
done
RUNTIME_LOG=$HOME/.rayline/local/runtime.log

WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK" || exit 2
WORK=$(cd "$WORK" && pwd) || exit 2
RESULTS=$WORK/results.tsv
printf 'harness\tprofile\tcase\tresult\texecuted\tcause\n' >"$RESULTS"
echo "== work dir: $WORK"

echo "== versions"
"$RAYLINE" --version || exit 1
for harness in "${selected[@]}"; do
  if installed "$harness"; then
    echo "$harness: $("$harness" --version 2>&1 | head -n 1)"
  else
    echo "$harness: not installed"
  fi
done
for i in "${!profile_files[@]}"; do
  echo "== profile ${profile_names[$i]} (${profile_modes[$i]}): ${profile_files[$i]}"
done
echo "== cases: ${cases[*]}"

# The cleanup trap below stops the local process, so the script must not run
# while one it didn't start is running, or when it can't tell.
if one_of local "${profile_modes[@]}"; then
  if ! status_out=$("$RAYLINE" local status 2>&1); then
    echo "$status_out" >&2
    echo "rayline local status failed" >&2
    exit 1
  fi
  if grep -q '^Local process: running' <<<"$status_out"; then
    echo "a local process is already running; stop it first" >&2
    exit 1
  fi
fi

# -------------------------------------------------------------------- runs

trap stop_local EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
for i in "${!profile_files[@]}"; do
  profile=${profile_names[$i]}
  profile_file=${profile_files[$i]}
  mode=${profile_modes[$i]}
  echo "== $profile ($mode mode)"
  if [ "$mode" = cloud ]; then
    run_profile
    continue
  fi
  # Set before `rayline local start`, so a signal during start still stops it.
  stopped=false
  if "$RAYLINE" local start --config "$profile_file"; then
    # After a start, status must exit 0 and report the process running.
    status_out=$("$RAYLINE" local status 2>&1)
    status=$?
    echo "$status_out"
    if [ "$status" -ne 0 ] || ! grep -q '^Local process: running' <<<"$status_out"; then
      echo "FAIL    rayline local status ($profile, exit $status)"
      fail=$((fail + 1))
    fi
    if [ -f "$RUNTIME_LOG" ]; then
      run_profile
    else
      echo "FAIL    $RUNTIME_LOG missing after rayline local start ($profile); local cells need it"
      fail=$((fail + 1))
    fi
    stop_local
  else
    # A refused start (say, a process is already running) must not stop a
    # process this script didn't start.
    stopped=true
    echo "FAIL    rayline local start ($profile)"
    fail=$((fail + 1))
  fi
done

echo "== pass=$pass fail=$fail skip=$skipped"
echo "== results: $RESULTS"
[ "$fail" -eq 0 ]
