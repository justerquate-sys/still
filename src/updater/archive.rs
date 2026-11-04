//! Deliberately narrow extraction: Still.app, or Still.exe and packaged notices.
//! Archive extract/unpack helpers are avoided so links, metadata and permissions
//! cannot escape the private staging directory.

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use super::{
    Error, Result,
    feed::{ArchiveFormat, Asset, decode_hash},
};

pub(crate) const MAX_UNPACKED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 8192;

pub(crate) fn hash_file(path: &Path) -> Result<[u8; 32]> {
    plain_file(path)?;
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(digest.finalize().into())
}

pub(crate) fn verify_asset(path: &Path, asset: &Asset) -> Result<()> {
    let metadata = plain_file(path)?;
    if metadata.len() != asset.size {
        return Err(Error::new("update archive has an incorrect size"));
    }
    if hash_file(path)? != decode_hash(&asset.sha256)? {
        return Err(Error::new("update archive SHA-256 verification failed"));
    }
    Ok(())
}

pub(crate) fn plain_file(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(Error::new(
            "update requires an ordinary file, without links",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::new(
                "update files must not be Windows reparse points",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Error::new("hard-linked update files are not accepted"));
        }
    }
    Ok(metadata)
}

pub(crate) fn plain_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(Error::new(
            "update requires an ordinary directory, without links",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::new(
                "update directories must not be Windows reparse points",
            ));
        }
    }
    Ok(())
}

pub(crate) fn extract(archive: &Path, output: &Path, format: ArchiveFormat) -> Result<PathBuf> {
    plain_file(archive)?;
    fs::create_dir(output)?;
    let mut state = Extraction {
        output,
        format,
        seen: HashSet::new(),
        total: 0,
    };
    match format {
        ArchiveFormat::TarGz => {
            let gzip = flate2::read::GzDecoder::new(File::open(archive)?);
            let mut tar = tar::Archive::new(gzip);
            for entry in tar.entries()? {
                let mut entry = entry?;
                let kind = entry.header().entry_type();
                if !kind.is_file() && !kind.is_dir() {
                    return Err(Error::new(
                        "update archive contains a link or unsupported entry",
                    ));
                }
                let path = entry.path_bytes();
                let name = std::str::from_utf8(&path)
                    .map_err(|_| Error::new("update archive paths must be UTF-8"))?
                    .to_owned();
                let size = entry.size();
                let executable = entry.header().mode()? & 0o111 != 0;
                state.entry(&name, kind.is_dir(), size, executable, &mut entry)?;
            }
        }
        ArchiveFormat::Zip => {
            let mut zip = zip::ZipArchive::new(File::open(archive)?)
                .map_err(|e| Error::new(format!("invalid update ZIP: {e}")))?;
            if zip.len() > MAX_ENTRIES {
                return Err(Error::new("update archive contains too many entries"));
            }
            for i in 0..zip.len() {
                let mut entry = zip
                    .by_index(i)
                    .map_err(|e| Error::new(format!("invalid update ZIP entry: {e}")))?;
                if entry.encrypted() {
                    return Err(Error::new("encrypted update archives are not supported"));
                }
                let mode = entry.unix_mode().unwrap_or(0);
                let kind = mode & 0o170000;
                if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                    return Err(Error::new(
                        "update archive contains a link or unsupported entry",
                    ));
                }
                let name = std::str::from_utf8(entry.name_raw())
                    .map_err(|_| Error::new("update archive paths must be UTF-8"))?
                    .to_owned();
                let is_dir = entry.is_dir();
                let size = entry.size();
                state.entry(&name, is_dir, size, mode & 0o111 != 0, &mut entry)?;
            }
        }
    }
    let payload = output.join(match format {
        ArchiveFormat::TarGz => "Still.app",
        ArchiveFormat::Zip => "Still.exe",
    });
    if format == ArchiveFormat::TarGz {
        plain_dir(&payload)?;
        plain_file(&payload.join("Contents/Info.plist"))?;
        plain_file(&payload.join("Contents/MacOS/Still"))?;
    } else {
        plain_file(&payload)?;
    }
    Ok(payload)
}

struct Extraction<'a> {
    output: &'a Path,
    format: ArchiveFormat,
    seen: HashSet<String>,
    total: u64,
}

impl Extraction<'_> {
    fn entry(
        &mut self,
        name: &str,
        directory: bool,
        size: u64,
        executable: bool,
        source: &mut impl Read,
    ) -> Result<()> {
        let relative = safe_path(name, self.format)?;
        if self.seen.len() >= MAX_ENTRIES
            || !self.seen.insert(name.trim_end_matches('/').to_lowercase())
        {
            return Err(Error::new(
                "duplicate update path or too many archive entries",
            ));
        }
        if (directory && size != 0)
            || self
                .total
                .checked_add(size)
                .is_none_or(|n| n > MAX_UNPACKED_BYTES)
        {
            return Err(Error::new("update archive exceeds its unpacked size limit"));
        }
        self.total += size;
        let path = self.output.join(relative);
        if directory {
            fs::create_dir_all(&path)?;
            set_mode(&path, 0o755)?;
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let n = io::copy(&mut source.take(size + 1), &mut output)?;
        if n != size {
            return Err(Error::new("update archive entry has an incorrect size"));
        }
        output.flush()?;
        output.sync_all()?;
        set_mode(&path, if executable { 0o755 } else { 0o644 })?;
        Ok(())
    }
}

pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn safe_path(name: &str, format: ArchiveFormat) -> Result<PathBuf> {
    let name = name.trim_end_matches('/');
    if name.is_empty()
        || name.len() > 1024
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(Error::new("unsafe update archive path"));
    }
    let components: Vec<_> = name.split('/').collect();
    for part in &components {
        let base = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                base.strip_prefix(prefix)
                    .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
            });
        if part.is_empty()
            || matches!(*part, "." | "..")
            || part.contains(':')
            || part.ends_with(['.', ' '])
            || reserved
        {
            return Err(Error::new("unsafe update archive path"));
        }
    }
    match format {
        ArchiveFormat::TarGz if components[0] == "Still.app" => {}
        ArchiveFormat::Zip
            if [
                "Still.exe",
                "Readme.txt",
                "ThirdPartyNotices.txt",
                "LICENSE",
                "LICENSE.txt",
                "Licenses",
            ]
            .contains(&name)
                || name.starts_with("Licenses/") => {}
        _ => return Err(Error::new("unexpected update archive layout")),
    }
    Ok(components.iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_archive_paths_on_every_host_os() {
        for name in [
            "../Still.app",
            "/Still.app",
            "Still.app/../evil",
            "Still.app//x",
            "Still.app/./x",
            "Still.app/C:/x",
            "Still.app\\x",
            "Still.app/NUL.txt",
            "Still.app/trailing.",
            "Still.app/trailing ",
            "Still.app/a\0b",
        ] {
            assert!(safe_path(name, ArchiveFormat::TarGz).is_err(), "{name}");
        }
        assert!(safe_path("Still.app/Contents/MacOS/Still", ArchiveFormat::TarGz).is_ok());
        assert!(safe_path("Still.exe", ArchiveFormat::Zip).is_ok());
        assert!(safe_path("x/Still.exe", ArchiveFormat::Zip).is_err());
        assert!(safe_path("Still.exe/", ArchiveFormat::TarGz).is_err());
    }

    #[test]
    fn rejects_symlinks_hardlinks_duplicates_and_expansion_bombs() {
        let dir = super::super::private_stage(&std::env::temp_dir()).unwrap();
        let tar_path = dir.join("archive");
        for kind in [tar::EntryType::Symlink, tar::EntryType::Link] {
            let gzip = flate2::write::GzEncoder::new(
                File::create(&tar_path).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_link_name("/tmp/evil").unwrap();
            header.set_cksum();
            tar.append_data(&mut header, "Still.app/link", io::empty())
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
            let output = dir.join(format!("out-{}", kind.as_byte()));
            assert!(extract(&tar_path, &output, ArchiveFormat::TarGz).is_err());
        }
        let out = dir.join("bounds");
        fs::create_dir(&out).unwrap();
        let mut state = Extraction {
            output: &out,
            format: ArchiveFormat::TarGz,
            seen: HashSet::new(),
            total: 0,
        };
        assert!(
            state
                .entry(
                    "Still.app/bomb",
                    false,
                    MAX_UNPACKED_BYTES + 1,
                    false,
                    &mut io::empty()
                )
                .is_err()
        );
        state
            .entry("Still.app/a", false, 1, false, &mut &b"a"[..])
            .unwrap();
        assert!(
            state
                .entry("Still.app/A", false, 1, false, &mut &b"a"[..])
                .is_err()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn detects_wrong_asset_hash_and_size() {
        let dir = super::super::private_stage(&std::env::temp_dir()).unwrap();
        let path = dir.join("asset");
        fs::write(&path, b"trusted").unwrap();
        let mut asset = Asset {
            url: "https://example.invalid/a".into(),
            size: 7,
            sha256: Sha256::digest(b"trusted")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            format: ArchiveFormat::Zip,
            macos_notarized: None,
        };
        verify_asset(&path, &asset).unwrap();
        asset.size = 8;
        assert!(verify_asset(&path, &asset).is_err());
        asset.size = 7;
        fs::write(&path, b"changed").unwrap();
        assert!(verify_asset(&path, &asset).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn extracts_packaged_windows_executable_and_notices_and_rejects_other_layouts() {
        use zip::{ZipWriter, write::SimpleFileOptions};
        let dir = super::super::private_stage(&std::env::temp_dir()).unwrap();
        for (case, name, accepted) in [
            ("valid", "Still.exe", true),
            ("nested", "nested/Still.exe", false),
            ("traversal", "../Still.exe", false),
            ("unexpected", "other.exe", false),
        ] {
            let path = dir.join(format!("{case}.zip"));
            let mut writer = ZipWriter::new(File::create(&path).unwrap());
            writer
                .start_file(
                    name,
                    SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(b"native Windows fixture").unwrap();
            if accepted {
                for name in [
                    "Readme.txt",
                    "ThirdPartyNotices.txt",
                    "LICENSE",
                    "LICENSE.txt",
                    "Licenses/zip-9.3.0/LICENSE",
                    "Licenses/Rust/licenses/MIT.txt",
                ] {
                    writer
                        .start_file(name, SimpleFileOptions::default())
                        .unwrap();
                    writer.write_all(b"packaged notices").unwrap();
                }
            }
            writer.finish().unwrap();
            let result = extract(&path, &dir.join(case), ArchiveFormat::Zip);
            assert_eq!(result.is_ok(), accepted, "{name}");
            if accepted {
                assert_eq!(
                    fs::read(result.unwrap()).unwrap(),
                    b"native Windows fixture"
                );
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
