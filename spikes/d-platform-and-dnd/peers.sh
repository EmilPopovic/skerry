#!/usr/bin/env bash
# Spike D — the drag matrix against real applications.
#
# One pair per invocation, so a failure names a peer and a direction rather than
# "drag and drop". Every pair is re-runnable, which is the only reason the matrix
# in README.md can be trusted after the code is edited.
#
# Peers come in two kinds. Most are launched and identified by their pid. A browser
# reuses a running process, so it is identified by the *window address* the
# compositor assigns — and tested against the user's real profile, which is what a
# user actually drops files onto.
#
#   ./peers.sh nautilus-wayland
#   ./peers.sh konsole-wayland
#   ./peers.sh firefox-x11
#   ./peers.sh list
set -u
cd "$(dirname "$0")"
BIN=./target/release/spike-d-platform
DEST=$(mktemp -d /tmp/skerry-spike-d-dest.XXXXXX)
# A source directory with exactly two items, so a peer opened on it has something
# grabbable near the top-left of its view and nothing else to confuse the search.
SRC=$(mktemp -d /tmp/skerry-spike-d-src.XXXXXX)
FF_PROFILE=${SPIKE_D_FF_PROFILE:-${XDG_CACHE_HOME:-$HOME/.cache}/skerry-spike-d-firefox}
printf 'one\n' > "$SRC/aaa-first.txt"
printf 'two\n' > "$SRC/bbb-second.txt"

peer_pid=""
cleanup() { [ -n "$peer_pid" ] && kill "$peer_pid" 2>/dev/null; }
trap cleanup EXIT

# hyprctl reports logical coordinates, which is what the virtual pointer takes.
rect_of_pid() {
  hyprctl clients -j | python3 -c '
import json,sys
pid=int(sys.argv[1])
for c in json.load(sys.stdin):
    if c.get("pid")==pid:
        print(*c["at"], *c["size"]); break
' "$1"
}

# Wait for a window rather than sleeping a guessed number of seconds: Firefox
# takes six seconds cold and one warm, and a fixed sleep turns that into a flake.
#
# The pid is not always enough. Firefox re-execs, so the window belongs to a
# child, and matching by window class is the only thing that finds it. The pid we
# then report is the window's own, because the drag resolves its target from that.
wait_for_window() {
  local pid=$1 klass=${2:-} before=${3:-}
  for _ in $(seq 1 100); do
    r=$(rect_of_pid "$pid")
    [ -n "$r" ] && { echo "$r $pid"; return 0; }
    if [ -n "$klass" ]; then
      r=$(hyprctl clients -j | python3 -c '
import json,sys
want, before = sys.argv[1].lower(), set(sys.argv[2].split())
for c in json.load(sys.stdin):
    if want in (c.get("class") or "").lower() and str(c["pid"]) not in before:
        print(*c["at"], *c["size"], c["pid"]); break
' "$klass" "$before")
      # Printed, not assigned: this runs inside a command substitution, so an
      # assignment here would be lost with the subshell. The caller re-reads the
      # pid from the last field.
      [ -n "$r" ] && { echo "$r"; return 0; }
    fi
    sleep 0.25
  done
  return 1
}

# Pids that already own a window of this class, so the class fallback cannot
# match a window that was open before we started — the user's own browser, for
# instance, which would turn "we dragged onto Firefox" into "we dragged onto
# whatever was already there".
# Window addresses of this class, so a peer that reuses an already-running process
# can still be identified. `firefox --new-window` opens a window whose pid was
# there before, so the pid names the browser and not the window.
addrs_of_class() {
  hyprctl clients -j | python3 -c '
import json,sys
want=sys.argv[1].lower()
print(" ".join(c["address"] for c in json.load(sys.stdin)
                if want in (c.get("class") or "").lower()))
' "$1"
}

new_addr_of_class() {
  local klass=$1 before=$2
  for _ in $(seq 1 100); do
    a=$(hyprctl clients -j | python3 -c '
import json,sys
want, before = sys.argv[1].lower(), set(sys.argv[2].split())
for c in json.load(sys.stdin):
    if want in (c.get("class") or "").lower() and c["address"] not in before:
        print(c["address"]); break
' "$klass" "$before")
    [ -n "$a" ] && { echo "$a"; return 0; }
    sleep 0.25
  done
  return 1
}

pids_of_class() {
  hyprctl clients -j | python3 -c '
import json,sys
want=sys.argv[1].lower()
print(" ".join(str(c["pid"]) for c in json.load(sys.stdin)
                if want in (c.get("class") or "").lower()))
' "$1"
}

launch() {
  local backend=$1; shift
  case $backend in
    wayland) env GDK_BACKEND=wayland MOZ_ENABLE_WAYLAND=1 QT_QPA_PLATFORM=wayland "$@" >/dev/null 2>&1 & ;;
    x11)     env GDK_BACKEND=x11 MOZ_ENABLE_WAYLAND=0 QT_QPA_PLATFORM=xcb "$@" >/dev/null 2>&1 & ;;
  esac
  peer_pid=$!
}

