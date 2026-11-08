#!/usr/bin/env bash
# Measure Firecracker sandbox density and lifecycle timings on this host,
# to set plan caps (see the proposal's capacity model).
#
# Starts VMs of the given size until COUNT are running (or creation fails),
# makes each one busy enough to touch some memory, then reports:
#   - create / pause / resume times (min / median / max)
#   - per-VM host memory actually used (cgroup memory.current) running
#     and the encrypted snapshot size when paused
#   - host memory and load before/after
# Everything it creates is destroyed at the end.
#
#   sudo HOME=/root scripts/fc-capacity.sh [--bin clawstainer] [--count N] [--memory MiB] [--cpus N]

set -uo pipefail

BIN=clawstainer COUNT=4 MEM=512 CPUS=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin) BIN="$2"; shift 2 ;;
    --count) COUNT="$2"; shift 2 ;;
    --memory) MEM="$2"; shift 2 ;;
    --cpus) CPUS="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 2; }

now() { date +%s.%N; }
ms() { python3 -c "print(int(($2 - $1) * 1000))"; }
stats() { python3 -c "
import statistics, sys
v = sorted(int(x) for x in sys.argv[1:])
print(f'min {v[0]:>6}  median {int(statistics.median(v)):>6}  max {v[-1]:>6}' if v else 'n/a')" "$@"; }
mem_avail() { awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo; }

echo "host: $(nproc) CPUs, $(awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo) MiB RAM, $(lscpu | awk -F: '/Model name/ {print $2; exit}' | xargs)"
echo "plan: $COUNT VMs x ${MEM} MiB, ${CPUS} vCPU"
avail_before=$(mem_avail)

ids=() creates=() pauses=() resumes=() rss=() snaps=() ballast=()
size_of() { "$BIN" exec "$1" 'wc -c < /dev/shm/ballast' 2>/dev/null \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['stdout'].strip())"; }
for ((i = 1; i <= COUNT; i++)); do
  t0=$(now)
  id=$("$BIN" create --runtime firecracker --memory "$MEM" --cpus "$CPUS" 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
  t1=$(now)
  if [[ -z "$id" ]]; then
    echo "create #$i failed; stopping at $((i - 1)) VMs"
    break
  fi
  ids+=("$id"); creates+=("$(ms "$t0" "$t1")")
  # Touch ~half the guest RAM so memory numbers reflect a working VM.
  "$BIN" exec "$id" "head -c $((MEM / 2))M /dev/zero > /dev/shm/ballast" >/dev/null
  ballast+=("$(size_of "$id")")
  rss+=("$(( $(cat /sys/fs/cgroup/clawstainer/"$id"/memory.current) / 1024 / 1024 ))")
  printf '  VM %2d %s  create %5d ms  host memory %4d MiB\n' "$i" "$id" "${creates[-1]}" "${rss[-1]}"
done
avail_running=$(mem_avail)
load_running=$(cut -d' ' -f1 /proc/loadavg)

for id in "${ids[@]}"; do
  t0=$(now); out=$("$BIN" pause "$id" 2>/dev/null); t1=$(now)
  pauses+=("$(ms "$t0" "$t1")")
  snaps+=("$(( $(echo "$out" | python3 -c "import json,sys; print(json.load(sys.stdin).get('snapshot_bytes',0))") / 1024 / 1024 ))")
done
avail_paused=$(mem_avail)
for id in "${ids[@]}"; do
  t0=$(now); "$BIN" resume "$id" >/dev/null 2>&1; t1=$(now)
  resumes+=("$(ms "$t0" "$t1")")
done
ok=0
for i in "${!ids[@]}"; do
  [[ "$(size_of "${ids[$i]}")" == "${ballast[$i]}" ]] && ok=$((ok + 1))
done

for id in "${ids[@]}"; do "$BIN" destroy "$id" >/dev/null 2>&1; done

echo
echo "VMs started:              ${#ids[@]} / $COUNT"
echo "create ms:                $(stats "${creates[@]}")"
echo "pause ms:                 $(stats "${pauses[@]}")"
echo "resume ms:                $(stats "${resumes[@]}")"
echo "host MiB per running VM:  $(stats "${rss[@]}")"
echo "snapshot MiB per VM:      $(stats "${snaps[@]}")"
echo "memory kept across pause: $ok / ${#ids[@]} VMs"
echo "host MemAvailable MiB:    before $avail_before, all running $avail_running, all paused $avail_paused"
echo "load (1 min) all running: $load_running"
