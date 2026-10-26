"""Release packaging regressions; all payloads are anonymous synthetic bytes."""

import hashlib
import importlib.util
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
import zipfile


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "package-release.py"
SPEC = importlib.util.spec_from_file_location("package_release", SCRIPT)
release = importlib.util.module_from_spec(SPEC)
if SCRIPT.exists():
    SPEC.loader.exec_module(release)


class PackageReleaseTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.binary = self.root / "synthetic-binary"
        self.binary.write_bytes(b"anonymous executable fixture\x00\x01")
        self.license = self.root / "LICENSE"
        self.license.write_text("Anonymous MIT license fixture\n", encoding="utf-8")

    def test_tag_must_equal_cargo_version_and_be_a_stable_version(self):
        release.validate_tag("0.1.0", "v0.1.0")
        for tag in ("v0.1.1", "0.1.0", "v0.1.0/../../private", "v0.1.0-rc.1"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.validate_tag("0.1.0", tag)

    def test_unix_archive_is_deterministic_executable_and_contains_only_public_files(self):
        (self.root / "session.json").write_text("never package adjacent state")
        first = release.create_archive(
            self.binary, self.license, "0.1.0", "x86_64-unknown-linux-gnu", self.root / "one"
        )
        os.utime(self.binary, (1234567890, 1234567890))
        second = release.create_archive(
            self.binary, self.license, "0.1.0", "x86_64-unknown-linux-gnu", self.root / "two"
        )
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(first.name, "yike-0.1.0-x86_64-unknown-linux-gnu.tar.gz")
        prefix = "yike-0.1.0-x86_64-unknown-linux-gnu/"
        with tarfile.open(first, "r:gz") as archive:
            self.assertEqual(set(archive.getnames()), {prefix + "yike", prefix + "LICENSE", prefix + "README.txt"})
            self.assertEqual(archive.extractfile(prefix + "yike").read(), self.binary.read_bytes())
            self.assertEqual(archive.getmember(prefix + "yike").mode, 0o755)
            self.assertIn(b"doctor --offline", archive.extractfile(prefix + "README.txt").read())

    def test_windows_zip_is_deterministic_and_contains_exe_license_and_generic_readme(self):
        first = release.create_archive(
            self.binary, self.license, "0.1.0", "x86_64-pc-windows-msvc", self.root / "one"
        )
        second = release.create_archive(
            self.binary, self.license, "0.1.0", "x86_64-pc-windows-msvc", self.root / "two"
        )
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(first.name, "yike-0.1.0-x86_64-pc-windows-msvc.zip")
        prefix = "yike-0.1.0-x86_64-pc-windows-msvc/"
        with zipfile.ZipFile(first) as archive:
            self.assertEqual(set(archive.namelist()), {prefix + "yike.exe", prefix + "LICENSE", prefix + "README.txt"})
            self.assertEqual(archive.read(prefix + "yike.exe"), self.binary.read_bytes())
            self.assertIn(b".\\yike.exe", archive.read(prefix + "README.txt"))

    def test_packaging_rejects_unsupported_target_empty_binary_and_symlink(self):
        with self.assertRaises(ValueError):
            release.create_archive(self.binary, self.license, "0.1.0", "../../state", self.root / "out")
        self.binary.write_bytes(b"")
        with self.assertRaises(ValueError):
            release.create_archive(self.binary, self.license, "0.1.0", "aarch64-apple-darwin", self.root / "out")
        if os.name != "nt":
            alias = self.root / "alias"
            alias.symlink_to(self.license)
            with self.assertRaises(ValueError):
                release.create_archive(alias, self.license, "0.1.0", "aarch64-apple-darwin", self.root / "out")

    def test_checksums_require_all_five_targets_and_reject_unexpected_files(self):
        output = self.root / "out"
        output.mkdir()
        with self.assertRaises(ValueError):
            release.write_checksums(output, "0.1.0")
        names = (
            "yike-0.1.0-x86_64-pc-windows-msvc.zip",
            "yike-0.1.0-x86_64-unknown-linux-gnu.tar.gz",
            "yike-0.1.0-aarch64-unknown-linux-gnu.tar.gz",
            "yike-0.1.0-aarch64-apple-darwin.tar.gz",
            "yike-0.1.0-x86_64-apple-darwin.tar.gz",
        )
        for name in names:
            (output / name).write_bytes(b"anonymous archive fixture")
        unexpected = output / "session.json"
        unexpected.write_text("anonymous state fixture")
        with self.assertRaises(ValueError):
            release.write_checksums(output, "0.1.0")
        unexpected.unlink()
        manifest = release.write_checksums(output, "0.1.0")
        digest = hashlib.sha256(b"anonymous archive fixture").hexdigest()
        self.assertEqual(manifest.read_text().splitlines(), [digest + "  " + name for name in sorted(names)])
        self.assertEqual(release.write_checksums(output, "0.1.0").read_bytes(), manifest.read_bytes())


if __name__ == "__main__":
    unittest.main()
