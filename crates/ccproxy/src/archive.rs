//! Just enough ZIP to unpack a release, and no more.
//!
//! The updater has to turn `commandcode-proxy-vX-windows-x86_64.zip` into a
//! directory tree. The obvious move is the `zip` crate, but it pulls a family
//! of dependencies (aes, zstd, bzip2, constant_time_eq, …) for features this
//! needs none of: the packages are deflate-only, and the only consumer is an
//! updater that already has to distrust its input.
//!
//! So this is a deliberately small reader: locate the central directory, walk
//! its entries, inflate each one with the deflate decoder already in the tree
//! (`flate2`, via `ureq`'s gzip support), and write the files out. What it
//! gains is the thing an updater actually needs — a byte budget and a path
//! policy it fully controls:
//!
//! * **No path escapes.** `..`, absolute names, drive letters and backslash
//!   separators are rejected outright. A release archive decides where files
//!   land, and a crafted one must not be able to write outside the staging
//!   directory it was given.
//! * **No size bombs.** Declared uncompressed sizes are checked against a cap
//!   before anything is allocated, and the inflated output is capped again
//!   while reading (a header can lie).
//!
//! It also does not support: encrypted entries, ZIP64, multi-disk, or any
//! compression method other than stored/deflate. Those are *errors*, not
//! silent skips — a package this cannot read is a package it refuses.

use std::path::{Component, Path, PathBuf};

/// Refuse an archive that claims to expand beyond this. The Windows package is
/// ~8 MB compressed / ~30 MB expanded; the cap is generous but finite.
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// One file's cap. The binaries are the largest members.
const MAX_ENTRY_BYTES: u64 = 128 * 1024 * 1024;
/// A pathologically nested archive is a decompression bomb by another name.
const MAX_ENTRIES: usize = 4096;

const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_LOCAL: u32 = 0x0403_4b50;

/// One file from the archive.
#[derive(Debug)]
pub struct Entry {
    /// Relative path with forward slashes, exactly as stored.
    pub name: String,
    pub data: Vec<u8>,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at.checked_add(2)?)?;
    let mut buf = [0u8; 2];
    buf.copy_from_slice(slice);
    Some(u16::from_le_bytes(buf))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(slice);
    Some(u32::from_le_bytes(buf))
}

/// The offset of the end-of-central-directory record, searched backwards
/// because it sits at the tail behind a comment of up to 64 KiB.
fn eocd_offset(bytes: &[u8]) -> Option<usize> {
    let tail = 22usize; // minimal EOCD
    if bytes.len() < tail {
        return None;
    }
    let start = bytes.len().saturating_sub(tail.saturating_add(65_535));
    let mut at = bytes.len().saturating_sub(tail);
    loop {
        if u32_at(bytes, at) == Some(SIG_EOCD) {
            return Some(at);
        }
        if at == start {
            return None;
        }
        at = at.checked_sub(1)?;
    }
}

