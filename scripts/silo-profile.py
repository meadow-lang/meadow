#!/usr/bin/env python3
"""Read a profile a Silo-built program took of itself.

    MEADOW_SILO_PROFILE=prof.txt ./program ...
    scripts/silo-profile.py ./program prof.txt [N] [NAMES]

The runtime writes where the program was loaded and, a line a sample, the
return addresses on the stack, innermost first (silo/src/profile.rs). This
puts them against the executable's symbols, as `nm` lists them, and prints
the N busiest (30 unless said): by the function running, by the innermost
function of the program itself (`mw.` and then what it is) with the runtime
it was calling charged to it, and by kind of cost.

NAMES, if given, is a file of `symbol name ...` lines -- what
`MEADOWBOOT_NAMES=1` prints -- which names the functions of a program
MeadowBoot compiled, whose symbols are numbers.
"""
import bisect, collections, re, subprocess, sys

exe, prof = sys.argv[1], sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
names = {}
if len(sys.argv) > 4:
    for line in open(sys.argv[4]):
        p = line.split()
        if len(p) >= 2:
            names[p[0].replace("meadow:", "meadow_").replace("/", "_")] = p[1]

syms = []
for line in subprocess.run(["nm", "-n", "--defined-only", exe], capture_output=True, text=True).stdout.splitlines():
    p = line.split()
    if len(p) == 3 and p[1] in "tTwW":
        syms.append((int(p[0], 16), p[2]))
syms.sort()
starts = [a for a, _ in syms]
lines = open(prof).read().splitlines()
base = int(lines[0].split()[1], 16)
# A position-independent executable's symbols are offsets from where it was
# loaded; a Mach-O's are from where it asked to be.
low = starts[0] if starts else 0
shift = base - (0x100000000 if low >= 0x100000000 else 0) if (low < base or low >= 0x100000000) else 0


def at(addr):
    i = bisect.bisect_right(starts, addr - shift) - 1
    return syms[i][1] if i >= 0 else "?"


def nice(x):
    x = re.sub(r"(\.\d+)+$", "", x)
    m = re.search(r"meadow_(\w+?)_([dlj]\d+)", x)
    if m:
        k = "meadow_%s_%s" % (m.group(1), m.group(2))
        return m.group(1) + "." + names[k] if k in names else re.sub(r"^mw\.[A-Z]\.\.meadow_", "", x)
    if x.startswith("mw."):
        return re.sub(r"^mw\.[A-Z]\.", "", x)
    m = re.findall(r"\d+([a-z_][A-Za-z_0-9]+)", x)
    return ":".join(m[-2:]) if x.startswith("_R") and m else x


def kind(x):
    if x.startswith("mw."):
        return "the program's code"
    if re.search(r"acquire|4step|clean|alloc|malloc|free|erase|died|build|_xzm|share", x):
        return "making and erasing blocks"
    if re.search(r"prim|global|equal|hash|text|string|array|record|st_set", x):
        return "primitives"
    if re.search(r"memmove|memcpy|copy", x):
        return "copying"
    if re.search(r"region|parcel|sched|segment|coroutine|ctx|tlv|tls", x):
        return "threads, regions, segments"
    return "other"


own, mine, kinds, n = collections.Counter(), collections.Counter(), collections.Counter(), 0
for line in lines[1:]:
    frames = [at(int(a, 16)) for a in line.split()]
    if not frames:
        continue
    n += 1
    own[nice(frames[0])] += 1
    kinds[kind(frames[0])] += 1
    for f in frames:
        if f.startswith("mw."):
            mine[nice(f)] += 1
            break
print(f"{n} samples (a millisecond of processor time each)")
print("-- by kind")
for k, v in kinds.most_common():
    print(f"{v:7d} {100 * v / max(n, 1):5.1f}%  {k}")
print("-- running")
for k, v in own.most_common(top):
    print(f"{v:7d} {100 * v / max(n, 1):5.1f}%  {k[:90]}")
print("-- the program's function it was in, or was called from")
for k, v in mine.most_common(top):
    print(f"{v:7d} {100 * v / max(n, 1):5.1f}%  {k[:90]}")
