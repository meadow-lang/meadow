#!/usr/bin/env python3
"""MeadowBoot's front end against the Rust `meadow`'s, on the same work.

    scripts/bench-meadowboot.py [--meadowboot EXE] [--meadow EXE] [--runs N]
                                [--only CASE,...] [--keep DIR]

Each case is run `--runs` times (3 unless said) by each compiler, and the
fastest run is reported -- the least disturbed by whatever else the machine
was doing. Every run starts from a copy of the package in a scratch directory,
so nothing in the checkout is touched, and the standard library is the
checkout's (`MEADOWBOOT_STD`).

The cases:

  euler, minml, tour    `build`, cold (no `target/`) and after an edit of one
                        file (`target/` as the build before left it)
  std                   `build` of a package that is only `main`: what
                        compiling the standard library costs, cold
  self                  `build` of MeadowBoot itself, cold
  repl                  sixty entries typed into the prompt, each checked
                        and not run (`:t`), against `meadow repl`'s
  lsp                   the language server: a file opened, then changed and
                        its diagnostics waited for, ten times

A cold `meadow` build uses the standard library precompiled into the
binary; a cold MeadowBoot build compiles it, unless `MEADOWBOOT_PRECOMPILED_STD`
names a precompiled one. Both numbers are printed where they differ.

With `MEADOWBOOT_TIMINGS` set, MeadowBoot says how long each pass took; the
script adds them up over every unit of a case and prints them beside the
wall times (`--passes`).
"""
import argparse, collections, json, os, re, shutil, subprocess, sys, tempfile, time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
EXE = ".exe" if os.name == "nt" else ""


def args():
    p = argparse.ArgumentParser()
    p.add_argument("--meadowboot", default=os.path.join(
        ROOT, "bootstrap", "target", "release", "native", "silo", "MeadowBoot" + EXE))
    p.add_argument("--meadow", default=shutil.which("meadow") or "meadow")
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--only", default="")
    p.add_argument("--passes", action="store_true")
    p.add_argument("--keep", default=None, help="scratch directory to use and keep")
    return p.parse_args()


A = args()
SCRATCH = A.keep or tempfile.mkdtemp(prefix="bench-meadowboot-")
os.makedirs(SCRATCH, exist_ok=True)
ENV = dict(os.environ)
ENV["MEADOWBOOT_STD"] = os.path.join(ROOT, "lib", "Std")
ENV.setdefault("TMP", SCRATCH)
ENV.setdefault("TEMP", SCRATCH)
if A.passes:
    ENV["MEADOWBOOT_TIMINGS"] = "1"


def run(cmd, cwd=None, stdin=None):
    """Wall time in ms, and what it wrote to standard error."""
    t = time.perf_counter()
    r = subprocess.run(cmd, cwd=cwd, env=ENV, input=stdin, capture_output=True)
    ms = (time.perf_counter() - t) * 1000
    if r.returncode != 0:
        sys.stderr.write("failed: %s\n%s\n" % (" ".join(cmd), r.stderr.decode("utf-8", "replace")[-2000:]))
    return ms, r.stderr.decode("utf-8", "replace") + r.stdout.decode("utf-8", "replace")


def passes(text):
    """MEADOWBOOT_TIMINGS lines summed by pass over every unit."""
    total = collections.OrderedDict()
    for line in text.splitlines():
        m = re.match(r"^  (\S+)\s+([0-9.]+) ms$", line)
        if m:
            total[m.group(1)] = total.get(m.group(1), 0.0) + float(m.group(2))
    return total


def fresh(src, name):
    dst = os.path.join(SCRATCH, name)
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(src, dst, ignore=shutil.ignore_patterns("target"))
    return dst


def touched(pkg):
    main = os.path.join(pkg, "src", "Main.mw")
    with open(main, "a", encoding="utf-8") as f:
        f.write("\n-- edited %f\n" % time.time())


def best(xs):
    return min(xs, key=lambda x: x[0])


def build_case(name, src):
    rows = []
    for who, cmd in (("meadow", [A.meadow, "build"]), ("MeadowBoot", [A.meadowboot, "build"])):
        cold, warm = [], []
        for i in range(A.runs):
            pkg = fresh(src, "%s-%s" % (name, who))
            cold.append(run(cmd + [pkg]))
            touched(pkg)
            warm.append(run(cmd + [pkg]))
        rows.append((who, best(cold), best(warm)))
    return rows


def std_case():
    src = os.path.join(SCRATCH, "std-src")
    os.makedirs(os.path.join(src, "src"), exist_ok=True)
    with open(os.path.join(src, "Meadow.toml"), "w") as f:
        f.write('[package]\nname = "OnlyMain"\nversion = "0.1.0"\n')
    with open(os.path.join(src, "src", "Main.mw"), "w") as f:
        f.write("fun main () = ()\n")
    rows = []
    # The Rust `meadow` keeps its compiled library under MEADOW_HOME: an empty
    # one makes it compile the library from source, as MeadowBoot does.
    home = os.path.join(SCRATCH, "meadow-home")
    for who, cmd in (("meadow", [A.meadow, "build"]), ("MeadowBoot", [A.meadowboot, "build"])):
        runs = []
        for i in range(A.runs):
            pkg = fresh(src, "std-%s" % who)
            shutil.rmtree(home, ignore_errors=True)
            os.makedirs(home)
            ENV["MEADOW_HOME"] = home
            runs.append(run(cmd + [pkg]))
            del ENV["MEADOW_HOME"]
        rows.append((who, best(runs), None))
    return rows


