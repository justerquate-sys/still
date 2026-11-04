#[cfg(not(windows))]
use std::fs;
use std::{path::Path, process::Command};

use super::{Error, Result, archive};

pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(windows)]
    windows_private_dir(path)?;
    #[cfg(not(any(unix, windows)))]
    fs::create_dir(path)?;
    Ok(())
}

pub(crate) fn verify_private_dir(path: &Path) -> Result<()> {
    archive::plain_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        let metadata = fs::symlink_metadata(path)?;
        // SAFETY: geteuid takes no pointers and returns the process owner.
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { geteuid() } {
            return Err(Error::new(
                "update staging directory is not private to this user",
            ));
        }
    }
    Ok(())
}

pub(crate) fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = command;
}

#[cfg(target_os = "macos")]
pub(crate) fn verify_bundle(path: &Path) -> Result<()> {
    archive::plain_dir(path)?;
    archive::plain_dir(&path.join("Contents"))?;
    archive::plain_dir(&path.join("Contents/MacOS"))?;
    archive::plain_file(&path.join("Contents/Info.plist"))?;
    archive::plain_file(&path.join("Contents/MacOS/Still"))?;
    for (key, expected) in [
        ("CFBundleIdentifier", super::APP_ID),
        ("CFBundleExecutable", "Still"),
        ("CFBundlePackageType", "APPL"),
    ] {
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(path.join("Contents/Info.plist"))
            .output()?;
        if !output.status.success()
            || output.stdout.len() > 1024
            || String::from_utf8_lossy(&output.stdout).trim() != expected
        {
            return Err(Error::new("update is not a Still application bundle"));
        }
    }
    let status = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(path)
        .output()?;
    if !status.status.success() {
        return Err(Error::new(
            "Still update failed macOS code signature verification",
        ));
    }
    Ok(())
}

/// Exchange without a window in which the canonical installation is absent.
/// The old installation is retained as `backup` for rollback and recovery.
pub(crate) fn replace(target: &Path, replacement: &Path, backup: &Path) -> Result<()> {
    if backup.try_exists()? {
        return Err(Error::new("update backup path is already occupied"));
    }
    #[cfg(target_os = "macos")]
    {
        exchange(target, replacement)?;
        if let Err(error) = fs::rename(replacement, backup) {
            // Keep the old canonical app in place if its backup cannot be saved.
            exchange(target, replacement).map_err(|rollback| {
                Error::new(format!(
                    "could not save update backup ({error}); rollback failed ({rollback})"
                ))
            })?;
            return Err(error.into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows_replace(target, replacement, backup)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        fs::rename(target, backup)?;
        if let Err(error) = fs::rename(replacement, target) {
            fs::rename(backup, target).map_err(|rollback| {
                Error::new(format!(
                    "replacement failed ({error}); rollback failed ({rollback})"
                ))
            })?;
            return Err(error.into());
        }
        Ok(())
    }
}

pub(crate) fn rollback(target: &Path, backup: &Path, failed: &Path) -> Result<()> {
    replace(target, backup, failed)
}

pub(crate) fn restart(target: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("/usr/bin/open")
            .arg("-n")
            .arg(target)
            .status()?;
        if !status.success() {
            return Err(Error::new("macOS could not reopen the updated Still app"));
        }
    }
    #[cfg(windows)]
    {
        Command::new(target).spawn()?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = target;
        return Err(Error::new("this platform cannot restart Still"));
    }
    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(target_os = "macos")]
fn exchange(a: &Path, b: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn renamex_np(
            from: *const std::ffi::c_char,
            to: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let a =
        CString::new(a.as_os_str().as_bytes()).map_err(|_| Error::new("invalid install path"))?;
    let b =
        CString::new(b.as_os_str().as_bytes()).map_err(|_| Error::new("invalid staging path"))?;
    // SAFETY: valid NUL-terminated filenames live for the call. RENAME_SWAP is
    // the Darwin atomic exchange flag; both paths are on the same filesystem.
    if unsafe { renamex_np(a.as_ptr(), b.as_ptr(), 0x0000_0002) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn windows_private_dir(path: &Path) -> Result<()> {
    use std::ffi::c_void;
    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateDirectoryW(path: *const u16, security: *const SecurityAttributes) -> i32;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    // Protected DACL: only this directory's owner and SYSTEM. Child objects
    // inherit these rights instead of a shared installation folder's ACL.
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: live UTF-16 string and writable descriptor pointer; Windows owns
    // the returned descriptor until it is released with LocalFree below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit: 0,
    };
    // SAFETY: both buffers/descriptor remain valid for CreateDirectoryW.
    let result = unsafe { CreateDirectoryW(wide(path).as_ptr(), &attributes) };
    let error = std::io::Error::last_os_error();
    // SAFETY: this is the descriptor allocated by the conversion API.
    unsafe {
        LocalFree(descriptor);
    }
    if result == 0 {
        return Err(error.into());
    }
    Ok(())
}

#[cfg(windows)]
fn windows_replace(target: &Path, replacement: &Path, backup: &Path) -> Result<()> {
    use std::{
        ffi::c_void,
        time::{Duration, Instant},
    };
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> i32;
    }
    let target_w = wide(target);
    let replacement_w = wide(replacement);
    let backup_w = wide(backup);
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        // SAFETY: the three NUL-terminated filename buffers outlive the call;
        // unused reserved pointers are null, as required by ReplaceFileW.
        if unsafe {
            ReplaceFileW(
                target_w.as_ptr(),
                replacement_w.as_ptr(),
                backup_w.as_ptr(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0
        {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        // ReplaceFile may have made partial progress for documented filesystem
        // errors. Restore the old executable before reporting such a failure.
        if !target.try_exists()? && backup.try_exists()? {
            windows_restore_missing(target, backup)?;
            return Err(error.into());
        }
        if !replacement.try_exists()? || backup.try_exists()? {
            return Err(error.into());
        }
        if !matches!(error.raw_os_error(), Some(5 | 32 | 33 | 1224)) || Instant::now() >= deadline {
            return Err(error.into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(windows)]
fn windows_restore_missing(target: &Path, backup: &Path) -> Result<()> {
    use std::{
        fs::{File, OpenOptions},
        io,
        time::{Duration, Instant},
    };
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(existing: *const u16, new: *const u16) -> i32;
    }
    // Do not rename the old backup again: antivirus may still hold a handle
    // denying DELETE. A fresh verified copy can be moved while preserving that
    // backup, and MoveFileW refuses to overwrite a concurrent installation.
    let restore = backup.with_file_name("restore-previous.exe");
    let expected = archive::hash_file(backup)?;
    let mut from = File::open(backup)?;
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&restore)?;
    io::copy(&mut from, &mut to)?;
    to.sync_all()?;
    drop(to);
    drop(from);
    if archive::hash_file(&restore)? != expected {
        return Err(Error::new(
            "previous Still executable could not be verified for recovery",
        ));
    }
    let restore_w = wide(&restore);
    let target_w = wide(target);
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        // SAFETY: live UTF-16 filenames; this operation never overwrites an
        // existing target. Both files are on the same installation volume.
        if unsafe { MoveFileW(restore_w.as_ptr(), target_w.as_ptr()) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if target.try_exists()? {
            if archive::hash_file(target)? == expected {
                return Ok(());
            }
            return Err(Error::new(
                "Still changed during update recovery; the verified backup was preserved",
            ));
        }
        if !matches!(error.raw_os_error(), Some(5 | 32 | 33 | 1224)) || Instant::now() >= deadline {
            return Err(Error::new(format!(
                "could not restore Still ({error}); previous executable retained at {}",
                backup.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
