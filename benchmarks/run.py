#!/usr/bin/env python3
"""Run the cross-language benchmark suite and print a table.

    ./run.py                        every task, every language whose toolchain is here
    ./run.py --task fib --task matmul
    ./run.py --lang meadow --lang rust --lang c
    ./run.py --reps 10              more repetitions; the minimum is reported
    ./run.py --list                 what is here, and what is missing

Each program prints exactly one line: a checksum of what it computed. The
harness checks that every language agrees on it before reporting any timing,
because a benchmark that is quietly computing something else is not a
benchmark. A task whose languages disagree is reported and not timed.

Timing is wall clock for the whole process, startup included. That is unfair
to the runtimes that have one, so `--list` and the table both carry a
`startup` row: the same measurement for a program that only prints, which is
the floor each language cannot go below.
"""

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
TASKS_DIR = HERE / "tasks"
WORK = HERE / "work"
REPO = HERE.parent

# What an executable is called here.
EXE = ".exe" if os.name == "nt" else ""
# The C compiler, and the Python: `cc` and `python3` are the Unix names, and
# neither is what a Windows box has. `CC` overrides.
CC = os.environ.get("CC") or ("clang" if os.name == "nt" else "cc")
CXX = os.environ.get("CXX") or ("clang++" if os.name == "nt" else "c++")
PYTHON = "py" if os.name == "nt" and shutil.which("py") else "python3"

# Built once by `meadow_build`; the debug compiler is slow enough to notice.
MEADOW = REPO / "buildtools" / "target" / "release" / f"meadow{EXE}"


# --- the tasks ----------------------------------------------------------------
#
# `threads` says whether the task is meant to use more than one core; it is
# only used to label the table, since a task's sources decide for themselves.

TASKS = [
    ("fib", False, "recursive calls, 64-bit integers, no allocation"),
    ("binarytrees", False, "allocation and collection: build, walk, discard"),
    ("matmul", False, "float arrays and three nested loops"),
    ("wordfreq", False, "strings, a hash map, and a sort"),
    ("rbtree", False, "a functional red-black tree nobody else holds: reuse"),
    ("mandelbrot", True, "data parallelism over a grid of float work"),
    ("contention", True, "many threads incrementing shared state"),
    ("pipeline", True, "message passing: producers, a queue, a consumer"),
]

# The same tasks with the allocator taken out: every language builds into one
# flat block of nodes or slots, indexed rather than pointed at, and throws the
# block away whole. `fib` allocates nothing and `matmul` is flat arrays
# already, so those two are the same programs -- which is the answer for them,
# and `SAME_AS` says so rather than a copy of the file pretending otherwise.
ARENA = [
    ("fib_arena", False, "nothing to arena: the same program as `fib`"),
    ("binarytrees_arena", False, "the trees, built into one flat block of nodes"),
    ("matmul_arena", False, "nothing to arena: the same program as `matmul`"),
    ("wordfreq_arena", False, "words as offsets into the corpus, table in flat arrays"),
    ("binarytrees_compact", False, "Meadow only: the long-lived tree in a compact region"),
]

SAME_AS = {"fib_arena": "fib", "matmul_arena": "matmul"}

TASK_BY_NAME = {name: (par, why) for name, par, why in TASKS + ARENA}


def sources_of(task):
    """The task whose programs `task` is run from: itself, unless it has none
    of its own."""
    return SAME_AS.get(task, task)


# --- the languages ------------------------------------------------------------


class Lang:
    """How to build and run one language's programs.

    `build` returns the argv to run, or None when the language needs no build
    step; `run` returns the argv that runs the result.
    """

    def __init__(self, name, ext, tool, build, run, stem=lambda t: t, note=""):
        self.name = name
        self.ext = ext
        self.tool = tool
        self._build = build
        self._run = run
        self.stem = stem
        self.note = note

    def source(self, task):
        task = sources_of(task)
        return TASKS_DIR / task / f"{self.stem(task)}{self.ext}"

    def out_dir(self, task):
        return WORK / self.name / task

    def available(self):
        return shutil.which(self.tool) is not None

    def build(self, task):
        return self._build(self, task)

    def run(self, task):
        return self._run(self, task)


def cap(s):
    return s[:1].upper() + s[1:]


def camel(s):
    """A Meadow package is named where a module is: `BinarytreesArena`, not
    `Binarytrees_arena`."""
    return "".join(cap(part) for part in s.split("_"))


