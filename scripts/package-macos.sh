#!/bin/zsh
# Build artifacts are possible without Apple credentials. Only --production
# creates an inspection eligible for the public signed update feed.
set -euo pipefail
cd "${0:A:h:h}"
production=false
signing_mode="${MACOS_SIGNING_MODE:-developer-id}"
if [[ "${1:-}" == --production ]]; then production=true; shift; fi
if (( $# != 0 )); then printf 'Usage: %s [--production]\n' "$0" >&2; exit 2; fi
app="$PWD/build/Still.app"
[[ -d "$app" ]] || { printf 'Build Still.app first with scripts/build.sh\n' >&2; exit 1; }
version="$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; m=json.load(sys.stdin); print(next(p["version"] for p in m["packages"] if p["id"] in m["workspace_members"] and p["name"] == "still"))')"
host_arch="$(uname -m)"
case "$host_arch" in
    arm64) platform=macos-aarch64; suffix=arm64 ;;
    x86_64) platform=macos-x86_64; suffix=x64 ;;
    *) printf 'Unsupported host architecture: %s\n' "$host_arch" >&2; exit 1 ;;
esac
mkdir -p dist
stage="$(mktemp -d "$PWD/dist/.still-package.XXXXXX")"
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
if $production; then
    case "$signing_mode" in
        personal)
            : "${RCODESIGN_PATH:?Verified pinned rcodesign executable is required}"
            : "${MACOS_CERTIFICATE_P12_PATH:?Persistent Mac code signing P12 file is required}"
            : "${MACOS_CERTIFICATE_PASSWORD_FILE:?P12 password file is required}"
            [[ "$(env -u RCODESIGN_PATH "$RCODESIGN_PATH" --version)" == 'apple-codesign 0.29.0' ]] || { printf 'Personal signing requires pinned apple-codesign 0.29.0\n' >&2; exit 1; }
            env -u RCODESIGN_PATH "$RCODESIGN_PATH" sign --p12-file "$MACOS_CERTIFICATE_P12_PATH" \
                --p12-password-file "$MACOS_CERTIFICATE_PASSWORD_FILE" --timestamp-url none \
                --code-signature-flags runtime --binary-identifier app.still.screenshot "$app"
            ;;
        developer-id)
            : "${MACOS_SIGNING_IDENTITY:?Developer ID Application signing identity is required}"
            : "${MACOS_SIGNING_KEYCHAIN:?A temporary release keychain is required}"
            : "${NOTARY_KEY_PATH:?App Store Connect API private key file is required}"
            : "${NOTARY_KEY_ID:?App Store Connect API key ID is required}"
            : "${NOTARY_ISSUER_ID:?App Store Connect API issuer ID is required}"
            [[ "$MACOS_SIGNING_IDENTITY" == 'Developer ID Application: '* ]] || { printf 'Developer ID mode requires a Developer ID Application identity\n' >&2; exit 1; }
            codesign --force --sign "$MACOS_SIGNING_IDENTITY" --keychain "$MACOS_SIGNING_KEYCHAIN" \
                --identifier app.still.screenshot --options runtime --timestamp "$app"
            codesign --verify --deep --strict "$app"
            ditto -c -k --keepParent "$app" "$stage/notarization.zip"
            xcrun notarytool submit "$stage/notarization.zip" --key "$NOTARY_KEY_PATH" \
                --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID" --wait \
                --timeout 30m --output-format json > "$stage/notarization.json"
            python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); assert r.get("status") == "Accepted", "Apple notarization was not accepted"' "$stage/notarization.json"
            xcrun stapler staple "$app"
            xcrun stapler validate "$app"
            spctl --assess --type execute --verbose=2 "$app"
            ;;
        *) printf 'MACOS_SIGNING_MODE must be personal or developer-id\n' >&2; exit 1 ;;
    esac
    codesign --verify --deep --strict "$app"
fi
name="Still-$version-macos-$suffix.tar.gz"
# No Finder metadata or wrapper directory. The updater replaces the full bundle.
COPYFILE_DISABLE=1 tar --no-mac-metadata -czf "$stage/$name" -C "$PWD/build" Still.app
# Inspect a fresh extraction, including its code signature and any stapled ticket,
# so the final tar bytes are proven to preserve the complete app bundle.
mkdir "$stage/extracted"
COPYFILE_DISABLE=1 tar -xzf "$stage/$name" -C "$stage/extracted"
codesign --verify --deep --strict "$stage/extracted/Still.app"
inspect_args=(--artifact "$stage/$name" --binary "$stage/extracted/Still.app/Contents/MacOS/Still" --version "$version" --platform "$platform")
if $production; then
    inspect_args+=(--production)
    if [[ "$signing_mode" == personal ]]; then inspect_args+=(--allow-personal-macos); fi
fi
cargo run --release --locked --bin still-release -- inspect "${inspect_args[@]}"
if [[ -e "dist/$name" || -e "dist/$name.release.json" ]]; then
    printf 'Release archive already exists; use a clean dist directory: %s\n' "$name" >&2
    exit 1
fi
mv "$stage/$name" "$stage/$name.release.json" dist/
printf 'Packaged %s\n' "$PWD/dist/$name"
