#!/usr/bin/env python3
"""Build the datadog-agent-native wheel and bundle the Rust library."""

from __future__ import annotations

import argparse
import platform
import shutil
import subprocess
import sys
from pathlib import Path

LIBRARY_FILENAMES = {
    "darwin": "libdatadog_agent_native.dylib",
    "linux": "libdatadog_agent_native.so",
    "windows": "datadog_agent_native.dll",
}

SYSTEM_ALIASES = {
    "darwin": "darwin",
    "macos": "darwin",
    "mac os": "darwin",
    "mac os x": "darwin",
    "linux": "linux",
    "windows": "windows",
    "win32": "windows",
}

ARCH_ALIASES = {
    "x86_64": "x86_64",
    "amd64": "x86_64",
    "arm64": "arm64",
    "aarch64": "arm64",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Build the Rust library, copy it into the Python package, and create a wheel."
        )
    )
    parser.add_argument(
        "--platform",
        choices=sorted(set(SYSTEM_ALIASES.values())),
        help="Platform folder name (default: detected host platform).",
    )
    parser.add_argument(
        "--arch",
        help="Architecture label used for the destination folder (default: detected host arch).",
    )
    parser.add_argument(
        "--platform-dir",
        help="Override the exact folder name under datadog_agent_native/bin/.",
    )
    parser.add_argument(
        "--target",
        help="Optional Rust target triple passed to cargo (e.g. x86_64-apple-darwin).",
    )
    parser.add_argument(
        "--binary-path",
        type=Path,
        help="Use an existing compiled library instead of building with cargo.",
    )
    parser.add_argument(
        "--skip-cargo",
        action="store_true",
        help="Assume the Rust library is already built and skip cargo build.",
    )
    parser.add_argument(
        "--skip-wheel",
        action="store_true",
        help="Do not run 'python -m build' after copying the binary.",
    )
    parser.add_argument(
        "--cargo-args",
        nargs=argparse.REMAINDER,
        help="Additional arguments forwarded to cargo build (use '-- --flag').",
    )
    return parser.parse_args()


def detect_platform() -> str:
    return SYSTEM_ALIASES.get((platform.system() or "").lower(), (platform.system() or "").lower())


def detect_arch() -> str:
    return ARCH_ALIASES.get((platform.machine() or "").lower(), (platform.machine() or "").lower())


def run(cmd: list[str], cwd: Path) -> None:
    print(f"[cmd] {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=str(cwd), check=True)


def cargo_build(
    crate_root: Path,
    target: str | None,
    lib_name: str,
    cargo_args: list[str] | None,
    skip: bool,
) -> Path:
    if not skip:
        cmd = ["cargo", "build", "--release", "-p", "datadog-agent-native"]
        if target:
            cmd.extend(["--target", target])
        if cargo_args:
            cmd.extend(cargo_args)
        run(cmd, crate_root)

    target_dir = crate_root / ".." / ".." / "target"
    if target:
        target_dir = target_dir / target / "release"
    else:
        target_dir = target_dir / "release"

    artifact_path = target_dir / lib_name
    if not artifact_path.exists():
        raise SystemExit(
            f"Expected to find {artifact_path}, but it does not exist. Build the crate first or supply --binary-path."
        )
    return artifact_path


def main() -> None:
    args = parse_args()

    script_dir = Path(__file__).resolve().parent
    python_dir = script_dir.parent
    crate_root = python_dir.parent
    package_dir = python_dir / "datadog_agent_native"
    bin_root = package_dir / "bin"

    platform_name = args.platform or detect_platform()
    arch_name = args.arch or detect_arch()

    lib_name = LIBRARY_FILENAMES.get(platform_name)
    if not lib_name:
        raise SystemExit(f"Unsupported platform '{platform_name}'. Choose from: {', '.join(LIBRARY_FILENAMES)}")

    dest_folder_name = args.platform_dir or (f"{platform_name}-{arch_name}" if arch_name else platform_name)
    destination_dir = bin_root / dest_folder_name
    destination_dir.mkdir(parents=True, exist_ok=True)

    if args.binary_path:
        artifact_path = args.binary_path.expanduser().resolve()
    else:
        artifact_path = cargo_build(
            crate_root=crate_root,
            target=args.target,
            lib_name=lib_name,
            cargo_args=args.cargo_args,
            skip=args.skip_cargo,
        )

    if not artifact_path.exists():
        raise SystemExit(f"Binary not found at {artifact_path}. Use --binary-path to point to the compiled library.")

    destination_path = destination_dir / lib_name
    if destination_path.exists():
        destination_path.unlink()

    shutil.copy2(artifact_path, destination_path)
    print(f"Copied {artifact_path} -> {destination_path}")

    if args.skip_wheel:
        return

    run([sys.executable, "-m", "build"], python_dir)
    print("Wheel created in", python_dir / "dist")


if __name__ == "__main__":
    main()
