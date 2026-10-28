from hashlib import sha256
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from zipfile import ZipFile

from package import package


class PackageTests(unittest.TestCase):
    def fixture(self, root: Path) -> tuple[Path, bytes]:
        (root / "Cargo.toml").write_text('[package]\nversion = "1.2.3"\n', encoding="utf-8")
        (root / "README.md").write_text("余量面板运行说明\n", encoding="utf-8")
        (root / "start-agentbar.cmd").write_bytes(b'@echo off\nstart "" "%~dp0agentbar.exe"\n')
        stale = root / "target/x86_64-pc-windows-gnu/release/agentbar.exe"
        stale.parent.mkdir(parents=True)
        stale.write_bytes(b"MZ-stale-build")
        selected = root / "target/x86_64-pc-windows-msvc/release/agentbar.exe"
        selected.parent.mkdir(parents=True)
        payload = b"MZ-selected-build"
        selected.write_bytes(payload)
        return selected, payload

    def test_explicit_build_wins_and_package_excludes_private_files(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            binary, payload = self.fixture(root)
            (root / "snapshot.json").write_text('{"private": true}', encoding="utf-8")
            archive = package(root, binary.relative_to(root))
            self.assertEqual(archive.name, "agentbar-1.2.3-windows-x64.zip")
            with ZipFile(archive) as bundle:
                self.assertEqual(set(bundle.namelist()), {"agentbar.exe", "start-agentbar.cmd", "README.md"})
                self.assertEqual(bundle.read("agentbar.exe"), payload)
                self.assertIn(b"\r\n", bundle.read("start-agentbar.cmd"))
                self.assertEqual(bundle.read("README.md"), (root / "README.md").read_bytes())
            sums = (root / "dist/SHA256SUMS.txt").read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(sums), 2)
            for line in sums:
                digest, name = line.split("  ", 1)
                self.assertEqual(sha256((root / "dist" / name).read_bytes()).hexdigest(), digest)

    def test_missing_explicit_build_does_not_fall_back_to_stale_binary(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            with self.assertRaises(FileNotFoundError):
                package(root, Path("missing/agentbar.exe"))
            self.assertFalse((root / "dist").exists())


if __name__ == "__main__":
    unittest.main()
