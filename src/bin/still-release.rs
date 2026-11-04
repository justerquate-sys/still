//! Release-only tooling. The installed app never contains or reads a signing key.
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const APP_ID: &str = "app.still.screenshot";
const MAX_ASSET: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED: u64 = 1024 * 1024 * 1024;
const MAX_MANIFEST: usize = 128 * 1024;
const PLATFORMS: [&str; 4] = [
    "macos-aarch64",
    "macos-x86_64",
    "windows-x86_64",
    "windows-aarch64",
];
const DEFAULT_REQUIRED: &str = "macos-aarch64,macos-x86_64,windows-x86_64";

fn main() {
    if let Err(error) = run() {
        eprintln!("still-release: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return usage();
    };
    if command == "--help" || command == "help" {
        return usage();
    }
    let options = Options::parse(args)?;
    match command.as_str() {
        "keygen" => keygen(options),
        "inspect" => inspect(options),
        "sign" => sign(options),
        "verify" => verify(options),
        _ => Err(format!("unknown command: {command}").into()),
    }
}

fn usage() -> Result<()> {
    println!(
        "Still release tooling\n\
         keygen --private-key FILE --public-key FILE\n\
         inspect --artifact FILE --binary FILE --platform PLATFORM --version VERSION [--production] [--allow-personal-macos]\n\
         sign --private-key FILE --artifacts DIR --version VERSION --repo OWNER/REPO --config FILE\n\
           [--notes FILE] [--published-at RFC3339] [--require-platforms LIST]\n\
           [--previous-manifest FILE --previous-signature FILE] [--allow-personal-macos]\n\
         verify --manifest FILE --signature FILE --config FILE [--artifacts DIR]\n\
           [--version VERSION] [--repo OWNER/REPO] [--require-platforms LIST] [--allow-personal-macos]\n\n\
         Keys are base64-encoded Ed25519 seeds/public keys (32 bytes). Never commit private keys.\n\
         inspect runs the native binary's --update-info and binds it to the archive.\n\
         sign accepts stable versions and production inspections only; outputs updates.json and updates.json.sig.\n\
         Mac releases require Developer ID and notarization unless --allow-personal-macos explicitly permits a persistent personal certificate."
    );
    Ok(())
}

struct Options(BTreeMap<String, String>);

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut result = BTreeMap::new();
        while let Some(key) = args.next() {
            if !key.starts_with("--") {
                return Err(format!("expected an option, got {key}").into());
            }
            let value = if key == "--production" || key == "--allow-personal-macos" {
                "true".into()
            } else {
                args.next().ok_or_else(|| format!("{key} needs a value"))?
            };
            if value.starts_with("--") || result.insert(key.clone(), value).is_some() {
                return Err(format!("missing value or duplicate option: {key}").into());
            }
        }
        Ok(Self(result))
    }
    fn take(&mut self, key: &str) -> Option<String> {
        self.0.remove(key)
    }
    fn require(&mut self, key: &str) -> Result<String> {
        self.take(key)
            .ok_or_else(|| format!("missing required option: {key}").into())
    }
    fn finish(self) -> Result<()> {
        match self.0.keys().next() {
            Some(key) => Err(format!("unknown option: {key}").into()),
            None => Ok(()),
        }
    }
}

fn stable_version(text: &str) -> Result<Version> {
    let version = Version::parse(text)?;
    if text.len() > 128 || !version.pre.is_empty() || !version.build.is_empty() {
        return Err("production updates require a stable version without build metadata".into());
    }
    Ok(version)
}

fn repo_name(repo: &str) -> Result<()> {
    let pieces: Vec<_> = repo.split('/').collect();
    if pieces.len() != 2
        || pieces.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || !p
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    {
        return Err("repository must be OWNER/REPO".into());
    }
    Ok(())
}

fn asset_name(version: &str, platform: &str) -> Result<String> {
    let suffix = match platform {
        "macos-aarch64" => "macos-arm64.tar.gz",
        "macos-x86_64" => "macos-x64.tar.gz",
        "windows-x86_64" => "windows-x64.zip",
        "windows-aarch64" => "windows-arm64.zip",
        _ => return Err(format!("unsupported platform: {platform}").into()),
    };
    Ok(format!("Still-{version}-{suffix}"))
}

fn keygen(mut options: Options) -> Result<()> {
    let private = PathBuf::from(options.require("--private-key")?);
    let public = PathBuf::from(options.require("--public-key")?);
    options.finish()?;
    if private == public {
        return Err("private and public key files must differ".into());
    }
    let mut seed = [0; 32];
    getrandom::fill(&mut seed).map_err(|_| "OS secure random generator failed")?;
    let key = SigningKey::from_bytes(&seed);
    seed.fill(0);
    let mut private_options = OpenOptions::new();
    private_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        private_options.mode(0o600);
    }
    let mut private_file = private_options.open(&private)?;
    let result = (|| -> Result<()> {
        let mut public_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&public)?;
        writeln!(private_file, "{}", STANDARD.encode(key.to_bytes()))?;
        private_file.sync_all()?;
        writeln!(
            public_file,
            "{}",
            STANDARD.encode(key.verifying_key().to_bytes())
        )?;
        public_file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&private);
    }
    result?;
    println!(
        "Created private key at {} and public key at {}. The private key was not printed.",
        private.display(),
        public.display()
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    enabled: bool,
    manifest_url: String,
    public_key: String,
}

