#!/usr/bin/env bash
# End-to-end smoke test for the Firecracker runtime on a KVM host.
#
# Creates a microVM, exercises exec / streaming / timeouts / networking /
# isolation / shell, destroys it, and checks that nothing leaked (VMM
# processes, TAP devices, nftables state, machine directories, state
# entries). Repeat with --loop N for a basic soak test.
#
#   sudo scripts/fc-smoke.sh [--bin path/to/clawstainer] [--loop N]
#
# Needs root, /dev/kvm, nft, and a clawstainer binary built with
# `--features firecracker`.

set -uo pipefail

BIN=clawstainer
LOOPS=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin) BIN="$2"; shift 2 ;;
    --loop) LOOPS="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 2; }
[[ -c /dev/kvm ]] || { echo "/dev/kvm missing" >&2; exit 2; }

STATE="${HOME}/.clawstainer/state.json"
FAILS=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; FAILS=$((FAILS + 1)); }
check() { local name="$1"; shift; if "$@"; then pass "$name"; else fail "$name"; fi; }

# Run a command in the VM and print one JSON field (default: stdout).
vm() {
  local id="$1" cmd="$2" field="${3:-stdout}"
  "$BIN" exec "$id" "$cmd" | python3 -c "import json,sys; d=json.load(sys.stdin); v=d.get('$field'); print(v if v is not None else '', end='')"
}

leak_check() {
  check "no firecracker processes" bash -c '! pgrep -x firecracker >/dev/null'
  check "no tap devices" bash -c '! ip -br link | grep -q "^tap-"'
  check "no nftables tables" bash -c '! nft list tables | grep -q clawstainer'
  check "no jails" bash -c '[[ -z "$(ls -A /var/lib/clawstainer/jail/firecracker 2>/dev/null)" ]]'
  check "no snapshots" bash -c '[[ -z "$(ls -A /var/lib/clawstainer/snapshots 2>/dev/null)" ]]'
  check "no VM cgroups" bash -c '[[ -z "$(find /sys/fs/cgroup/clawstainer -mindepth 1 -maxdepth 1 -type d 2>/dev/null)" ]]'
  check "no machines in state" python3 -c "
import json, os, sys
p = '$STATE'
s = json.load(open(p)) if os.path.exists(p) else {'machines': {}, 'network': {}}
sys.exit(1 if s['machines'] or s['network'].get('vm_slots') else 0)"
}

for ((i = 1; i <= LOOPS; i++)); do
  echo "== round $i/$LOOPS"
  start=$(date +%s.%N)
  err=$(mktemp)
  out=$("$BIN" create --runtime firecracker --memory 512 --cpus 1 2>"$err")
  id=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['id'])" "$out" 2>/dev/null)
  if [[ -z "$id" ]]; then
    fail "create: $out $(cat "$err")"
    rm -f "$err"
    continue
  fi
  rm -f "$err"
  printf '  ok    create %s (%.1fs)\n' "$id" "$(echo "$(date +%s.%N) - $start" | bc)"

  # The VMM runs jailed: own uid, no capabilities, own cgroup, seccomp.
  pid=$(pgrep -a -x firecracker | awk -v id="$id" '$0 ~ id {print $1; exit}')
  check "VMM is not root" test "$(stat -c %u /proc/"$pid")" -ge 400000
  check "VMM has no capabilities" grep -q '^CapEff:\s*0000000000000000$' /proc/"$pid"/status
  # The jailer pivot_roots in its own mount namespace, so check what the
  # VMM can see rather than the link text.
  check "VMM sees only its jail" bash -c "test -e /proc/$pid/root/rootfs.ext4 && test ! -e /proc/$pid/root/etc/passwd && test ! -e /proc/$pid/root/root/.clawstainer"
  check "VMM in its own cgroup" grep -q "^0::/clawstainer/$id\$" /proc/"$pid"/cgroup
  check "VMM memory limit set" test "$(cat /sys/fs/cgroup/clawstainer/"$id"/memory.max)" != "max"
  check "seccomp on VMM threads" bash -c "grep -h '^Seccomp:' /proc/$pid/task/*/status | grep -q 'Seccomp:\s*2'"

  check "exec stdout" test "$(vm "$id" 'echo hello')" = "hello"
  check "exec exit code" test "$(vm "$id" 'exit 7' exit_code)" = "7"
  check "exec stderr" test "$(vm "$id" 'echo err >&2' stderr)" = "err"
  check "exec truncates at 1 MiB" test "$(vm "$id" 'head -c 3000000 /dev/zero' total_bytes)" = "3000000"
  check "exec timeout" test "$("$BIN" exec --timeout 1 "$id" 'sleep 20' | python3 -c 'import json,sys; print(json.load(sys.stdin)["timed_out"])')" = "True"
  addr=$(vm "$id" 'ip -4 -br addr show eth0')
  check "guest has its address" test "${addr/10.200./}" != "$addr"
  check "dns resolves" test -n "$(vm "$id" 'getent hosts example.com')"
  check "egress works" test "$(vm "$id" 'timeout 5 bash -c "</dev/tcp/1.1.1.1/443" && echo ok')" = "ok"
  check "host services blocked" test "$(vm "$id" 'timeout 3 bash -c "</dev/tcp/$(ip route | awk "/default/{print \$3}")/22" 2>/dev/null && echo open || echo blocked')" = "blocked"
  check "metadata blocked" test "$(vm "$id" 'timeout 3 bash -c "</dev/tcp/169.254.169.254/80" 2>/dev/null && echo open || echo blocked')" = "blocked"
  check "smtp blocked" test "$(vm "$id" 'timeout 3 bash -c "</dev/tcp/1.1.1.1/25" 2>/dev/null && echo open || echo blocked')" = "blocked"
  check "shell has a pty" bash -c "printf 'tty\nexit\n' | '$BIN' shell $id 2>/dev/null | grep -q /dev/pts/"
  check "status keeps the pid" python3 -c "
