# Releasing Still

Source code and stable update downloads are public at
<https://github.com/justerquate-sys/still>. Push a stable `vX.Y.Z` tag matching
`Cargo.toml` to run `.github/workflows/release.yml`. A manual run with
`publish: false` builds and tests all four platforms without publishing or
requiring signing credentials. Publishing from a manual run requires an existing
version tag.

## Trust setup

Generate the update key once, outside the source checkout:

```sh
cargo run --release --locked --bin still-release -- keygen \
  --private-key /secure/location/still-update-private.key \
  --public-key /secure/location/still-update-public.key
```

The private file contains one base64-encoded 32-byte Ed25519 seed. Key generation
uses the OS random generator, refuses to overwrite files, and sets private file
permissions to `0600` on Unix. Keep a secure backup. Do not put it in the
repository, a release artifact, command arguments, or a log.

Set the repository Actions secret `UPDATE_SIGNING_KEY` to the private file's
contents through standard input (`gh secret set UPDATE_SIGNING_KEY < FILE`).
Commit only its public key in `Resources/update-config.json`:

```json
{
  "enabled": true,
  "manifest_url": "https://github.com/justerquate-sys/still/releases/latest/download/updates.json",
  "public_key": "BASE64_PUBLIC_KEY"
}
```

The public key is embedded in each executable. An existing installation will
reject a feed signed by another key; changing the key requires a migration
release signed by the old key. Never replace or delete the old private key
without planning that migration.

## Mac signing modes

The repository's `MACOS_SIGNING_MODE` Actions variable selects `personal` or
`developer-id`. This owner's workflow defaults to **personal**, which does not
require Apple Developer membership. Both modes require a persistent certificate
signature, hardened runtime, and successful strict code signature validation of
the final archive's extracted app.

### Personal releases

Configure these repository Actions secrets:

| Secret | Value |
| --- | --- |
| `MACOS_CERTIFICATE_P12_BASE64` | Base64 of a password-protected P12 export of the persistent app-specific signing certificate **and private key** |
| `MACOS_CERTIFICATE_PASSWORD` | P12 export password |

Keep this certificate and private key backed up outside the repository, and reuse
them for later versions. A self-signed code signing certificate is supported;
production personal releases cannot use an ad hoc signature. CI uses upstream
`rcodesign` 0.29.0, downloaded for the native Mac architecture and checked against
its pinned official release SHA-256, to sign directly from the P12 and a temporary
password file. It enables hardened runtime and disables timestamping. Personal
signing does not import the certificate or change any OS trust settings. Apple's
`codesign --verify --deep --strict` validates the result. The inspector checks the
certificate-based designated requirement for `app.still.screenshot` and records
`macos_notarized: false` in the
signed update manifest. The Ed25519 update signature is required in both modes.

