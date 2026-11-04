//! Signed release metadata. The detached signature authenticates the exact bytes,
//! before serde sees any attacker-controlled JSON.

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};

use super::{Error, Result};

pub const MAX_MANIFEST_BYTES: u64 = 128 * 1024;
pub const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;
pub const PLATFORMS: [&str; 4] = [
    "macos-aarch64",
    "macos-x86_64",
    "windows-x86_64",
    "windows-aarch64",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub format: ArchiveFormat,
    /// Signed release disclosure. Signature verification remains mandatory for
    /// either personal Mac signing or Apple Developer ID distribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macos_notarized: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveFormat {
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "zip")]
    Zip,
}

#[derive(Debug)]
pub(crate) struct VerifiedRelease {
    pub version: Version,
    pub asset: Asset,
}

pub(crate) fn validate_https(url: &str) -> Result<()> {
    if url.len() > 8192 || url.contains('#') || url.chars().any(char::is_control) {
        return Err(Error::new("invalid update URL"));
    }
    let uri: ureq::http::Uri = url.parse().map_err(|_| Error::new("invalid update URL"))?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none_or(str::is_empty)
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
    {
        return Err(Error::new(
            "updates require an HTTPS URL without credentials",
        ));
    }
    Ok(())
}

pub(crate) fn decode_key(text: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = STANDARD
        .decode(text.trim())
        .map_err(|_| Error::new("invalid update public key encoding"))?
        .try_into()
        .map_err(|_| Error::new("update public key must be 32 bytes"))?;
    let key =
        VerifyingKey::from_bytes(&bytes).map_err(|_| Error::new("invalid update public key"))?;
    if key.is_weak() {
        return Err(Error::new("weak update public key"));
    }
    Ok(key)
}

pub(crate) fn verify_release(
    bytes: &[u8],
    signature_text: &[u8],
    key: &VerifyingKey,
    current: &Version,
    platform: &str,
) -> Result<Option<VerifiedRelease>> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_MANIFEST_BYTES || signature_text.len() > 256 {
        return Err(Error::new("update metadata exceeds its size limit"));
    }
    let signature_text = std::str::from_utf8(signature_text)
        .map_err(|_| Error::new("invalid update signature encoding"))?;
    let signature = STANDARD
        .decode(signature_text.trim())
        .map_err(|_| Error::new("invalid update signature encoding"))?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| Error::new("update signature must be 64 bytes"))?;
    key.verify_strict(bytes, &signature)
        .map_err(|_| Error::new("update manifest signature is not trusted"))?;

    let manifest: Manifest = serde_json::from_slice(bytes)?;
    if manifest.schema != 1
        || manifest.version.len() > 128
        || manifest.notes.as_ref().is_some_and(|s| s.len() > 16 * 1024)
        || manifest
            .published_at
            .as_ref()
            .is_some_and(|s| s.len() > 128)
        || manifest.assets.is_empty()
        || manifest.assets.len() > PLATFORMS.len()
    {
        return Err(Error::new("unsupported or invalid update manifest"));
    }
    let version =
        Version::parse(&manifest.version).map_err(|_| Error::new("invalid update version"))?;
    for (name, asset) in &manifest.assets {
        if !PLATFORMS.contains(&name.as_str()) {
            return Err(Error::new("unknown update platform"));
        }
        validate_https(&asset.url)?;
        if asset.size == 0 || asset.size > MAX_ASSET_BYTES {
            return Err(Error::new("update archive exceeds its size limit"));
        }
        decode_hash(&asset.sha256)?;
        let expected = if name.starts_with("macos-") {
            ArchiveFormat::TarGz
        } else {
            ArchiveFormat::Zip
        };
        if asset.format != expected {
            return Err(Error::new("incorrect archive format for update platform"));
        }
    }
    // Build metadata does not make a release newer. Old/same signed feeds are
    // harmless and never trigger a download or a downgrade.
    if version.cmp_precedence(current) != std::cmp::Ordering::Greater {
        return Ok(None);
    }
    let asset = manifest
        .assets
        .get(platform)
        .ok_or_else(|| Error::new("update has no asset for this platform"))?
        .clone();
    Ok(Some(VerifiedRelease { version, asset }))
}

