#!/usr/bin/env bash
# TEST ENVIRONMENTS ONLY. Do not run on a real host.
#
# Apple's virtual CPU (Lima/vz with nested virtualization) exposes
# /sys/devices/system/cpu/cpu*/cache/index*/ without size or geometry
# files. Firecracker's jailer copies those files into each jail on aarch64
# and aborts when they are missing. This script mounts a tmpfs over each
# CPU's cache directory and fills in plausible values so the jailer can run.
# Real hardware (e.g. a Hetzner dedicated server) does not need it.
#
#   sudo scripts/nested-virt-cache-sysfs.sh          # apply
#   sudo scripts/nested-virt-cache-sysfs.sh --undo   # remove the overlays

set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 2; }

for cache in /sys/devices/system/cpu/cpu[0-9]*/cache; do
  if [[ "${1:-}" == "--undo" ]]; then
    mountpoint -q "$cache" && umount "$cache"
    continue
  fi
  [[ -e "$cache/index0/size" ]] && continue
  mountpoint -q "$cache" && continue
  # Keep the real level/type/shared_cpu_* values before covering them.
  declare -A keep=()
  for idx in "$cache"/index*; do
    for f in level type shared_cpu_map shared_cpu_list; do
      keep["$(basename "$idx")/$f"]=$(cat "$idx/$f")
    done
  done
  mount -t tmpfs -o size=1m,mode=0755 tmpfs "$cache"
  for key in "${!keep[@]}"; do
    mkdir -p "$cache/$(dirname "$key")"
    echo "${keep[$key]}" > "$cache/$key"
  done
  for idx in "$cache"/index*; do
    case "$(cat "$idx/level")/$(cat "$idx/type")" in
      1/Data)        size=64; ways=4 ;;
      1/Instruction) size=64; ways=4 ;;
      *)             size=4096; ways=16 ;;
    esac
    echo "${size}K" > "$idx/size"
    echo 64 > "$idx/coherency_line_size"
    echo "$ways" > "$idx/ways_of_associativity"
    echo $(( size * 1024 / 64 / ways )) > "$idx/number_of_sets"
  done
  unset keep
done
echo "cache sysfs overlays: $(mount | grep -c 'cpu/cpu[0-9]*/cache')"
