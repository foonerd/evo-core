//! `tar.gz` / `tar.xz` / `zip` as in `PLUGIN_PACKAGING` §9.
//!
//! On Unix, all three formats preserve the permission bits of files
//! inside the bundle. For tar (gz or xz), the underlying tar archive
//! natively carries mode. For zip, pack records Unix mode via the
//! standard external-attributes field and extract restores it via
//! `std::os::unix::fs::PermissionsExt`. Without this, a plugin's
//! `plugin.bin` +x bit is silently lost across a zip round-trip, and
//! the steward cannot `exec` it at admission.

use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use walkdir::WalkDir;
use xz2::read::XzDecoder;
use xz2::write::XzEncoder;
use zip::write::SimpleFileOptions;
use zip::write::ZipWriter;
use zip::ZipArchive;

/// Archive container used by `pack` and `install`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackFormat {
    TarGz,
    TarXz,
    Zip,
}

/// Infer format from a filename extension, or return `None`.
pub fn from_path_extension(p: &Path) -> Option<PackFormat> {
    let s = p.to_string_lossy().to_lowercase();
    if s.ends_with(".tar.gz") || s.ends_with(".tgz") {
        return Some(PackFormat::TarGz);
    }
    if s.ends_with(".tar.xz") || s.ends_with(".txz") {
        return Some(PackFormat::TarXz);
    }
    if s.ends_with(".zip") {
        return Some(PackFormat::Zip);
    }
    None
}

/// When `--out` has no clear extension, default to `PLUGIN_TOOL` §7.
pub fn default_format() -> PackFormat {
    PackFormat::TarGz
}

/// Read first bytes to guess format; on failure, caller can try `from_path_extension`
pub fn sniff_bytes_head(path: &Path) -> std::io::Result<Option<PackFormat>> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 6];
    let n = f.read(&mut buf)?;
    if n >= 2 && buf[0] == 0x50 && buf[1] == 0x4B {
        return Ok(Some(PackFormat::Zip));
    }
    if n >= 2 && buf[0] == 0x1f && buf[1] == 0x8b {
        return Ok(Some(PackFormat::TarGz));
    }
    if n >= 6
        && buf[0] == 0xfd
        && buf[1] == 0x37
        && buf[2] == 0x7a
        && buf[3] == 0x58
        && buf[4] == 0x5a
        && buf[5] == 0x00
    {
        return Ok(Some(PackFormat::TarXz));
    }
    Ok(None)
}

/// Sniff first (magic), then file extension, then error.
pub fn detect_format(archive: &Path) -> anyhow::Result<PackFormat> {
    if let Some(f) = sniff_bytes_head(archive)
        .with_context(|| format!("read {}", archive.display()))?
    {
        return Ok(f);
    }
    if let Some(f) = from_path_extension(archive) {
        return Ok(f);
    }
    anyhow::bail!("unrecognised archive: {}", archive.display());
}

/// Pack a plugin directory into a single top-level directory named `root_name`.
pub fn pack(
    source_dir: &Path,
    root_name: &str,
    out: &Path,
    format: PackFormat,
) -> Result<(), anyhow::Error> {
    if !source_dir.is_dir() {
        anyhow::bail!("not a directory: {}", source_dir.display());
    }
    if root_name.is_empty()
        || root_name.contains('/')
        || root_name.contains('\\')
    {
        anyhow::bail!("invalid root name: {root_name}");
    }
    if let Some(p) = out.parent() {
        let _ = fs::create_dir_all(p);
    }
    match format {
        PackFormat::TarGz => {
            let file = fs::File::create(out)
                .with_context(|| out.display().to_string())?;
            let enc = GzEncoder::new(file, Compression::default());
            let mut ar = tar::Builder::new(enc);
            ar.append_dir_all(root_name, source_dir)
                .with_context(|| "tar append (gzip)")?;
            ar.finish().with_context(|| "tar finish (gzip)")?;
        }
        PackFormat::TarXz => {
            let file = fs::File::create(out)
                .with_context(|| out.display().to_string())?;
            let enc = XzEncoder::new(file, 6);
            let mut ar = tar::Builder::new(enc);
            ar.append_dir_all(root_name, source_dir)
                .with_context(|| "tar append (xz)")?;
            ar.finish().with_context(|| "tar finish (xz)")?;
        }
        PackFormat::Zip => {
            pack_zip(source_dir, root_name, out)?;
        }
    }
    Ok(())
}