/// Reject anything that could land outside the destination.
///
/// `Component::Normal` only for every segment: no `..`, no `RootDir`, no
/// prefix (a Windows drive). Backslashes are rejected separately because
/// `Path` on Windows treats them as separators while the ZIP spec does not —
/// `a\..\..\b` would otherwise pass a `Component` walk and then be split by
/// the filesystem.
fn safe_relative(name: &str) -> Option<PathBuf> {
    // A leading slash is an absolute name. Joining it onto the destination
    // would happen to stay inside, but an archive that says "absolute" is not
    // one whose paths should be trusted at all.
    if name.is_empty() || name.starts_with('/') || name.contains('\\') || name.contains(':') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in name.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        let candidate = Path::new(part);
        let mut components = candidate.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(seg)), None) => out.push(seg),
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Read every file in `bytes`.
pub fn read(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let eocd =
        eocd_offset(bytes).ok_or_else(|| "不是有效的 zip（找不到目录尾记录）".to_string())?;
    let count =
        usize::from(u16_at(bytes, eocd.checked_add(10).ok_or("zip 截断")?).ok_or("zip 截断")?);
    let cd_size =
        u64::from(u32_at(bytes, eocd.checked_add(12).ok_or("zip 截断")?).ok_or("zip 截断")?);
    let cd_offset =
        usize::try_from(u32_at(bytes, eocd.checked_add(16).ok_or("zip 截断")?).ok_or("zip 截断")?)
            .map_err(|_| "zip 目录偏移越界".to_string())?;
    if count > MAX_ENTRIES {
        return Err(format!("zip 条目过多（{count}）"));
    }
    if cd_size > MAX_TOTAL_BYTES {
        return Err("zip 目录异常大".to_string());
    }

    let mut entries = Vec::with_capacity(count);
    let mut at = cd_offset;
    let mut total = 0u64;
    for _ in 0..count {
        if u32_at(bytes, at) != Some(SIG_CENTRAL) {
            return Err("zip 目录项签名不符".to_string());
        }
        let method = u16_at(bytes, at.checked_add(10).ok_or("zip 截断")?).ok_or("zip 截断")?;
        let compressed =
            u64::from(u32_at(bytes, at.checked_add(20).ok_or("zip 截断")?).ok_or("zip 截断")?);
        let uncompressed =
            u64::from(u32_at(bytes, at.checked_add(24).ok_or("zip 截断")?).ok_or("zip 截断")?);
        let name_len =
            usize::from(u16_at(bytes, at.checked_add(28).ok_or("zip 截断")?).ok_or("zip 截断")?);
        let extra_len =
            usize::from(u16_at(bytes, at.checked_add(30).ok_or("zip 截断")?).ok_or("zip 截断")?);
        let comment_len =
            usize::from(u16_at(bytes, at.checked_add(32).ok_or("zip 截断")?).ok_or("zip 截断")?);
        let local_offset = usize::try_from(
            u32_at(bytes, at.checked_add(42).ok_or("zip 截断")?).ok_or("zip 截断")?,
        )
        .map_err(|_| "zip 条目偏移越界".to_string())?;

        let name_at = at.checked_add(46).ok_or("zip 截断")?;
        let name_bytes = bytes
            .get(name_at..name_at.checked_add(name_len).ok_or("zip 截断")?)
            .ok_or("zip 截断")?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();

        total = total.saturating_add(uncompressed);
        if total > MAX_TOTAL_BYTES {
            return Err("zip 解压后体积超过上限".to_string());
        }
        if uncompressed > MAX_ENTRY_BYTES {
            return Err(format!("zip 条目过大：{name}"));
        }

        at = name_at
            .checked_add(name_len)
            .and_then(|v| v.checked_add(extra_len))
            .and_then(|v| v.checked_add(comment_len))
            .ok_or("zip 截断")?;

        // Directory entries carry no data; the writer creates parents anyway.
        if name.ends_with('/') {
            continue;
        }
        let data = entry_data(bytes, local_offset, compressed, uncompressed, method, &name)?;
        entries.push(Entry { name, data });
    }
    Ok(entries)
}

/// Slice and inflate one entry, using the central directory's sizes.
///
/// The local header repeats the name and extra fields with *different* lengths
/// than the central one (the central copy may carry a comment, the local one a
/// data descriptor), so the payload offset has to be recomputed from the local
/// header — reusing the central lengths silently reads the wrong bytes.
fn entry_data(
    bytes: &[u8],
    local_offset: usize,
    compressed: u64,
    uncompressed: u64,
    method: u16,
    name: &str,
) -> Result<Vec<u8>, String> {
    if u32_at(bytes, local_offset) != Some(SIG_LOCAL) {
        return Err(format!("zip 本地头签名不符：{name}"));
    }
    let name_len = usize::from(
        u16_at(bytes, local_offset.checked_add(26).ok_or("zip 截断")?).ok_or("zip 截断")?,
    );
    let extra_len = usize::from(
        u16_at(bytes, local_offset.checked_add(28).ok_or("zip 截断")?).ok_or("zip 截断")?,
    );
    let start = local_offset
        .checked_add(30)
        .and_then(|v| v.checked_add(name_len))
        .and_then(|v| v.checked_add(extra_len))
        .ok_or("zip 截断")?;
    let end = start
        .checked_add(usize::try_from(compressed).map_err(|_| "zip 条目过大".to_string())?)
        .ok_or("zip 截断")?;
    let raw = bytes.get(start..end).ok_or("zip 数据越界")?;
    match method {
        0 => Ok(raw.to_vec()),
        8 => inflate(raw, uncompressed, name),
        other => Err(format!("zip 使用了不支持的压缩方式 {other}：{name}")),
    }
}

