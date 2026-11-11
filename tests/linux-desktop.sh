#!/usr/bin/env bash
# A real run of the Linux programs, without a real desk.
#
# Two pretend desktops (Xvfb displays) each get their own copy of Screen
# Portals. The copies pair with each other over 127.0.0.1, and then a
# pretend user (xdotool) pushes the mouse from the first desktop to the
# second, types, copies things, and comes back. Every step checks what
# actually happened on the displays.
#
#     bash tests/linux-desktop.sh <folder with the built programs>
#
# Needs: Xvfb, xdotool, xclip, xinput, python3, curl. Sound is checked too
# when a sound server (pulseaudio) can be started.

set -euo pipefail

BIN=$(cd "${1:-target/debug}" && pwd)
CLI="$BIN/screen-portals"
APP="$BIN/ScreenPortals"
WORK=$(mktemp -d)
A=:91 # the desk on the left, 1280x800
B=:92 # the desk on the right, 1024x768
unset WAYLAND_DISPLAY XDG_SESSION_TYPE

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

# "x y" of the mouse on a display.
where() { DISPLAY=$1 xdotool getmouselocation | sed -E 's/x:([0-9]+) y:([0-9]+).*/\1 \2/'; }
# Wait (up to about 10 s) until a command succeeds.
eventually() {
    for _ in $(seq 100); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.1
    done
    return 1
}
# Push the mouse of desktop A sideways in small steps, like a hand would.
push() {
    for _ in $(seq "$2"); do
        DISPLAY=$A xdotool mousemove_relative -- "$1" 0
        sleep 0.03
    done
}
clipboard_is() { [ "$(DISPLAY=$1 timeout 5 xclip -o -selection clipboard 2>/dev/null)" = "$2" ]; }

step "Start two pretend desktops"
Xvfb $A -screen 0 1280x800x24 -nolisten tcp >"$WORK/xvfb-a.log" 2>&1 &
Xvfb $B -screen 0 1024x768x24 -nolisten tcp >"$WORK/xvfb-b.log" 2>&1 &
eventually env DISPLAY=$A xdotool getmouselocation || fail "desktop A did not start"
eventually env DISPLAY=$B xdotool getmouselocation || fail "desktop B did not start"

step "doctor: the session is usable and the cursor lands where it is sent"
DISPLAY=$A "$CLI" doctor pointer --config-dir "$WORK/a" --port 47901 || fail "doctor"

step "A Wayland session is refused with an explanation"
if WAYLAND_DISPLAY=wayland-0 DISPLAY=$A "$CLI" doctor --config-dir "$WORK/a" >"$WORK/wayland.log" 2>&1; then
    fail "doctor accepted a Wayland session"
fi
grep -q "Wayland" "$WORK/wayland.log" || fail "no explanation about Wayland"

step "Pair the two desktops (B sits to the right of A)"
# The pretend user's input arrives through the same door as played-back
# input (XTEST), which the program normally ignores; A is told to accept it.
(yes y | DISPLAY=$A SCREEN_PORTALS_ACCEPT_XTEST=1 "$CLI" pair --side right --name desk-a \
    --config-dir "$WORK/a" --port 47901 --no-discovery >"$WORK/a.log" 2>&1) &
sleep 1
(yes y | DISPLAY=$B "$CLI" pair --name desk-b \
    --config-dir "$WORK/b" --port 47902 --no-discovery --connect 127.0.0.1:47901 >"$WORK/b.log" 2>&1) &
paired() { "$CLI" peers --config-dir "$WORK/$1" | grep -q "desk-$2"; }
eventually paired a b || fail "A never paired with B"
eventually paired b a || fail "B never paired with A"
sleep 1
DISPLAY=$B stdbuf -oL xinput test-xi2 --root >"$WORK/b-input.log" 2>&1 &

step "Copied text is shared in both directions"
printf 'hello from b' | DISPLAY=$B xclip -selection clipboard
eventually clipboard_is $A 'hello from b' || fail "text copied on B did not reach A"
printf 'hello from a' | DISPLAY=$A xclip -selection clipboard
eventually clipboard_is $B 'hello from a' || fail "text copied on A did not reach B"

step "Copy a large picture on A (it should follow the cursor)"
python3 - "$WORK/picture.png" <<'PY'
import os, struct, sys, zlib
w, h = 640, 400
rows = b"".join(b"\x00" + os.urandom(w * 3) for _ in range(h))
def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows, 1)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
PY
ls -l "$WORK/picture.png"
DISPLAY=$A xclip -selection clipboard -t image/png -i "$WORK/picture.png"
sleep 0.5

