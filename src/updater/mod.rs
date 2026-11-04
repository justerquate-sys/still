//! Shared, signed GitHub update transport and installation for the native apps.
//! Call checks on a worker thread. Applying only starts after the UI has become
//! idle and explicitly commits the helper to run when this process exits.

mod archive;
mod config;
mod feed;
mod native;

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub use config::Config;
pub use feed::{ArchiveFormat, Asset, MAX_ASSET_BYTES, MAX_MANIFEST_BYTES, Manifest, PLATFORMS};

pub const APP_ID: &str = "app.still.screenshot";
const STAGE_PREFIX: &str = ".still-update-";
const HELPER_ARGUMENT: &str = "--still-update-helper";
#[cfg(windows)]
const HELPER_NAME: &str = "Still-update-helper.exe";
#[cfg(not(windows))]
const HELPER_NAME: &str = "Still-update-helper";

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Error(String);

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self(format!("invalid update JSON: {error}"))
    }
}

pub const fn platform() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return "macos-aarch64";
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return "macos-x86_64";
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return "windows-x86_64";
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        return "windows-aarch64";
    }
    #[allow(unreachable_code)]
    "unsupported"
}

/// The canonical existing installation. Only the actual Still layout is
/// accepted; download metadata never supplies paths to replace or delete.
#[derive(Debug, Clone)]
pub struct InstallContext {
    root: PathBuf,
    executable: PathBuf,
}

impl InstallContext {
    pub fn current() -> Result<Self> {
        Self::from_executable(std::env::current_exe()?)
    }

    pub fn from_executable(path: impl AsRef<Path>) -> Result<Self> {
        archive::plain_file(path.as_ref())?;
        let executable = fs::canonicalize(path)?;
        #[cfg(target_os = "macos")]
        {
            let macos = executable
                .parent()
                .ok_or_else(|| Error::new("invalid Still executable path"))?;
            let contents = macos
                .parent()
                .ok_or_else(|| Error::new("invalid Still bundle path"))?;
            let root = contents
                .parent()
                .ok_or_else(|| Error::new("invalid Still bundle path"))?;
            if executable.file_name().and_then(|s| s.to_str()) != Some("Still")
                || macos.file_name().and_then(|s| s.to_str()) != Some("MacOS")
                || contents.file_name().and_then(|s| s.to_str()) != Some("Contents")
                || root.file_name().and_then(|s| s.to_str()) != Some("Still.app")
            {
                return Err(Error::new(
                    "automatic updates require an installed Still.app bundle",
                ));
            }
            native::verify_bundle(root)?;
            Ok(Self {
                root: root.to_path_buf(),
                executable,
            })
        }
        #[cfg(windows)]
        {
            if !executable
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("Still.exe"))
            {
                return Err(Error::new(
                    "automatic updates require the portable Still.exe installation",
                ));
            }
            Ok(Self {
                root: executable.clone(),
                executable,
            })
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = executable;
            Err(Error::new(
                "Still automatic updates support macOS and Windows",
            ))
        }
    }

    pub fn install_root(&self) -> &Path {
        &self.root
    }
}

/// Files authenticated and fully validated on a background thread. Dropping an
/// unused prepared update removes only its private staging directory.
#[derive(Debug)]
pub struct PreparedUpdate {
    pub version: Version,
    stage: PathBuf,
    context: InstallContext,
    original_hash: [u8; 32],
    applying: AtomicBool,
}

impl PreparedUpdate {
    pub fn version_string(&self) -> String {
        self.version.to_string()
    }
}

impl Drop for PreparedUpdate {
    fn drop(&mut self) {
        if !self.applying.load(Ordering::Acquire) {
            let _ = fs::remove_dir_all(&self.stage);
        }
    }
}

/// A started helper is initially cancellable. Commit immediately before asking
/// the native application to quit. The inherited read pipe then reaches EOF
/// only when the OS closes the parent's descriptors at actual process exit.
#[must_use = "commit_on_exit() before quitting, or drop to cancel the update"]
pub struct ApplyProcess {
    input: Option<ChildStdin>,
    child: Option<Child>,
    stage: PathBuf,
}

