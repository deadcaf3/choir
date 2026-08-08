#!/bin/bash
# Phase-0 gate measurement (D8), Linux target: Firecracker microVM
# snapshot-restore latency. Expects ~/fc/firecracker, ~/fc/vmlinux and
# ~/fc/rootfs.ext4 (see PHASE0.md). Run with access to /dev/kvm.
# Reference points from plan.md Key Finding 5: 4-28 ms restores reported.
set -euo pipefail

FC=~/fc/firecracker
KERNEL=~/fc/vmlinux
ROOTFS=~/fc/rootfs.ext4
RUNS=${1:-10}
SNAP_DIR=$(mktemp -d)
trap 'rm -rf "$SNAP_DIR"; pkill -f "firecracker --api-sock $SNAP_DIR" 2>/dev/null || true' EXIT

api() { # api <sock> <method> <path> <json>
  curl -sSf --unix-socket "$1" -X "$2" "http://localhost$3" \
    -H "Content-Type: application/json" -d "$4"
}

# 1) Boot a microVM, pause it, snapshot it.
S1="$SNAP_DIR/boot.sock"
$FC --api-sock "$S1" >/dev/null 2>&1 &
sleep 0.3
api "$S1" PUT /boot-source "{\"kernel_image_path\":\"$KERNEL\",\"boot_args\":\"console=ttyS0 reboot=k panic=1 pci=off\"}"
api "$S1" PUT /drives/rootfs "{\"drive_id\":\"rootfs\",\"path_on_host\":\"$ROOTFS\",\"is_root_device\":true,\"is_read_only\":false}"
api "$S1" PUT /machine-config '{"vcpu_count":1,"mem_size_mib":128}'
api "$S1" PUT /actions '{"action_type":"InstanceStart"}'
sleep 3  # let the guest reach userspace
api "$S1" PATCH /vm '{"state":"Paused"}'
api "$S1" PUT /snapshot/create "{\"snapshot_type\":\"Full\",\"snapshot_path\":\"$SNAP_DIR/snap.file\",\"mem_file_path\":\"$SNAP_DIR/mem.file\"}"
pkill -f "firecracker --api-sock $S1" || true
sleep 0.3
echo "snapshot created: $(du -sh "$SNAP_DIR/mem.file" | cut -f1) memory file"

# 2) Restore it RUNS times; time process spawn -> VM resumed.
times_ms=()
for i in $(seq 1 "$RUNS"); do
  SR="$SNAP_DIR/restore$i.sock"
  t0=$(date +%s%N)
  $FC --api-sock "$SR" >/dev/null 2>&1 &
  until [ -S "$SR" ]; do :; done
  api "$SR" PUT /snapshot/load "{\"snapshot_path\":\"$SNAP_DIR/snap.file\",\"mem_backend\":{\"backend_type\":\"File\",\"backend_path\":\"$SNAP_DIR/mem.file\"},\"resume_vm\":true}"
  t1=$(date +%s%N)
  times_ms+=($(( (t1 - t0) / 1000000 )))
  pkill -f "firecracker --api-sock $SR" || true
done

mapfile -t sorted < <(printf '%s\n' "${times_ms[@]}" | sort -n)
n=${#sorted[@]}
echo "firecracker snapshot restore (spawn -> resumed), $RUNS runs:"
echo "p50: ${sorted[$(( n / 2 ))]} ms"
echo "p90: ${sorted[$(( n * 9 / 10 ))]} ms"
echo "max: ${sorted[$(( n - 1 ))]} ms"
