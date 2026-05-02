//! `evo-plugin-tool` - `lint`, `sign`, `verify`, `pack`, `install` (see `docs/engineering/PLUGIN_TOOL.md`).

mod archive;
mod bundle;
mod catalogue_cmd;
mod exit_code;
mod install;
mod lint;
mod pack_cmd;
mod paths;
mod sign;
mod verify_cmd;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

const MAX_URL_DEFAULT: u64 = paths::DEFAULT_MAX_URL_BYTES;

/// Exit: 0 ok, 1 usage/manifest, 2 trust, 3 io, 4 network
#[derive(Parser)]
#[command(
    name = "evo-plugin-tool",
    about = "Plugin author CLI: lint, sign, verify, pack, install",
    version
)]
struct Cli {
    #[command(subcommand)]
    sub: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// Validate `manifest.toml` and the artefact path
    Lint { plugin_dir: PathBuf },
    /// Operations on catalogue documents.
    Catalogue {
        #[command(subcommand)]
        sub: CatalogueSub,
    },
    /// Write `manifest.sig` (ed25519 over `signing_message`, per evo_trust)
    Sign {
        plugin_dir: PathBuf,
        /// PKCS#8 PEM private key (cleartext)
        #[arg(long, value_name = "PEMFILE")]
        key: PathBuf,
    },
    /// Check signature and authorisation (loads trust from opt + etc and revocations)
    Verify {
        plugin_dir: PathBuf,
        /// Admit unsigned bundles (at `sandbox` trust class only). Off by default.
        #[arg(long)]
        allow_unsigned: bool,
        /// Refuse admission when the manifest declares a trust class stronger
        /// than the signing key's `max_trust_class`. Default behaviour is to
        /// degrade to the key's maximum; this flag opts in to strict refusal.
        #[arg(long)]
        strict_trust: bool,
        #[arg(long, value_name = "DIR", default_value = "/opt/evo/trust")]
        trust_dir_opt: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "/etc/evo/trust.d")]
        trust_dir_etc: PathBuf,
        #[arg(
            long,
            value_name = "FILE",
            default_value = "/etc/evo/revocations.toml"
        )]
        revocations: PathBuf,
    },
    /// Create `.tar.gz` (default) / `.tar.xz` / `.zip` of the bundle
    Pack {
        plugin_dir: PathBuf,
        /// Output file (format from extension) or use `<n>-<ver>.<ext>` in cwd
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Archive format. One of `tar-gz`, `tar-xz`, `zip`. Default `tar-gz`.
        #[arg(long, value_enum)]
        format: Option<PackFormatArg>,
    },
    /// Unpack/accept a local directory, file, or URL, verify, install under --to
    Install {
        /// Path to bundle, archive, or `http(s)://` URL
        source: String,
        #[arg(
            long,
            default_value = paths::DEFAULT_SEARCH_ROOT,
            value_name = "DIR"
        )]
        to: PathBuf,
        /// Same semantics as `verify --allow-unsigned`.
        #[arg(long)]
        allow_unsigned: bool,
        /// Same semantics as `verify --strict-trust`.
        #[arg(long)]
        strict_trust: bool,
        #[arg(long, value_name = "DIR", default_value = "/opt/evo/trust")]
        trust_dir_opt: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "/etc/evo/trust.d")]
        trust_dir_etc: PathBuf,
        #[arg(
            long,
            value_name = "FILE",
            default_value = "/etc/evo/revocations.toml"
        )]
        revocations: PathBuf,
        /// chown(1) argument (e.g. `nobody:audio`); runs `chown -R` on the installed path (Unix)
        #[arg(long, value_name = "USER:GROUP")]
        chown: Option<String>,
        #[arg(long, default_value_t = MAX_URL_DEFAULT, value_name = "N")]
        max_url_bytes: u64,
    },
}

#[derive(Subcommand)]
enum CatalogueSub {
    /// Parse and validate a catalogue document. Surfaces parser
    /// errors (missing required fields, schema_version out of range)
    /// as a non-zero exit. With `--schema-version N` additionally
    /// pins the document's `schema_version` to N exactly.
    Lint {
        /// Path to the catalogue TOML document.
        path: PathBuf,
        /// If set, additionally require the document to declare
        /// `schema_version = <N>`. Useful at distribution-author
        /// time to catch a fixture-update slip-through.
        #[arg(long, value_name = "N")]
        schema_version: Option<u32>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum PackFormatArg {
    TarGz,
    TarXz,
    Zip,
}

impl From<PackFormatArg> for archive::PackFormat {
    fn from(v: PackFormatArg) -> Self {
        match v {
            PackFormatArg::TarGz => Self::TarGz,
            PackFormatArg::TarXz => Self::TarXz,
            PackFormatArg::Zip => Self::Zip,
        }
    }
}

fn run(cli: Cli) -> Result<(), anyhow::Error> {
    match cli.sub {
        Sub::Lint { plugin_dir } => lint::run(&plugin_dir),
        Sub::Catalogue { sub } => match sub {
            CatalogueSub::Lint {
                path,
                schema_version,
            } => catalogue_cmd::lint(&path, schema_version),
        },
        Sub::Sign { plugin_dir, key } => sign::run(&plugin_dir, &key),
        Sub::Verify {
            plugin_dir,
            allow_unsigned,
            strict_trust,
            trust_dir_opt,
            trust_dir_etc,
            revocations,
        } => verify_cmd::run(
            &plugin_dir,
            &verify_cmd::VerifyArgs {
                allow_unsigned,
                degrade_trust: !strict_trust,
                trust_dir_opt,
                trust_dir_etc,
                revocations_path: revocations,
            },
        ),
        Sub::Pack {
            plugin_dir,
            out,
            format,
        } => pack_cmd::run(&plugin_dir, out.as_deref(), format.map(Into::into)),
        Sub::Install {
            source,
            to,
            allow_unsigned,
            strict_trust,
            trust_dir_opt,
            trust_dir_etc,
            revocations,
            chown,
            max_url_bytes,
        } => install::run(
            &source,
            &to,
            &verify_cmd::VerifyArgs {
                allow_unsigned,
                degrade_trust: !strict_trust,
                trust_dir_opt,
                trust_dir_etc,
                revocations_path: revocations,
            },
            chown.as_deref(),
            max_url_bytes,
        ),
    }
}

fn main() -> ExitCode {
    // Route clap diagnostics through our documented exit-code contract
    // (PLUGIN_TOOL.md section 8): 0 for help/version, 1 for every other
    // CLI-usage error. Clap's own default is 2 for usage errors, which
    // would collide with our documented "trust / signature" exit code.
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            let code: u8 = match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => 0,
                _ => 1,
            };
            return ExitCode::from(code);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::from(exit_code::code_from_error(&e))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_paths_are_documented() {
        use crate::paths;
        assert_eq!(paths::DEFAULT_SEARCH_ROOT, "/var/lib/evo/plugins");
    }
}