impl ApplyProcess {
    pub fn commit_on_exit(mut self) {
        if let Some(mut input) = self.input.take() {
            // A crash/ordinary Quit while start_apply is still on its worker
            // must not turn an uncommitted EOF into an installation request.
            if let Err(error) = input.write_all(b"COMMIT\n") {
                eprintln!("Still update helper could not be committed: {error}");
                self.input = Some(input);
                return;
            }
            // Closing it here would let the helper apply while our old app was
            // still running. The OS owns this one descriptor until process exit.
            std::mem::forget(input);
        }
    }
}

impl Drop for ApplyProcess {
    fn drop(&mut self) {
        if let Some(mut input) = self.input.take() {
            let _ = input.write_all(b"CANCEL\n");
            drop(input);
            if let Some(mut child) = self.child.take() {
                let stage = self.stage.clone();
                let _ = std::thread::Builder::new()
                    .name("still-update-cancel".into())
                    .spawn(move || {
                        let _ = child.wait();
                        // A cancelled helper never replaced the installation.
                        // Reap it and remove only its own now-unused private stage.
                        let _ = fs::remove_dir_all(stage);
                    });
            }
        }
    }
}

/// No networking happens until this function is called with an enabled Config.
/// It runs a single bounded check/download; the native application schedules it.
pub fn check_and_stage(
    config: &Config,
    context: &InstallContext,
) -> Result<Option<PreparedUpdate>> {
    let agent = http_agent();
    let manifest = download_bytes(&agent, &config.manifest_url, MAX_MANIFEST_BYTES)?;
    let signature = download_bytes(&agent, &format!("{}.sig", config.manifest_url), 256)?;
    let current = current_version()?;
    let Some(release) =
        feed::verify_release(&manifest, &signature, &config.key, &current, platform())?
    else {
        return Ok(None);
    };
    let original_hash = archive::hash_file(&context.executable)?;
    let parent = context
        .root
        .parent()
        .ok_or_else(|| Error::new("installation has no writable parent directory"))?;
    let stage = private_stage(parent)?;
    let prepared = PreparedUpdate {
        version: release.version.clone(),
        stage,
        context: context.clone(),
        original_hash,
        applying: AtomicBool::new(false),
    };
    write_private(&prepared.stage.join("manifest.json"), &manifest)?;
    write_private(&prepared.stage.join("manifest.sig"), &signature)?;
    download_asset(&agent, &release.asset, &prepared.stage.join("archive.bin"))?;
    let payload = archive::extract(
        &prepared.stage.join("archive.bin"),
        &prepared.stage.join("payload"),
        release.asset.format,
    )?;
    validate_payload(&payload, &release.version)?;
    Ok(Some(prepared))
}

/// Copies the old executable into staging before launching it. On Windows the
/// helper must never execute from the installed Still.exe that it will replace.
pub fn start_apply(prepared: &PreparedUpdate) -> Result<ApplyProcess> {
    if prepared.applying.swap(true, Ordering::AcqRel) {
        return Err(Error::new("this Still update is already being applied"));
    }
    let result = (|| {
        native::verify_private_dir(&prepared.stage)?;
        if archive::hash_file(&prepared.context.executable)? != prepared.original_hash {
            return Err(Error::new(
                "Still changed after the update was prepared; check again",
            ));
        }
        let helper = prepared.stage.join(HELPER_NAME);
        let mut from = File::open(&prepared.context.executable)?;
        let mut to = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&helper)?;
        io::copy(&mut from, &mut to)?;
        to.sync_all()?;
        // Windows cannot map an executable while its copy still has a write
        // handle. Close both files before hashing or launching the helper.
        drop(to);
        drop(from);
        archive::set_mode(&helper, 0o700)?;
        if archive::hash_file(&helper)? != prepared.original_hash {
            return Err(Error::new("update helper copy could not be verified"));
        }
        let token = stage_token(&prepared.stage)?.to_owned();
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(prepared.stage.join("helper.log"))?;
        archive::set_mode(&prepared.stage.join("helper.log"), 0o600)?;
        let mut command = Command::new(&helper);
        command
            .arg(HELPER_ARGUMENT)
            .arg(token)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        native::hide_console(&mut command);
        let mut child = command.spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| Error::new("update helper could not open its parent pipe"))?;
        Ok(ApplyProcess {
            input: Some(input),
            child: Some(child),
            stage: prepared.stage.clone(),
        })
    })();
    if result.is_err() {
        prepared.applying.store(false, Ordering::Release);
    }
    result
}