fn config(path: &Path) -> Result<VerifyingKey> {
    let config: Config = serde_json::from_slice(&read_limited(path, MAX_MANIFEST)?)?;
    if !config.enabled || !config.manifest_url.starts_with("https://github.com/") {
        return Err("update config must be enabled and use the public GitHub release feed".into());
    }
    let bytes: [u8; 32] = STANDARD
        .decode(config.public_key.trim())?
        .try_into()
        .map_err(|_| "public key must contain 32 bytes")?;
    let key = VerifyingKey::from_bytes(&bytes)?;
    if key.is_weak() {
        return Err("weak public key".into());
    }
    Ok(key)
}

fn check_feed_url(path: &Path, repo: &str) -> Result<()> {
    let config: Config = serde_json::from_slice(&read_limited(path, MAX_MANIFEST)?)?;
    if config.manifest_url
        != format!("https://github.com/{repo}/releases/latest/download/updates.json")
    {
        return Err("embedded manifest URL does not match the release repository".into());
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
struct BinaryInfo {
    app_id: String,
    version: String,
    platform: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Inspection {
    schema: u32,
    app_id: String,
    version: String,
    platform: String,
    artifact: String,
    sha256: String,
    size: u64,
    binary_sha256: String,
    production: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    macos_notarized: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    macos_designated_requirement: Option<String>,
}

fn inspect(mut options: Options) -> Result<()> {
    let artifact = PathBuf::from(options.require("--artifact")?);
    let binary = PathBuf::from(options.require("--binary")?);
    let platform = options.require("--platform")?;
    let version = options.require("--version")?;
    let production = options.take("--production").is_some();
    let allow_personal = options.take("--allow-personal-macos").is_some();
    options.finish()?;
    stable_version(&version)?;
    if artifact.file_name().and_then(|n| n.to_str()) != Some(&asset_name(&version, &platform)?) {
        return Err("archive filename does not match its version and platform".into());
    }
    check_native_platform(&platform)?;
    let binary = fs::canonicalize(binary)?;
    let output = Command::new(&binary).arg("--update-info").output()?;
    if !output.status.success() || output.stdout.len() > 8192 {
        return Err("native executable failed --update-info".into());
    }
    let info: BinaryInfo = serde_json::from_slice(&output.stdout)?;
    if info.app_id != APP_ID || info.version != version || info.platform != platform {
        return Err(format!("native executable identity does not match release: {info:?}").into());
    }
    let archive_binary = inspect_archive(&artifact, &platform)?;
    let (binary_sha256, _) = digest_file(&binary)?;
    if archive_binary != binary_sha256 {
        return Err("archive contains a different executable than the one inspected".into());
    }
    if allow_personal && (!production || !platform.starts_with("macos-")) {
        return Err("--allow-personal-macos applies only to a production Mac inspection".into());
    }
    let macos = if production && platform.starts_with("macos-") {
        Some(verify_macos(&binary, allow_personal)?)
    } else {
        None
    };
    let (sha256, size) = digest_file(&artifact)?;
    let inspection = Inspection {
        schema: 1,
        app_id: APP_ID.into(),
        version,
        platform,
        artifact: artifact.file_name().unwrap().to_string_lossy().into_owned(),
        sha256,
        size,
        binary_sha256,
        production,
        macos_notarized: macos.as_ref().map(|(notarized, _)| *notarized),
        macos_designated_requirement: macos.map(|(_, requirement)| requirement),
    };
    let path = sidecar(&artifact);
    write_new(&path, &json_bytes(&inspection)?)?;
    println!("Inspected {}", artifact.display());
    Ok(())
}

fn check_native_platform(platform: &str) -> Result<()> {
    let native = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    if native != platform {
        return Err(
            format!("inspect must run natively on {platform}, this host is {native}").into(),
        );
    }
    Ok(())
}

fn verify_macos(binary: &Path, allow_personal: bool) -> Result<(bool, String)> {
    let app = binary
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("Mac executable must be in Still.app/Contents/MacOS")?;
    if !Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .status()?
        .success()
    {
        return Err("codesign rejected the production Mac app".into());
    }
    let signature = Command::new("codesign")
        .args(["--display", "--verbose=4"])
        .arg(app)
        .output()?;
    let details = String::from_utf8_lossy(&signature.stderr);
    if !signature.status.success()
        || !details.contains("Authority=")
        || details.contains("Signature=adhoc")
        || !details.contains("(runtime)")
    {
        return Err(
            "production Mac apps require a certificate signature and hardened runtime".into(),
        );
    }
    let requirements = Command::new("codesign")
        .args(["--display", "--requirements", "-"])
        .arg(app)
        .output()?;
    if !requirements.status.success() {
        return Err("could not read the Mac designated requirement".into());
    }
    let requirements = format!(
        "{}\n{}",
        String::from_utf8_lossy(&requirements.stdout),
        String::from_utf8_lossy(&requirements.stderr)
    );
    let requirement = requirements.lines()
        .find_map(|line| line.trim_start_matches(['#', ' ']).strip_prefix("designated => "))
        .filter(|line| stable_requirement(line))
        .ok_or("Mac app needs a stable certificate-based designated requirement for app.still.screenshot")?
        .to_owned();
    if allow_personal {
        return Ok((false, requirement));
    }
    if !details.contains("Authority=Developer ID Application:") || !details.contains("Timestamp=") {
        return Err("Developer ID and secure timestamp are required; personal signing needs explicit --allow-personal-macos".into());
    }
    for (program, args) in [
        ("spctl", vec!["--assess", "--type", "execute"]),
        ("xcrun", vec!["stapler", "validate"]),
    ] {
        if !Command::new(program)
            .args(args)
            .arg(app)
            .status()?
            .success()
        {
            return Err(format!("{program} rejected the production Mac app").into());
        }
    }
    Ok((true, requirement))
}

fn stable_requirement(requirement: &str) -> bool {
    requirement.len() <= 16 * 1024
        && requirement.contains("identifier \"app.still.screenshot\"")
        && !requirement.contains("cdhash")
        && (requirement.contains("anchor") || requirement.contains("certificate"))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    published_at: Option<String>,
    assets: BTreeMap<String, Asset>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    url: String,
    sha256: String,
    size: u64,
    format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    macos_notarized: Option<bool>,
}

fn sign(mut options: Options) -> Result<()> {
    let private = PathBuf::from(options.require("--private-key")?);
    let artifacts = PathBuf::from(options.require("--artifacts")?);
    let version_text = options.require("--version")?;
    let repo = options.require("--repo")?;
    let config_path = PathBuf::from(options.require("--config")?);
    let notes = options
        .take("--notes")
        .map(fs::read_to_string)
        .transpose()?;
    let published_at = options.take("--published-at");
    let required = required_platforms(options.take("--require-platforms"))?;
    let previous = options.take("--previous-manifest");
    let previous_sig = options.take("--previous-signature");
    let allow_personal = options.take("--allow-personal-macos").is_some();
    options.finish()?;
    let version = stable_version(&version_text)?;
    repo_name(&repo)?;
    let key = config(&config_path)?;
    check_feed_url(&config_path, &repo)?;
    match (previous, previous_sig) {
        (Some(manifest), Some(signature)) => {
            let (_, previous) =
                authenticated_manifest(Path::new(&manifest), Path::new(&signature), &key)?;
            if version.cmp_precedence(&stable_version(&previous.version)?)
                != std::cmp::Ordering::Greater
            {
                return Err("release must be newer than the previous signed feed".into());
            }
        }
        (None, None) => (),
        _ => return Err("previous manifest and signature must be supplied together".into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&private)?.permissions().mode() & 0o077 != 0 {
            return Err(
                "private key permissions must exclude group and other users (chmod 600)".into(),
            );
        }
    }
    let mut encoded_key = fs::read_to_string(&private)?;
    let mut seed: [u8; 32] = STANDARD
        .decode(encoded_key.trim())
        .map_err(|_| "invalid private key encoding")?
        .try_into()
        .map_err(|_| "private key must contain a 32-byte seed")?;
    encoded_key.clear();
    let signing_key = SigningKey::from_bytes(&seed);
    seed.fill(0);
    if signing_key.verifying_key() != key {
        return Err("private key does not match the pinned public key".into());
    }
    let mut assets = BTreeMap::new();
    let mut archives = BTreeSet::new();
    for platform in PLATFORMS {
        let name = asset_name(&version_text, platform)?;
        let path = artifacts.join(&name);
        if !path.try_exists()? {
            continue;
        }
        let inspection: Inspection =
            serde_json::from_slice(&read_limited(&sidecar(&path), MAX_MANIFEST)?)?;
        let (sha256, size) = digest_file(&path)?;
        if inspection.schema != 1
            || inspection.app_id != APP_ID
            || inspection.version != version_text
            || inspection.platform != platform
            || inspection.artifact != name
            || inspection.sha256 != sha256
            || inspection.size != size
            || !inspection.production
            || inspection.binary_sha256 != inspect_archive(&path, platform)?
        {
            return Err(format!("production inspection does not match {name}").into());
        }
        if platform.starts_with("macos-") {
            if inspection.macos_notarized.is_none()
                || (!allow_personal && inspection.macos_notarized != Some(true))
                || inspection
                    .macos_designated_requirement
                    .as_deref()
                    .is_none_or(|r| !stable_requirement(r))
            {
                return Err("Mac inspection needs notarization and a stable certificate identity; personal releases require --allow-personal-macos".into());
            }
        } else if inspection.macos_notarized.is_some()
            || inspection.macos_designated_requirement.is_some()
        {
            return Err("Windows inspections must not contain Mac signing metadata".into());
        }
        archives.insert(name.clone());
        assets.insert(
            platform.into(),
            Asset {
                url: format!("https://github.com/{repo}/releases/download/v{version_text}/{name}"),
                sha256,
                size,
                format: if platform.starts_with("macos-") {
                    "tar.gz"
                } else {
                    "zip"
                }
                .into(),
                macos_notarized: inspection.macos_notarized,
            },
        );
    }
    for entry in fs::read_dir(&artifacts)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if (name.ends_with(".zip") || name.ends_with(".tar.gz")) && !archives.contains(&name) {
            return Err(format!("unexpected archive in release directory: {name}").into());
        }
    }
    let manifest = Manifest {
        schema: 1,
        version: version_text,
        notes,
        published_at,
        assets,
    };
    validate_manifest(&manifest, &required, Some(&repo), allow_personal)?;
    let bytes = json_bytes(&manifest)?;
    let signature = signing_key.sign(&bytes);
    key.verify_strict(&bytes, &signature)?;
    let manifest_path = artifacts.join("updates.json");
    let signature_path = artifacts.join("updates.json.sig");
    if manifest_path.exists() || signature_path.exists() {
        return Err(
            "updates.json or its signature already exists; refusing to overwrite a signed feed"
                .into(),
        );
    }
    write_new(&manifest_path, &bytes)?;
    write_new(
        &signature_path,
        format!("{}\n", STANDARD.encode(signature.to_bytes())).as_bytes(),
    )?;
    println!(
        "Signed stable release {} with {} platform archives",
        manifest.version,
        manifest.assets.len()
    );
    Ok(())
}

fn verify(mut options: Options) -> Result<()> {
    let path = PathBuf::from(options.require("--manifest")?);
    let signature = PathBuf::from(options.require("--signature")?);
    let config_path = PathBuf::from(options.require("--config")?);
    let artifacts = options.take("--artifacts").map(PathBuf::from);
    let version = options.take("--version");
    let repo = options.take("--repo");
    let required = required_platforms(options.take("--require-platforms"))?;
    let allow_personal = options.take("--allow-personal-macos").is_some();
    options.finish()?;
    let key = config(&config_path)?;
    if let Some(repo) = &repo {
        repo_name(repo)?;
        check_feed_url(&config_path, repo)?;
    }
    let (_, manifest) = authenticated_manifest(&path, &signature, &key)?;
    validate_manifest(&manifest, &required, repo.as_deref(), allow_personal)?;
    if version.is_some_and(|v| v != manifest.version) {
        return Err("signed manifest has the wrong version".into());
    }
    if let Some(artifacts) = artifacts {
        for (platform, asset) in &manifest.assets {
            let path = artifacts.join(asset_name(&manifest.version, platform)?);
            let (hash, size) = digest_file(&path)?;
            if hash != asset.sha256 || size != asset.size {
                return Err(format!("archive hash or size mismatch: {}", path.display()).into());
            }
            inspect_archive(&path, platform)?;
        }
    }
    println!(
        "Verified release {} ({} platform archives)",
        manifest.version,
        manifest.assets.len()
    );
    Ok(())
}

fn required_platforms(text: Option<String>) -> Result<BTreeSet<String>> {
    let names: BTreeSet<_> = text
        .unwrap_or_else(|| DEFAULT_REQUIRED.into())
        .split(',')
        .map(str::to_owned)
        .collect();
    if names.is_empty() || names.iter().any(|name| !PLATFORMS.contains(&name.as_str())) {
        return Err(
            "required platforms must be a comma-separated list of supported platforms".into(),
        );
    }
    Ok(names)
}

fn authenticated_manifest(
    path: &Path,
    signature: &Path,
    key: &VerifyingKey,
) -> Result<(Vec<u8>, Manifest)> {
    let bytes = read_limited(path, MAX_MANIFEST)?;
    let signature = read_limited(signature, 256)?;
    let signature = STANDARD.decode(std::str::from_utf8(&signature)?.trim())?;
    key.verify_strict(&bytes, &Signature::from_slice(&signature)?)
        .map_err(|_| "manifest signature is not trusted")?;
    let manifest = serde_json::from_slice(&bytes)?;
    Ok((bytes, manifest))
}

fn validate_manifest(
    manifest: &Manifest,
    required: &BTreeSet<String>,
    repo: Option<&str>,
    allow_personal: bool,
) -> Result<()> {
    stable_version(&manifest.version)?;
    if manifest.schema != 1
        || manifest.assets.is_empty()
        || manifest.assets.len() > 4
        || manifest.notes.as_ref().is_some_and(|s| s.len() > 16 * 1024)
        || manifest
            .published_at
            .as_ref()
            .is_some_and(|s| s.len() > 128)
    {
        return Err("invalid release manifest".into());
    }
    for platform in required {
        if !manifest.assets.contains_key(platform) {
            return Err(format!("missing required platform: {platform}").into());
        }
    }
    for (platform, asset) in &manifest.assets {
        let name = asset_name(&manifest.version, platform)?;
        let expected_format = if platform.starts_with("macos-") {
            "tar.gz"
        } else {
            "zip"
        };
        if platform.starts_with("macos-") {
            if asset.macos_notarized.is_none()
                || (!allow_personal && asset.macos_notarized != Some(true))
            {
                return Err("Mac assets require notarization unless --allow-personal-macos explicitly permits personal signing".into());
            }
        } else if asset.macos_notarized.is_some() {
            return Err("Windows asset must not contain Mac signing metadata".into());
        }
        if asset.format != expected_format
            || asset.size == 0
            || asset.size > MAX_ASSET
            || asset.sha256.len() != 64
            || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || !asset.url.starts_with("https://github.com/")
            || !asset
                .url
                .ends_with(&format!("/releases/download/v{}/{name}", manifest.version))
            || asset.url.chars().any(char::is_control)
        {
            return Err(format!("invalid asset metadata: {platform}").into());
        }
        if let Some(repo) = repo
            && asset.url
                != format!(
                    "https://github.com/{repo}/releases/download/v{}/{name}",
                    manifest.version
                )
        {
            return Err("asset URL does not match the repository, version, and platform".into());
        }
    }
    Ok(())
}

fn sidecar(archive: &Path) -> PathBuf {
    PathBuf::from(format!("{}.release.json", archive.display()))
}

fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MANIFEST {
        return Err("release metadata exceeds 128 KiB".into());
    }
    Ok(bytes)
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > limit {
        return Err(format!("file empty or too large: {}", path.display()).into());
    }
    Ok(bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_file(path: &Path) -> Result<(String, u64)> {
    let size = fs::metadata(path)?.len();
    if size == 0 || size > MAX_ASSET {
        return Err(format!("file empty or larger than 512 MiB: {}", path.display()).into());
    }
    let (digest, _, read) = digest_reader(File::open(path)?)?;
    if read != size {
        return Err("file changed while hashing".into());
    }
    Ok((digest, size))
}

fn digest_reader(mut reader: impl Read) -> Result<(String, Vec<u8>, u64)> {
    let mut hash = Sha256::new();
    let mut prefix = Vec::new();
    let mut count = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        if count > MAX_EXPANDED {
            return Err("archive entry exceeds the expanded size limit".into());
        }
        if prefix.len() < buffer.len() {
            prefix.extend_from_slice(&buffer[..read.min(buffer.len() - prefix.len())]);
        }
        hash.update(&buffer[..read]);
    }
    Ok((hex(hash.finalize()), prefix, count))
}

fn safe_path(text: &str) -> Result<String> {
    let name = text.trim_end_matches('/');
    if name.is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(char::is_control)
        || name
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.ends_with('.') || p.ends_with(' '))
    {
        return Err(format!("unsafe archive path: {text:?}").into());
    }
    Ok(name.into())
}

fn architecture(prefix: &[u8], platform: &str) -> Result<()> {
    let correct = if platform.starts_with("macos-") {
        let cpu = if platform == "macos-aarch64" {
            0x0100_000c_u32
        } else {
            0x0100_0007
        };
        prefix.len() >= 8
            && prefix[..4] == [0xcf, 0xfa, 0xed, 0xfe]
            && u32::from_le_bytes(prefix[4..8].try_into()?) == cpu
    } else if prefix.len() >= 64 && &prefix[..2] == b"MZ" {
        let offset = u32::from_le_bytes(prefix[60..64].try_into()?) as usize;
        let machine = if platform == "windows-aarch64" {
            0xaa64_u16
        } else {
            0x8664
        };
        prefix.get(offset..offset.saturating_add(4)) == Some(&b"PE\0\0"[..])
            && prefix.get(offset.saturating_add(4)..offset.saturating_add(6))
                == Some(&machine.to_le_bytes()[..])
    } else {
        false
    };
    if !correct {
        return Err(format!("archive executable does not match {platform}").into());
    }
    Ok(())
}

fn inspect_archive(path: &Path, platform: &str) -> Result<String> {
    if !PLATFORMS.contains(&platform) {
        return Err("unsupported archive platform".into());
    }
    let mut names = BTreeSet::new();
    let mut binary = None;
    let mut total = 0_u64;
    let mut check_entry =
        |name: String, directory: bool, size: u64, reader: &mut dyn Read| -> Result<()> {
            let name = safe_path(&name)?;
            if !names.insert(name.to_lowercase()) || names.len() > 20_000 {
                return Err("archive contains duplicate paths or too many entries".into());
            }
            total = total
                .checked_add(size)
                .ok_or("expanded archive size overflow")?;
            if total > MAX_EXPANDED {
                return Err("archive exceeds the expanded size limit".into());
            }
            let main = if platform.starts_with("macos-") {
                if name != "Still.app" && !name.starts_with("Still.app/") {
                    return Err("Mac archive root must be Still.app".into());
                }
                "Still.app/Contents/MacOS/Still"
            } else {
                if ![
                    "Still.exe",
                    "Readme.txt",
                    "ThirdPartyNotices.txt",
                    "LICENSE",
                    "LICENSE.txt",
                    "Licenses",
                ]
                .contains(&name.as_str())
                    && !name.starts_with("Licenses/")
                {
                    return Err(format!("unexpected Windows archive path: {name}").into());
                }
                "Still.exe"
            };
            if name == main {
                if directory || size == 0 {
                    return Err("archive executable is empty or a directory".into());
                }
                let (hash, prefix, read) = digest_reader(reader)?;
                if read != size {
                    return Err("archive executable size mismatch".into());
                }
                architecture(&prefix, platform)?;
                binary = Some(hash);
            }
            Ok(())
        };
    if platform.starts_with("macos-") {
        let mut archive = tar::Archive::new(GzDecoder::new(File::open(path)?));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            if !kind.is_file() && !kind.is_dir() {
                return Err("release archives must not contain links or special files".into());
            }
            let name = entry
                .path()?
                .to_str()
                .ok_or("archive path is not UTF-8")?
                .to_owned();
            let size = entry.size();
            check_entry(name, kind.is_dir(), size, &mut entry)?;
        }
    } else {
        let mut archive = zip::ZipArchive::new(File::open(path)?)?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            if entry
                .unix_mode()
                .is_some_and(|mode| ![0, 0o100000, 0o040000].contains(&(mode & 0o170000)))
            {
                return Err("release archives must not contain links or special files".into());
            }
            let name = std::str::from_utf8(entry.name_raw())?.to_owned();
            let directory = entry.is_dir();
            let size = entry.size();
            check_entry(name, directory, size, &mut entry)?;
        }
    }
    let required = if platform.starts_with("macos-") {
        vec![
            "still.app/contents/info.plist",
            "still.app/contents/_codesignature/coderesources",
            "still.app/contents/resources/thirdpartynotices.txt",
        ]
    } else {
        vec!["readme.txt", "thirdpartynotices.txt"]
    };
    if required.iter().any(|name| !names.contains(*name))
        || !names
            .iter()
            .any(|name| name.contains("licenses/rust/copyright-library.html"))
    {
        return Err("archive is missing its app metadata, signature, or license notices".into());
    }
    binary.ok_or_else(|| "archive has no Still executable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "still-release-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options(pairs: &[(&str, &str)]) -> Options {
        Options(
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        )
    }

    fn fixture_binary(platform: &str) -> Vec<u8> {
        if platform.starts_with("macos-") {
            let mut binary = vec![0xcf, 0xfa, 0xed, 0xfe];
            binary.extend(
                if platform.ends_with("aarch64") {
                    0x0100_000c_u32
                } else {
                    0x0100_0007
                }
                .to_le_bytes(),
            );
            binary
        } else {
            let mut binary = vec![0; 128];
            binary[..2].copy_from_slice(b"MZ");
            binary[60..64].copy_from_slice(&64_u32.to_le_bytes());
            binary[64..68].copy_from_slice(b"PE\0\0");
            binary[68..70].copy_from_slice(
                &if platform.ends_with("aarch64") {
                    0xaa64_u16
                } else {
                    0x8664
                }
                .to_le_bytes(),
            );
            binary
        }
    }

    fn fixture_archive(path: &Path, platform: &str, extra: Option<&str>) {
        let binary = fixture_binary(platform);
        let mut files: Vec<(&str, &[u8])> = if platform.starts_with("macos-") {
            vec![
                ("Still.app/Contents/MacOS/Still", &binary),
                ("Still.app/Contents/Info.plist", b"plist fixture"),
                (
                    "Still.app/Contents/_CodeSignature/CodeResources",
                    b"signature fixture",
                ),
                (
                    "Still.app/Contents/Resources/ThirdPartyNotices.txt",
                    b"notices",
                ),
                (
                    "Still.app/Contents/Resources/Licenses/Rust/COPYRIGHT-library.html",
                    b"copyright",
                ),
            ]
        } else {
            vec![
                ("Still.exe", &binary),
                ("Readme.txt", b"readme"),
                ("ThirdPartyNotices.txt", b"notices"),
                ("Licenses/Rust/COPYRIGHT-library.html", b"copyright"),
            ]
        };
        if let Some(extra) = extra {
            files.push((extra, b"unexpected"));
        }
        if platform.starts_with("macos-") {
            let encoder = flate2::write::GzEncoder::new(
                File::create(path).unwrap(),
                flate2::Compression::default(),
            );
            let mut archive = tar::Builder::new(encoder);
            for (name, bytes) in files {
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o755);
                header.set_size(bytes.len() as u64);
                header.set_cksum();
                archive.append_data(&mut header, name, bytes).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap();
        } else {
            let mut archive = zip::ZipWriter::new(File::create(path).unwrap());
            for (name, bytes) in files {
                archive
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                archive.write_all(bytes).unwrap();
            }
            archive.finish().unwrap();
        }
    }

    fn fixture_release(directory: &Path, version: &str, key: &SigningKey) {
        let key_path = directory.join("private.key");
        if !key_path.exists() {
            fs::write(&key_path, STANDARD.encode(key.to_bytes())).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        fs::write(directory.join("config.json"), serde_json::to_vec(&serde_json::json!({
            "enabled": true,
            "manifest_url": "https://github.com/owner/repo/releases/latest/download/updates.json",
            "public_key": STANDARD.encode(key.verifying_key().to_bytes()),
        })).unwrap()).unwrap();
        for platform in PLATFORMS {
            let path = directory.join(asset_name(version, platform).unwrap());
            fixture_archive(&path, platform, None);
            let (sha256, size) = digest_file(&path).unwrap();
            fs::write(
                sidecar(&path),
                json_bytes(&Inspection {
                    schema: 1,
                    app_id: APP_ID.into(),
                    version: version.into(),
                    platform: platform.into(),
                    artifact: path.file_name().unwrap().to_string_lossy().into_owned(),
                    sha256,
                    size,
                    binary_sha256: inspect_archive(&path, platform).unwrap(),
                    production: true,
                    macos_notarized: platform.starts_with("macos-").then_some(true),
                    macos_designated_requirement: platform.starts_with("macos-").then(|| {
                        "identifier \"app.still.screenshot\" and anchor apple generic".into()
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        }
    }

    fn sign_fixture(directory: &Path, version: &str) -> Options {
        options(&[
            (
                "--private-key",
                directory.join("private.key").to_str().unwrap(),
            ),
            ("--artifacts", directory.to_str().unwrap()),
            ("--version", version),
            ("--repo", "owner/repo"),
            ("--config", directory.join("config.json").to_str().unwrap()),
            ("--require-platforms", &PLATFORMS.join(",")),
        ])
    }

    fn verify_fixture(directory: &Path) -> Options {
        options(&[
            (
                "--manifest",
                directory.join("updates.json").to_str().unwrap(),
            ),
            (
                "--signature",
                directory.join("updates.json.sig").to_str().unwrap(),
            ),
            ("--config", directory.join("config.json").to_str().unwrap()),
            ("--artifacts", directory.to_str().unwrap()),
            ("--repo", "owner/repo"),
            ("--require-platforms", &PLATFORMS.join(",")),
        ])
    }

    #[test]
    fn all_four_platforms_sign_verify_and_reject_modified_downloads() {
        let directory = TestDirectory::new();
        fixture_release(&directory.0, "2.5.0", &SigningKey::from_bytes(&[42; 32]));
        sign(sign_fixture(&directory.0, "2.5.0")).unwrap();
        verify(verify_fixture(&directory.0)).unwrap();
        assert!(
            sign(sign_fixture(&directory.0, "2.5.0")).is_err(),
            "must not overwrite a signed feed"
        );
        let path = directory
            .0
            .join(asset_name("2.5.0", "windows-aarch64").unwrap());
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"tampered")
            .unwrap();
        assert!(
            verify(verify_fixture(&directory.0)).is_err(),
            "must reject modified download bytes"
        );
        let manifest = directory.0.join("updates.json");
        OpenOptions::new()
            .append(true)
            .open(manifest)
            .unwrap()
            .write_all(b" ")
            .unwrap();
        assert!(
            verify(verify_fixture(&directory.0))
                .unwrap_err()
                .to_string()
                .contains("signature")
        );
    }

    #[test]
    fn refuses_development_inspections_and_unexpected_archive_layouts() {
        let directory = TestDirectory::new();
        fixture_release(&directory.0, "2.5.0", &SigningKey::from_bytes(&[42; 32]));
        let archive = directory
            .0
            .join(asset_name("2.5.0", "macos-aarch64").unwrap());
        let mut inspection: Inspection =
            serde_json::from_slice(&fs::read(sidecar(&archive)).unwrap()).unwrap();
        inspection.production = false;
        fs::write(sidecar(&archive), json_bytes(&inspection).unwrap()).unwrap();
        assert!(sign(sign_fixture(&directory.0, "2.5.0")).is_err());
        let zip = directory.0.join("unexpected.zip");
        fixture_archive(&zip, "windows-x86_64", Some("wrapper/Still.exe"));
        assert!(inspect_archive(&zip, "windows-x86_64").is_err());
        let mac = directory.0.join("unexpected.tar.gz");
        fixture_archive(&mac, "macos-aarch64", Some("._Still.app"));
        assert!(inspect_archive(&mac, "macos-aarch64").is_err());
    }

    #[test]
    fn personal_mac_releases_require_explicit_policy_and_signed_metadata() {
        let directory = TestDirectory::new();
        fixture_release(&directory.0, "2.5.0", &SigningKey::from_bytes(&[42; 32]));
        for platform in ["macos-aarch64", "macos-x86_64"] {
            let path = sidecar(&directory.0.join(asset_name("2.5.0", platform).unwrap()));
            let mut inspection: Inspection =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            inspection.macos_notarized = Some(false);
            inspection.macos_designated_requirement = Some("identifier \"app.still.screenshot\" and certificate leaf = H\"0123456789012345678901234567890123456789\"".into());
            fs::write(path, json_bytes(&inspection).unwrap()).unwrap();
        }
        assert!(sign(sign_fixture(&directory.0, "2.5.0")).is_err());
        let mut arguments = sign_fixture(&directory.0, "2.5.0");
        arguments
            .0
            .insert("--allow-personal-macos".into(), "true".into());
        sign(arguments).unwrap();
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(directory.0.join("updates.json")).unwrap()).unwrap();
        assert_eq!(
            manifest.assets["macos-aarch64"].macos_notarized,
            Some(false)
        );
        assert!(verify(verify_fixture(&directory.0)).is_err());
        let mut arguments = verify_fixture(&directory.0);
        arguments
            .0
            .insert("--allow-personal-macos".into(), "true".into());
        verify(arguments).unwrap();
        assert!(!stable_requirement(
            "cdhash H\"0123456789012345678901234567890123456789\""
        ));
    }

    #[test]
    fn previous_signed_version_and_pinned_key_are_enforced() {
        let previous = TestDirectory::new();
        let next = TestDirectory::new();
        let key = SigningKey::from_bytes(&[42; 32]);
        fixture_release(&previous.0, "2.5.0", &key);
        sign(sign_fixture(&previous.0, "2.5.0")).unwrap();
        fixture_release(&next.0, "2.4.0", &key);
        let mut sign_options = sign_fixture(&next.0, "2.4.0");
        sign_options.0.insert(
            "--previous-manifest".into(),
            previous.0.join("updates.json").to_str().unwrap().into(),
        );
        sign_options.0.insert(
            "--previous-signature".into(),
            previous.0.join("updates.json.sig").to_str().unwrap().into(),
        );
        assert!(
            sign(sign_options)
                .unwrap_err()
                .to_string()
                .contains("newer")
        );
        fs::write(
            next.0.join("private.key"),
            STANDARD.encode(SigningKey::from_bytes(&[43; 32]).to_bytes()),
        )
        .unwrap();
        assert!(
            sign(sign_fixture(&next.0, "2.4.0"))
                .unwrap_err()
                .to_string()
                .contains("pinned public key")
        );
    }

    #[test]
    fn stable_versions_and_asset_names_are_unambiguous() {
        for version in ["2.5.0-beta.1", "2.5.0+build", "v2.5.0", "../2.5.0"] {
            assert!(stable_version(version).is_err());
        }
        assert_eq!(
            asset_name("2.5.0", "macos-aarch64").unwrap(),
            "Still-2.5.0-macos-arm64.tar.gz"
        );
        assert!(asset_name("2.5.0", "linux-x86_64").is_err());
        assert!(repo_name("owner/repo").is_ok());
        assert!(repo_name("owner/../../repo").is_err());
    }

    #[test]
    fn archive_paths_and_executable_architectures_are_checked() {
        for path in [
            "../Still.exe",
            "/Still.exe",
            "Still.app/../evil",
            "C:/Still.exe",
            "Licenses/a\\evil",
            "Licenses/CON.",
        ] {
            assert!(safe_path(path).is_err(), "{path}");
        }
        let mut macho = vec![0xcf, 0xfa, 0xed, 0xfe];
        macho.extend(0x0100_000c_u32.to_le_bytes());
        assert!(architecture(&macho, "macos-aarch64").is_ok());
        assert!(architecture(&macho, "macos-x86_64").is_err());
        let mut pe = vec![0; 128];
        pe[..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&64_u32.to_le_bytes());
        pe[64..68].copy_from_slice(b"PE\0\0");
        pe[68..70].copy_from_slice(&0x8664_u16.to_le_bytes());
        assert!(architecture(&pe, "windows-x86_64").is_ok());
        assert!(architecture(&pe, "windows-aarch64").is_err());
    }

    #[test]
    fn signed_bytes_and_archive_hashes_detect_tampering() {
        let key = SigningKey::from_bytes(&[42; 32]);
        let bytes = b"{\"schema\":1}\n";
        let signature = key.sign(bytes);
        assert!(key.verifying_key().verify_strict(bytes, &signature).is_ok());
        assert!(
            key.verifying_key()
                .verify_strict(b"{\"schema\":1}", &signature)
                .is_err()
        );
        let (hash, _, size) = digest_reader(&b"abc"[..]).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