/// Inflate raw deflate data, capped at the declared size plus one byte so a
/// lying header cannot expand without bound.
fn inflate(raw: &[u8], declared: u64, name: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let cap = declared.saturating_add(1).min(MAX_ENTRY_BYTES);
    let mut out = Vec::new();
    let decoder = flate2::read::DeflateDecoder::new(raw);
    match decoder.take(cap).read_to_end(&mut out) {
        Ok(_) => {}
        Err(e) => return Err(format!("zip 解压失败（{name}）：{e}")),
    }
    if u64::try_from(out.len()).unwrap_or(u64::MAX) > declared {
        return Err(format!("zip 条目实际体积超过声明值：{name}"));
    }
    Ok(out)
}

/// Unpack `entries` under `dir`, creating parents as needed.
///
/// Returns the relative paths written, for the caller's log line. An entry
/// that fails the path policy aborts the whole unpack: a package that contains
/// one such name is not a package to install half of.
pub fn write_to(entries: &[Entry], dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::with_capacity(entries.len());
    for entry in entries {
        let relative = safe_relative(&entry.name).ok_or_else(|| {
            format!(
                "压缩包内有可疑路径，已拒绝解压：{}",
                entry.name.chars().take(80).collect::<String>()
            )
        })?;
        let target = dir.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("无法创建目录：{e}"))?;
        }
        std::fs::write(&target, &entry.data)
            .map_err(|e| format!("无法写入 {}：{e}", target.display()))?;
        written.push(relative);
    }
    Ok(written)
}

