//! Deterministic archiving: a ZIP for Windows and a `.tar.gz` for Linux.
//!
//! Both are byte-reproducible from the same input (spec §5.4): entries are
//! sorted by normalised relative path, names use `/` and are UTF-8, ZIP
//! timestamps are pinned to the DOS minimum, and the tar records fixed
//! mtime/uid/gid with modes from the layout. Archiving the same staged tree
//! twice yields identical SHA-256.

use std::io::Write;

use crc32fast::Hasher;
use flate2::write::DeflateEncoder;
use flate2::Compression;

use crate::stage::StagedFile;
use crate::{DistError, Result};

pub mod zip {
    pub const DEFLATE_METHOD: u16 = 8;
    pub const STORED_METHOD: u16 = 0;
    /// DOS date 1980-01-01, time 00:00 — the legal minimum, pinned so the
    /// archive does not embed build wall-clock time.
    pub const FIXED_TIME: u16 = 0;
    pub const FIXED_DATE: u16 = 0x0021;
}

/// Build the Windows ZIP. Entry names are forward-slash relative paths,
/// deflated, with no enclosing top-level directory.
pub fn build_zip(files: &[StagedFile]) -> Result<Vec<u8>> {
    let mut locals: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let mut offset: u32 = 0;

    for file in files {
        let data =
            std::fs::read(&file.abs).map_err(|e| format!("read {}: {e}", file.abs.display()))?;
        let name = file.rel.replace('\\', "/");
        if name.is_empty() || name.starts_with('/') || name.contains(':') || name.contains("..") {
            return Err(DistError(format!("unsafe zip entry name `{name}`")));
        }
        let name_bytes = name.as_bytes();
        let mut crc = Hasher::new();
        crc.update(&data);
        let crc = crc.finalize();

        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data)?;
        let compressed = encoder.finish()?;
        let (method, stored) = if compressed.len() < data.len() {
            (zip::DEFLATE_METHOD, compressed)
        } else {
            (zip::STORED_METHOD, data.clone())
        };

        let local_start = offset;
        let mut local = Vec::new();
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes()); // version needed
        local.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        local.extend_from_slice(&method.to_le_bytes());
        local.extend_from_slice(&zip::FIXED_TIME.to_le_bytes());
        local.extend_from_slice(&zip::FIXED_DATE.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&u32::try_from(stored.len())?.to_le_bytes());
        local.extend_from_slice(&u32::try_from(data.len())?.to_le_bytes());
        local.extend_from_slice(&u16::try_from(name_bytes.len())?.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // extra length
        local.extend_from_slice(name_bytes);
        local.extend_from_slice(&stored);
        locals.extend_from_slice(&local);
        offset = offset.saturating_add(u32::try_from(local.len())?);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&zip::FIXED_TIME.to_le_bytes());
        central.extend_from_slice(&zip::FIXED_DATE.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&u32::try_from(stored.len())?.to_le_bytes());
        central.extend_from_slice(&u32::try_from(data.len())?.to_le_bytes());
        central.extend_from_slice(&u16::try_from(name_bytes.len())?.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&local_start.to_le_bytes());
        central.extend_from_slice(name_bytes);
    }

    let cd_offset = offset;
    let mut out = locals;
    out.extend_from_slice(&central);
    let cd_size = u32::try_from(central.len())?;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&u16::try_from(files.len())?.to_le_bytes());
    out.extend_from_slice(&u16::try_from(files.len())?.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

/// Build the Linux `.tar.gz`. Modes come from the staged layout; mtime, uid
/// and gid are pinned so the archive is reproducible.
pub fn build_tar_gz(files: &[StagedFile]) -> Result<Vec<u8>> {
    let mut tar = tar::Builder::new(Vec::new());
    for file in files {
        let data =
            std::fs::read(&file.abs).map_err(|e| format!("read {}: {e}", file.abs.display()))?;
        let name = file.rel.replace('\\', "/");
        if name.is_empty() || name.starts_with('/') || name.contains(':') || name.contains("..") {
            return Err(DistError(format!("unsafe tar entry name `{name}`")));
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(file.mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_username("").unwrap_or(());
        header.set_groupname("").unwrap_or(());
        header.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut header, &name, &data[..])?;
    }
    let tar_bytes = tar.into_inner()?;

    // gzip envelope written by hand: MTIME=0, FLG=0, no original filename, OS
    // = Unix. flate2's GzEncoder embeds host metadata, so we emit a raw deflate
    // stream and wrap it ourselves — this is what makes the archive reproducible.
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&tar_bytes)?;
    let deflated = encoder.finish()?;

    let mut crc = Hasher::new();
    crc.update(&tar_bytes);
    let crc = crc.finalize();

    let mut out = Vec::new();
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00]);
    out.extend_from_slice(&0u32.to_le_bytes()); // MTIME
    out.extend_from_slice(&[0x02]); // XFL: maximum compression
    out.extend_from_slice(&[0x03]); // OS: Unix
    out.extend_from_slice(&deflated);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&u32::try_from(tar_bytes.len())?.to_le_bytes()); // ISIZE
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(rel: &str, bytes: &[u8]) -> StagedFile {
        let dir = std::env::temp_dir().join(format!("xtask-archive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        StagedFile {
            rel: rel.to_string(),
            abs: path,
            mode: 0o644,
        }
    }

    #[test]
    fn zip_is_deterministic_and_readable_by_ccproxy() {
        let files = vec![
            staged("CCProxyTray.exe", b"tray"),
            staged("service/ccproxy.exe", b"proxy-binary"),
            staged("service/webui/index.html", b"<html></html>"),
        ];
        let a = build_zip(&files).unwrap();
        let b = build_zip(&files).unwrap();
        assert_eq!(a, b, "same staged tree must archive to identical bytes");

        let entries = ccproxy::archive::read(&a).expect("our own zip must be readable");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "CCProxyTray.exe",
                "service/ccproxy.exe",
                "service/webui/index.html"
            ]
        );
        assert!(names.iter().all(|n| !n.contains('\\')));
    }

    #[test]
    fn zip_rejects_unsafe_names() {
        let bad = vec![staged("../evil.exe", b"x")];
        assert!(build_zip(&bad).is_err());
    }
}
