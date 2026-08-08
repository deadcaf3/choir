#!/usr/bin/env python3
"""Phase-0 spike (D8): whole-directory clonefile(2) provisioning benchmark.

Finding 2026-08-08: per-file `cp -Rc` = p50 373 ms for 2000 files (misses the
<50 ms warm target); one dir-level clonefile = p50 15.4 ms (passes). Lesson:
provision workspaces with a single tree-level CoW clone (APFS clonefile here;
btrfs/ZFS snapshot on the Linux target), never per-file copies.
"""
import ctypes, os, shutil, sys, tempfile, time

files = int(sys.argv[1]) if len(sys.argv) > 1 else 2000
runs = int(sys.argv[2]) if len(sys.argv) > 2 else 20
libc = ctypes.CDLL("/usr/lib/libSystem.dylib", use_errno=True)
work = tempfile.mkdtemp()
try:
    src = os.path.join(work, "src")
    os.makedirs(src)
    for i in range(files):
        d = os.path.join(src, f"dir{i % 50}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}.dat"), "wb") as f:
            f.write(os.urandom(2048))
    times = []
    for i in range(runs):
        dst = os.path.join(work, f"ws{i}")
        t0 = time.perf_counter_ns()
        r = libc.clonefile(src.encode(), dst.encode(), 0)
        t1 = time.perf_counter_ns()
        assert r == 0, os.strerror(ctypes.get_errno())
        times.append((t1 - t0) / 1e6)
    times.sort()
    p = lambda q: times[min(len(times) - 1, int((len(times) - 1) * q))]
    print(f"dir-level clonefile, {files} files, {runs} runs: "
          f"p50={p(.5):.2f} ms p90={p(.9):.2f} ms max={times[-1]:.2f} ms")
finally:
    shutil.rmtree(work)