pub(crate) fn decode_hash(text: &str) -> Result<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new("invalid update SHA-256 digest"));
    }
    let mut bytes = [0_u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::new("invalid update SHA-256 digest"))?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed(version: &str, platform: &str) -> (Vec<u8>, Vec<u8>, VerifyingKey) {
        let key = SigningKey::from_bytes(&[17; 32]);
        let bytes = serde_json::to_vec(&Manifest {
            schema: 1,
            version: version.into(),
            notes: None,
            published_at: None,
            assets: BTreeMap::from([(
                platform.into(),
                Asset {
                    url: "https://example.invalid/Still.tar.gz".into(),
                    sha256: "ab".repeat(32),
                    size: 99,
                    format: if platform.starts_with("macos-") {
                        ArchiveFormat::TarGz
                    } else {
                        ArchiveFormat::Zip
                    },
                    macos_notarized: None,
                },
            )]),
        })
        .unwrap();
        let sig = STANDARD.encode(key.sign(&bytes).to_bytes()).into_bytes();
        (bytes, sig, key.verifying_key())
    }

    #[test]
    fn authenticates_exact_bytes_before_parsing_and_rejects_wrong_key() {
        let (mut bytes, sig, key) = signed("2.5.0", "macos-aarch64");
        let current = Version::parse("2.4.0").unwrap();
        assert!(
            verify_release(&bytes, &sig, &key, &current, "macos-aarch64")
                .unwrap()
                .is_some()
        );
        bytes.push(b' ');
        assert!(verify_release(&bytes, &sig, &key, &current, "macos-aarch64").is_err());
        assert!(
            verify_release(
                &bytes,
                &sig,
                &SigningKey::from_bytes(&[18; 32]).verifying_key(),
                &current,
                "macos-aarch64"
            )
            .is_err()
        );
        assert!(
            verify_release(b"not JSON", &sig, &key, &current, "macos-aarch64")
                .unwrap_err()
                .to_string()
                .contains("signature")
        );
    }

    #[test]
    fn no_downgrades_replays_or_wrong_platforms() {
        let current = Version::parse("2.4.0").unwrap();
        for version in ["2.3.9", "2.4.0", "2.4.0+build", "2.4.0-rc.1"] {
            let (bytes, sig, key) = signed(version, "macos-aarch64");
            assert!(
                verify_release(&bytes, &sig, &key, &current, "macos-aarch64")
                    .unwrap()
                    .is_none()
            );
        }
        let (bytes, sig, key) = signed("2.5.0", "macos-aarch64");
        assert!(verify_release(&bytes, &sig, &key, &current, "windows-x86_64").is_err());
    }

    #[test]
    fn limits_and_https_are_enforced() {
        for url in [
            "http://example.com/a",
            "https://a:b@example.com/a",
            "file:///a",
            "https://example.com/a#b",
            "https:///",
            "https://example.com/\n",
        ] {
            assert!(validate_https(url).is_err(), "{url}");
        }
        assert!(
            validate_https("https://github.com/org/repo/releases/download/v2.5.0/Still.zip")
                .is_ok()
        );
        assert!(decode_hash(&"z".repeat(64)).is_err());
        let (bytes, sig, key) = signed("2.5.0", "macos-aarch64");
        assert!(
            verify_release(
                &vec![b' '; MAX_MANIFEST_BYTES as usize + 1],
                &sig,
                &key,
                &Version::new(2, 4, 0),
                "macos-aarch64"
            )
            .is_err()
        );
        assert!(
            verify_release(
                &bytes,
                &[b'A'; 257],
                &key,
                &Version::new(2, 4, 0),
                "macos-aarch64"
            )
            .is_err()
        );
    }

    #[test]
    fn authenticated_but_malformed_releases_are_rejected() {
        let key = SigningKey::from_bytes(&[17; 32]);
        let (bytes, _, _) = signed("2.5.0", "macos-aarch64");
        let valid: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let mut cases = Vec::new();
        let mut manifest = valid.clone();
        manifest["schema"] = 2.into();
        cases.push(manifest);
        let mut manifest = valid.clone();
        manifest["version"] = "not semver".into();
        cases.push(manifest);
        let mut manifest = valid.clone();
        manifest["assets"]["macos-aarch64"]["size"] = (MAX_ASSET_BYTES + 1).into();
        cases.push(manifest);
        let mut manifest = valid.clone();
        manifest["assets"]["macos-aarch64"]["sha256"] = "ab".into();
        cases.push(manifest);
        let mut manifest = valid.clone();
        manifest["assets"]["macos-aarch64"]["url"] = "http://example.invalid/asset".into();
        cases.push(manifest);
        let mut manifest = valid.clone();
        manifest["assets"]["macos-aarch64"]["format"] = "zip".into();
        cases.push(manifest);
        let mut manifest = valid;
        manifest["assets"] = serde_json::json!({"linux-x86_64": {"url":"https://example.invalid/a", "size":1,"sha256":"ab".repeat(32),"format":"zip"}});
        cases.push(manifest);
        for manifest in cases {
            let bytes = serde_json::to_vec(&manifest).unwrap();
            let signature = STANDARD.encode(key.sign(&bytes).to_bytes());
            assert!(
                verify_release(
                    &bytes,
                    signature.as_bytes(),
                    &key.verifying_key(),
                    &Version::new(2, 4, 0),
                    "macos-aarch64"
                )
                .is_err()
            );
        }
    }
}
