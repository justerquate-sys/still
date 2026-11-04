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
if [[ "$configuration" == release ]]; then args+=(--release); fi
"$cargo_command" "${args[@]}"
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
cp "target/$configuration/Still" "$app/Contents/MacOS/Still"
cp Resources/Info.plist "$app/Contents/Info.plist"
cp Resources/Still.icns "$app/Contents/Resources/Still.icns"
codesign --force --sign "${STILL_SIGNING_IDENTITY:--}" --identifier app.still.screenshot "$app"
codesign --verify --deep --strict "$app"
# Replace the bundle without truncating a binary that may still be running.
if [[ -e "$destination" ]]; then mv "$destination" "$stage/Previous.app"; fi
mv "$app" "$destination"
printf 'Built %s\n' "$destination"