**Personal Mac releases are not Apple-notarized.** The first installation can
require macOS's normal approval for an unidentified developer, and screen capture
requires the usual Screen Recording permission. These scripts do not change
system trust, remove quarantine, or change Gatekeeper policy. Reusing the same
signing identity provides stable app identity; it does not make Apple trust the
certificate. Apple explains this distinction in
[macOS Code Signing In Depth](https://developer.apple.com/library/archive/technotes/tn2206/_index.html).

The release CLI defaults to the stricter Developer ID policy. The personal
workflow explicitly supplies `--allow-personal-macos` to inspection, signing, and
verification; a development bundle still cannot enter a production feed.

For local personal packaging after `scripts/build.sh`, download the verified tool
with `python3 scripts/download-rcodesign.py --destination build/personal-signer`,
then supply file paths without putting the P12 password in process arguments:

```sh
MACOS_SIGNING_MODE=personal \
RCODESIGN_PATH="$PWD/build/personal-signer/rcodesign" \
MACOS_CERTIFICATE_P12_PATH=/secure/location/still-signing.p12 \
MACOS_CERTIFICATE_PASSWORD_FILE=/secure/location/still-signing-password.txt \
./scripts/package-macos.sh --production
```

Keep the P12 and password file outside the checkout with owner-only permissions.
The tool is pinned to the upstream
[Apple Codesign 0.29.0 release](https://github.com/indygreg/apple-platform-rs/releases/tag/apple-codesign%2F0.29.0).

### Developer ID releases

To switch, set `MACOS_SIGNING_MODE` to `developer-id`, supply a Developer ID
Application P12 and password through the same secrets above, and add:

| Secret | Value |
| --- | --- |
| `MACOS_SIGNING_IDENTITY` | Exact `Developer ID Application: Name (TEAMID)` identity |
| `NOTARY_KEY_P8_BASE64` | Base64 of the App Store Connect team API private key (`.p8`) authorized for notarization |
| `NOTARY_KEY_ID` | That API key's key ID |
| `NOTARY_ISSUER_ID` | That API key's issuer UUID |

Developer ID mode imports the certificate into a temporary keychain and signs with
`--options runtime --timestamp`, submits through `notarytool`, staples, and checks
Gatekeeper assessment. Temporary credential files and the keychain are removed
even when a build fails. It records `macos_notarized: true` only after these
checks pass.

Apple describes the requirements in
[Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
and the API key flags in
[Migrating to the latest notarization tool](https://developer.apple.com/documentation/technotes/tn3147-migrating-to-the-latest-notarization-tool).
Missing credentials for the selected mode stop production publication.
Notarization credentials are unnecessary in personal mode.

## Windows signing

Ed25519 update authentication is mandatory. Authenticode is optional: add
`WINDOWS_CERTIFICATE_P12_BASE64` and `WINDOWS_CERTIFICATE_PASSWORD` to import a
certificate for the CI packaging step. The portable MSVC builds link the Visual
C++ runtime statically. Unsigned Windows downloads may still show SmartScreen
reputation prompts.

## What a release checks

Native GitHub runners build macOS Apple Silicon and Intel, and Windows x64 and
ARM64. Each runs formatting, Clippy, and unit tests; Windows also runs
`Still.exe --self-test`. The pinned Rust toolchain is read from
`rust-toolchain.toml`.

`scripts/build.sh` derives bundle versions from Cargo and includes dependency and
Rust runtime notices. `scripts/package-macos.sh --production` signs using
`MACOS_SIGNING_MODE` (local script default: `developer-id`) and notarizes in
Developer ID mode. It creates `Still-X.Y.Z-macos-{arm64,x64}.tar.gz` rooted at
`Still.app`. `scripts/build-windows.ps1 -Production` creates
`Still-X.Y.Z-windows-{x64,arm64}.zip` with `Still.exe`, notices, licenses, and a
readme at its root.

Each native packager invokes `still-release inspect`. It runs the final
executable's `--update-info`, checks app ID/version/platform, checks the archive
executable's Mach-O or PE architecture, and confirms that the archived executable
matches the inspected executable. A `.release.json` sidecar binds those results
to the final archive hash and size. Sidecars are internal CI artifacts.

Publishing jobs are serialized so concurrent version tags cannot race to replace
the latest stable feed. The publishing job requires all four production
inspections and rejects missing or unexpected archives, unsafe paths, links,
missing notices, a different pinned
key, prerelease versions, and releases no newer than the previous signed feed.
It hashes each archive and signs the exact bytes of `updates.json`, producing
`updates.json.sig` as a detached base64 Ed25519 signature. The manifest binds
version, platform, download URL, SHA-256, size, archive format, and Mac notarization
status.

Only after every platform succeeds does CI create a draft GitHub release. It
downloads the draft assets and verifies their signed metadata, hashes, and
layout before publishing and making the release `latest`. A failure after draft
creation leaves the draft available for inspection; it never activates that
feed. Re-running against an existing release refuses to overwrite it.

To verify downloaded release files locally:

```sh
cargo run --release --locked --bin still-release -- verify \
  --manifest dist/updates.json --signature dist/updates.json.sig \
  --config Resources/update-config.json --artifacts dist \
  --version X.Y.Z --repo justerquate-sys/still \
  --require-platforms macos-aarch64,macos-x86_64,windows-x86_64,windows-aarch64 \
  --allow-personal-macos
```

Omit `--allow-personal-macos` to require notarized Developer ID Mac assets.

For an emergency fix, publish a higher version. Installed clients reject
downgrades and ignore replayed versions.
