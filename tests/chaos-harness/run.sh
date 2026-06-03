#!/bin/bash
# Chaos harness: N iterations of `eo9 -c "cat /notes.txt"` under the seeded chaos layer,
# with a per-run watchdog. No CPU hogs — the chaos feature supplies the perturbation.
# See docs/spikes/timing-strategies.md §4.1 and the escalation ladder in §6.
#
# Usage: BIN=path/to/eo9 ITERS=100 BASE_SEED=1 TIMEOUT_S=30 tests/chaos-harness/run.sh
# A hang prints the iteration and seed (replay: EO9_CHAOS_SEED=<seed>) and samples the
# process before killing it. Exit code: number of hangs (0 = clean).
set -u
BIN="${BIN:?set BIN to the eo9 binary (built with --features chaos)}"
ITERS="${ITERS:-100}"
BASE_SEED="${BASE_SEED:-1}"
TIMEOUT_S="${TIMEOUT_S:-30}"
WORK="${WORK:-$(mktemp -d /tmp/eo9-chaos.XXXXXX)}"

mkdir -p "$WORK/fsroot"
echo "chaos says hello" > "$WORK/fsroot/notes.txt"
export EO9_STORE="$WORK/store"
# Prime the store once (seeding is idempotent; keeps iteration timing uniform).
# The priming run gets the same watchdog as iterations: a pre-fix binary under chaos
# can hang HERE too — it did, the first time this harness ran (see the spike doc).
EO9_CHAOS_SLEEP_PCT=0 "$BIN" --fs-root "$WORK/fsroot" -c "cat /notes.txt" >/dev/null 2>&1 &
prime=$!
pt=0
while [ "$pt" -lt $((TIMEOUT_S * 4)) ]; do
  kill -0 "$prime" 2>/dev/null || break
  sleep 0.25
  pt=$((pt + 1))
done
kill -9 "$prime" 2>/dev/null; wait "$prime" 2>/dev/null

hangs=0
for i in $(seq 1 "$ITERS"); do
  seed=$((BASE_SEED + i))
  out="$WORK/out.txt"
  EO9_CHAOS_SEED=$seed "$BIN" --fs-root "$WORK/fsroot" -c "cat /notes.txt" >"$out" 2>&1 &
  pid=$!
  ticks=$((TIMEOUT_S * 4))
  waited=0
  hung=1
  while [ "$waited" -lt "$ticks" ]; do
    if ! kill -0 "$pid" 2>/dev/null; then hung=0; break; fi
    sleep 0.25
    waited=$((waited + 1))
  done
  if [ "$hung" -eq 1 ]; then
    hangs=$((hangs + 1))
    echo "HANG iteration=$i seed=$seed (replay with EO9_CHAOS_SEED=$seed)"
    sample "$pid" 2 2>/dev/null | head -40 > "$WORK/hang-$i-$seed.sample" || true
    kill -9 "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
  else
    wait "$pid" 2>/dev/null
    if ! grep -q "chaos says hello" "$out"; then
      echo "WRONG-OUTPUT iteration=$i seed=$seed:"; cat "$out"
    fi
  fi
done
echo "done: $hangs hang(s) / $ITERS iterations (work dir: $WORK)"
exit "$hangs"