def meadow_package(lang, task):
    """A Meadow benchmark is one file; a build that emits an executable needs a
    package. So the file becomes `src/Main.mw` of a package made here."""
    out = lang.out_dir(task)
    (out / "src").mkdir(parents=True, exist_ok=True)
    (out / "Meadow.toml").write_text(
        f'[package]\nname = "{camel(task)}"\nversion = "0.1.0"\n'
    )
    shutil.copyfile(lang.source(task), out / "src" / "Main.mw")
    build = [str(MEADOW), "build", "--release", "--emit", "exe", str(out)]
    if lang.name.endswith("-silo"):
        build += ["--runtime", "silo"]
    return build


def native_exe(lang, task):
    native = lang.out_dir(task) / "target" / "release" / "native"
    if lang.name.endswith("-silo"):
        native = native / "silo"
    return [str(native / f"{camel(task)}{EXE}")]


def simple(argv):
    """A build that writes `out` into the task's work directory."""

    def go(lang, task):
        lang.out_dir(task).mkdir(parents=True, exist_ok=True)
        return [
            a.format(
                src=lang.source(task),
                out=lang.out_dir(task) / f"out{EXE}",
                objs=lang.out_dir(task) / "objs",
            )
            for a in argv
        ]

    return go


def run_out(lang, task):
    return [str(lang.out_dir(task) / f"out{EXE}")]


def interpreted(argv):
    def go(lang, task):
        return [a.format(src=lang.source(task)) for a in argv]

    return go


def ocaml_build(lang, task):
    """Compile a copy of the source inside `work/`.

    `ocamlopt` writes `.cmi`, `.cmx` and `.o` beside the *source file*, and no
    combination of `-I` and `-o` moves them. Compiling a copy is what keeps the
    task directories holding nothing but the programs.
    """
    out = lang.out_dir(task)
    out.mkdir(parents=True, exist_ok=True)
    src = out / lang.source(task).name
    shutil.copyfile(lang.source(task), src)
    return [
        "ocamlfind", "ocamlopt", "-package", "unix", "-linkpkg", "-O3",
        "-w", "-a", "-o", str(out / "out"), str(src),
    ]


def java_build(lang, task):
    out = lang.out_dir(task)
    out.mkdir(parents=True, exist_ok=True)
    return ["javac", "-d", str(out), str(lang.source(task))]


def java_run(lang, task):
    return ["java", "-cp", str(lang.out_dir(task)), cap(sources_of(task))]


LANGS = [
    Lang(
        "meadow", ".mw", str(MEADOW), meadow_package, native_exe,
        note="`meadow build --release`: -O2, compiled ahead of time",
    ),
    Lang(
        "meadow-silo", ".mw", str(MEADOW), meadow_package, native_exe,
        stem=lambda t: t,
        note="`meadow build --release --runtime silo`: the runtime of its own, "
             "counted by reference, on the native stack",
    ),
    Lang(
        "rust", ".rs", "rustc",
        simple(["rustc", "-C", "opt-level=3", "-o", "{out}", "{src}"]), run_out,
        note="`-C opt-level=3`, what `cargo build --release` uses",
    ),
    Lang(
        "c", ".c", CC,
        simple(
            [CC, "-O3", "-ffp-contract=off", "-o", "{out}", "{src}"]
            + ([] if os.name == "nt" else ["-lpthread", "-lm"])
        ),
        run_out,
        note="`-O3 -ffp-contract=off`",
    ),
    Lang(
        "cpp", ".cpp", CXX,
        simple(
            [CXX, "-O3", "-std=c++17", "-o", "{out}", "{src}"]
            # clang 15 against a newer MSVC STL, which asks for clang 16
            # but compiles this fine.
            + (["-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH"] if os.name == "nt" else [])
        ),
        run_out,
        note="`-O3 -std=c++17`",
    ),
    Lang(
        "go", ".go", "go",
        simple(["go", "build", "-o", "{out}", "{src}"]), run_out,
        note="`go build`",
    ),
    Lang(
        "haskell", ".hs", "ghc",
        simple([
            "ghc", "-O2", "-threaded", "-rtsopts", "-with-rtsopts=-N",
            "-outputdir", "{objs}", "-o", "{out}", "{src}",
        ]), run_out,
        note="`ghc -O2 -threaded -with-rtsopts=-N`",
    ),
    Lang(
        "java", ".java", "javac", java_build, java_run, stem=cap,
        note="`javac`, default JVM settings",
    ),
    Lang(
        "ocaml", ".ml", "ocamlfind", ocaml_build, run_out,
        note="`ocamlopt -O3`, native code",
    ),
    Lang(
        "mlton", ".sml", "mlton",
        simple(["mlton", "-output", "{out}", "{src}"]), run_out,
        note="`mlton`, whole-program compilation",
    ),
    Lang(
        "koka", ".kk", "koka",
        simple([
            "koka", "-O2", "--no-debug", "--builddir", "{objs}",
            "-o", "{out}", "{src}",
        ]), run_out,
        note="`koka -O2`, Perceus reference counting",
    ),
    Lang(
        "python", ".py", PYTHON, lambda l, t: None,
        interpreted([PYTHON, "{src}"]),
        note="CPython, no flags",
    ),
    Lang(
        "js", ".js", "node", lambda l, t: None, interpreted(["node", "{src}"]),
        note="Node, no flags",
    ),
]

