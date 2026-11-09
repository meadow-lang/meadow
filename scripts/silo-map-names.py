#!/usr/bin/env python3
"""Name the offsets a Silo program's report of how it died gives, on Windows.

    python scripts/silo-map-names.py Program.map < report.txt

The map is the one the MSVC linker writes with `/MAP` (see `docs/RUNTIME.md`,
"A native program that dies on Windows"); each offset in the executable the
report prints, `MEADOW_SILO_OVERFLOW_TRACE`'s, is given the symbol it falls in
and how far into it.
"""
import bisect
import re
import sys

base = None
syms = []
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    m = re.match(r"\s*Preferred load address is ([0-9a-fA-F]+)", line)
    if m:
        base = int(m.group(1), 16)
        continue
    m = re.match(r"\s*[0-9a-fA-F]{4}:[0-9a-fA-F]{8}\s+(\S+)\s+([0-9a-fA-F]{16})\s", line)
    if m and base is not None:
        syms.append((int(m.group(2), 16) - base, m.group(1)))
syms.sort()
addrs = [a for a, _ in syms]


def name(off):
    i = bisect.bisect_right(addrs, off) - 1
    if i < 0:
        return "?"
    return f"{syms[i][1]}+{off - syms[i][0]:#x}"


for line in sys.stdin:
    line = line.rstrip("\n")
    m = re.search(r"at offset 0x([0-9a-f]+)", line) or re.match(r"0x([0-9a-f]+)$", line)
    if m:
        print(f"{line}  {name(int(m.group(1), 16))}")
    else:
        print(line)
