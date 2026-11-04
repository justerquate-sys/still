#!/usr/bin/env python3
"""Fetch the pinned upstream personal-signing tool and verify its release SHA-256."""
import argparse
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request


VERSION = "0.29.0"
# Published .tar.gz.sha256 files from the upstream apple-codesign/0.29.0 release:
# https://github.com/indygreg/apple-platform-rs/releases/tag/apple-codesign%2F0.29.0
ARCHIVES = {
    "arm64": ("aarch64", "d1a532150adaf90048260d76359261aa716abafc45c53c5dc18845029184334a"),
    "x86_64": ("x86_64", "14ef11bedd51a8d95eafd767939ae96d5900e5a61511bef75bb21db6e7c74140"),
}
MAX_BYTES = 256 * 1024 * 1024


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", required=True, type=Path)
    args = parser.parse_args()
    if sys.platform != "darwin" or platform.machine() not in ARCHIVES:
        raise SystemExit("rcodesign download requires a native Apple Silicon or Intel Mac")
    architecture, expected_hash = ARCHIVES[platform.machine()]
    package = f"apple-codesign-{VERSION}-{architecture}-apple-darwin"
    url = f"https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign/{VERSION}/{package}.tar.gz"
    args.destination.mkdir(parents=True, exist_ok=True)
    executable = args.destination.resolve() / "rcodesign"
    if executable.exists():
        raise SystemExit(f"Refusing to overwrite {executable}")
    with tempfile.TemporaryDirectory(prefix=".download-", dir=args.destination) as temporary:
        archive = Path(temporary) / "rcodesign.tar.gz"
        digest = hashlib.sha256()
        size = 0
        with urllib.request.urlopen(url, timeout=60) as response, archive.open("xb") as output:
            while data := response.read(1024 * 1024):
                size += len(data)
                if size > MAX_BYTES:
                    raise RuntimeError("rcodesign download exceeds its size limit")
                digest.update(data)
                output.write(data)
        if digest.hexdigest() != expected_hash:
            raise RuntimeError("rcodesign archive does not match the pinned upstream SHA-256")
        # Read only the expected regular executable, never arbitrary archive paths.
        with tarfile.open(archive, "r:gz") as source:
            entry = source.getmember(f"{package}/rcodesign")
            if not entry.isfile() or not 0 < entry.size <= MAX_BYTES:
                raise RuntimeError("rcodesign archive has an invalid executable")
            staged = Path(temporary) / "rcodesign"
            with source.extractfile(entry) as data, staged.open("xb") as output:
                shutil.copyfileobj(data, output)
        staged.chmod(0o500)
        version = subprocess.check_output([str(staged), "--version"], text=True).strip()
        if version != f"apple-codesign {VERSION}":
            raise RuntimeError("rcodesign executable reports the wrong version")
        os.rename(staged, executable)
    print(f"Verified apple-codesign {VERSION}: {executable}")


if __name__ == "__main__":
    main()
