//! `install` - Strategy B: obtain bundle, `verify`, promote into `search_root`.
//!
//! Atomicity (PLUGIN_TOOL.md section 3, normative): failing operations
//! must not leave a partial or half-valid tree inside any configured
//! `search_roots`. The copy-and-replace is staged through a hidden
//! sibling directory (`.<name>.installing.<pid>`) and promoted via a
//! same-filesystem `rename`, which is atomic on Unix and on NTFS when
//! `MoveFileEx` is used (std::fs::rename maps to MoveFileExW with
//! MOVEFILE_REPLACE_EXISTING analog on Windows). If any step before
//! the final rename fails, the destination path is untouched; the
//! staging directory is left on disk with a clear error message so
//! the operator can clean it up or retry.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::archive;
use crate::verify_cmd::{self, VerifyArgs};

/// Bring a bundle onto disk, verify, then atomically promote to
/// `to/<plugin.name>/`.
pub fn run(
    source: &str,
    to: &Path,
    verify: &VerifyArgs,
    chown: Option<&str>,
    max_url_bytes: u64,
) -> Result<(), anyhow::Error> {
    // Refuse a privileged install with no ownership story. Without
    // `--chown`, the bundle ends up root-owned 0600 and a steward
    // running as a non-root service user cannot read it. The
    // failure mode is silent at install time but loud and confusing
    // at admission. Force the operator to opt in explicitly: pass
    // `--chown user:group` for the runtime account, or
    // `--chown root:root` if the steward really runs as root.
    if chown.is_none() && running_as_root() {
        anyhow::bail!(
            "refusing to install as root without --chown: the bundle \
             would be created root-owned 0600 and a non-root steward \
             service user could not read it. Pass \
             `--chown <user>:<group>` to set the owning identity (use \
             `--chown root:root` only if the steward runs as root)"
        );
    }

    let work = tempfile::tempdir()?;
    let bundle = obtain_bundle(source, work.path(), max_url_bytes)
        .with_context(|| "resolve install source")?;

    verify_cmd::run(&bundle, verify).with_context(|| {
        "verify (install is atomic only after this succeeds)"
    })?;

    let b = crate::bundle::load_out_of_process_bundle(&bundle)
        .with_context(|| "reload bundle for install path")?;
    let name = b.manifest.plugin.name;
    if !to.is_dir() {
        fs::create_dir_all(to)
            .with_context(|| format!("create search root {}", to.display()))?;
    }

    let dest = to.join(&name);
    // Hidden sibling: not a search_root admission candidate because the
    // leading dot and the `.installing.<pid>` suffix do not match any
    // admitted bundle shape. Keyed on pid so concurrent installs of the
    // same plugin by two operators never race on the same path.
    let staging = to.join(format!(".{name}.installing.{}", std::process::id()));

    // Recover from a previous crashed run of this pid on this path.
    if staging.exists() {
        fs::remove_dir_all(&staging).with_context(|| {
            format!("clear previous staging {}", staging.display())
        })?;
    }

    // Copy into staging first. Any failure here leaves `dest`
    // untouched; the staging path carries a hidden leading dot so it
    // does not look like an admission candidate to the steward.
    copy_dir_all(&bundle, &staging).with_context(|| {
        format!("copy bundle into staging {}", staging.display())
    })?;

    // Only now, with a verified complete tree at `staging`, do we
    // touch `dest`. Order: remove old dest (if any) → atomic rename.
    // On a same-filesystem rename this is as close to atomic as the
    // filesystem supports; a crash between the remove and the rename
    // leaves `dest` absent and `staging` intact, recoverable by a
    // re-run of `install`.
    if dest.exists() {
        fs::remove_dir_all(&dest)
            .with_context(|| format!("remove existing {}", dest.display()))?;
    }
    fs::rename(&staging, &dest).with_context(|| {
        format!(
            "promote staging to {} (staging left at {} on failure)",
            dest.display(),
            staging.display()
        )
    })?;
    eprintln!("installed to {}", dest.display());

    if let Some(spec) = chown {
        chown_tree(&dest, spec)
            .with_context(|| format!("--chown {spec} on {}", dest.display()))?;
    }
    let _ = work;
    Ok(())
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn obtain_bundle(
    source: &str,
    work: &Path,
    max_url_bytes: u64,
) -> Result<PathBuf, anyhow::Error> {
    if is_url(source) {
        let p = work.join("download");
        download_url(source, &p, max_url_bytes)
            .with_context(|| format!("download {source}"))?;
        let ex = work.join("extracted");
        // Wrap the extract error so the operator sees "URL content was
        // not a recognised plugin archive" instead of just "unrecognised
        // archive: /tmp/.../download".
        return archive::extract(&p, &ex).with_context(|| {
            format!(
                "downloaded URL {source} did not contain a recognised \
                 plugin archive (.tar.gz / .tar.xz / .zip)"
            )
        });
    }
    let local = Path::new(source);
    if local.is_dir() {
        return local
            .canonicalize()
            .with_context(|| local.display().to_string());
    }
    if !local.is_file() {
        anyhow::bail!("source not a file, directory, or http(s) URL: {source}");
    }
    let ex = work.join("extracted");
    archive::extract(local, &ex)
}

fn download_url(
    source: &str,
    out_file: &Path,
    max_url_bytes: u64,
) -> Result<(), anyhow::Error> {
    let r = ureq::get(source).call()?;
    if !(200..300).contains(&r.status()) {
        anyhow::bail!("HTTP {} fetching {source}", r.status());
    }
    let cap = max_url_bytes.saturating_add(1);
    let mut v = vec![];
    r.into_reader()
        .take(cap)
        .read_to_end(&mut v)
        .with_context(|| "read response body")?;
    if v.len() as u64 > max_url_bytes {
        anyhow::bail!("response body exceeds {max_url_bytes} bytes");
    }
    if let Some(p) = out_file.parent() {
        let _ = fs::create_dir_all(p);
    }
    fs::write(out_file, v).with_context(|| out_file.display().to_string())?;
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "copy_dir_all: not a directory",
        ));
    }
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            if let Some(p) = to.parent() {
                let _ = fs::create_dir_all(p);
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// True if this process's effective UID is root (Unix). Used by
/// the install path to reject a privileged install that would
/// produce root-owned plugin bundles unreadable by a non-root
/// steward. Backed by `rustix::process::geteuid` which wraps the
/// syscall with a safe API (the workspace forbids `unsafe_code`).
fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        rustix::process::geteuid().is_root()
    }
    #[cfg(not(unix))]
    {
        // Non-Unix targets do not have the same root concept; the
        // chown path itself is also Unix-only, so the guard is
        // moot. Return false so the install path proceeds as
        // before.
        false
    }
}

/// Recursively `chown` via the system `chown(1)` (supports `user:group` the same as the OS).
///
/// Uses `.context(...)` on the `Command::status()` io::Error so that
/// `exit_code::code_from_error`'s `e.is::<io::Error>()` downcast still
/// matches and emits exit 3 (see PLUGIN_TOOL.md section 8).
fn chown_tree(path: &Path, spec: &str) -> Result<(), anyhow::Error> {
    #[cfg(unix)]
    {
        use std::process::Command;
        let status = Command::new("chown")
            .arg("-R")
            .arg(spec)
            .arg(path)
            .status()
            .with_context(|| {
                format!("invoke chown(1) on {}", path.display())
            })?;
        if !status.success() {
            anyhow::bail!(
                "chown -R {spec} {} failed (need privileges?)",
                path.display()
            );
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, spec);
        anyhow::bail!("--chown is only supported on Unix");
    }
}
