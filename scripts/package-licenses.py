#!/usr/bin/env python3
"""Copy the licenses of resolved Rust dependencies and the Rust standard library."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--target")
    args = parser.parse_args()
    cargo = os.environ.get("CARGO", "cargo")
    command = [cargo, "metadata", "--locked", "--format-version", "1"]
    if args.target:
        command += ["--filter-platform", args.target]
    metadata = json.loads(subprocess.check_output(command))
    included = {node["id"] for node in metadata["resolve"]["nodes"]}
    for package in metadata["packages"]:
        if not package["source"] or package["id"] not in included:
            continue
        destination = args.destination / f'{package["name"]}-{package["version"]}'
        destination.mkdir(parents=True, exist_ok=True)
        (destination / "SPDX.txt").write_text((package["license"] or "See package notices") + "\n")
        directory = Path(package["manifest_path"]).parent
        for source in directory.iterdir():
            if source.is_file() and source.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "COPYRIGHT")):
                shutil.copy2(source, destination / source.name)
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    rust_notices = sysroot / "share" / "doc" / "rust"
    destination = args.destination / "Rust"
    destination.mkdir(parents=True, exist_ok=True)
    shutil.copy2(rust_notices / "COPYRIGHT-library.html", destination / "COPYRIGHT-library.html")
    shutil.copytree(rust_notices / "licenses", destination / "licenses", dirs_exist_ok=True)
    for source in [Path("LICENSE"), Path("LICENSE.txt")]:
        if source.is_file():
            shutil.copy2(source, args.destination / source.name)


if __name__ == "__main__":
    main()