LANG_BY_NAME = {l.name: l for l in LANGS}


# --- running ------------------------------------------------------------------


def measure(argv, cwd, reps):
    """Run `argv` `reps` times. Returns (checksum, seconds) with the shortest
    run's time, or (None, error) if it failed.

    The minimum rather than the mean: the work is deterministic, so the spread
    is the operating system and the shortest run is the one it interfered with
    least.
    """
    best = None
    out = None
    for _ in range(reps):
        start = time.perf_counter()
        try:
            p = subprocess.run(
                argv, cwd=cwd, capture_output=True, text=True, timeout=600
            )
        except subprocess.TimeoutExpired:
            return None, "timed out after 600s"
        except OSError as e:
            return None, str(e)
        elapsed = time.perf_counter() - start
        if p.returncode != 0:
            tail = (p.stderr or p.stdout).strip().splitlines()
            return None, tail[-1] if tail else f"exit {p.returncode}"
        line = p.stdout.strip()
        if out is None:
            out = line
        elif out != line:
            return None, f"not deterministic: {out!r} then {line!r}"
        best = elapsed if best is None else min(best, elapsed)
    return out, best


STARTUP = {
    "meadow-silo": 'use Std.Console (println)\n\nfun main () = println "0"\n',
    "meadow": 'use Std.Console (println)\n\nfun main () = println "0"\n',
    "rust": 'fn main() { println!("0"); }\n',
    "c": '#include <stdio.h>\nint main(void) { printf("0\\n"); return 0; }\n',
    "cpp": '#include <cstdio>\nint main() { std::printf("0\\n"); }\n',
    "go": 'package main\n\nimport "fmt"\n\nfunc main() { fmt.Println(0) }\n',
    "haskell": "main :: IO ()\nmain = putStrLn \"0\"\n",
    "ocaml": 'let () = print_endline "0"\n',
    "mlton": 'val () = print "0\\n"\n',
    "koka": 'fun main()\n  println("0")\n',
    "java": 'public class Startup { public static void main(String[] a) { System.out.println(0); } }\n',
    "python": 'print(0)\n',
    "js": 'console.log(0);\n',
}


def startup_floor(lang, reps):
    """What the language costs before the benchmark has done anything: the same
    measurement, for a program that only prints."""
    task = "startup"
    src = TASKS_DIR / task
    src.mkdir(parents=True, exist_ok=True)
    text = STARTUP.get(lang.name)
    if text is None:
        return None
    path = src / f"{lang.stem(task)}{lang.ext}"
    if not path.exists() or path.read_text() != text:
        path.write_text(text)
    build = lang.build(task)
    if build and subprocess.run(build, capture_output=True).returncode != 0:
        return None
    _, secs = measure(lang.run(task), HERE, reps)
    return secs if isinstance(secs, float) else None


# --- reporting ----------------------------------------------------------------


def fmt(seconds):
    if seconds is None:
        return "—"
    if seconds < 1:
        return f"{seconds * 1000:.0f}ms"
    return f"{seconds:.2f}s"


def table(results, langs, tasks, floors):
    """A markdown table: a row per task, a column per language, times relative
    to the fastest in that row underneath."""
    head = "| task | " + " | ".join(l.name for l in langs) + " |"
    rule = "|---" * (len(langs) + 1) + "|"
    rows = [head, rule]
    for task in tasks:
        cells = []
        times = [results.get((task, l.name), (None, None))[1] for l in langs]
        ok = [t for t in times if isinstance(t, float)]
        fastest = min(ok) if ok else None
        for l, t in zip(langs, times):
            if not isinstance(t, float):
                note = results.get((task, l.name), (None, None))[1]
                cells.append("—" if note is None else "✗")
            elif fastest and t > fastest:
                cells.append(f"{fmt(t)} ({t / fastest:.1f}×)")
            else:
                cells.append(f"**{fmt(t)}**")
        par = "⇉ " if TASK_BY_NAME.get(task, (False,))[0] else ""
        rows.append(f"| {par}{task} | " + " | ".join(cells) + " |")
    rows.append(
        "| _startup_ | "
        + " | ".join(fmt(floors.get(l.name)) for l in langs)
        + " |"
    )
    return "\n".join(rows)


