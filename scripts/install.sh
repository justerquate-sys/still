#!/bin/zsh
set -euo pipefail
cd "${0:A:h:h}"
if [[ ! -d build/Still.app ]]; then ./scripts/build.sh; fi
if pgrep -x Still > /dev/null; then
    printf 'Quit Still before installing an update.\n' >&2
    exit 1
fi
codesign --verify --deep --strict build/Still.app
destination="$HOME/Applications/Still.app"
mkdir -p "$HOME/Applications"
if [[ -e "$destination" ]]; then
    identifier=$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$destination/Contents/Info.plist" 2>/dev/null || true)
    if [[ "$identifier" != app.still.screenshot ]]; then
        printf 'A different app already exists at %s. Installation stopped.\n' "$destination" >&2
        exit 1
    fi
fi
stage="$(mktemp -d "$HOME/Applications/.still-install.XXXXXX")"
cleanup() {
    if [[ ! -e "$destination" && -e "$stage/Previous.app" ]]; then
        if ! mv "$stage/Previous.app" "$destination"; then
            printf 'Restoration failed; previous app preserved at %s\n' "$stage/Previous.app" >&2
            return
        fi
    fi
    rm -rf "$stage"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# Stage and verify the new bundle before touching the installed app.
ditto build/Still.app "$stage/Still.app"
codesign --verify --deep --strict "$stage/Still.app"
if [[ -e "$destination" ]]; then
    # Keep a durable backup outside the temporary directory, even if interrupted.
    mkdir -p build/backups
    backup="$PWD/build/backups/Still-$(date +%Y%m%d-%H%M%S)-$$.app"
    ditto "$destination" "$backup"
    codesign --verify --deep --strict "$backup"
    mv "$destination" "$stage/Previous.app"
fi
mv "$stage/Still.app" "$destination"
if [[ "${STILL_SKIP_OPEN:-0}" != 1 ]]; then open "$destination"; fi
printf 'Installed %s\n' "$destination"
