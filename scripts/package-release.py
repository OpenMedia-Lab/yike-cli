#!/usr/bin/env python3
"""Create allowlisted release archives and anonymous smoke checks (Python 3.11+)."""

import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile


ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
)


def validate_tag(version, tag):
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version):
        raise ValueError("release version must be a stable MAJOR.MINOR.PATCH version")
    if tag != "v" + version:
        raise ValueError("release tag must equal v + Cargo.toml package.version")


def manifest_version():
    with (ROOT / "Cargo.toml").open("rb") as source:
        return tomllib.load(source)["package"]["version"]


def archive_name(version, target):
    validate_tag(version, "v" + version)
    if target not in TARGETS:
        raise ValueError("unsupported release target")
    suffix = ".zip" if target.endswith("windows-msvc") else ".tar.gz"
    return "yike-" + version + "-" + target + suffix


def read_regular_file(path):
    if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
        raise ValueError("release inputs must be nonempty regular files: " + str(path))
    return path.read_bytes()


def create_archive(binary, license_path, version, target, output):
    filename = archive_name(version, target)
    windows = target.endswith("windows-msvc")
    executable = "yike.exe" if windows else "yike"
    command = ".\\yike.exe" if windows else "./yike"
    readme = (
        "Yike CLI " + version + " (" + target + ")\n\n"
        "Resumable original-media transfers for your own Baidu Photo library.\n"
        "Read LICENSE before use. Extract this archive into a directory you control.\n"
        "Run " + command + " --help for commands.\n"
        "Run " + command + " doctor --offline for an anonymous local diagnostic.\n"
        "Authentication is only needed for online account operations.\n"
        "Keep credentials and transfer state private. Use only your own media.\n"
        "Linux GNU packages also run in a compatible Linux distribution under WSL.\n"
        "Keep WSL private state in the native Linux filesystem.\n"
        "These binaries are unsigned; this release does not claim Apple notarization.\n"
    ).encode("utf-8")
    entries = (
        (executable, read_regular_file(binary), 0o755),
        ("LICENSE", read_regular_file(license_path), 0o644),
        ("README.txt", readme, 0o644),
    )
    output.mkdir(parents=True, exist_ok=True)
    archive_path = output / filename
    prefix = "yike-" + version + "-" + target + "/"
    # Explicit entries prevent adjacent state, credentials or media entering an archive.
    with archive_path.open("xb") as destination:
        if windows:
            with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
                for name, payload, mode in entries:
                    info = zipfile.ZipInfo(prefix + name, date_time=(1980, 1, 1, 0, 0, 0))
                    info.create_system = 3
                    info.external_attr = (0o100000 | mode) << 16
                    info.compress_type = zipfile.ZIP_DEFLATED
                    archive.writestr(info, payload, compresslevel=9)
        else:
            with gzip.GzipFile(filename="", fileobj=destination, mode="wb", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                    for name, payload, mode in entries:
                        info = tarfile.TarInfo(prefix + name)
                        info.size = len(payload)
                        info.mode = mode
                        info.mtime = 0
                        archive.addfile(info, io.BytesIO(payload))
    return archive_path


def write_checksums(directory, version):
    expected = {archive_name(version, target) for target in TARGETS}
    actual = {path.name for path in directory.iterdir()} - {"SHA256SUMS"}
    if actual != expected:
        raise ValueError("release asset set differs; missing=" + str(sorted(expected - actual)) + "; unexpected=" + str(sorted(actual - expected)))
    lines = []
    for name in sorted(expected):
        digest = hashlib.sha256(read_regular_file(directory / name)).hexdigest()
        lines.append(digest + "  " + name + "\n")
    manifest = directory / "SHA256SUMS"
    if manifest.is_symlink():
        raise ValueError("checksum manifest must not be a symlink")
    manifest.write_text("".join(lines), encoding="ascii", newline="\n")
    return manifest


def smoke(binary, version):
    executable = str(binary.resolve(strict=True))
    result = subprocess.run([executable, "--version"], check=True, capture_output=True, text=True, timeout=30)
    if result.stdout.strip() != "yike " + version:
        raise ValueError("binary version differs from Cargo.toml")
    subprocess.run([executable, "--help"], check=True, capture_output=True, timeout=30)
    with tempfile.TemporaryDirectory(prefix="yike-anonymous-smoke-") as scratch:
        state = Path(scratch) / "state"
        target = Path(scratch) / "target"
        target.mkdir()
        result = subprocess.run(
            [executable, "--state-dir", str(state), "--json", "doctor", "--offline", "--target", str(target)],
            check=True, capture_output=True, text=True, timeout=60,
        )
        diagnostic = json.loads(result.stdout)
        if (
            diagnostic["session_saved"] is not False
            or diagnostic["capabilities"] is not None
            or diagnostic["local_verified_receipts"] != 0
            or diagnostic["target"]["writable"] is not True
            or (state / "session.json").exists()
        ):
            raise ValueError("offline anonymous diagnostic contract failed")
    print("Anonymous offline smoke passed: " + version)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    version_command = commands.add_parser("version", help="validate an optional tag against Cargo.toml")
    version_command.add_argument("--tag")
    smoke_command = commands.add_parser("smoke", help="run --version, --help and anonymous doctor --offline")
    smoke_command.add_argument("--binary", type=Path, required=True)
    package = commands.add_parser("package", help="package only executable, LICENSE and generated README")
    package.add_argument("--tag", required=True)
    package.add_argument("--binary", type=Path, required=True)
    package.add_argument("--target", choices=TARGETS, required=True)
    package.add_argument("--out-dir", type=Path, default=ROOT / "dist")
    checksums = commands.add_parser("checksums", help="require the complete asset set and write SHA256SUMS")
    checksums.add_argument("--tag", required=True)
    checksums.add_argument("--directory", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    version = manifest_version()
    validate_tag(version, getattr(args, "tag", None) or "v" + version)
    if args.command == "version":
        print(version)
    elif args.command == "smoke":
        smoke(args.binary, version)
    elif args.command == "package":
        print(create_archive(args.binary, ROOT / "LICENSE", version, args.target, args.out_dir))
    elif args.command == "checksums":
        print(write_checksums(args.directory, version))


if __name__ == "__main__":
    main()
