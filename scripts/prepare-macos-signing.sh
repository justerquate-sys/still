#!/bin/bash
# CI-only: materialize temporary signing credentials without changing OS trust.
set -euo pipefail
[[ "${GITHUB_ACTIONS:-}" == true ]] || { printf 'Run this helper only in GitHub Actions\n' >&2; exit 1; }
: "${RUNNER_TEMP:?}"
: "${GITHUB_ENV:?}"
: "${MACOS_CERTIFICATE_P12_BASE64:?Persistent Mac code signing certificate is required}"
: "${MACOS_CERTIFICATE_PASSWORD:?Certificate export password is required}"
signing_mode="${MACOS_SIGNING_MODE:-developer-id}"
case "$signing_mode" in
    personal) ;;
    developer-id)
        : "${MACOS_SIGNING_IDENTITY:?Developer ID Application identity is required}"
        : "${NOTARY_KEY_P8_BASE64:?App Store Connect API key is required}"
        : "${NOTARY_KEY_ID:?App Store Connect API key ID is required}"
        : "${NOTARY_ISSUER_ID:?App Store Connect API issuer ID is required}"
        ;;
    *) printf 'MACOS_SIGNING_MODE must be personal or developer-id\n' >&2; exit 1 ;;
esac
umask 077
signing_directory="$RUNNER_TEMP/still-macos-signing"
mkdir "$signing_directory"
export STILL_SIGNING_DIRECTORY="$signing_directory"
python3 - <<'PY'
import base64, os
from pathlib import Path
directory = Path(os.environ['STILL_SIGNING_DIRECTORY'])
files = [('MACOS_CERTIFICATE_P12_BASE64', 'certificate.p12')]
if os.environ.get('MACOS_SIGNING_MODE', 'developer-id') == 'developer-id':
    files.append(('NOTARY_KEY_P8_BASE64', 'notary-key.p8'))
for variable, filename in files:
    (directory / filename).write_bytes(base64.b64decode(os.environ[variable].strip(), validate=True))
if os.environ.get('MACOS_SIGNING_MODE', 'developer-id') == 'personal':
    (directory / 'certificate-password.txt').write_text(os.environ['MACOS_CERTIFICATE_PASSWORD'], encoding='utf-8')
PY
if [[ "$signing_mode" == personal ]]; then
    printf 'MACOS_CERTIFICATE_P12_PATH=%s\nMACOS_CERTIFICATE_PASSWORD_FILE=%s\n' \
        "$signing_directory/certificate.p12" "$signing_directory/certificate-password.txt" >> "$GITHUB_ENV"
    exit 0
fi
keychain="$signing_directory/release.keychain-db"
keychain_password="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$signing_directory/certificate.p12" -k "$keychain" \
    -P "$MACOS_CERTIFICATE_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" >/dev/null
rm "$signing_directory/certificate.p12"
printf 'MACOS_SIGNING_KEYCHAIN=%s\n' "$keychain" >> "$GITHUB_ENV"
if [[ "$signing_mode" == developer-id ]]; then
    printf 'NOTARY_KEY_PATH=%s\n' "$signing_directory/notary-key.p8" >> "$GITHUB_ENV"
fi
