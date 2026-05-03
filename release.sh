#!/usr/bin/env python3
"""release.sh - Python release helper for Rust plugins.

Usage:
  ./release.sh           # auto-increment patch  (0.1.0 -> 0.1.1)
  ./release.sh minor     # auto-increment minor  (0.1.0 -> 0.2.0)
  ./release.sh major     # auto-increment major  (0.1.0 -> 1.0.0)
  ./release.sh 0.2.3     # explicit version
"""

from __future__ import annotations

import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

PLUGIN_ROOT = Path("plugins")
ACTIONS_URL = "https://github.com/redbug26/kkc-plugins-rust/actions"
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+$")


@dataclass
class PluginFiles:
    name: str
    manifest: Path
    cargo_toml: Path


def run(cmd: list[str], *, capture: bool = False) -> str:
    if capture:
        completed = subprocess.run(cmd, check=True, text=True, capture_output=True)
        return completed.stdout
    subprocess.run(cmd, check=True)
    return ""


def parse_semver(version: str) -> tuple[int, int, int]:
    if not VERSION_RE.match(version):
        raise ValueError(f"Invalid semantic version: {version}")
    major, minor, patch = version.split(".")
    return int(major), int(minor), int(patch)


def read_section_version(file_path: Path, section: str) -> str:
    in_section = False
    for line in file_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_section = stripped == f"[{section}]"
            continue
        if in_section and stripped.startswith("version"):
            match = re.match(r'^version\s*=\s*"([^"]+)"$', stripped)
            if match:
                return match.group(1)
            raise ValueError(f"Malformed version line in {file_path}: {stripped}")
    raise ValueError(f"Could not find [{section}] version in {file_path}")


def write_section_version(file_path: Path, section: str, new_version: str) -> None:
    lines = file_path.read_text(encoding="utf-8").splitlines(keepends=True)
    in_section = False
    replaced = False

    for idx, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_section = stripped == f"[{section}]"
            continue

        if in_section and re.match(r"^\s*version\s*=", line):
            indent = line[: len(line) - len(line.lstrip())]
            newline = "\n" if line.endswith("\n") else ""
            lines[idx] = f'{indent}version = "{new_version}"{newline}'
            replaced = True
            break

    if not replaced:
        raise ValueError(f"Could not update [{section}] version in {file_path}")

    file_path.write_text("".join(lines), encoding="utf-8")


def collect_plugins() -> list[PluginFiles]:
    if not PLUGIN_ROOT.exists():
        raise SystemExit(f"No plugin root found at {PLUGIN_ROOT}/")

    plugins: list[PluginFiles] = []
    for directory in sorted(p for p in PLUGIN_ROOT.iterdir() if p.is_dir()):
        manifest = directory / "plugin.toml"
        cargo_toml = directory / "Cargo.toml"
        if not manifest.exists() or not cargo_toml.exists():
            print(f"Skipping {directory}: missing plugin.toml or Cargo.toml")
            continue
        plugins.append(PluginFiles(name=directory.name, manifest=manifest, cargo_toml=cargo_toml))

    if not plugins:
        raise SystemExit(
            f"No releasable Rust plugin found (plugin.toml + Cargo.toml) under {PLUGIN_ROOT}/"
        )

    return plugins


def compute_new_version(current: str, arg: str) -> str:
    major, minor, patch = parse_semver(current)
    if arg == "major":
        return f"{major + 1}.0.0"
    if arg == "minor":
        return f"{major}.{minor + 1}.0"
    if arg == "patch":
        return f"{major}.{minor}.{patch + 1}"
    if VERSION_RE.match(arg):
        return arg
    raise SystemExit(f"Usage: {Path(sys.argv[0]).name} [major|minor|patch|X.Y.Z]")


def main() -> int:
    plugins = collect_plugins()

    all_versions: list[str] = []
    per_plugin_info: list[tuple[PluginFiles, str, str]] = []
    for plugin in plugins:
        manifest_version = read_section_version(plugin.manifest, "plugin")
        cargo_version = read_section_version(plugin.cargo_toml, "package")
        parse_semver(manifest_version)
        parse_semver(cargo_version)
        all_versions.extend([manifest_version, cargo_version])
        per_plugin_info.append((plugin, manifest_version, cargo_version))

    distinct_versions = sorted({*all_versions}, key=parse_semver)
    current = distinct_versions[-1]

    if len(distinct_versions) > 1:
        print("Version mismatch detected across plugin manifests.")
        print(f"Auto-align base version to latest detected: {current}")
        for plugin, manifest_version, cargo_version in per_plugin_info:
            print(
                f"  {plugin.name:<12} plugin.toml={manifest_version:<8} Cargo.toml={cargo_version:<8}"
            )
        print("")

    arg = sys.argv[1] if len(sys.argv) > 1 else "patch"
    new_version = compute_new_version(current, arg)

    print(f"Current version : {current}")
    print(f"New version     : {new_version}")
    print(f"Plugins         : {' '.join(plugin.name for plugin in plugins)}")
    print("")

    confirm = input("Proceed? [y/N] ").strip()
    if not confirm.lower().startswith("y"):
        print("Aborted.")
        return 0

    status = run(["git", "status", "--porcelain"], capture=True).strip()
    if status:
        print("")
        print("Pending changes detected - committing before bump:")
        run(["git", "status", "--short"])
        run(["git", "add", "-A"])
        run(["git", "commit", "-m", "chore: pre-release"])
        run(["git", "push", "origin", "main"])
        print("Pre-release commit pushed")

    for plugin in plugins:
        write_section_version(plugin.manifest, "plugin", new_version)
        write_section_version(plugin.cargo_toml, "package", new_version)
    print("Versions aligned and updated")

    print("")
    print("Building release plugins to verify...")
    run(["cargo", "build", "--release", "--workspace"])
    print("Build OK")

    files_to_commit: list[str] = []
    for plugin in plugins:
        files_to_commit.append(str(plugin.manifest))
        files_to_commit.append(str(plugin.cargo_toml))

    run(["git", "add", *files_to_commit])
    run(["git", "commit", "-m", f"chore: bump rust plugins to v{new_version}"])
    run(["git", "push", "origin", "main"])
    print("Commit pushed")

    run(["git", "tag", f"v{new_version}"])
    run(["git", "push", "origin", f"v{new_version}"])
    print(f"Tag v{new_version} pushed")

    print("")
    print(f"Release v{new_version} triggered")
    print(f"Follow progress at: {ACTIONS_URL}")
    print("")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())