def main():
    # The table has arrows in it, and a Windows console is not UTF-8
    # unless it is told.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--task", action="append", default=[])
    ap.add_argument("--lang", action="append", default=[])
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--json", type=Path, help="also write the raw numbers here")
    ap.add_argument("--no-startup", action="store_true")
    ap.add_argument(
        "--arena",
        action="store_true",
        help="the arena tasks instead: the same work with the allocator taken out",
    )
    args = ap.parse_args()

    langs = [LANG_BY_NAME[n] for n in args.lang] if args.lang else LANGS
    every = ARENA if args.arena else TASKS
    tasks = args.task or [t for t, _, _ in every]

    missing = [l for l in langs if not l.available()]
    langs = [l for l in langs if l.available()]

    if args.list:
        print("tasks")
        for name, par, why in TASKS + ARENA:
            mark = "⇉" if par else " "
            print(f"  {mark} {name:<12} {why}")
        print("\nlanguages")
        for l in LANGS:
            have = "  " if l.available() else "✗ "
            print(f"  {have}{l.name:<8} {l.note}")
        if missing:
            print(
                "\nnot installed: "
                + ", ".join(f"{l.name} (`{l.tool}`)" for l in missing)
            )
        return 0

    if MEADOW.exists() is False and any(l.name == "meadow" for l in langs):
        print(f"meadow: no release build at {MEADOW}", file=sys.stderr)
        print("  cd buildtools && cargo build --release -p meadow", file=sys.stderr)
        return 2

    print(f"{platform.platform()}, {os.cpu_count()} cores, min of {args.reps}\n")
    if missing:
        print("skipping (not installed): " + ", ".join(l.name for l in missing) + "\n")

    if any(t.startswith("wordfreq") for t in tasks):
        WORK.mkdir(parents=True, exist_ok=True)
        corpus = WORK / "corpus.txt"
        if not corpus.exists():
            print(f"generating {corpus.name} ...")
            subprocess.run(
                [sys.executable, str(TASKS_DIR / "wordfreq" / "corpus.py"), str(corpus)],
                check=True,
            )

    results = {}
    for task in tasks:
        sums = {}
        for lang in langs:
            if not lang.source(task).exists():
                continue
            build = lang.build(task)
            if build:
                b = subprocess.run(build, capture_output=True, text=True, cwd=HERE)
                if b.returncode != 0:
                    tail = (b.stderr or b.stdout).strip().splitlines()
                    results[(task, lang.name)] = (
                        None,
                        f"build failed: {tail[-1] if tail else ''}",
                    )
                    print(f"  {task:<12} {lang.name:<8} build failed")
                    continue
                # Koka writes its executable without the execute bit.
                built = Path(lang.run(task)[0])
                if built.is_file() and not os.access(built, os.X_OK):
                    built.chmod(built.stat().st_mode | 0o111)
            out, secs = measure(lang.run(task), HERE, args.reps)
            results[(task, lang.name)] = (out, secs)
            if out is None:
                print(f"  {task:<12} {lang.name:<8} {secs}")
            else:
                sums.setdefault(out, []).append(lang.name)
                print(f"  {task:<12} {lang.name:<8} {fmt(secs):>8}   {out}")
        if len(sums) > 1:
            print(f"\n  !! {task}: the languages disagree, so its times mean nothing")
            for out, who in sums.items():
                print(f"       {', '.join(who)}: {out}")
            print()
        print()

    floors = {}
    if not args.no_startup:
        for lang in langs:
            floors[lang.name] = startup_floor(lang, args.reps)

    print()
    print(table(results, langs, tasks, floors))

    if args.json:
        args.json.write_text(
            json.dumps(
                {
                    "machine": platform.platform(),
                    "cores": os.cpu_count(),
                    "reps": args.reps,
                    "startup": floors,
                    "results": {
                        f"{t}/{l}": {"checksum": o, "seconds": s}
                        for (t, l), (o, s) in results.items()
                    },
                },
                indent=2,
            )
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