/// Handle before creating AppKit/Win32 UI. An arbitrary plan path is never
/// accepted: the helper derives all authority from its own private directory,
/// embedded key, signed feed, and a byte-identical original installation.
pub fn run_helper_from_args() -> Result<bool> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|a| a != HELPER_ARGUMENT) {
        return Ok(false);
    }
    if args.len() != 2 {
        return Err(Error::new("invalid update helper arguments"));
    }
    let token = args[1]
        .to_str()
        .ok_or_else(|| Error::new("invalid update helper token"))?;
    run_helper(token)?;
    Ok(true)
}

fn run_helper(token: &str) -> Result<()> {
    let executable = std::env::current_exe()?;
    archive::plain_file(&executable)?;
    let executable = fs::canonicalize(executable)?;
    if executable.file_name().and_then(|n| n.to_str()) != Some(HELPER_NAME) {
        return Err(Error::new(
            "update helper must run from its separate staging copy",
        ));
    }
    let stage = executable
        .parent()
        .ok_or_else(|| Error::new("invalid helper location"))?;
    if stage_token(stage)? != token {
        return Err(Error::new("update helper stage token mismatch"));
    }
    native::verify_private_dir(stage)?;
    let parent = stage
        .parent()
        .ok_or_else(|| Error::new("invalid helper parent directory"))?;
    #[cfg(target_os = "macos")]
    let target = parent.join("Still.app");
    #[cfg(windows)]
    let target = parent.join("Still.exe");
    #[cfg(not(any(target_os = "macos", windows)))]
    let target = parent.join("unsupported");
    let old_executable = payload_executable(&target);
    if archive::hash_file(&executable)? != archive::hash_file(&old_executable)? {
        return Err(Error::new(
            "installed Still identity differs from its update helper",
        ));
    }

    // Commit is explicit. EOF alone (ordinary Quit/crash while start_apply was
    // on a worker) cancels. Even COMMIT must be followed by actual parent exit:
    // the committed guard retains its writer until the OS closes descriptors.
    let mut parent_input = Vec::new();
    io::stdin().lock().take(16).read_to_end(&mut parent_input)?;
    if !parent_authorized(&parent_input) {
        return Ok(());
    }

    let result = (|| {
        let config = Config::embedded()?
            .ok_or_else(|| Error::new("updates are disabled in this executable"))?;
        let manifest = read_limited(&stage.join("manifest.json"), MAX_MANIFEST_BYTES)?;
        let signature = read_limited(&stage.join("manifest.sig"), 256)?;
        let release = feed::verify_release(
            &manifest,
            &signature,
            &config.key,
            &current_version()?,
            platform(),
        )?
        .ok_or_else(|| Error::new("prepared update is no longer newer than Still"))?;
        let archive_path = stage.join("archive.bin");
        archive::verify_asset(&archive_path, &release.asset)?;
        // Discard the UI-validated extraction and rebuild from the authenticated
        // archive immediately before replacing; staged file edits gain no authority.
        let payload_dir = stage.join("payload");
        if payload_dir.try_exists()? {
            fs::remove_dir_all(&payload_dir)?;
        }
        let payload = archive::extract(&archive_path, &payload_dir, release.asset.format)?;
        validate_payload(&payload, &release.version)?;
        let context = InstallContext::from_executable(&old_executable)?;
        if context.root != target
            || archive::hash_file(&executable)? != archive::hash_file(&old_executable)?
        {
            return Err(Error::new(
                "Still installation changed while the helper waited",
            ));
        }
        apply_transaction(
            &target,
            &payload,
            &stage.join("backup"),
            &stage.join("failed-update"),
            native::restart,
        )?;
        // Keep the old installation and helper.log for recovery. None of these
        // private files are arbitrary caller-supplied cleanup paths.
        for file in ["archive.bin", "manifest.json", "manifest.sig"] {
            let _ = fs::remove_file(stage.join(file));
        }
        let _ = fs::remove_dir(&payload_dir);
        Ok(())
    })();
    if result.is_err() {
        // The parent has already exited. Preserve normal app availability even
        // if revalidation or replacement fails before a new app can launch.
        let _ = native::restart(&target);
    }
    result
}

fn parent_authorized(bytes: &[u8]) -> bool {
    bytes == b"COMMIT\n"
}

