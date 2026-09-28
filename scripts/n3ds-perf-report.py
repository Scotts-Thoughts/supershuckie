#!/usr/bin/env python3
"""Summarise a stack-sample profile written by `n3ds_perf_lab --profile`.

    python scripts/n3ds-perf-report.py <profile.txt> <the lab exe> [--frames A-B,C-D]
                                       [--top 40] [--callers FUNC] [--parity odd|even]

Prints self and inclusive sample shares per function (the lab exe's own symbols from `nm`;
other modules by DLL name; addresses outside every module are generated code, `[jit]`), and
optionally who calls a given function. `--frames` keeps only samples taken while those frames
ran (the lab tags each sample with the frame being emulated).
"""
import argparse
import bisect
import collections
import os
import re
import subprocess
import sys

BINUTILS = r"C:\msys64\ucrt64\bin"


def load_symbols(exe):
    image_base = 0x140000000
    out = subprocess.run([os.path.join(BINUTILS, "objdump.exe"), "-p", exe], capture_output=True, text=True).stdout
    m = re.search(r"ImageBase\s+([0-9a-fA-F]+)", out)
    if m:
        image_base = int(m.group(1), 16)
    out = subprocess.run([os.path.join(BINUTILS, "nm.exe"), "-n", "-C", "--defined-only", exe],
                         capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
    addrs, names = [], []
    for line in out.splitlines():
        parts = line.split(" ", 2)
        if len(parts) < 3 or parts[1] not in ("T", "t"):
            continue
        addrs.append(int(parts[0], 16) - image_base)
        names.append(parts[2])
    return addrs, names


def short(name, width=110):
    # Drop argument lists and long template arguments, keep the qualified function name.
    depth, out = 0, []
    for ch in name:
        if ch in "<(":
            if depth == 0:
                out.append("<>" if ch == "<" else "()")
            depth += 1
        elif ch in ">)":
            depth = max(0, depth - 1)
        elif depth == 0:
            out.append(ch)
    s = "".join(out)
    return s if len(s) <= width else s[: width - 1] + "…"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("profile")
    ap.add_argument("exe")
    ap.add_argument("--frames", default="")
    ap.add_argument("--parity", choices=["odd", "even"])
    ap.add_argument("--top", type=int, default=40)
    ap.add_argument("--callers", default="")
    ap.add_argument("--callees", default="")
    ap.add_argument("--chain", type=int, default=0,
                    help="also list the most common first N frames of our own code (outside system DLLs)")
    args = ap.parse_args()

    ranges = []
    for part in filter(None, args.frames.split(",")):
        a, b = part.split("-")
        ranges.append((int(a), int(b)))

    modules, samples = [], []
    with open(args.profile, encoding="utf-8", errors="replace") as f:
        for line in f:
            if line.startswith("module "):
                _, start, end, path = line.rstrip("\n").split(" ", 3)
                modules.append((int(start, 16), int(end, 16), path))
                continue
            parts = line.split()
            if not parts:
                continue
            frame = int(parts[0])
            if ranges and not any(a <= frame <= b for a, b in ranges):
                continue
            if args.parity and (frame % 2 == 1) != (args.parity == "odd"):
                continue
            samples.append([int(x, 16) for x in parts[1:]])

    exe_name = os.path.basename(args.exe).lower()
    exe_mod = next((m for m in modules if os.path.basename(m[2]).lower() == exe_name), None)
    addrs, names = load_symbols(args.exe)
    modules.sort()
    mod_starts = [m[0] for m in modules]
    cache = {}

    def resolve(pc):
        r = cache.get(pc)
        if r is not None:
            return r
        i = bisect.bisect_right(mod_starts, pc) - 1
        if i < 0 or pc >= modules[i][1]:
            r = "[jit]"
        elif exe_mod and modules[i][0] == exe_mod[0]:
            j = bisect.bisect_right(addrs, pc - exe_mod[0]) - 1
            r = short(names[j]) if j >= 0 else "[exe?]"
        else:
            r = "[" + os.path.basename(modules[i][2]) + "]"
        cache[pc] = r
        return r

    chains = collections.Counter()
    total = len(samples)
    if not total:
        print("no samples")
        return
    self_c, incl_c = collections.Counter(), collections.Counter()
    callers, callees = collections.Counter(), collections.Counter()
    for stack in samples:
        fns = [resolve(pc) for pc in stack]
        self_c[fns[0]] += 1
        if args.chain:
            own = [fn for fn in fns if not (fn.startswith("[") and fn.endswith(".dll]") or fn.endswith(".DLL]"))]
            top = fns[0] if fns[0].startswith("[") else ""
            chains[(top + " <- " if top and top != (own[0] if own else "") else "") + " <- ".join(own[: args.chain])] += 1
        seen = set()
        for k, fn in enumerate(fns):
            if fn in seen:
                continue
            seen.add(fn)
            incl_c[fn] += 1
            if args.callers and args.callers in fn and k + 1 < len(fns):
                callers[fns[k + 1]] += 1
            if args.callees and args.callees in fn and k > 0:
                callees[fns[k - 1]] += 1

    print(f"{total} samples")
    print(f"\nself (top {args.top}):")
    for fn, c in self_c.most_common(args.top):
        print(f"  {100 * c / total:5.1f}%  {fn}")
    print(f"\ninclusive (top {args.top}):")
    for fn, c in incl_c.most_common(args.top):
        print(f"  {100 * c / total:5.1f}%  {fn}")
    if args.chain:
        print(f"\nfirst {args.chain} frames of own code (prefixed by the system DLL on top, if any):")
        for fn, c in chains.most_common(args.top):
            print(f"  {100 * c / total:5.1f}%  {fn}")
    if args.callers:
        print(f"\ncallers of *{args.callers}*:")
        for fn, c in callers.most_common(25):
            print(f"  {100 * c / total:5.1f}%  {fn}")
    if args.callees:
        print(f"\ncallees of *{args.callees}*:")
        for fn, c in callees.most_common(25):
            print(f"  {100 * c / total:5.1f}%  {fn}")


if __name__ == "__main__":
    sys.exit(main())
