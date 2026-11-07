#!/usr/bin/env python3
"""Measures comparison time when almost nothing changed. Meant for CI runners.

Usage: bench.py <path to hayakagami binary> [folders] [files per folder]
Everything happens under a new temporary folder, which is removed at the end.
"""

import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPEATS = 5


def generate(root, folders, per_folder):
    for index in range(folders):
        folder = root / f"g{index % 50:02d}" / f"d{index:05d}"
        folder.mkdir(parents=True)
        for number in range(per_folder):
            (folder / f"file{number:03d}.dat").write_bytes(b"x" * (200 + number % 100))


def timed(command, accepted=(0,)):
    started = time.perf_counter()
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    elapsed = time.perf_counter() - started
    if result.returncode not in accepted:
        print(result.stdout)
        raise SystemExit(f"failed ({result.returncode}): {' '.join(map(str, command))}")
    return elapsed, result.stdout


def drop_caches():
    """Empties the OS file cache where the runner allows it (Linux only)."""
    if platform.system() != "Linux":
        return False
    subprocess.run(["sync"], check=True)
    done = subprocess.run(
        ["sudo", "sh", "-c", "echo 3 > /proc/sys/vm/drop_caches"], capture_output=True
    )
    return done.returncode == 0


def baseline_command(source, dest):
    if platform.system() == "Windows":
        command = ["robocopy", str(source), str(dest), "/MIR", "/L", "/NFL", "/NDL", "/NJH", "/NJS", "/NP"]
        return "robocopy /MIR /L", command, tuple(range(8))
    command = ["rsync", "-a", "--delete", "--dry-run", f"{source}/", f"{dest}/"]
    return "rsync -a --delete --dry-run", command, (0,)


def repeat(command, accepted=(0,)):
    return statistics.median(timed(command, accepted)[0] for _ in range(REPEATS))


def cold(command, accepted=(0,)):
    samples = []
    for _ in range(3):
        if not drop_caches():
            return None
        samples.append(timed(command, accepted)[0])
    return statistics.median(samples)


def seconds(value):
    return "-" if value is None else f"{value:.3f}"


def main():
    binary = str(Path(sys.argv[1]).resolve())
    folders = int(sys.argv[2]) if len(sys.argv) > 2 else 2000
    per_folder = int(sys.argv[3]) if len(sys.argv) > 3 else 50
    work = Path(tempfile.mkdtemp(prefix="hkgm-bench-"))
    try:
        os.environ["HAYAKAGAMI_HOME"] = str(work / "home")
        source = work / "source"
        dest = work / "dest"
        started = time.perf_counter()
        generate(source, folders, per_folder)
        generated = time.perf_counter() - started

        mirror = [binary, "mirror", str(source), str(dest)]
        first, _ = timed(mirror)
        _, full_output = timed(mirror + ["--full"])
        _, cached_output = timed(mirror)
        print(full_output)
        print(cached_output)
        if "destination: cache" not in cached_output:
            raise SystemExit("the cache was not used on the repeated run")

        label, baseline, accepted = baseline_command(source, dest)
        rows = [
            ("Hayakagami: cache", repeat(mirror), cold(mirror)),
            ("Hayakagami: --full", repeat(mirror + ["--full"]), cold(mirror + ["--full"])),
            (label, repeat(baseline, accepted), cold(baseline, accepted)),
        ]

        changed = source / "g10" / "d00010" / "file010.dat"
        changed.write_bytes(b"changed content, longer than before" * 20)
        one_change, output = timed(mirror)
        print(output)
        if (dest / "g10" / "d00010" / "file010.dat").read_bytes() != changed.read_bytes():
            raise SystemExit("the changed file was not copied")

        total = folders * per_folder
        lines = [
            f"## {platform.system()} ({total:,} files, {folders:,} folders)",
            "",
            f"No changes: median of {REPEATS} runs. Cold: median of 3 runs after dropping the OS file cache.",
            "",
            "| Command | Warm (s) | Cold (s) |",
            "|---|---:|---:|",
        ]
        lines += [f"| {name} | {seconds(warm)} | {seconds(coldness)} |" for name, warm, coldness in rows]
        lines += [
            "",
            f"Generating files: {generated:.1f} s. First copy: {first:.1f} s. One changed file, cached: {one_change:.3f} s.",
            "",
        ]
        report = "\n".join(lines)
        print(report)
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as handle:
                handle.write(report)
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    main()