/// Unpack `bytes` under `dir`.
pub fn extract(bytes: &[u8], dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = read(bytes)?;
    if entries.is_empty() {
        return Err("zip 内没有文件".to_string());
    }
    write_to(&entries, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an archive whose members are described explicitly, so a member
    /// can be declared as deflate (or as an unsupported method) without
    /// patching bytes afterwards.
    ///
    /// `(name, body as stored in the archive, method, uncompressed length)`.
    fn archive_with(files: &[(&str, &[u8], u16, u32)]) -> Vec<u8> {
        let mut locals = Vec::new();
        let mut central = Vec::new();
        let mut offset = 0u32;
        for (name, body, method, uncompressed) in files {
            let name_bytes = name.as_bytes();
            let stored_len = u32::try_from(body.len()).unwrap();
            let mut local = Vec::new();
            local.extend_from_slice(&SIG_LOCAL.to_le_bytes());
            local.extend_from_slice(&20u16.to_le_bytes()); // version
            local.extend_from_slice(&0u16.to_le_bytes()); // flags
            local.extend_from_slice(&method.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes()); // time
            local.extend_from_slice(&0u16.to_le_bytes()); // date
            local.extend_from_slice(&0u32.to_le_bytes()); // crc (unchecked)
            local.extend_from_slice(&stored_len.to_le_bytes());
            local.extend_from_slice(&uncompressed.to_le_bytes());
            local.extend_from_slice(&u16::try_from(name_bytes.len()).unwrap().to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes()); // extra
            local.extend_from_slice(name_bytes);
            local.extend_from_slice(body);
            locals.extend_from_slice(&local);

            central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // made by
            central.extend_from_slice(&20u16.to_le_bytes()); // needed
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&stored_len.to_le_bytes());
            central.extend_from_slice(&uncompressed.to_le_bytes());
            central.extend_from_slice(&u16::try_from(name_bytes.len()).unwrap().to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name_bytes);
            offset = offset.saturating_add(u32::try_from(local.len()).unwrap());
        }
        let mut out = locals;
        let cd_offset = u32::try_from(out.len()).unwrap();
        out.extend_from_slice(&central);
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&u16::try_from(files.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&u16::try_from(files.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&u32::try_from(central.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Stored members only — the common case in these tests.
    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let described: Vec<(&str, &[u8], u16, u32)> = files
            .iter()
            .map(|(name, data)| (*name, *data, 0, u32::try_from(data.len()).unwrap()))
            .collect();
        archive_with(&described)
    }

    #[test]
    fn a_stored_archive_round_trips() {
        let bytes = archive(&[("service/ccproxy.exe", b"binary"), ("package.json", b"{}")]);
        let entries = read(&bytes).expect("readable");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries.first().map(|e| e.name.as_str()),
            Some("service/ccproxy.exe")
        );
        assert_eq!(
            entries.first().map(|e| e.data.as_slice()),
            Some(&b"binary"[..])
        );

        let dir = std::env::temp_dir().join(format!("ccproxy-archive-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let written = extract(&bytes, &dir).expect("extract");
        assert_eq!(written.len(), 2);
        assert_eq!(
            std::fs::read(dir.join("service").join("ccproxy.exe")).expect("file"),
            b"binary"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_traversal_name_is_refused_before_anything_is_written() {
        for hostile in [
            "../../evil.exe",
            "../evil.exe",
            "a/../../evil.exe",
            "C:/evil.exe",
            "a\\..\\evil.exe",
            "/abs/evil.exe",
        ] {
            assert!(
                safe_relative(hostile).is_none(),
                "{hostile} must not be accepted"
            );
        }
        assert_eq!(
            safe_relative("./service/ccproxy.exe"),
            Some(PathBuf::from("service").join("ccproxy.exe"))
        );
        // A whole archive is rejected, not just the bad member: half an
        // install is worse than none.
        let bytes = archive(&[("ok.txt", b"fine"), ("../evil.exe", b"nope")]);
        let entries = read(&bytes).expect("readable");
        let dir = std::env::temp_dir().join(format!("ccproxy-archive-evil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = write_to(&entries, &dir).expect_err("must refuse");
        assert!(err.contains("可疑路径"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_or_unknown_archive_is_an_error_not_a_panic() {
        assert!(read(b"").is_err());
        assert!(read(b"not a zip at all").is_err());
        let mut bytes = archive(&[("a.txt", b"hello")]);
        bytes.truncate(40);
        assert!(read(&bytes).is_err());
        // A deflate-only reader must refuse a method it cannot inflate.
        let zstd = archive_with(&[("a.txt", b"hello", 93, 5)]);
        let err = read(&zstd).expect_err("unsupported method");
        assert!(err.contains("压缩方式"), "{err}");
    }

    #[test]
    fn a_deflated_member_is_inflated() {
        use flate2::write::DeflateEncoder;
        use std::io::Write;
        let payload = b"repeated payload ".repeat(64);
        let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&payload).expect("encode");
        let compressed = encoder.finish().expect("finish");
        assert!(
            compressed.len() < payload.len(),
            "deflate must actually shrink"
        );
        let bytes = archive_with(&[(
            "payload.bin",
            &compressed,
            8,
            u32::try_from(payload.len()).unwrap(),
        )]);
        let entries = read(&bytes).expect("inflated");
        assert_eq!(entries.first().map(|e| e.data.clone()), Some(payload));
    }

    #[test]
    fn a_member_that_expands_past_its_declared_size_is_refused() {
        // A header that understates the uncompressed size is how a
        // decompression bomb gets past a size check, so the reader trusts the
        // header only up to the cap and then verifies the result.
        use flate2::write::DeflateEncoder;
        use std::io::Write;
        let payload = b"x".repeat(1024);
        let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&payload).expect("encode");
        let compressed = encoder.finish().expect("finish");
        let bytes = archive_with(&[("bomb.bin", &compressed, 8, 16)]);
        let err = read(&bytes).expect_err("declared 16 bytes cannot hold 1024");
        assert!(err.contains("超过声明值"), "{err}");
    }
}
