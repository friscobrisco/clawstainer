#!/usr/bin/env bash
# Fault test: SIGKILL the CLI at several points during pause and resume.
# The VM must always come back usable (recovered on the next command),
# with no plaintext snapshot left in its jail.
#
#   sudo HOME=/root scripts/fc-interrupt.sh [path/to/clawstainer]
C="${1:-clawstainer}"
export HOME=/root
o() { python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('status') or d.get('stdout','').strip() or d.get('message'))"; }
ID=$($C create --runtime firecracker --memory 512 --cpus 1 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")
echo "id=$ID"
$C exec $ID 'nohup bash -c "while :; do echo \$((++n)) > /dev/shm/c; sleep 0.1; done" >/dev/null 2>&1 &' >/dev/null
for delay in 0.3 1.0 1.5 2.0 2.5 3.5; do
  echo "== kill pause after ${delay}s"
  $C pause $ID >/dev/null 2>&1 & p=$!; sleep $delay; kill -9 $p 2>/dev/null; wait $p 2>/dev/null
  st=$(python3 -c "import json; print(json.load(open('/root/.clawstainer/state.json'))['machines']['$ID']['status'])")
  echo "   state=$st plaintext=$(test -e /var/lib/clawstainer/jail/firecracker/$ID/root/snapshot.work && echo LEFT || echo none)"
  if [ "$st" = paused ]; then echo "   resume: $($C resume $ID 2>&1 | o)"; fi
  echo "   exec: $($C exec $ID 'cat /dev/shm/c; timeout 5 touch /root/w && echo writable' 2>/tmp/exec.err | o | tr '\n' ' ') $(tr "\n" " " < /tmp/exec.err | cut -c1-300)"
  echo "   plaintext after: $(test -e /var/lib/clawstainer/jail/firecracker/$ID/root/snapshot.work && echo LEFT || echo none)"
done
echo "== kill resume midway"
$C pause $ID >/dev/null
$C resume $ID >/dev/null 2>&1 & p=$!; sleep 1.0; kill -9 $p 2>/dev/null; wait $p 2>/dev/null
st=$(python3 -c "import json; print(json.load(open('/root/.clawstainer/state.json'))['machines']['$ID']['status'])")
echo "   state=$st VMMs=$(pgrep -a -x firecracker | grep -c $ID)"
echo "   resume again: $($C resume $ID 2>&1 | o)"
echo "   exec: $($C exec $ID 'cat /dev/shm/c' 2>&1 | o)  VMMs=$(pgrep -a -x firecracker | grep -c $ID)"
$C destroy $ID >/dev/null; echo "destroyed; VMMs left=$(pgrep -c -x firecracker)"