fn apply_transaction(
    target: &Path,
    payload: &Path,
    backup: &Path,
    failed: &Path,
    launch: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    native::replace(target, payload, backup)?;
    if let Err(error) = launch(target) {
        native::rollback(target, backup, failed)
            .map_err(|rollback| Error::new(format!("Still could not restart ({error}); rollback failed ({rollback}); backup retained at {}", backup.display())))?;
        return Err(Error::new(format!(
            "Still could not restart; the previous version was restored: {error}"
        )));
    }
    Ok(())
}

fn validate_payload(payload: &Path, expected_version: &Version) -> Result<()> {
    #[cfg(target_os = "macos")]
    native::verify_bundle(payload)?;
    let executable = payload_executable(payload);
    archive::plain_file(&executable)?;
    let mut command = Command::new(executable);
    command
        .arg("--update-info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    native::hide_console(&mut command);
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::new("update identity check has no output pipe"))?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.by_ref().take(4097).read_to_end(&mut bytes)?;
        Ok::<_, io::Error>(bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new("Still update identity check timed out"));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let bytes = reader
        .join()
        .map_err(|_| Error::new("update identity reader failed"))??;
    if !status.success() || bytes.len() > 4096 {
        return Err(Error::new(
            "Still update executable could not identify itself",
        ));
    }
    #[derive(Deserialize)]
    struct Identity {
        app_id: String,
        version: String,
        platform: String,
    }
    let info: Identity = serde_json::from_slice(&bytes)?;
    if info.app_id != APP_ID
        || info.platform != platform()
        || Version::parse(&info.version).ok().as_ref() != Some(expected_version)
    {
        return Err(Error::new(
            "update executable has the wrong application, version or platform",
        ));
    }
    Ok(())
}

fn payload_executable(payload: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        payload.join("Contents/MacOS/Still")
    }
    #[cfg(not(target_os = "macos"))]
    {
        payload.to_path_buf()
    }
}

fn current_version() -> Result<Version> {
    Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|_| Error::new("Still build has an invalid version"))
}

fn stage_token(stage: &Path) -> Result<&str> {
    let name = stage
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::new("invalid update staging directory"))?;
    let token = name
        .strip_prefix(STAGE_PREFIX)
        .ok_or_else(|| Error::new("invalid update staging directory"))?;
    if token.len() != 32
        || !token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(Error::new("invalid update staging token"));
    }
    Ok(token)
}

fn private_stage(parent: &Path) -> Result<PathBuf> {
    archive::plain_dir(parent)?;
    for _ in 0..8 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|e| {
            Error::new(format!("could not create secure update staging token: {e}"))
        })?;
        let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = parent.join(format!("{STAGE_PREFIX}{token}"));
        match native::create_private_dir(&path) {
            Ok(()) => {
                native::verify_private_dir(&path)?;
                return Ok(path);
            }
            Err(_) if path.try_exists()? => continue,
            Err(error) => {
                return Err(Error::new(format!(
                    "Still cannot stage an update beside its installation: {error}"
                )));
            }
        }
    }
    Err(Error::new(
        "could not create a unique update staging directory",
    ))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    archive::set_mode(path, 0o600)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if archive::plain_file(path)?.len() > limit {
        return Err(Error::new("update metadata exceeds its size limit"));
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::new("update metadata exceeds its size limit"));
    }
    Ok(bytes)
}

fn http_agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(5)
        .timeout_global(Some(Duration::from_secs(300)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(270)))
        .user_agent(concat!("Still/", env!("CARGO_PKG_VERSION")))
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

fn response(agent: &ureq::Agent, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
    feed::validate_https(url)?;
    let response = agent
        .get(url)
        .header("Accept-Encoding", "identity")
        .call()
        .map_err(|e| Error::new(format!("Still update download failed: {e}")))?;
    if response.status().as_u16() != 200 {
        return Err(Error::new(
            "Still update server did not return a complete download",
        ));
    }
    Ok(response)
}

fn download_bytes(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = response(agent, url)?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::new("update metadata exceeds its size limit"));
    }
    Ok(bytes)
}