def self_case():
    rows = []
    for who, cmd in (("meadow", [A.meadow, "build"]), ("MeadowBoot", [A.meadowboot, "build"])):
        runs = []
        for i in range(max(1, A.runs - 1)):
            pkg = fresh(os.path.join(ROOT, "bootstrap"), "self-%s" % who)
            # A copy outside the checkout has to find its `Meadow.lock`'s
            # neighbours by the same relative paths: there are none, every
            # dependency is fetched by git.
            runs.append(run(cmd + [pkg]))
        rows.append((who, best(runs), None))
    return rows


def repl_case():
    entries = "".join(":t [%d, 2, 3] |> map (\\x -> x * %d) |> filter (\\x -> x > 2)\n" % (i, i) for i in range(60))
    rows = []
    for who, cmd in (("meadow", [A.meadow]), ("MeadowBoot", [A.meadowboot])):
        none = best([run(cmd, stdin=b":q\n") for _ in range(A.runs)])
        some = best([run(cmd, stdin=(entries + ":q\n").encode()) for _ in range(A.runs)])
        per = (some[0] - none[0]) / 60
        rows.append((who, (per, some[1]), (none[0], none[1])))
    return rows


# --- the language server ----------------------------------------------------------

def lsp_session(cmd, pkg, edits=10):
    """ms from starting the server to the first diagnostics of the opened
    file, and the median ms from each change to its diagnostics."""
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, env=ENV)

    def send(msg):
        body = json.dumps(msg).encode()
        p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        p.stdin.flush()

    def recv():
        length = 0
        while True:
            line = p.stdout.readline()
            if not line:
                return None
            line = line.strip()
            if not line:
                break
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        return json.loads(p.stdout.read(length))

    def until_diagnostics(uri):
        while True:
            m = recv()
            if m is None:
                return False
            if m.get("method") == "textDocument/publishDiagnostics" and m["params"]["uri"] == uri:
                return True
            if "id" in m and "method" in m:
                send({"jsonrpc": "2.0", "id": m["id"], "result": None})

    main = os.path.join(pkg, "src", "Main.mw")
    uri = "file:///" + main.replace("\\", "/").lstrip("/")
    text = open(main, encoding="utf-8").read()
    t0 = time.perf_counter()
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "processId": None, "rootUri": "file:///" + pkg.replace("\\", "/").lstrip("/"), "capabilities": {}}})
    while True:
        m = recv()
        if m is None or m.get("id") == 1:
            break
    send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    send({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {"textDocument": {
        "uri": uri, "languageId": "meadow", "version": 1, "text": text}}})
    until_diagnostics(uri)
    opened = (time.perf_counter() - t0) * 1000
    times = []
    for v in range(edits):
        changed = text + "\n-- edit %d\n" % v
        t = time.perf_counter()
        send({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
            "textDocument": {"uri": uri, "version": v + 2},
            "contentChanges": [{"text": changed}]}})
        until_diagnostics(uri)
        times.append((time.perf_counter() - t) * 1000)
    send({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": None})
    send({"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
    times.sort()
    return opened, times[len(times) // 2]


def lsp_case():
    rows = []
    for who, cmd in (("meadow", [A.meadow, "lsp"]), ("MeadowBoot", [A.meadowboot, "lsp"])):
        pkg = fresh(os.path.join(ROOT, "examples", "MiniML"), "lsp-%s" % who)
        run(([A.meadow] if who == "meadow" else [A.meadowboot]) + ["build", pkg])
        opened, edit = lsp_session(cmd, pkg)
        rows.append((who, (edit, ""), (opened, "")))
    return rows


CASES = [
    ("euler", lambda: build_case("euler", os.path.join(ROOT, "examples", "Euler")), "cold build", "after an edit"),
    ("minml", lambda: build_case("minml", os.path.join(ROOT, "examples", "MiniML")), "cold build", "after an edit"),
    ("tour", lambda: build_case("tour", os.path.join(ROOT, "examples", "Tour")), "cold build", "after an edit"),
    ("std", std_case, "Std from source", None),
    ("self", self_case, "cold build", None),
    ("repl", repl_case, "per entry", "start and quit"),
    ("lsp", lsp_case, "edit to diagnostics", "open to diagnostics"),
]

only = set(x for x in A.only.split(",") if x)
print("scratch: %s" % SCRATCH)
print("%-6s %-11s %14s %14s" % ("case", "compiler", "", ""))
for name, f, first, second in CASES:
    if only and name not in only:
        continue
    for who, a, b in f():
        line = "%-6s %-11s %10.0f ms %s" % (name, who, a[0], first)
        if b is not None:
            line += "   %8.0f ms %s" % (b[0], second)
        print(line, flush=True)
        if A.passes and who == "MeadowBoot":
            for k, v in passes(a[1]).items():
                print("%-20s %-12s %8.1f ms" % ("", k, v))
if not A.keep:
    shutil.rmtree(SCRATCH, ignore_errors=True)
