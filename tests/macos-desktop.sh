#!/usr/bin/env bash
# A real run of the Mac programs on a Mac, without a second computer.
#
# The real Mac program pairs with a pretend computer (examples/pretend_peer)
# over 127.0.0.1. A pretend user (tests/macos-input.swift) then moves the
# Mac's mouse through the edge, types, clicks and copies, and the pretend
# computer reports what reached it. Then the pretend computer takes over the
# Mac's cursor and keyboard, and the Mac reports what arrived.
#
#     bash tests/macos-desktop.sh <folder with the built programs> <ScreenPortals.app>
#
# Needs: the macOS command-line tools (swiftc), and permission for the
# terminal it runs in to control the computer (GitHub's Mac machines have it).

set -euo pipefail

BIN=$(cd "${1:-target/release}" && pwd)
APP=${2:-}
CLI="$BIN/screen-portals"
PEER="$BIN/examples/pretend_peer"
WORK=$(mktemp -d)
INPUT="$WORK/macos-input"
MAC_PORT=47930
PEER_PORT=47931

step() { printf '\n== %s\n' "$*"; }
fail() {
    printf '\nFAILED: %s\n' "$*"
    for log in "$WORK"/*.log; do
        printf -- '--- %s\n' "$log"
        tail -n 40 "$log" || true
    done
    exit 1
}
cleanup() {
    # shellcheck disable=SC2046
    kill $(jobs -p) 2>/dev/null || true
}
trap cleanup EXIT

eventually() {
    for _ in $(seq 100); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.1
    done
    return 1
}
peer_saw() { grep -qF -- "$1" "$WORK/peer.log"; }
count() { grep -cF -- "$1" "$WORK/peer.log" || true; }
tell_peer() { echo "$*" >&3; sleep 0.3; }
mac_x() { "$INPUT" where | cut -d' ' -f1; }

step "Build the pretend user"
swiftc -O "$(dirname "$0")/macos-input.swift" -o "$INPUT"
read -r W H <<<"$("$INPUT" size)"
echo "main display ${W}x${H}, cursor at $("$INPUT" where)"

step "doctor: allowed to use the mouse, and the cursor lands where it is sent"
"$CLI" doctor pointer sound --config-dir "$WORK/mac" --port $MAC_PORT || fail "doctor"

step "Pair the Mac with a pretend computer to its right"
mkfifo "$WORK/peer.in"
"$PEER" $PEER_PORT "$WORK/peer" <"$WORK/peer.in" >"$WORK/peer.log" 2>&1 &
exec 3>"$WORK/peer.in"
eventually peer_saw LISTENING || fail "the pretend computer did not start"
(yes y | "$CLI" pair --side right --name the-mac --config-dir "$WORK/mac" --port $MAC_PORT \
    --no-discovery --connect 127.0.0.1:$PEER_PORT >"$WORK/mac.log" 2>&1) &
eventually peer_saw CONNECTED || fail "the Mac never connected to the pretend computer"
remembered() { "$CLI" peers --config-dir "$WORK/mac" | grep -q pretend-pc; }
eventually remembered || fail "the Mac did not remember the pairing"
sleep 1

step "Copied text is shared in both directions"
printf 'hello from the mac' | pbcopy
eventually peer_saw 'Clipboard("hello from the mac")' || fail "text copied on the Mac did not arrive"
tell_peer copy hello from the pc
eventually test "$(pbpaste)" = "hello from the pc" || fail "text copied on the pretend computer cannot be pasted on the Mac"

step "Copy a picture on the Mac (it should follow the cursor)"
python3 - "$WORK/picture.png" <<'PY'
import os, struct, sys, zlib
w, h = 320, 200
rows = b"".join(b"\x00" + os.urandom(w * 3) for _ in range(h))
def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows, 1)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
PY
osascript -e "set the clipboard to (read (POSIX file \"$WORK/picture.png\") as «class PNGf»)"
sleep 0.5

step "Push the mouse through the Mac's right edge"
"$INPUT" goto $((W - 60)) 300
"$INPUT" move 15 0 8
eventually peer_saw 'Abs(' || fail "the cursor did not arrive on the pretend computer"
before=$(count 'Abs(')
# Further in, away from the edge it came through.
"$INPUT" move 10 5 10
sleep 0.3
after=$(count 'Abs(')
echo "pointer updates on the pretend computer: $before then $after"
[ "$after" -gt "$before" ] || fail "moving the Mac's mouse did not move the pretend computer's cursor"

step "Typing and clicking land on the pretend computer, not on the Mac"
"$INPUT" key 0
"$INPUT" click
eventually peer_saw 'Key(4, true)' || fail "the A key did not arrive"
eventually peer_saw 'Button(1, true)' || fail "the click did not arrive"

step "The picture copied on the Mac arrived"
size=$(stat -f %z "$WORK/picture.png")
eventually peer_saw "ClipboardImage len=" || fail "no picture arrived"
grep -F "ClipboardImage len=" "$WORK/peer.log" | head -1
grep -qF "ClipboardImage len=$size" "$WORK/peer.log" || echo "note: the picture arrived re-encoded (not byte for byte)"

step "Ctrl+Alt+Home brings the cursor home"
"$INPUT" chord 73 3B 3A
sleep 0.5
x0=$(mac_x)
"$INPUT" move -20 0 5
sleep 0.3
x1=$(mac_x)
echo "the Mac's cursor: $x0 then $x1"
[ "$x1" -lt "$x0" ] || fail "the Mac's own cursor does not move after the escape keys"

step "Copy a file on the Mac, then cross again (it should follow)"
mkdir -p "$WORK/my files"
printf 'carried across\n' >"$WORK/my files/a note.txt"
osascript -e "set the clipboard to (POSIX file \"$WORK/my files/a note.txt\")"
sleep 0.5
"$INPUT" goto $((W - 60)) 300
"$INPUT" move 15 0 8
eventually peer_saw 'ClipboardFiles(' || fail "the copied file did not arrive"
# Carried files wait in a temporary folder until pasted; the pretend
# computer reports where.
copy=$(grep -F 'ClipboardFiles(' "$WORK/peer.log" | tail -1 | sed -E 's/.*\["([^"]*)".*/\1/')
echo "arrived as $copy"
[ -n "$copy" ] && cmp -s "$copy" "$WORK/my files/a note.txt" || fail "the file arrived damaged or not at all"

step "Push the mouse back through the left side of the pretend computer"
# Push left a step at a time until the cursor is back at the Mac's right
# edge (where the engine puts it on the way home).
for _ in $(seq 10); do
    "$INPUT" move -40 0 1
    sleep 0.2
    [ "$(mac_x)" -gt $((W - 100)) ] && break
done
x0=$(mac_x)
echo "the Mac's cursor came back at x=$x0"
[ "$x0" -gt $((W - 100)) ] || fail "the cursor did not come back at the Mac's right edge"
"$INPUT" move -20 0 3
sleep 0.3
x1=$(mac_x)
echo "then: $x1"
[ "$x1" -lt "$x0" ] || fail "the Mac's own cursor does not move after coming home"

step "The pretend computer takes over the Mac's cursor and keyboard"
"$INPUT" listen 6 >"$WORK/mac-input.log" 2>&1 &
sleep 0.5
tell_peer move -900 0
sleep 0.5
x0=$(mac_x)
echo "the Mac's cursor after the pretend computer crossed: $x0 $("$INPUT" where | cut -d' ' -f2)"
[ "$x0" -gt $((W - 80)) ] || fail "the pretend computer's cursor did not appear at the Mac's right edge"
tell_peer move -200 0
sleep 0.3
x1=$(mac_x)
echo "then: $x1"
[ "$x1" -lt "$x0" ] || fail "the pretend computer could not move the Mac's cursor"
tell_peer key 4 1
tell_peer key 4 0
tell_peer button 1 1
tell_peer button 1 0
sleep 1
grep -q "key 0 down" "$WORK/mac-input.log" || fail "the A key from the pretend computer did not arrive on the Mac"
grep -q "button left down" "$WORK/mac-input.log" || fail "the click from the pretend computer did not arrive on the Mac"
tell_peer move 3000 0

if [ -n "$APP" ]; then
    step "The Mac app starts, serves its window and quits cleanly"
    "$APP/Contents/MacOS/ScreenPortals" --hidden --no-update --config-dir "$WORK/app" >"$WORK/app.log" 2>&1 &
    app=$!
    eventually test -s "$WORK/app/window.txt" || fail "the app did not start"
    curl --silent --fail "$(cat "$WORK/app/window.txt")" | grep -qi "screen portals" || fail "the settings page is not served"
    kill -TERM "$app"
    wait "$app" || fail "the app did not exit cleanly"
    [ ! -e "$WORK/app/window.txt" ] || fail "the app left its window note behind"
fi

printf '\nAll checks passed.\n'
