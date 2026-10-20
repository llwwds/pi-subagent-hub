#!/usr/bin/env bash
# Verified QA instance lifecycle helper.
#
# This is the only supported start/status/stop path for temporary Rust QA
# daemons. It never uses an inherited PIHUB_HOME for lifecycle writes, never
# signals a PID that was not identity-verified, and never deletes a directory
# whose canonical ownership was not confirmed by the receipt.
#
# The receipt is a same-user convenience record, not a privilege boundary: a
# receipt edited by the same user is in scope, and status/stop still re-derive
# live identity (ps command/parent/group/start plus health pid+version+name)
# before anything is signalled.
#
# Usage:
#   scripts/qa-instance.sh start --home DIR --binary FILE [--receipt FILE]
#       [--app-root DIR] [--env-file FILE] [--work-dir DIR]
#   scripts/qa-instance.sh status --home DIR [--receipt FILE]
#   scripts/qa-instance.sh stop --home DIR [--receipt FILE] [--cleanup]
#       [--stop-client FILE] [--stop-receipt FILE]
#
# `stop --cleanup` removes only the receipt's canonical home/work directory
# after the daemon has actually exited. Without `--cleanup`, it only stops.
#
# `stop --stop-client FILE` names an explicitly trusted, canonical, executable
# client to issue the stop request when the receipt's launch binary can no
# longer be executed (it was replaced on disk). The helper still re-verifies
# the live daemon identity from the unchanged receipt, verifies the stop
# client itself (regular executable, 4-part numeric version on the supported
# major line, recorded sha256/bytes), waives only the "launch file bytes still
# equal the recorded bytes" check, and writes a separate stop receipt so the
# original receipt stays verbatim. Same-user trust boundary; no privilege
# entity is added.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_RECEIPT="/tmp/pihub-rust-uiqa.json"
# Only the major line is pinned here; the full version is recorded from the
# binary itself (see start_instance), so a 3.0.x build needs no helper edit.
EXPECTED_MAJOR="3"
# One bounded deadline (seconds) for the stop request plus the daemon exit
# confirmation that follows it; the stop client never runs unbounded.
STOP_DEADLINE_SECS="20"

die() { printf '%s\n' "$*" >&2; exit 1; }
canon() { python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$1"; }
# True when both paths exist and name the same inode (catches symlink and
# hardlink aliases, not just equal strings).
same_file() { python3 -c 'import os,sys; raise SystemExit(0 if os.path.exists(sys.argv[2]) and os.path.samefile(sys.argv[1], sys.argv[2]) else 1)' "$1" "$2"; }
json() { python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d[sys.argv[2]])' "$1" "$2"; }

command="${1:-}"; shift || true
[[ "$command" == start || "$command" == status || "$command" == stop ]] || {
  die "usage: $0 start|status|stop --home DIR [options]"
}

HOME_RAW=""
BIN=""
APP_ROOT="$REPO_ROOT"
RECEIPT="$DEFAULT_RECEIPT"
ENV_FILE=""
WORK_RAW=""
CLEANUP=0
STOP_CLIENT_RAW=""
STOP_RECEIPT_RAW=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --home) HOME_RAW="${2:-}"; shift 2 ;;
    --binary) BIN="${2:-}"; shift 2 ;;
    --app-root) APP_ROOT="${2:-}"; shift 2 ;;
    --receipt) RECEIPT="${2:-}"; shift 2 ;;
    --env-file) ENV_FILE="${2:-}"; shift 2 ;;
    --work-dir) WORK_RAW="${2:-}"; shift 2 ;;
    --cleanup) CLEANUP=1; shift ;;
    --stop-client) STOP_CLIENT_RAW="${2:-}"; shift 2 ;;
    --stop-receipt) STOP_RECEIPT_RAW="${2:-}"; shift 2 ;;
    *) die "unknown option: $1" ;;
  esac
done
[[ -n "$HOME_RAW" ]] || die "--home is required"
if [[ -n "$STOP_CLIENT_RAW" || -n "$STOP_RECEIPT_RAW" ]]; then
  [[ "$command" == stop ]] || die "--stop-client/--stop-receipt are only valid for stop"
fi
[[ -z "$STOP_RECEIPT_RAW" || -n "$STOP_CLIENT_RAW" ]] \
  || die "--stop-receipt requires --stop-client (the default path updates the receipt in place)"