import json, sys; m = json.load(open('$STATE'))['machines']['$id']; sys.exit(0 if m['pid'] else 1)"

  # Pause/resume: a background process must survive with its memory.
  vm "$id" 'nohup bash -c "while :; do echo \$((++n)) > /dev/shm/count; sleep 0.1; done" >/dev/null 2>&1 & echo $! > /dev/shm/pid' >/dev/null
  sleep 1
  before_pid=$(vm "$id" 'cat /dev/shm/pid')
  check "pause" bash -c "'$BIN' pause $id >/dev/null"
  check "no VMM while paused" bash -c "! pgrep -a -x firecracker | grep -q $id"
  check "snapshot is encrypted at rest" bash -c "head -c 8 /var/lib/clawstainer/snapshots/$id/memory.snap | grep -q CLAWSNP1"
  check "snapshot outside the jail, root-only" test "$(stat -c '%u %a' /var/lib/clawstainer/snapshots/$id/memory.snap)" = "0 600"
  check "no plaintext snapshot left" test ! -e "/var/lib/clawstainer/jail/firecracker/$id/root/snapshot.work"
  check "exec refused while paused" bash -c "! '$BIN' exec $id true >/dev/null 2>&1"

  # A corrupted snapshot must fail to resume and leave the VM paused.
  snap="/var/lib/clawstainer/snapshots/$id/memory.snap"
  cp "$snap" "$snap.bak"
  printf '\xff' | dd of="$snap" bs=1 seek=4096 conv=notrunc status=none
  check "corrupt snapshot refused" bash -c "! '$BIN' resume $id >/dev/null 2>&1"
  check "still paused after failed resume" python3 -c "
import json, sys; sys.exit(0 if json.load(open('$STATE'))['machines']['$id']['status'] == 'paused' else 1)"
  check "no VMM after failed resume" bash -c "! pgrep -a -x firecracker | grep -q $id"
  mv "$snap.bak" "$snap"

  check "resume" bash -c "'$BIN' resume $id >/dev/null"
  check "process survived pause" test "$(vm "$id" 'kill -0 $(cat /dev/shm/pid) && cat /dev/shm/pid')" = "$before_pid"
  c1=$(vm "$id" 'cat /dev/shm/count'); sleep 0.5; c2=$(vm "$id" 'cat /dev/shm/count')
  check "process keeps running after resume" test "$c2" -gt "$c1"
  guest_t=$(vm "$id" 'date +%s'); host_t=$(date +%s)
  check "guest clock resynced" test $(( guest_t > host_t ? guest_t - host_t : host_t - guest_t )) -le 2
  check "network after resume" test "$(vm "$id" 'timeout 5 bash -c "</dev/tcp/1.1.1.1/443" && echo ok')" = "ok"
  check "snapshot removed after resume" test ! -e "$snap"

  check "destroy" bash -c "'$BIN' destroy $id >/dev/null"
  leak_check
done

echo
if ((FAILS)); then
  echo "$FAILS check(s) failed"
  exit 1
fi
echo "all checks passed"
