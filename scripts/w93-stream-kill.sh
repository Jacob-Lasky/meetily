#!/usr/bin/env bash
# Live repro/verification for the Meetily audio-stream error flood (W93).
#
# Opens a real cpal/ALSA capture stream on a PRIVATE PipeWire null sink, then
# kills ONLY that test process's PipeWire objects with `pw-cli destroy`.
# DO NOT replace this with a pipewire/pipewire-pulse/wireplumber restart: that
# drops audio for every other app on the desktop (Discord, Slack, calls).
#
# Usage: scripts/w93-stream-kill.sh <test-binary> [observe-secs] [mode]
#   mode flood-stop (default): what production ships (AUTO_REOPEN off): the
#                           dead stream is torn down, ONE announcement, no
#                           reopen, bounded log lines.
#   mode recover:           the reopen machinery, which production keeps off:
#                           the sound server is "back" immediately, so the
#                           stream must reopen and resume capture.
#   mode exhaust:           the PCM vanishes too, so reopen can never work;
#                           recovery must give up after its bounded attempts.
#   mode prefix:            no recovery task (the pre-fix shape), for baselines.
# Prints per-second W93_TICK lines and a summary of log lines per minute.
set -u
BIN=${1:?path to the app_lib test binary (cargo test --lib --no-run)}
MODE=${3:-flood-stop}
# Exhaust: 1+2+4+8+16 s of backoff plus quick failed opens is ~35 s. A reopen
# that STALLS adds up to 15 s per attempt (5 x 15 + 31 = 106 s worst case); pass
# a larger observe-secs to test that.
case "$MODE" in exhaust) DEFAULT_OBSERVE=50 ;; *) DEFAULT_OBSERVE=20 ;; esac
OBSERVE=${2:-$DEFAULT_OBSERVE}
TEST=audio::stream::live_tests::live_stream_kill_flood
WORK=$(mktemp -d /tmp/w93-run.XXXXXX)
SINK=w93_sink
PCM=w93_monitor
MOD=

cleanup() {
  if [ -n "${STIM:-}" ]; then pkill -P "$STIM" 2>/dev/null; kill "$STIM" 2>/dev/null; fi
  [ -n "${TESTPID:-}" ] && kill "$TESTPID" 2>/dev/null
  [ -n "$MOD" ] && pactl unload-module "$MOD" 2>/dev/null
}
trap cleanup EXIT

# Private sink; the default sink is left alone.
DEFAULT_BEFORE=$(pactl get-default-sink)
MOD=$(pactl load-module module-null-sink sink_name=$SINK sink_properties=device.description=w93)
[ "$(pactl get-default-sink)" = "$DEFAULT_BEFORE" ] || pactl set-default-sink "$DEFAULT_BEFORE"

# Extra ALSA config layered on the system one (does not touch ~/.asoundrc).
cat > "$WORK/alsa.conf" <<ALSA
<confdir:alsa.conf>
pcm.$PCM {
  type pulse
  device "$SINK.monitor"
  hint { show on description "w93 test monitor" }
}
ALSA
export ALSA_CONFIG_PATH="$WORK/alsa.conf"

# Keep the sink busy so the monitor has samples to deliver.
# Continuous stimulus for the whole run (a single clip ends before the kill).
( while :; do paplay --device=$SINK /usr/share/sounds/alsa/Front_Center.wav >/dev/null 2>&1 || sleep 1; done ) &
STIM=$!

RECOVER=1; [ "$MODE" = prefix ] && RECOVER=0
MEETILY_W93_PCM=$PCM MEETILY_W93_OBSERVE_SECS=$OBSERVE MEETILY_W93_RECOVER=$RECOVER MEETILY_W93_EXPECT=$MODE \
  "$BIN" "$TEST" --ignored --nocapture --test-threads=1 >"$WORK/out.log" 2>&1 &
TESTPID=$!

for _ in $(seq 1 60); do grep -q W93_READY "$WORK/out.log" && break; sleep 0.5; done
grep -q W93_READY "$WORK/out.log" || { echo "stream never opened"; tail -20 "$WORK/out.log"; exit 2; }
sleep 3
PID=$(grep -o 'W93_READY pid=[0-9]*' "$WORK/out.log" | cut -d= -f2)

# Destroy ONLY the PipeWire objects owned by the test process (its stream node
# and its pipewire-PipeWire objects). That drops the test stream exactly as a
# pipewire-pulse restart drops Meetily's, without touching any other app.
IDS=$(pw-dump | python3 -c '
import json,sys
pid=sys.argv[1]
for o in json.load(sys.stdin):
    p=(o.get("info") or {}).get("props") or {}
    if o.get("type") in ("PipeWire:Interface:Node","PipeWire:Interface:Client") and \
       str(p.get("application.process.id", p.get("pipewire.sec.pid","")))==pid:
        print(o["id"])
' "$PID")
echo "test pid=$PID pipewire objects=${IDS:-none}"
[ -n "$IDS" ] || { echo "no pipewire object found for the test process"; exit 3; }
# Exhaust mode: make the device unavailable BEFORE the stream dies, so the
# first reopen attempt can never succeed.
if [ "$MODE" = exhaust ]; then
  # pipewire-pulse silently falls back to the default source for an unknown
  # device, so "device gone" is modelled by removing the PCM from ALSA's view.
  printf '<confdir:alsa.conf>\n' > "$WORK/alsa.conf"
fi
DESTROYED=0
for id in $IDS; do pw-cli destroy "$id" >/dev/null 2>&1 && DESTROYED=$((DESTROYED+1)); done
[ "$DESTROYED" -gt 0 ] || { echo "pw-cli destroy failed for every object"; exit 4; }
echo "destroyed objects at t0 (mode=$MODE); observing ${OBSERVE}s"
wait "$TESTPID"; TEST_RC=$?; TESTPID=
LINES_TOTAL=$(wc -l < "$WORK/out.log")
echo "---- ticks (every 5th) ----"; grep W93_TICK "$WORK/out.log" | awk 'NR%5==0'
echo "---- recovery lines ----"; grep -E "W93_OUTCOME|W93_STALE|STREAM_RECOVERY|requesting a stream reopen" "$WORK/out.log"
echo "---- summary ----"
echo "total log lines (whole run): $LINES_TOTAL"
echo "ERROR/WARN lines: $(grep -cE ' (ERROR|WARN) ' "$WORK/out.log")"
echo "ALSA noise lines: $(grep -c 'ALSA lib' "$WORK/out.log")"
echo "'Too many' lines: $(grep -c 'Too many' "$WORK/out.log")"
echo "log kept at $WORK/out.log (delete when done)"
echo "observed seconds: $OBSERVE (lines/min ~= total * 60 / $((OBSERVE+4)))"
if [ "$TEST_RC" -eq 0 ]; then echo "RESULT: PASS (mode=$MODE)"; else echo "RESULT: FAIL (mode=$MODE, test exit $TEST_RC; see $WORK/out.log)"; exit 1; fi