# Canonicalize and check the owned-root allowlist before anything touches the
# filesystem, so a refused target is left untouched. Only `start` creates the
# home; status/stop never re-create a directory they are about to validate.
HOME_DIR="$(canon "$HOME_RAW")"
case "$HOME_DIR" in
  /private/tmp/pihub-rust-qa-*|/private/tmp/pihub-rust-uiqa-*) ;;
  *) die "refusing non-QA home (canonical): $HOME_DIR" ;;
esac
RECEIPT_DIR="$(dirname "$RECEIPT")"
[[ -d "$RECEIPT_DIR" ]] || die "receipt directory does not exist: $RECEIPT_DIR"

# This Python block validates receipt + daemon.json + health + live process
# identity. It only writes a small shell file for the caller; it never signals
# or removes anything.
validate_receipt() {
  local require_ready="$1"
  local allow_launch_change="${2:-0}"
  python3 - "$RECEIPT" "$HOME_DIR" "$require_ready" "$EXPECTED_MAJOR" "$allow_launch_change" <<'PY'
import hashlib, json, os, re, shlex, subprocess, sys, urllib.request
from pathlib import Path
receipt_path, expected_home, require_ready, expected_major, allow_launch_change = sys.argv[1], sys.argv[2], sys.argv[3] == "1", sys.argv[4], sys.argv[5] == "1"
def fail(message):
    print(f"identity check failed: {message}", file=sys.stderr)
    raise SystemExit(2)
try:
    receipt = json.loads(Path(receipt_path).read_text())
except Exception as error:
    fail(f"cannot read receipt: {error}")
if receipt.get("phase") != "ready":
    fail(f"receipt phase is not ready: {receipt.get('phase')}")
recorded_version = receipt.get("version")
if not isinstance(recorded_version, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+", recorded_version):
    fail(f"receipt version is not a 4-part numeric version: {recorded_version!r}")
if recorded_version.split(".")[0] != expected_major:
    fail(f"receipt version is not on the {expected_major}.x line: {recorded_version}")
home = receipt.get("home")
if home != expected_home:
    # /tmp -> /private/tmp is an allowed system alias. Compare canonical paths;
    # the literal receipt home is retained only for command-identity matching.
    if not home or os.path.realpath(home) != expected_home:
        fail(f"receipt home mismatch: {home} != {expected_home}")
if require_ready and receipt.get("kind") != "uiqa-instance":
    fail("receipt kind is not uiqa-instance")
daemon_path = Path(home) / "state/daemon.json"
control_path = Path(home) / "state/control.json"
try:
    daemon = json.loads(daemon_path.read_text())
    control = json.loads(control_path.read_text())
except Exception as error:
    fail(f"daemon/control state unreadable: {error}")
pid = int(daemon.get("pid", 0))
if pid <= 0 or pid != int(receipt.get("pid", 0)):
    fail(f"pid mismatch: receipt={receipt.get('pid')} daemon={pid}")
port = int(daemon.get("port", 0))
receipt_port = receipt.get("port")
if receipt_port is None:
    url = receipt.get("url", "")
    try:
        receipt_port = int(url.split(":")[2].split("/")[0])
    except Exception:
        fail("receipt has neither port nor parseable url")
if port <= 0 or port != int(receipt_port):
    fail(f"port mismatch: receipt={receipt_port} daemon={port}")
if daemon.get("version") != receipt.get("version"):
    fail(f"daemon version mismatch: receipt={receipt.get('version')} daemon={daemon.get('version')}")
command = subprocess.run(["ps", "-p", str(pid), "-o", "command="], capture_output=True, text=True)
start = subprocess.run(["ps", "-p", str(pid), "-o", "lstart="], capture_output=True, text=True)
ppid = subprocess.run(["ps", "-p", str(pid), "-o", "ppid="], capture_output=True, text=True)
pgid = subprocess.run(["ps", "-p", str(pid), "-o", "pgid="], capture_output=True, text=True)
if command.returncode != 0:
    fail(f"process {pid} is not alive")
live_command = command.stdout.strip()
live_start = start.stdout.strip()
live_ppid = ppid.stdout.strip()
live_pgid = pgid.stdout.strip()
if receipt.get("processCommand") is not None:
    if live_command != receipt.get("processCommand"):
        fail(f"process command mismatch: {live_command!r} != {receipt.get('processCommand')!r}")
    if live_start != receipt.get("processStart"):
        fail(f"process start identity mismatch: {live_start!r} != {receipt.get('processStart')!r}")
    if live_ppid != str(receipt.get("ppid")) or live_pgid != str(receipt.get("pgid")):
        fail("process parent/group identity mismatch")
else:
    # Legacy receipt format: one recorded ps line with pid/ppid/pgid/lstart/cmd.
    recorded = receipt.get("process")
    if not recorded:
        fail("receipt has no process identity fields")
    live_process = subprocess.run(
        ["ps", "-p", str(pid), "-o", "pid=,ppid=,pgid=,lstart=,command="],
        capture_output=True, text=True,
    ).stdout
    def norm(value):
        return " ".join(value.split())
    if norm(live_process) != norm(recorded):
        fail(f"legacy process identity mismatch: {norm(live_process)!r} != {norm(recorded)!r}")
if not any(f"__daemon --home {candidate}" in live_command for candidate in {home, expected_home}):
    fail("process command does not bind the receipt home")
token = control.get("token")
if not token:
    fail("control token missing")
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
request = urllib.request.Request(f"http://127.0.0.1:{port}/v1/health")
request.add_header("Authorization", "Bearer " + token)
try:
    with opener.open(request, timeout=5) as response:
        health = json.loads(response.read())["data"]
except Exception as error:
    fail(f"health check failed: {error}")
if health.get("pid") != pid or health.get("version") != receipt.get("version") or health.get("name") != "pi-subagent-hub":
    fail(f"health identity mismatch: {health}")
binary = receipt.get("binary", {}).get("path")
if not binary:
    fail("receipt has no binary path")
recorded_bytes = int(receipt["binary"].get("bytes", -1))
launch_missing = not os.path.isfile(binary)
launch_bytes = -1 if launch_missing else os.path.getsize(binary)
launch_changed = launch_missing or launch_bytes != recorded_bytes
if launch_changed and not allow_launch_change:
    if launch_missing:
        fail(f"receipt binary is missing: {binary}")
    fail("receipt binary size changed")
launch_sha = ""
launch_mtime_ns = -1
if allow_launch_change and not launch_missing:
    launch_sha = hashlib.sha256(Path(binary).read_bytes()).hexdigest()
    launch_mtime_ns = os.stat(binary).st_mtime_ns
print(f"BIN={shlex.quote(binary)}")
print(f"HOME_DIR={shlex.quote(expected_home)}")
print(f"PID={pid}")
print(f"PORT={port}")
print(f"LAUNCH_MISSING={1 if launch_missing else 0}")
print(f"LAUNCH_BYTES={launch_bytes}")
print(f"LAUNCH_CHANGED={1 if launch_changed else 0}")
print(f"LAUNCH_SHA={shlex.quote(launch_sha)}")
print(f"LAUNCH_MTIME_NS={launch_mtime_ns}")
PY
}

start_instance() {
  [[ -n "$BIN" ]] || die "--binary is required for start"
  [[ -x "$BIN" ]] || die "binary is not executable: $BIN"
  BIN="$(canon "$BIN")"
  if [[ -e "$RECEIPT" ]]; then
    phase="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("phase",""))' "$RECEIPT" 2>/dev/null || true)"
    [[ "$phase" == "stopped" ]] || die "receipt already exists and is not stopped: $phase"
  fi
  [[ ! -e "$HOME_DIR/state/daemon.json" ]] || die "daemon state already exists under $HOME_DIR"
  mkdir -p "$HOME_DIR"
  if [[ -n "$ENV_FILE" ]]; then
    [[ -f "$ENV_FILE" ]] || die "env file missing: $ENV_FILE"
    # shellcheck disable=SC1090
    source "$ENV_FILE"
  fi
  export PIHUB_HOME="$HOME_DIR"
  export PIHUB_APP_ROOT="$(canon "$APP_ROOT")"
  WORK_DIR=""
  if [[ -n "$WORK_RAW" ]]; then
    WORK_DIR="$(canon "$WORK_RAW")"
    case "$WORK_DIR" in
      /private/tmp/pihub-rust-qa-*|/private/tmp/pihub-rust-uiqa-*) ;;
      *) die "refusing non-QA work directory: $WORK_DIR" ;;
    esac
    mkdir -p "$WORK_DIR"
  fi
  # Record the version the binary reports; only the major line is checked here,
  # while the CLI's own major guard governs cross-major behaviour.
  VERSION="$("$BIN" --version | tr -d '[:space:]')"
  [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "binary --version is not a 4-part numeric version: '$VERSION'"
  [[ "${VERSION%%.*}" == "$EXPECTED_MAJOR" ]] || die "binary version is not on the $EXPECTED_MAJOR.x line: $VERSION"
  binary_sha="$(shasum -a 256 "$BIN" | awk '{print $1}')"
  binary_bytes="$(wc -c < "$BIN" | tr -d ' ')"

  if ! "$BIN" daemon start --json >/dev/null; then
    die "daemon start failed; no stop or cleanup attempted"
  fi
  META="$HOME_DIR/state/daemon.json"
  CONTROL="$HOME_DIR/state/control.json"
  [[ -f "$META" && -f "$CONTROL" ]] || die "daemon start did not publish state files"
  chmod 600 "$CONTROL"
  PID="$(json "$META" pid)"
  PORT="$(json "$META" port)"
  PROCESS_COMMAND="$(ps -p "$PID" -o command= | sed 's/^ *//;s/ *$//')"
  PROCESS_START="$(ps -p "$PID" -o lstart= | sed 's/^ *//;s/ *$//')"
  PARENT_PID="$(ps -p "$PID" -o ppid= | tr -d ' ')"
  PGID="$(ps -p "$PID" -o pgid= | tr -d ' ')"
  [[ "$PROCESS_COMMAND" == *"__daemon --home $HOME_DIR"* ]] || die "started process does not bind $HOME_DIR"

  python3 - "$PORT" "$CONTROL" "$PID" "$VERSION" <<'PY'
import json, sys, urllib.request
port, control_path, expected_pid, expected_version = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
token = json.load(open(control_path))["token"]
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
request = urllib.request.Request(f"http://127.0.0.1:{port}/v1/health")
request.add_header("Authorization", "Bearer " + token)
with opener.open(request, timeout=8) as response:
    health = json.loads(response.read())["data"]
if health.get("pid") != expected_pid or health.get("version") != expected_version or health.get("name") != "pi-subagent-hub":
    raise SystemExit(f"health identity mismatch: {health}")
PY

  TOKEN_MODE="$(python3 -c 'import os,sys;print(oct(os.stat(sys.argv[1]).st_mode & 0o777)[2:].zfill(3))' "$CONTROL")"
  PROCESS_LINE="$(ps -p "$PID" -o pid=,ppid=,pgid=,lstart=,command= | tr -s ' ' | sed 's/^ *//;s/ *$//')"
  python3 - "$RECEIPT" "$BIN" "$binary_sha" "$binary_bytes" "$VERSION" "$HOME_DIR" "$WORK_DIR" "$PORT" "$PID" "$PARENT_PID" "$PGID" "$PROCESS_COMMAND" "$PROCESS_START" "$CONTROL" "$TOKEN_MODE" "$RECEIPT" "$REPO_ROOT" "$PROCESS_LINE" <<'PY'
import json, os, sys, datetime, tempfile
from pathlib import Path
(receipt_path, binary, binary_sha, binary_bytes, version, home, work, port, pid, ppid, pgid,
 process_command, process_start, token_file, token_mode, _, repo, process_line) = sys.argv[1:]
payload = {
  "phase": "ready",
  "kind": "uiqa-instance",
  "version": version,
  "createdAt": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00","Z"),
  "binary": {"path": binary, "sha256": binary_sha, "bytes": int(binary_bytes)},
  "home": home,
  "workDir": work or None,
  "appRoot": repo,
  "pid": int(pid), "ppid": int(ppid), "pgid": int(pgid),
  "processCommand": process_command,
  "processStart": process_start,
  "process": process_line,
  "port": int(port),
  "url": f"http://127.0.0.1:{port}/",
  "tokenFile": token_file,
  "tokenFileMode": token_mode,
  "tokenDisclosure": "read from tokenFile; never printed",
  "cleanup": {"owner": "the daemon identity above; production is never referenced"},
}
out = Path(receipt_path)
tmp = out.with_name(out.name + f".tmp-{os.getpid()}")
tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n")
os.chmod(tmp, 0o600)
os.replace(tmp, out)
print(json.dumps({"ok": True, "home": home, "pid": int(pid), "port": int(port), "receipt": receipt_path}))
PY
}

