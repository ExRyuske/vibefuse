#!/usr/bin/env python3
"""Writes THIRD-PARTY-NOTICES.txt: the license of every crate compiled into
VibeFuse on macOS, Windows or Linux, with the license files the crates ship.

Texts several crates share verbatim are printed once at the end. The
packaging script runs it; by hand, from the repository root:

    python3 scripts/third-party.py [output file]
"""

import hashlib
import json
import os
import re
import subprocess
import sys

TARGETS = ["aarch64-apple-darwin", "x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"]
OUT = sys.argv[1] if len(sys.argv) > 1 else "THIRD-PARTY-NOTICES.txt"
LICENSE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|NOTICE|COPYRIGHT|UNLICENSE)", re.IGNORECASE)
# Code compiled from sources bundled inside a crate, under its own license.
BUNDLED = {
    "libusb1-sys": ["libusb/COPYING"],
}
# Libraries inside prebuilt binaries a crate downloads, with their texts
# (Skia's build of skia-bindings: the macOS renderer).
PREBUILT = {
    "skia-bindings": "packaging/skia-licenses.txt",
}


def metadata(target):
    output = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--filter-platform", target],
        text=True,
    )
    return json.loads(output)


def runtime_packages(meta):
    """Packages reachable from the root through normal (not build or dev)
    dependencies."""
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]
    seen, stack = set(), [root]
    while stack:
        current = stack.pop()
        for dep in nodes[current]["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])
    return seen


def digest(text):
    return hashlib.sha256(re.sub(r"\s+", " ", text).strip().encode()).hexdigest()


def license_files(directory, name):
    """License files of a crate, including REUSE-style `LICENSES/` folders."""
    found = []
    for entry in sorted(os.listdir(directory)):
        path = os.path.join(directory, entry)
        if not LICENSE_FILE.match(entry):
            continue
        if os.path.isdir(path):
            found += [os.path.join(entry, f) for f in sorted(os.listdir(path))]
        else:
            found.append(entry)
    found += [f for f in BUNDLED.get(name, []) if os.path.exists(os.path.join(directory, f))]
    return found


def main():
    packages = {}
    for target in TARGETS:
        meta = metadata(target)
        by_id = {p["id"]: p for p in meta["packages"]}
        for package_id in runtime_packages(meta):
            package = by_id[package_id]
            packages[(package["name"], package["version"])] = package

    # Texts several crates share verbatim are printed once, at the end.
    texts = {}
    entries = []
    for (name, version), package in sorted(packages.items()):
        directory = os.path.dirname(package["manifest_path"])
        files = []
        for file in license_files(directory, name):
            with open(os.path.join(directory, file), encoding="utf-8", errors="replace") as f:
                text = f.read().strip()
            key = digest(text)
            texts.setdefault(key, [file, text, 0])[2] += 1
            files.append((file, key))
        entries.append((name, version, package, files))

    shared = {}
    for key, (file, text, count) in texts.items():
        if count > 1:
            first_line = next((l.strip() for l in text.splitlines() if l.strip()), file)
            shared[key] = (f"[{len(shared) + 1}] {first_line[:70]}", text)

    sections = []
    for name, version, package, files in entries:
        lines = [
            "=" * 78,
            f"{name} {version}",
            f"License: {package.get('license') or 'see below'}",
        ]
        if package.get("repository"):
            lines.append(f"Source: {package['repository']}")
        for file, key in files:
            if key in shared:
                lines.append(f"{file}: text {shared[key][0].split(']')[0]}] at the end of this file")
            else:
                lines.append(f"\n--- {file} ---\n{texts[key][1]}")
        if name in PREBUILT:
            with open(PREBUILT[name], encoding="utf-8") as f:
                lines.append("\n" + f.read().strip())
        if not files and name not in PREBUILT:
            lines.append("(The crate ships no license file; its license is named above.)")
        sections.append("\n".join(lines))

    header = (
        "VibeFuse is built from the third-party components listed below.\n"
        "Each is used under the license named with it.\n\n"
        "Slint (https://slint.dev) is used under the GNU General Public License 3.0,\n"
        "one of the licenses it is offered under, like VibeFuse itself.\n"
        "libusb (macOS, Linux) is compiled in under the GNU Lesser General Public\n"
        "License 2.1 or later; its source is part of the libusb1-sys crate, and the\n"
        "source of VibeFuse lets you rebuild the program with a modified libusb.\n"
    )
    appendix = [
        "=" * 78 + f"\n{title}\n\n{text}" for title, text in shared.values()
    ]
    body = "\n\n".join([header] + sections + appendix)
    with open(OUT, "w", encoding="utf-8") as f:
        f.write(body + "\n")
    print(f"{OUT}: {len(sections)} components, {len(shared)} shared texts")


if __name__ == "__main__":
    main()
