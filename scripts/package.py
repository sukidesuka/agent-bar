"""Package only the executable and public instructions, never local snapshots."""
import argparse
from hashlib import sha256
from pathlib import Path
import shutil
import tomllib
from zipfile import ZipFile, ZIP_DEFLATED


def package(root: Path, binary: Path | None = None) -> Path:
    if binary is None:
        candidates = (
            root / "target/x86_64-pc-windows-gnu/release/agentbar.exe",
            root / "target/release/agentbar.exe",
        )
        binary = next((path for path in candidates if path.is_file()), None)
    elif not binary.is_absolute():
        binary = root / binary
    if binary is None or not binary.is_file():
        raise FileNotFoundError("Build the Windows release executable first.")

    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    shutil.copy2(binary, dist / "agentbar.exe")
    shutil.copy2(root / "README.md", dist / "README.md")
    launcher = (root / "start-agentbar.cmd").read_text(encoding="utf-8")
    launcher = launcher.replace("\r\n", "\n")
    (dist / "start-agentbar.cmd").write_bytes(launcher.replace("\n", "\r\n").encode("utf-8"))
    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    version = manifest["package"]["version"]
    archive = dist / f"agentbar-{version}-windows-x64.zip"
    with ZipFile(archive, "w", ZIP_DEFLATED) as bundle:
        for name in ("agentbar.exe", "start-agentbar.cmd", "README.md"):
            bundle.write(dist / name, name)

    checksums = "".join(
        f"{sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
        for path in (dist / "agentbar.exe", archive)
    )
    (dist / "SHA256SUMS.txt").write_text(checksums, encoding="utf-8", newline="\n")
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, help="Exact Windows executable to package")
    args = parser.parse_args()
    try:
        archive = package(Path(__file__).resolve().parents[1], args.binary)
    except FileNotFoundError as error:
        parser.exit(1, f"{error}\n")
    print(f"Created {archive.name} ({archive.stat().st_size:,} bytes)")


if __name__ == "__main__":
    main()
