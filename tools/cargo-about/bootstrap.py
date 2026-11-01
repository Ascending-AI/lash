#!/usr/bin/env python3
"""Fetch the checksum-pinned Linux notice generator, without a Cargo build."""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
import platform
import tarfile
import tempfile
import urllib.request

VERSION = "0.9.3"
SHA256 = "3ad1e94ea1e63d69d1961f4fb15ccbbe9575d906111f96d2cb4dce6df48f71b4"


def main() -> None:
    if (platform.system(), platform.machine()) != ("Linux", "x86_64"):
        raise RuntimeError("the pinned cargo-about executable requires Linux x86_64")
    name = f"cargo-about-{VERSION}-x86_64-unknown-linux-musl"
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "lash" / name
    executable = cache / "cargo-about"
    if not executable.is_file():
        cache.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=cache) as directory:
            archive = Path(directory) / "tool.tar.gz"
            urllib.request.urlretrieve(
                f"https://github.com/EmbarkStudios/cargo-about/releases/download/{VERSION}/{name}.tar.gz",
                archive,
            )
            if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
                raise RuntimeError("cargo-about download checksum mismatch")
            with tarfile.open(archive) as tar:
                binary = tar.extractfile(f"{name}/cargo-about")
                temporary = Path(directory) / "cargo-about"
                temporary.write_bytes(binary.read())
            temporary.chmod(0o755)
            temporary.replace(executable)
    print(executable)


if __name__ == "__main__":
    main()
