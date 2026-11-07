#!/usr/bin/env python3
"""Writes the licenses of the crates built into the programs for this platform.

Usage: third_party_notices.py <output file>
Build scripts and test tools are left out, because they are not distributed.
"""

import json
import subprocess
import sys
from pathlib import Path

LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "copyright", "ofl", "ufl", "unlicense")
CODE_SUFFIXES = (".rs", ".toml", ".json", ".py", ".sh", ".yml", ".yaml")


def host_triple():
    output = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    return next(line.split(": ")[1] for line in output.splitlines() if line.startswith("host: "))


def distributed_packages(metadata):
    """Follows normal dependencies from this workspace's own packages."""
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    seen = set()
    pending = list(metadata["workspace_members"])
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        for dep in nodes[current]["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]):
                pending.append(dep["pkg"])
    return seen - set(metadata["workspace_members"])


def is_font_notice(path, folder):
    """Bundled fonts keep their terms in text files beside them."""
    return path.parent == folder / "fonts" and path.suffix.lower() == ".txt"


def license_files(manifest_path):
    """License texts in a crate's folder or one level down, as bundled fonts keep them."""
    folder = Path(manifest_path).parent
    candidates = list(folder.iterdir()) + list(folder.glob("*/*"))
    return sorted(
        path
        for path in candidates
        if path.is_file()
        and path.suffix.lower() not in CODE_SUFFIXES
        and (path.name.lower().startswith(LICENSE_PREFIXES) or is_font_notice(path, folder))
    )


def main():
    output = Path(sys.argv[1])
    metadata = json.loads(
        subprocess.run(
            [
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--filter-platform",
                host_triple(),
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=True,
        ).stdout
    )
    wanted = distributed_packages(metadata)
    packages = sorted(
        (package for package in metadata["packages"] if package["id"] in wanted),
        key=lambda package: (package["name"], package["version"]),
    )
    unlicensed = [package["name"] for package in packages if not package["license"]]
    if unlicensed:
        raise SystemExit(f"crates without a license field: {', '.join(unlicensed)}")

    parts = [
        "Hayakagami is built with the following crates. Where a crate offers a choice",
        "of licenses, every license it ships is reproduced here. A crate that ships no",
        "license text uses a license whose text appears under other crates in this file.",
        "",
    ]
    without_text = 0
    for package in packages:
        parts += ["=" * 78, f"{package['name']} {package['version']}", f"License: {package['license']}"]
        if package.get("repository"):
            parts.append(f"Source: {package['repository']}")
        parts.append("=" * 78)
        files = license_files(package["manifest_path"])
        if not files:
            without_text += 1
            parts.append("This crate ships no license text. Its license is named above, and the text is in its source repository.")
        for path in files:
            parts += ["", f"--- {path.relative_to(Path(package['manifest_path']).parent).as_posix()} ---", path.read_text(encoding="utf-8", errors="replace").strip()]
        parts.append("")
    output.write_text("\n".join(parts) + "\n", encoding="utf-8", newline="\n")
    print(f"{len(packages)} crates, {without_text} without a license text, {output.stat().st_size:,} bytes")


if __name__ == "__main__":
    main()