step "Push the mouse through A's right edge"
DISPLAY=$A xdotool mousemove 1200 400
DISPLAY=$B xdotool mousemove 500 100
push 20 6
sleep 0.5
read -r bx by <<<"$(where $B)"
echo "mouse on B: $bx $by"
[ "$bx" -lt 90 ] || fail "the mouse did not arrive at B's left edge (x=$bx)"
[ "$by" -gt 330 ] && [ "$by" -lt 440 ] || fail "the mouse arrived at the wrong height on B (y=$by)"

step "Moving the mouse now moves B's cursor"
push 20 10
sleep 0.3
read -r bx2 _ <<<"$(where $B)"
echo "mouse on B: $bx2"
[ "$bx2" -gt $((bx + 150)) ] || fail "B's cursor did not follow (x=$bx2 after x=$bx)"
read -r ax _ <<<"$(where $A)"
[ "$ax" -gt 400 ] && [ "$ax" -lt 900 ] || fail "A's hidden cursor is not resting near the middle (x=$ax)"

step "Typing and clicking land on B"
DISPLAY=$A xdotool key a
DISPLAY=$A xdotool click 1
sleep 0.5
grep -A4 "RawKeyPress" "$WORK/b-input.log" | grep -q "detail: 38" || fail "the A key was not pressed on B"
grep -A4 "RawButtonPress" "$WORK/b-input.log" | grep -q "detail: 1" || fail "the mouse button was not pressed on B"

step "The picture copied on A can be pasted on B"
picture_arrived() { DISPLAY=$B timeout 5 xclip -o -selection clipboard -t image/png 2>/dev/null | cmp -s - "$WORK/picture.png"; }
eventually picture_arrived || fail "the picture did not arrive intact on B"

step "Push the mouse back through B's left edge"
for _ in $(seq 40); do
    push -20 1
    read -r ax ay <<<"$(where $A)"
    # While A's cursor is away it rests hidden near the middle of A.
    if [ "$ax" -gt 1000 ]; then break; fi
done
sleep 0.5
read -r ax ay <<<"$(where $A)"
echo "mouse on A: $ax $ay"
[ "$ax" -gt 1150 ] || fail "the mouse did not come back to A's right edge (x=$ax)"
read -r before _ <<<"$(where $B)"
push -20 5
sleep 0.3
read -r after _ <<<"$(where $B)"
[ "$before" = "$after" ] || fail "B's cursor still moves although the mouse is back on A"

step "Ctrl+Alt+Home brings the mouse home from B"
DISPLAY=$A xdotool mousemove 1200 400
push 20 6
sleep 0.5
read -r bx _ <<<"$(where $B)"
[ "$bx" -lt 90 ] || fail "the mouse did not cross a second time (x=$bx)"
DISPLAY=$A xdotool key ctrl+alt+Home
sleep 0.5
read -r before _ <<<"$(where $B)"
push -20 5
sleep 0.3
read -r after _ <<<"$(where $B)"
[ "$before" = "$after" ] || fail "the escape keys did not bring the mouse home"

step "The windowed application starts, serves its window and quits cleanly"
DISPLAY=$A "$APP" --hidden --no-update --config-dir "$WORK/app" >"$WORK/app.log" 2>&1 &
app=$!
eventually test -s "$WORK/app/window.txt" || fail "the application did not start"
curl --silent --fail "$(cat "$WORK/app/window.txt")" | grep -qi "screen portals" || fail "the settings page is not served"
kill -TERM "$app"
wait "$app" || fail "the application did not exit cleanly"
[ ! -e "$WORK/app/window.txt" ] || fail "the application left its window note behind"

step "Sound"
if command -v pulseaudio >/dev/null && pulseaudio --start --exit-idle-time=-1 >"$WORK/pulse.log" 2>&1 \
    && pactl load-module module-null-sink sink_name=pretend_speakers >/dev/null \
    && pactl set-default-sink pretend_speakers; then
    DISPLAY=$A "$CLI" doctor sound --config-dir "$WORK/a" --port 47901 || fail "doctor sound"
    [ "$(pactl get-default-sink)" = "pretend_speakers" ] || fail "the sound was not given back to the speakers"
    if pactl list short sinks | grep -q screen_portals_away; then fail "the pretend output was left behind"; fi
else
    echo "skipped: no sound server could be started here"
fi

printf '\nAll checks passed.\n'
