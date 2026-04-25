//! `pack` — bundle archives per `PLUGIN_PACKAGING` §9.

use std::path::Path;

use crate::archive::{self, PackFormat};

pub fn run(
    plugin_dir: &Path,
    out: Option<&Path>,
    format: Option<PackFormat>,
) -> Result<(), anyhow::Error> {
    use anyhow::Context;
    let b = crate::bundle::load_out_of_process_bundle(plugin_dir)
        .with_context(|| format!("load bundle {}", plugin_dir.display()))?;
    let root = b.manifest.plugin.name.as_str();
    let fmt = format
        .or_else(|| out.and_then(archive::from_path_extension))
        .unwrap_or_else(archive::default_format);
    let out_path: std::path::PathBuf = match out {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => {
            let ext = match fmt {
                PackFormat::TarGz => "tar.gz",
                PackFormat::TarXz => "tar.xz",
                PackFormat::Zip => "zip",
            };
            let v = b.manifest.plugin.version.to_string();
            std::path::PathBuf::from(format!("{root}-{v}.{ext}"))
        }
    };
    archive::pack(plugin_dir, root, &out_path, fmt)
        .with_context(|| "write archive")?;
    eprintln!("wrote {}", out_path.display());
    Ok(())
}