/// Zip-specific pack path, factored out so the Unix mode-preservation
/// dance does not clutter the main match.
fn pack_zip(
    source_dir: &Path,
    root_name: &str,
    out: &Path,
) -> Result<(), anyhow::Error> {
    let file =
        fs::File::create(out).with_context(|| out.display().to_string())?;
    let mut zw = ZipWriter::new(std::io::BufWriter::new(file));
    let base_opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for w in WalkDir::new(source_dir).min_depth(1) {
        let w = w?;
        let file_path = w.path();
        if file_path == source_dir {
            continue;
        }
        let rel = file_path
            .strip_prefix(source_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if file_path.is_dir() {
            continue;
        }
        let zip_name = format!(
            "{}/{}",
            root_name,
            rel.to_string_lossy().replace('\\', "/")
        );
        let opts = zip_file_opts(file_path, base_opts)?;
        let mut f = fs::File::open(file_path)?;
        let mut v = vec![];
        f.read_to_end(&mut v)?;
        zw.start_file(&zip_name, opts)?;
        Write::write_all(&mut zw, &v)?;
    }
    zw.finish()?;
    Ok(())
}

/// Apply the source file's Unix permissions to the zip entry, so
/// `extract` can restore them. Windows: returns `base_opts` unchanged;
/// zip preserves no permission bits on platforms without the concept.
fn zip_file_opts(
    _file_path: &Path,
    base_opts: SimpleFileOptions,
) -> std::io::Result<SimpleFileOptions> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(_file_path)?.permissions().mode();
        Ok(base_opts.unix_permissions(mode & 0o777))
    }
    #[cfg(not(unix))]
    {
        Ok(base_opts)
    }
}

/// Extract the archive to `out` and return the single top-level directory path.
pub fn extract(archive: &Path, out: &Path) -> Result<PathBuf, anyhow::Error> {
    let format = detect_format(archive)?;
    if out.exists() {
        let n = fs::read_dir(out)?.count();
        if n > 0 {
            anyhow::bail!("extract target must be empty: {}", out.display());
        }
    } else {
        fs::create_dir_all(out)
            .with_context(|| format!("create {}", out.display()))?;
    }

    match format {
        PackFormat::TarGz => {
            let f = fs::File::open(archive)
                .with_context(|| format!("open {}", archive.display()))?;
            let dec = GzDecoder::new(f);
            let mut ar = tar::Archive::new(dec);
            ar.unpack(out).with_context(|| "tar.gz unpack")?;
        }
        PackFormat::TarXz => {
            let f = fs::File::open(archive)
                .with_context(|| format!("open {}", archive.display()))?;
            let dec = XzDecoder::new(f);
            let mut ar = tar::Archive::new(dec);
            ar.unpack(out).with_context(|| "tar.xz unpack")?;
        }
        PackFormat::Zip => {
            extract_zip(archive, out)?;
        }
    }
    single_top_level_dir(out)
}

/// Zip-specific extract path; also applies Unix mode bits recorded
/// by `pack_zip` (external attributes via `unix_permissions`).
fn extract_zip(archive: &Path, out: &Path) -> Result<(), anyhow::Error> {
    let f = fs::File::open(archive)
        .with_context(|| format!("open {}", archive.display()))?;
    let mut ar = ZipArchive::new(f)?;
    for i in 0..ar.len() {
        let mut file = ar.by_index(i).with_context(|| "zip by_index")?;
        let relp = match file.enclosed_name() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => continue,
        };
        let p = out.join(&relp);
        if file.is_dir() {
            let _ = fs::create_dir_all(&p);
            continue;
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)
                .with_context(|| parent.display().to_string())?;
        }
        let unix_mode = file.unix_mode();
        let mut o =
            fs::File::create(&p).with_context(|| p.display().to_string())?;
        std::io::copy(&mut file, &mut o)
            .with_context(|| p.display().to_string())?;
        // Close the file before chmod to guarantee the permissions we
        // set are the permissions the next open sees.
        drop(o);
        apply_unix_mode(&p, unix_mode)
            .with_context(|| p.display().to_string())?;
    }
    Ok(())
}

/// Apply a Unix mode (low 9 bits) recovered from a zip entry to a
/// file on disk. No-op on non-Unix and when the zip entry carries no
/// mode.
fn apply_unix_mode(_p: &Path, _mode: Option<u32>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(m) = _mode {
            let perms = std::fs::Permissions::from_mode(m & 0o777);
            std::fs::set_permissions(_p, perms)?;
        }
    }
    Ok(())
}

fn single_top_level_dir(out: &Path) -> Result<PathBuf, anyhow::Error> {
    let entries: Vec<std::fs::DirEntry> = fs::read_dir(out)
        .with_context(|| out.display().to_string())?
        .filter_map(|e| e.ok())
        .collect();
    if entries.is_empty() {
        anyhow::bail!("empty archive or unpack: {}", out.display());
    }
    if entries.len() == 1 {
        let p = entries[0].path();
        if p.is_dir() {
            return Ok(p);
        }
    }
    anyhow::bail!(
        "expected exactly one top-level directory; got {} entries in {}",
        entries.len(),
        out.display()
    );
}