fn download_asset(agent: &ureq::Agent, asset: &Asset, path: &Path) -> Result<()> {
    let mut response = response(agent, &asset.url)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    archive::set_mode(path, 0o600)?;
    let mut source = response.body_mut().as_reader().take(asset.size + 1);
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > asset.size {
            return Err(Error::new("update archive exceeds its declared size"));
        }
        digest.update(&buffer[..n]);
        file.write_all(&buffer[..n])?;
    }
    if bytes != asset.size
        || <[u8; 32]>::from(digest.finalize()) != feed::decode_hash(&asset.sha256)?
    {
        return Err(Error::new(
            "update archive size or SHA-256 verification failed",
        ));
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_preserves_backup_and_failed_launch_restores_original() {
        let dir = private_stage(&std::env::temp_dir()).unwrap();
        for fail_launch in [false, true] {
            let case = dir.join(if fail_launch { "fail" } else { "ok" });
            fs::create_dir(&case).unwrap();
            let target = case.join("Still");
            let payload = case.join("new");
            let backup = case.join("backup");
            let failed = case.join("failed");
            fs::write(&target, b"old executable").unwrap();
            fs::write(&payload, b"new executable").unwrap();
            let result = apply_transaction(&target, &payload, &backup, &failed, |_| {
                if fail_launch {
                    Err(Error::new("synthetic launch error"))
                } else {
                    Ok(())
                }
            });
            if fail_launch {
                assert!(result.is_err());
                assert_eq!(fs::read(&target).unwrap(), b"old executable");
                assert_eq!(fs::read(&failed).unwrap(), b"new executable");
            } else {
                result.unwrap();
                assert_eq!(fs::read(&target).unwrap(), b"new executable");
                assert_eq!(fs::read(&backup).unwrap(), b"old executable");
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_occupied_backup_and_invalid_stage_authority() {
        let dir = private_stage(&std::env::temp_dir()).unwrap();
        assert_eq!(stage_token(&dir).unwrap().len(), 32);
        for path in [
            "/tmp/update-plan.json",
            "/tmp/.still-update-../",
            "/tmp/.still-update-a",
            "/tmp/.still-update-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(stage_token(Path::new(path)).is_err());
        }
        let target = dir.join("Still");
        let payload = dir.join("new");
        let backup = dir.join("backup");
        fs::write(&target, b"old").unwrap();
        fs::write(&payload, b"new").unwrap();
        fs::write(&backup, b"keep").unwrap();
        assert!(native::replace(&target, &payload, &backup).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read(&backup).unwrap(), b"keep");
        fs::remove_dir_all(dir).unwrap();
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "fixture timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn helper_pipe_cancels_on_drop_and_waits_for_committed_parent_exit() {
        let dir = private_stage(&std::env::temp_dir()).unwrap();
        let source = dir.join("pipe-fixture.rs");
        fs::write(&source, r#"use std::io::Read; fn main() { let mut bytes=Vec::new(); std::io::stdin().read_to_end(&mut bytes).unwrap(); let argument=std::env::args_os().nth(1).unwrap(); if argument == "--still-update-helper" { assert_eq!(bytes, b"CANCEL\n"); return; } std::fs::write(argument, bytes).unwrap(); }"#).unwrap();
        let helper = dir.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        assert!(
            Command::new("rustc")
                .arg("--edition=2024")
                .arg(&source)
                .arg("-o")
                .arg(&helper)
                .status()
                .unwrap()
                .success()
        );

        let cancelled_stage = private_stage(&dir).unwrap();
        let cancelled_record = dir.join("cancelled");
        let mut child = Command::new(&helper)
            .arg(&cancelled_record)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        drop(ApplyProcess {
            input: Some(input),
            child: Some(child),
            stage: cancelled_stage.clone(),
        });
        wait_until(|| cancelled_record.exists() && !cancelled_stage.exists());
        assert_eq!(fs::read(&cancelled_record).unwrap(), b"CANCEL\n");

        // Exercise the real helper-copy path: the executable's write handle
        // must be closed before spawn (especially on Windows).
        let copy_stage = private_stage(&dir).unwrap();
        let prepared = PreparedUpdate {
            version: Version::new(9, 8, 7),
            stage: copy_stage.clone(),
            context: InstallContext {
                root: helper.clone(),
                executable: helper.clone(),
            },
            original_hash: archive::hash_file(&helper).unwrap(),
            applying: AtomicBool::new(false),
        };
        let copied_helper = start_apply(&prepared).unwrap();
        assert_eq!(
            archive::hash_file(&copy_stage.join(HELPER_NAME)).unwrap(),
            prepared.original_hash
        );
        drop(prepared); // A live helper must keep its own stage after this drop.
        assert!(copy_stage.exists());
        drop(copied_helper);
        wait_until(|| !copy_stage.exists());

        let stage = private_stage(&dir).unwrap();
        let record = dir.join("committed");
        let ready = dir.join("parent-ready");
        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "updater::tests::committed_parent_fixture",
                "--nocapture",
            ])
            .env("STILL_UPDATE_TEST_PIPE_HELPER", &helper)
            .env("STILL_UPDATE_TEST_PIPE_STAGE", &stage)
            .env("STILL_UPDATE_TEST_PIPE_RECORD", &record)
            .env("STILL_UPDATE_TEST_PIPE_READY", &ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until(|| ready.exists());
        assert!(
            !record.exists(),
            "committing must hold the pipe while the parent remains alive"
        );
        parent.stdin.take().unwrap().write_all(b"x").unwrap();
        wait_until(|| parent.try_wait().unwrap().is_some());
        assert!(parent.wait().unwrap().success());
        wait_until(|| record.exists());
        let committed = fs::read(&record).unwrap();
        assert_eq!(committed, b"COMMIT\n");
        assert!(parent_authorized(&committed));
        // A worker's guard may never reach the UI before ordinary Quit. The
        // actual pipe then closes without a message and must cancel the update.
        let bare_record = dir.join("uncommitted-eof");
        let mut bare = Command::new(&helper)
            .arg(&bare_record)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        drop(bare.stdin.take());
        assert!(bare.wait().unwrap().success());
        assert!(!parent_authorized(&fs::read(&bare_record).unwrap()));
        assert!(!parent_authorized(b"CANCEL\n"));
        assert!(!parent_authorized(b"COMMIT\nCANCEL\n"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn committed_parent_fixture() {
        let Some(helper) = std::env::var_os("STILL_UPDATE_TEST_PIPE_HELPER") else {
            return;
        };
        let stage = PathBuf::from(std::env::var_os("STILL_UPDATE_TEST_PIPE_STAGE").unwrap());
        let record = std::env::var_os("STILL_UPDATE_TEST_PIPE_RECORD").unwrap();
        let ready = std::env::var_os("STILL_UPDATE_TEST_PIPE_READY").unwrap();
        let mut child = Command::new(helper)
            .arg(record)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        ApplyProcess {
            input: Some(input),
            child: Some(child),
            stage,
        }
        .commit_on_exit();
        fs::write(ready, b"ready").unwrap();
        let mut release = [0];
        std::io::stdin().read_exact(&mut release).unwrap();
        std::process::exit(0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn signed_macos_bundle_extracts_verifies_and_rejects_tampering() {
        use std::io::{Seek, SeekFrom};
        let dir = private_stage(&std::env::temp_dir()).unwrap();
        let app = dir.join("Still.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        let source = dir.join("fixture.rs");
        let info = serde_json::json!({"app_id":APP_ID,"version":"9.8.7","platform":platform()})
            .to_string();
        let program = format!(
            "fn main() {{ assert_eq!(std::env::args().nth(1).as_deref(), Some(\"--update-info\")); println!(\"{{}}\", {info:?}); }}"
        );
        fs::write(&source, program).unwrap();
        let status = Command::new("rustc")
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(app.join("Contents/MacOS/Still"))
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(app.join("Contents/Info.plist"), format!(r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>{APP_ID}</string><key>CFBundleExecutable</key><string>Still</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleVersion</key><string>987</string><key>CFBundleShortVersionString</key><string>9.8.7</string></dict></plist>"#)).unwrap();
        let sign = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app)
            .output()
            .unwrap();
        assert!(
            sign.status.success(),
            "{}",
            String::from_utf8_lossy(&sign.stderr)
        );
        let path = dir.join("bundle.tar.gz");
        let gzip = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gzip);
        tar.append_dir_all("Still.app", &app).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let extracted =
            archive::extract(&path, &dir.join("extracted"), ArchiveFormat::TarGz).unwrap();
        validate_payload(&extracted, &Version::new(9, 8, 7)).unwrap();
        assert!(validate_payload(&extracted, &Version::new(9, 8, 8)).is_err());
        // Corrupt the signed Mach-O header: codesign must reject it before any
        // downloaded executable is run again.
        let mut executable = OpenOptions::new()
            .write(true)
            .open(extracted.join("Contents/MacOS/Still"))
            .unwrap();
        executable.seek(SeekFrom::Start(0)).unwrap();
        executable.write_all(b"BAD!").unwrap();
        drop(executable);
        assert!(validate_payload(&extracted, &Version::new(9, 8, 7)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