status_instance() {
  if [[ ! -f "$RECEIPT" ]]; then die "receipt missing: $RECEIPT"; fi
  if ! validation="$(validate_receipt 1)"; then
    die "status refused: identity validation failed"
  fi
  python3 - "$RECEIPT" <<'PY'
import json, sys
d=json.load(open(sys.argv[1]))
port=d.get("port")
if port is None:
    try: port=int(d["url"].split(":")[2].split("/")[0])
    except Exception: port=None
print(json.dumps({"ok": True, "phase": d["phase"], "home": d["home"], "pid": d["pid"], "port": port, "version": d["version"], "processStart": d.get("processStart") or d.get("process")}, ensure_ascii=False))
PY
}

stop_instance() {
  [[ -f "$RECEIPT" ]] || die "receipt missing: $RECEIPT"
  ALLOW_LAUNCH_CHANGE=0
  [[ -z "$STOP_CLIENT_RAW" ]] || ALLOW_LAUNCH_CHANGE=1
  if ! validation="$(validate_receipt 1 "$ALLOW_LAUNCH_CHANGE")"; then
    die "stop refused: identity validation failed; nothing was stopped"
  fi
  # shellcheck disable=SC2086
  eval "$validation"
  [[ "$HOME_DIR" == "$(canon "$HOME_RAW")" ]] || die "home changed during validation"
  # An explicit stop client must itself be trusted before it is executed: a real
  # executable, canonical path, 4-part numeric version on the supported major
  # line, with sha256/bytes recorded. When it is supplied, the receipt's launch
  # binary is never executed.
  STOP_CLIENT=""
  STOP_CLIENT_SHA=""
  STOP_CLIENT_BYTES=""
  STOP_CLIENT_VERSION=""
  if [[ -n "$STOP_CLIENT_RAW" ]]; then
    STOP_CLIENT="$(canon "$STOP_CLIENT_RAW")"
    [[ -f "$STOP_CLIENT" ]] || die "--stop-client is not a regular file: $STOP_CLIENT"
    [[ -x "$STOP_CLIENT" ]] || die "--stop-client is not executable: $STOP_CLIENT"
    STOP_CLIENT_VERSION="$("$STOP_CLIENT" --version 2>/dev/null | tr -d '[:space:]' || true)"
    [[ "$STOP_CLIENT_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] \
      || die "--stop-client --version is not a 4-part numeric version: '$STOP_CLIENT_VERSION'"
    [[ "${STOP_CLIENT_VERSION%%.*}" == "$EXPECTED_MAJOR" ]] \
      || die "--stop-client is not on the $EXPECTED_MAJOR.x line: $STOP_CLIENT_VERSION"
    STOP_CLIENT_SHA="$(shasum -a 256 "$STOP_CLIENT" | awk '{print $1}')"
    STOP_CLIENT_BYTES="$(wc -c < "$STOP_CLIENT" | tr -d ' ')"
    [[ -n "$STOP_CLIENT_SHA" && -n "$STOP_CLIENT_BYTES" ]] || die "cannot hash --stop-client: $STOP_CLIENT"
  fi
  CLIENT_BIN="${STOP_CLIENT:-$BIN}"
  # Derive the separate stop record path before the client runs, and refuse any
  # path that resolves to the receipt itself (including symlink/hardlink
  # aliases): the stop record must never overwrite the receipt it documents.
  STOP_RECEIPT=""
  if [[ -n "$STOP_CLIENT" ]]; then
    STOP_RECEIPT="$STOP_RECEIPT_RAW"
    if [[ -z "$STOP_RECEIPT" ]]; then
      case "$RECEIPT" in
        *.json) STOP_RECEIPT="${RECEIPT%.json}-stopped.json" ;;
        *) STOP_RECEIPT="$RECEIPT.stopped.json" ;;
      esac
    fi
    [[ -d "$(dirname "$STOP_RECEIPT")" ]] || die "stop receipt directory does not exist: $(dirname "$STOP_RECEIPT")"
    if same_file "$RECEIPT" "$STOP_RECEIPT"; then
      die "--stop-receipt must not be the same file as --receipt: $(canon "$STOP_RECEIPT")"
    fi
  fi
  RECEIPT_SHA_BEFORE="$(shasum -a 256 "$RECEIPT" | awk '{print $1}')"
  stop_output="$(mktemp "${TMPDIR:-/tmp}/pihub-qa-stop.XXXXXX")"
  trap 'rm -f "$stop_output"' RETURN
  set +e
  # One bounded deadline covers both the stop request and the exit confirmation.
  # The client runs in its own session, so a hung client - and only it and its
  # own descendants - can be terminated; the verified daemon PID is never
  # signalled and nothing is cleaned up on any failure or timeout.
  python3 - "$CLIENT_BIN" "$HOME_DIR" "$PID" "$STOP_DEADLINE_SECS" "$stop_output" <<'PY'
import os, signal, subprocess, sys, time
from pathlib import Path
binary, home, pid_text, deadline_text, log_path = sys.argv[1:6]
daemon_pid = int(pid_text)
daemon_state = Path(home) / "state/daemon.json"
deadline = float(deadline_text)
request_limit = deadline / 2.0

def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True

env = dict(os.environ)
env["PIHUB_HOME"] = home
log = open(log_path, "wb")
started = time.monotonic()
client = subprocess.Popen(
    [binary, "daemon", "stop", "--json"], env=env,
    stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
)
request_deadline = started + request_limit
code = None
while time.monotonic() < request_deadline:
    code = client.poll()
    if code is not None:
        break
    time.sleep(0.05)
if code is None:
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(client.pid, sig)
        except OSError:
            pass
        try:
            client.wait(timeout=2)
            break
        except subprocess.TimeoutExpired:
            continue
    log.close()
    print(f"stop request did not finish within {request_limit:g}s; terminated only the helper's stop client; no daemon kill, no cleanup attempted", file=sys.stderr)
    raise SystemExit(4)
log.close()
if code != 0:
    print(f"daemon stop failed (exit {code}); no PID killing or cleanup attempted", file=sys.stderr)
    raise SystemExit(2)
while time.monotonic() < started + deadline:
    if not alive(daemon_pid) and not daemon_state.exists():
        raise SystemExit(0)
    time.sleep(0.05)
print(f"daemon did not exit within the {deadline:g}s verified stop window; no PID killing or cleanup attempted", file=sys.stderr)
raise SystemExit(3)
PY
  stop_status=$?
  set -e
  if [[ $stop_status -ne 0 ]]; then
    cat "$stop_output" >&2
    exit "$stop_status"
  fi
  if [[ -n "$STOP_CLIENT" ]]; then
    python3 - "$RECEIPT" "$STOP_RECEIPT" "$STOP_CLIENT" "$STOP_CLIENT_SHA" "$STOP_CLIENT_BYTES" \
      "$STOP_CLIENT_VERSION" "$HOME_DIR" "$PID" "$PORT" "$LAUNCH_MISSING" "$LAUNCH_BYTES" \
      "$LAUNCH_CHANGED" "$LAUNCH_SHA" "$LAUNCH_MTIME_NS" "$BIN" "$RECEIPT_SHA_BEFORE" "$CLEANUP" \
      "$STOP_DEADLINE_SECS" <<'PY'
import datetime, hashlib, json, os, sys
from pathlib import Path
(receipt_path, stop_receipt_path, client, client_sha, client_bytes, client_version, home,
 pid, port, launch_missing, launch_bytes, launch_changed, launch_sha, launch_mtime_ns,
 launch_bin, receipt_sha_before, cleanup, deadline) = sys.argv[1:20]
original_path = Path(receipt_path)
original = json.loads(original_path.read_text())
after_sha = hashlib.sha256(original_path.read_bytes()).hexdigest()
client_stat = os.stat(client)
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")
payload = {
  "phase": "stopped",
  "kind": "uiqa-stop-receipt",
  "stoppedAt": stamp,
  "exitConfirmed": True,
  "cleanupRequested": cleanup == "1",
  "stopDeadlineSecs": float(deadline),
  "originalReceipt": {
    "path": receipt_path,
    "phase": original.get("phase"),
    "sha256Before": receipt_sha_before,
    "sha256After": after_sha,
    "unmodified": receipt_sha_before == after_sha,
    "policy": "the original receipt is preserved verbatim; this file is the separate stop record",
  },
  "home": original.get("home"),
  "daemon": {
    "pid": int(pid),
    "ppid": original.get("ppid"),
    "pgid": original.get("pgid"),
    "port": int(port),
    "version": original.get("version"),
    "processStart": original.get("processStart"),
    "processCommand": original.get("processCommand"),
    "identityRecheck": ["receipt phase/home", "daemon.json pid+port+version", "control token",
                        "ps command/ppid/pgid/start", "GET /v1/health pid+version+name"],
  },
  "launchFile": {
    "path": launch_bin,
    "recorded": {"version": original.get("version"),
                 "sha256": original.get("binary", {}).get("sha256"),
                 "bytes": original.get("binary", {}).get("bytes")},
    "observed": {"exists": launch_missing == "0", "bytes": int(launch_bytes),
                 "sha256": launch_sha or None, "mtimeNs": int(launch_mtime_ns)},
    "changed": launch_changed == "1",
    "bytesGuardExemption": "only the 'launch file bytes still equal the recorded bytes' check is waived for an explicit --stop-client; that condition is known to be false after the on-disk replacement",
    "reason": "the receipt's launch binary was replaced on disk by a newer candidate build, so the recorded launch file can no longer be executed as the stop client",
  },
  "stopClient": {
    "path": client,
    "sha256": client_sha,
    "bytes": int(client_bytes),
    "version": client_version,
    "mtimeNs": client_stat.st_mtime_ns,
    "verified": ["regular file", "executable", "canonical path", "sha256 recorded",
                 "4-part numeric version on the supported major line"],
    "invocation": "explicit PIHUB_HOME=<verified home>, bounded request+exit deadline, timeout kills only this client tree",
  },
}
out = Path(stop_receipt_path)
tmp = out.with_name(out.name + f".tmp-{os.getpid()}")
tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n")
os.chmod(tmp, 0o600)
os.replace(tmp, out)
PY
  else
  python3 - "$RECEIPT" <<'PY'
import json, os, sys, datetime
from pathlib import Path
p=Path(sys.argv[1]); d=json.loads(p.read_text())
d["phase"]="stopped"; d["stoppedAt"]=datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00","Z")
d["exitConfirmed"]=True
tmp=p.with_name(p.name+f".tmp-{os.getpid()}"); tmp.write_text(json.dumps(d,ensure_ascii=False,indent=2)+"\n"); os.chmod(tmp,0o600); os.replace(tmp,p)
PY
  fi
  if [[ $CLEANUP -eq 1 ]]; then
    python3 - "$RECEIPT" <<'PY'
import json, os, shutil, sys
from pathlib import Path
d=json.loads(Path(sys.argv[1]).read_text())
roots=[]
for key in ("home","workDir"):
    value=d.get(key)
    if not value: continue
    path=os.path.realpath(value)
    if not (path.startswith("/private/tmp/pihub-rust-qa-") or path.startswith("/private/tmp/pihub-rust-uiqa-")):
        raise SystemExit(f"refusing cleanup outside owned QA roots: {path}")
    roots.append(path)
for root in roots:
    if os.path.islink(root):
        raise SystemExit(f"refusing symlink cleanup root: {root}")
    if os.path.isdir(root):
        for base, dirs, files in os.walk(root):
            for name in dirs + files:
                target=os.path.join(base,name)
                try: os.chmod(target, 0o700 if os.path.isdir(target) else 0o600)
                except OSError: pass
        os.chmod(root,0o700)
        shutil.rmtree(root)
PY
  fi
  if [[ -n "$STOP_CLIENT" ]]; then
    python3 - "$STOP_RECEIPT" "$CLEANUP" <<'PY'
import json, sys
d=json.load(open(sys.argv[1]))
print(json.dumps({"ok": True, "home": d["home"], "pid": d["daemon"]["pid"], "stopped": True,
                  "cleanup": sys.argv[2] == "1", "stopReceipt": sys.argv[1],
                  "originalReceiptUnmodified": d["originalReceipt"]["unmodified"],
                  "stopClient": d["stopClient"]["path"],
                  "launchFileChanged": d["launchFile"]["changed"]}, ensure_ascii=False))
PY
  else
  python3 - "$RECEIPT" <<'PY'
import json, sys
d=json.load(open(sys.argv[1]))
print(json.dumps({"ok": True, "home": d["home"], "pid": d["pid"], "stopped": True, "cleanup": bool(d.get("exitConfirmed"))}, ensure_ascii=False))
PY
  fi
}

case "$command" in
  start) start_instance ;;
  status) status_instance ;;
  stop) stop_instance ;;
esac