# A peer that already runs: open one more window in it and target that window.
run_pair_addr() {
  local label=$1 klass=$2 mode=$3; shift 3
  echo "=== $label (existing process, $mode) ==="
  local before addr
  before=$(addrs_of_class "$klass")
  "$@" >/dev/null 2>&1 &
  addr=$(new_addr_of_class "$klass" "$before") \
    || { echo "  FAIL  $label: no new window appeared"; return 1; }
  echo "  new window   $addr"
  sleep 2
  case $mode in
    wl)  timeout 60 $BIN dnd-source "addr:$addr" "$label" ;;
    x11) timeout 60 $BIN xdnd-source "addr:$addr" "$label" ;;
  esac
  local rc=$?
  echo "  exit $rc"
  echo
}

run_pair() {
  local label=$1 backend=$2 mode=$3 klass=$4; shift 4
  echo "=== $label ($backend, $mode) ==="
  local before
  before=$(pids_of_class "$klass")
  launch "$backend" "$@"
  local rect
  rect=$(wait_for_window "$peer_pid" "$klass" "$before") \
    || { echo "  FAIL  $label: no window appeared"; return 1; }
  # A fifth field means the window was found by class rather than by the pid we
  # launched — Firefox re-execs, so the window belongs to a child.
  local found_pid
  found_pid=$(echo "$rect" | awk '{print $5}')
  [ -n "$found_pid" ] && peer_pid=$found_pid
  echo "  peer window  $(echo "$rect" | awk '{print $1, $2, $3, $4}') (pid $peer_pid)"
  # The point is resolved by the spike itself, from the peer's pid, after its own
  # window exists — mapping a window under a tiling compositor moves everything
  # already on the workspace.
  sleep 1
  case $mode in
    wl)  timeout 60 $BIN dnd-source "pid:$peer_pid" "$label" ;;
    x11) timeout 60 $BIN xdnd-source "pid:$peer_pid" "$label" ;;
    in)  timeout 60 $BIN dnd-in     "pid:$peer_pid" "$label" ;;
  esac
  local rc=$?
  # What the peer did with the drop, read from the filesystem rather than believed.
  local landed
  landed=$(find "$DEST" -maxdepth 1 -name 'dragged-*' | wc -l)
  echo "  files now in the destination directory: $landed"
  echo "  exit $rc"
  echo
  kill "$peer_pid" 2>/dev/null; peer_pid=""
  sleep 1
}

case ${1:-list} in
  list)
    echo "pairs: nautilus-wayland nautilus-x11 konsole-wayland konsole-x11"
    echo "       firefox-wayland firefox-x11 designer-wayland designer-x11 self"
    echo "       nautilus-in-wayland nautilus-in-x11   (peer to us)"
    echo "       firefox-fresh                          (a --no-remote profile)"
    echo "destination directory for the file-landing check: $DEST"
    echo "source directory a peer is opened on: $SRC"
    ;;
  self)              timeout 90 $BIN dnd-pair ;;
  nautilus-wayland)  run_pair nautilus wayland wl nautilus nautilus --new-window "$DEST" ;;
  # The reverse direction. The source directory holds the fixture files, so
  # whatever the peer picks up is one of ours.
  nautilus-in-wayland) run_pair nautilus-in wayland in nautilus nautilus --new-window "$SRC" ;;
  nautilus-in-x11)     run_pair nautilus-in x11 in nautilus nautilus --new-window "$SRC" ;;
  nautilus-x11)      run_pair nautilus x11 x11 nautilus nautilus --new-window "$DEST" ;;
  konsole-wayland)   run_pair konsole wayland wl konsole konsole --workdir "$DEST" ;;
  konsole-x11)       run_pair konsole x11 x11 konsole konsole --workdir "$DEST" ;;
  # A running Firefox makes `--new-window` hand off to the existing process and
  # exit, so the pair has to start an independent instance with its own profile
  # or there is no window belonging to the pid we are watching.
  # A fresh --no-remote profile puts an onboarding page in the window, so these
  # use the running browser and the user's own profile instead.
  firefox-wayland)   run_pair_addr firefox firefox wl  firefox --new-window about:blank ;;
  firefox-x11)       run_pair_addr firefox firefox x11 env GDK_BACKEND=x11 MOZ_ENABLE_WAYLAND=0 firefox --new-window about:blank ;;
  # A profile under /tmp is refused by a confined Firefox ("Your Firefox profile
  # cannot be loaded"), so this lives under the cache directory, which the sandbox
  # allows. Override with SPIKE_D_FF_PROFILE.
  firefox-fresh)     mkdir -p "$FF_PROFILE"
                     run_pair firefox wayland wl firefox \
                       firefox --no-remote --profile "$FF_PROFILE" about:blank ;;
  designer-wayland)  run_pair designer wayland wl Designer designer6 ;;
  designer-x11)      run_pair designer x11 x11 Designer designer6 ;;
  *) echo "unknown pair: $1"; exit 2 ;;
esac
