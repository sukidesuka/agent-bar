"""Package only the executable and public instructions, never local snapshots."""
from pathlib import Path
import shutil
import tomllib
from zipfile import ZipFile, ZIP_DEFLATED

root = Path(__file__).resolve().parents[1]
binary = root / "target/x86_64-pc-windows-gnu/release/agentbar.exe"
if not binary.exists():
    binary = root / "target/release/agentbar.exe"
if not binary.exists():
    raise SystemExit("Build the Windows release executable first.")

dist = root / "dist"
dist.mkdir(exist_ok=True)
shutil.copy2(binary, dist / "agentbar.exe")
shutil.copy2(root / "README.md", dist / "README.md")
launcher = (root / "start-agentbar.cmd").read_text().replace("\r\n", "\n")
(dist / "start-agentbar.cmd").write_bytes(launcher.replace("\n", "\r\n").encode())
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
archive = dist / f"agentbar-{version}-windows-x64.zip"
with ZipFile(archive, "w", ZIP_DEFLATED) as bundle:
    for name in ("agentbar.exe", "start-agentbar.cmd", "README.md"):
        bundle.write(dist / name, name)
print(f"Created {archive.name} ({archive.stat().st_size:,} bytes)")
