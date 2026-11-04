#!/bin/zsh
set -euo pipefail
cd "${0:A:h:h}"
configuration="${1:-release}"
if [[ "$configuration" != release && "$configuration" != debug ]]; then
    printf 'Usage: %s [release|debug]\n' "$0" >&2
    exit 2
fi
cargo_command="${CARGO:-$HOME/.cargo/bin/cargo}"
if [[ ! -x "$cargo_command" ]]; then cargo_command="$(command -v cargo)"; fi
args=(build --locked --target-dir "$PWD/target" --bin Still)
target="${STILL_BUILD_TARGET:-}"
binary_directory="target"
if [[ -n "$target" ]]; then
    args+=(--target "$target")
    binary_directory+="/$target"
fi
if [[ "$configuration" == release ]]; then args+=(--release); fi
"$cargo_command" "${args[@]}"
version="$("$cargo_command" metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; m=json.load(sys.stdin); print(next(p["version"] for p in m["packages"] if p["id"] in m["workspace_members"] and p["name"] == "still"))')"
mkdir -p build
stage="$(mktemp -d "$PWD/build/.still-build.XXXXXX")"
app="$stage/Still.app"
destination="$PWD/build/Still.app"
cleanup() {
    if [[ ! -e "$destination" && -e "$stage/Previous.app" ]]; then
        if ! mv "$stage/Previous.app" "$destination"; then
            printf 'Previous build preserved at %s\n' "$stage/Previous.app" >&2
            return
        fi
    fi
    rm -rf "$stage"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary_directory/$configuration/Still" "$app/Contents/MacOS/Still"
cp Resources/Info.plist "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"
cp Resources/Still.icns "$app/Contents/Resources/Still.icns"
cp Resources/ThirdPartyNotices.txt "$app/Contents/Resources/ThirdPartyNotices.txt"
license_args=("$app/Contents/Resources/Licenses")
if [[ -n "$target" ]]; then license_args+=(--target "$target"); fi
CARGO="$cargo_command" python3 scripts/package-licenses.py "${license_args[@]}"
# Local builds use ad hoc signing unless an identity is supplied, as before.
# package-macos.sh re-signs production builds
# with the configured persistent personal or notarized Developer ID identity.
codesign --force --sign "${STILL_SIGNING_IDENTITY:--}" --identifier app.still.screenshot "$app"
codesign --verify --deep --strict "$app"
# Replace the bundle without truncating a binary that may still be running.
if [[ -e "$destination" ]]; then mv "$destination" "$stage/Previous.app"; fi
mv "$app" "$destination"
printf 'Built %s\n' "$destination"
