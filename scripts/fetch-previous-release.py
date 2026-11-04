#!/usr/bin/env python3
"""Fetch the previous public signed feed; only an HTTP 404 means first release."""
import json
import os
from pathlib import Path
import urllib.error
import urllib.request


def main():
    repo = os.environ["RELEASE_REPOSITORY"]
    destination = Path(os.environ["PREVIOUS_RELEASE_DIRECTORY"])
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repo}/releases/latest",
        headers={"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28", "Authorization": f"Bearer {os.environ['GH_TOKEN']}"},
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            release = json.load(response)
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
        print("No previous public release; this will be the first signed feed.")
        return
    if release["draft"] or release["prerelease"] or not release["tag_name"].startswith("v"):
        raise RuntimeError("Latest release must be a stable version tag")
    tag = release["tag_name"]
    if any(character not in "0123456789.v" for character in tag):
        raise RuntimeError("Unexpected release tag")
    destination.mkdir()
    for name, maximum in [("updates.json", 128 * 1024), ("updates.json.sig", 256)]:
        url = f"https://github.com/{repo}/releases/download/{tag}/{name}"
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read(maximum + 1)
        if not data or len(data) > maximum:
            raise RuntimeError(f"Previous {name} is empty or too large")
        (destination / name).write_bytes(data)
    print(f"Downloaded previous signed feed for {tag}.")


if __name__ == "__main__":
    main()
