//! End-to-end integration tests for `evo-plugin-tool`.
//!
//! Drives the compiled binary via `std::process::Command`, not via
//! library imports (the crate has no library target). Exercises every
//! documented exit code (0 / 1 / 2 / 3), the full sign-then-verify
//! round-trip, all three archive formats with Unix mode preservation,
//! install atomicity (PLUGIN_TOOL.md section 3), and the
//! archive-top-level-rename rule (PLUGIN_TOOL.md section 2).
//!
//! Unix-only: mode-bit assertions require `std::os::unix::fs`, and the
//! CI matrix's Primary targets (see `BUILDING.md` section 3) are
//! Linux-glibc and Linux-musl. Non-Unix targets skip the whole file.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::SigningKey;
use pkcs8::LineEnding;

// -------------------------------------------------------------------
// Shared helpers
// -------------------------------------------------------------------

fn evo_plugin_tool() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_evo-plugin-tool"))
}

/// Deterministic seeds; distinct bytes chosen for hex-dump readability.
const KEY_SEED_A: [u8; 32] = [0xAA; 32];
const KEY_SEED_B: [u8; 32] = [0xBB; 32];

fn key(seed: [u8; 32]) -> SigningKey {
    SigningKey::from_bytes(&seed)
}

/// Valid manifest; plugin name `org.example.echo`, trust class `standard`.
const MANIFEST_STANDARD: &str = r#"[plugin]
name = "org.example.echo"
version = "0.1.0"
contract = 1

[target]
shelf = "example.echo"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "standard"

[prerequisites]
evo_min_version = "0.1.0"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000
"#;

fn write_bundle(plugin_dir: &Path) {
    std::fs::write(plugin_dir.join("manifest.toml"), MANIFEST_STANDARD)
        .unwrap();
    // Shebang content is irrelevant (the tool never execs plugin.bin);
    // the +x bit is the contract we assert across pack/extract/install.
    let bin = plugin_dir.join("plugin.bin");
    std::fs::write(&bin, b"#!/bin/sh\necho evo-test-plugin\n").unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
}

fn write_private_key_pem(path: &Path, seed: [u8; 32]) {
    let k = key(seed);
    let pem = k.to_pkcs8_pem(LineEnding::LF).unwrap();
    std::fs::write(path, pem.as_bytes()).unwrap();
}

fn install_trust_key(
    trust_dir: &Path,
    stem: &str,
    seed: [u8; 32],
    name_prefixes: &[&str],
    max_class: &str,
) {
    let k = key(seed);
    let pem = k.verifying_key().to_public_key_pem(LineEnding::LF).unwrap();
    std::fs::write(trust_dir.join(format!("{stem}.pem")), pem).unwrap();
    let prefixes = name_prefixes
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let meta = format!(
        "[authorisation]\nname_prefixes = [{prefixes}]\n\
         max_trust_class = \"{max_class}\"\n"
    );
    std::fs::write(trust_dir.join(format!("{stem}.meta.toml")), meta).unwrap();
}

fn run_tool(args: &[&str]) -> Output {
    Command::new(evo_plugin_tool())
        .args(args)
        .output()
        .expect("spawn evo-plugin-tool")
}

fn exit_code(o: &Output) -> i32 {
    o.status.code().expect("process exited without a code")
}

fn stderr_str(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn assert_executable(p: &Path) {
    let meta = std::fs::metadata(p).unwrap();
    let mode = meta.permissions().mode();
    assert_ne!(
        mode & 0o111,
        0,
        "expected executable bit on {} (mode={:o})",
        p.display(),
        mode
    );
}

/// Two tempdirs plus a (non-existent) revocations file, for `verify`
/// and `install` invocations that do not need a real revocations list.
struct TrustFixture {
    opt: tempfile::TempDir,
    etc: tempfile::TempDir,
    revs: PathBuf,
}

impl TrustFixture {
    fn new() -> Self {
        let opt = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        let revs = etc.path().join("revocations.toml");
        Self { opt, etc, revs }
    }
    fn opt_path(&self) -> &str {
        self.opt.path().to_str().unwrap()
    }
    fn etc_path(&self) -> &str {
        self.etc.path().to_str().unwrap()
    }
    fn revs_path(&self) -> &str {
        self.revs.to_str().unwrap()
    }
}

// -------------------------------------------------------------------
// Lint
// -------------------------------------------------------------------

#[test]
fn lint_accepts_valid_bundle() {
    let d = tempfile::tempdir().unwrap();
    write_bundle(d.path());
    let o = run_tool(&["lint", d.path().to_str().unwrap()]);
    assert_eq!(exit_code(&o), 0, "stderr: {}", stderr_str(&o));
}

#[test]
fn lint_rejects_missing_manifest_with_exit_3() {
    let d = tempfile::tempdir().unwrap();
    let o = run_tool(&["lint", d.path().to_str().unwrap()]);
    // Missing manifest: std::fs::read_to_string returns io::Error;
    // anyhow preserves it through .with_context so the downcast fires
    // and emits exit 3 per PLUGIN_TOOL.md section 8.
    assert_eq!(
        exit_code(&o),
        3,
        "expected exit 3 (I/O); stderr: {}",
        stderr_str(&o)
    );
}

// -------------------------------------------------------------------
// Sign + Verify round-trip
// -------------------------------------------------------------------

#[test]
fn sign_writes_signature_that_verify_accepts() {
    let plugin = tempfile::tempdir().unwrap();
    let keyd = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();

    write_bundle(plugin.path());
    let keyf = keyd.path().join("signing.pem");
    write_private_key_pem(&keyf, KEY_SEED_A);
    install_trust_key(
        t.opt.path(),
        "example",
        KEY_SEED_A,
        &["org.example.*"],
        "platform",
    );

    let o = run_tool(&[
        "sign",
        plugin.path().to_str().unwrap(),
        "--key",
        keyf.to_str().unwrap(),
    ]);
    assert_eq!(exit_code(&o), 0, "sign stderr: {}", stderr_str(&o));
    assert!(plugin.path().join("manifest.sig").is_file());

    let o = run_tool(&[
        "verify",
        plugin.path().to_str().unwrap(),
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(exit_code(&o), 0, "verify stderr: {}", stderr_str(&o));
}

#[test]
fn verify_rejects_unrecognised_signature_with_exit_2() {
    let plugin = tempfile::tempdir().unwrap();
    let keyd = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();

    write_bundle(plugin.path());
    let keyf = keyd.path().join("signing.pem");
    write_private_key_pem(&keyf, KEY_SEED_A);
    // Signed by A, trust only B - verify must refuse.
    install_trust_key(
        t.opt.path(),
        "other",
        KEY_SEED_B,
        &["org.example.*"],
        "platform",
    );

    let o = run_tool(&[
        "sign",
        plugin.path().to_str().unwrap(),
        "--key",
        keyf.to_str().unwrap(),
    ]);
    assert_eq!(exit_code(&o), 0);

    let o = run_tool(&[
        "verify",
        plugin.path().to_str().unwrap(),
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(
        exit_code(&o),
        2,
        "expected exit 2 (trust); stderr: {}",
        stderr_str(&o)
    );
}

#[test]
fn verify_default_degrades_class_above_key_max() {
    // Manifest declares `standard`; key max is `unprivileged` (lower).
    // Default: degrade to `unprivileged` and admit.
    let plugin = tempfile::tempdir().unwrap();
    let keyd = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();

    write_bundle(plugin.path());
    let keyf = keyd.path().join("signing.pem");
    write_private_key_pem(&keyf, KEY_SEED_A);
    install_trust_key(
        t.opt.path(),
        "example",
        KEY_SEED_A,
        &["org.example.*"],
        "unprivileged",
    );

    assert_eq!(
        exit_code(&run_tool(&[
            "sign",
            plugin.path().to_str().unwrap(),
            "--key",
            keyf.to_str().unwrap(),
        ])),
        0
    );

    let o = run_tool(&[
        "verify",
        plugin.path().to_str().unwrap(),
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(
        exit_code(&o),
        0,
        "degrade default should admit; stderr: {}",
        stderr_str(&o)
    );
}

#[test]
fn verify_strict_mode_refuses_class_above_key_max() {
    // Same configuration as the degrade test, with --strict-trust:
    // refusal, exit 2. The canonical test that the strict flag
    // actually changes behaviour (this was the pre-tightening bug
    // where `--degrade-trust` could not be disabled).
    let plugin = tempfile::tempdir().unwrap();
    let keyd = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();

    write_bundle(plugin.path());
    let keyf = keyd.path().join("signing.pem");
    write_private_key_pem(&keyf, KEY_SEED_A);
    install_trust_key(
        t.opt.path(),
        "example",
        KEY_SEED_A,
        &["org.example.*"],
        "unprivileged",
    );

    assert_eq!(
        exit_code(&run_tool(&[
            "sign",
            plugin.path().to_str().unwrap(),
            "--key",
            keyf.to_str().unwrap(),
        ])),
        0
    );

    let o = run_tool(&[
        "verify",
        plugin.path().to_str().unwrap(),
        "--strict-trust",
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(
        exit_code(&o),
        2,
        "--strict-trust should refuse; stderr: {}",
        stderr_str(&o)
    );
}

// -------------------------------------------------------------------
// Pack / Extract round-trip (exercised via install --allow-unsigned)
// -------------------------------------------------------------------

/// Pack then install-round-trip an unsigned bundle through `archive_out`
/// using the given format. Returns (installed-plugin.bin-path, kept
/// target tempdir) so the caller can inspect the installed tree.
fn pack_then_install(
    plugin: &Path,
    archive_out: &Path,
    format: &str,
) -> (PathBuf, tempfile::TempDir) {
    let o = run_tool(&[
        "pack",
        plugin.to_str().unwrap(),
        "--out",
        archive_out.to_str().unwrap(),
        "--format",
        format,
    ]);
    assert_eq!(
        exit_code(&o),
        0,
        "pack --format {format}; stderr: {}",
        stderr_str(&o)
    );
    assert!(
        archive_out.is_file(),
        "pack did not produce {}",
        archive_out.display()
    );

    let target = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();
    let o = run_tool(&[
        "install",
        archive_out.to_str().unwrap(),
        "--to",
        target.path().to_str().unwrap(),
        "--allow-unsigned",
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(
        exit_code(&o),
        0,
        "install --format {format}; stderr: {}",
        stderr_str(&o)
    );

    let installed = target.path().join("org.example.echo").join("plugin.bin");
    (installed, target)
}

#[test]
fn pack_extract_tar_gz_preserves_executable_bit() {
    let plugin = tempfile::tempdir().unwrap();
    let ad = tempfile::tempdir().unwrap();
    write_bundle(plugin.path());
    let (installed, _t) = pack_then_install(
        plugin.path(),
        &ad.path().join("bundle.tar.gz"),
        "tar-gz",
    );
    assert!(installed.is_file());
    assert_executable(&installed);
}

#[test]
fn pack_extract_tar_xz_preserves_executable_bit() {
    let plugin = tempfile::tempdir().unwrap();
    let ad = tempfile::tempdir().unwrap();
    write_bundle(plugin.path());
    let (installed, _t) = pack_then_install(
        plugin.path(),
        &ad.path().join("bundle.tar.xz"),
        "tar-xz",
    );
    assert!(installed.is_file());
    assert_executable(&installed);
}

#[test]
fn pack_extract_zip_preserves_executable_bit() {
    // This is the path that failed before the archive.rs fix: zip pack
    // did not record Unix mode, extract did not restore it, and a zip
    // round-trip left plugin.bin at mode 0644 - unexecutable.
    let plugin = tempfile::tempdir().unwrap();
    let ad = tempfile::tempdir().unwrap();
    write_bundle(plugin.path());
    let (installed, _t) =
        pack_then_install(plugin.path(), &ad.path().join("bundle.zip"), "zip");
    assert!(installed.is_file());
    assert_executable(&installed);
}

// -------------------------------------------------------------------
// Install: local dir source, rejection, top-level rename, atomicity
// -------------------------------------------------------------------

#[test]
fn install_from_local_directory_promotes_to_target() {
    let plugin = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();
    write_bundle(plugin.path());

    let o = run_tool(&[
        "install",
        plugin.path().to_str().unwrap(),
        "--to",
        target.path().to_str().unwrap(),
        "--allow-unsigned",
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(exit_code(&o), 0, "stderr: {}", stderr_str(&o));

    let dest = target.path().join("org.example.echo");
    assert!(dest.is_dir(), "{} missing", dest.display());
    assert!(dest.join("manifest.toml").is_file());
    assert!(dest.join("plugin.bin").is_file());
    assert_executable(&dest.join("plugin.bin"));
}

#[test]
fn install_rejects_unsigned_bundle_by_default_with_exit_2() {
    // No --allow-unsigned and no manifest.sig: must refuse with exit 2.
    // The normative check (PLUGIN_TOOL.md section 3) is also that no
    // partial tree lands under the search root on failure.
    let plugin = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();
    write_bundle(plugin.path());

    let o = run_tool(&[
        "install",
        plugin.path().to_str().unwrap(),
        "--to",
        target.path().to_str().unwrap(),
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(exit_code(&o), 2);

    // Normative: no partial tree under `search_root` on failure.
    let dest = target.path().join("org.example.echo");
    assert!(
        !dest.exists(),
        "failed install left partial tree at {}",
        dest.display()
    );
    // And also no hidden `.*.installing.<pid>` directory should remain
    // (verify happens before staging is created).
    let leftover_any =
        std::fs::read_dir(target.path()).unwrap().any(|e| e.is_ok());
    assert!(
        !leftover_any,
        "target root should be empty after failed install"
    );
}

#[test]
fn install_renames_top_level_when_archive_dir_differs_from_plugin_name() {
    // PLUGIN_TOOL.md section 2: the final path under `--to` must use
    // manifest.plugin.name regardless of what the archive's top-level
    // directory is called. Craft a .tar.gz whose root is "weirdroot"
    // but whose manifest says "org.example.echo".
    let plugin = tempfile::tempdir().unwrap();
    write_bundle(plugin.path());
    let archive_dir = tempfile::tempdir().unwrap();
    let archive = archive_dir.path().join("bundle.tar.gz");
    pack_tar_gz_with_root(plugin.path(), "weirdroot", &archive);

    let target = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();
    let o = run_tool(&[
        "install",
        archive.to_str().unwrap(),
        "--to",
        target.path().to_str().unwrap(),
        "--allow-unsigned",
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(exit_code(&o), 0, "stderr: {}", stderr_str(&o));

    assert!(target.path().join("org.example.echo").is_dir());
    assert!(!target.path().join("weirdroot").exists());
}

#[test]
fn install_no_staging_left_in_search_root_on_success() {
    // After a successful install, the hidden `.<n>.installing.<pid>`
    // directory must be renamed, not copied; leaving it behind would
    // look like a partial second install to anyone scanning the tree.
    let plugin = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();
    write_bundle(plugin.path());

    let o = run_tool(&[
        "install",
        plugin.path().to_str().unwrap(),
        "--to",
        target.path().to_str().unwrap(),
        "--allow-unsigned",
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
    ]);
    assert_eq!(exit_code(&o), 0, "stderr: {}", stderr_str(&o));

    // Walk immediate children of target: should be exactly
    // "org.example.echo" and nothing else.
    let children: Vec<_> = std::fs::read_dir(target.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        children,
        vec!["org.example.echo".to_string()],
        "unexpected siblings under search root: {children:?}"
    );
}

fn pack_tar_gz_with_root(src: &Path, root: &str, out: &Path) {
    let f = std::fs::File::create(out).unwrap();
    let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
    let mut ar = tar::Builder::new(enc);
    ar.append_dir_all(root, src).unwrap();
    ar.finish().unwrap();
}

// -------------------------------------------------------------------
// Exit-code contract (PLUGIN_TOOL.md section 8)
// -------------------------------------------------------------------

#[test]
fn help_exits_zero() {
    let o = run_tool(&["--help"]);
    assert_eq!(exit_code(&o), 0);
}

#[test]
fn version_exits_zero() {
    let o = run_tool(&["--version"]);
    assert_eq!(exit_code(&o), 0);
}

#[test]
fn unknown_subcommand_returns_exit_1() {
    // Clap defaults to exit 2 for usage errors, which would collide
    // with our documented "trust / signature" exit 2. main() remaps
    // clap errors to exit 1 (usage). This test pins that remapping.
    let o = run_tool(&["no-such-subcommand"]);
    assert_eq!(
        exit_code(&o),
        1,
        "expected exit 1 (usage); stderr: {}",
        stderr_str(&o)
    );
}

#[test]
fn unknown_flag_returns_exit_1() {
    let o = run_tool(&["lint", "--no-such-flag"]);
    assert_eq!(
        exit_code(&o),
        1,
        "expected exit 1 (usage); stderr: {}",
        stderr_str(&o)
    );
}

// -------------------------------------------------------------------
// verify --describe-json (sign-time drift check)
// -------------------------------------------------------------------

#[test]
fn verify_with_matching_describe_json_succeeds() {
    let plugin = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();
    write_bundle(plugin.path());

    // Plugin's runtime describe matches the manifest exactly.
    // The manifest declares request_types = ["echo"]; the JSON
    // mirrors that. No drift; verify succeeds with exit 0.
    let describe_path = plugin.path().join("describe.json");
    std::fs::write(
        &describe_path,
        r#"{"request_types": ["echo"], "course_correct_verbs": [], "accepts_custody": false, "flags": {}}"#,
    )
    .unwrap();

    let o = run_tool(&[
        "verify",
        plugin.path().to_str().unwrap(),
        "--allow-unsigned",
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
        "--describe-json",
        describe_path.to_str().unwrap(),
    ]);
    assert_eq!(exit_code(&o), 0, "stderr: {}", stderr_str(&o));
}

#[test]
fn verify_with_drifted_describe_json_refuses_with_exit_1() {
    let plugin = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();
    write_bundle(plugin.path());

    // Manifest declares request_types = ["echo"]; describe()
    // claims the implementation provides ["echo", "ping"]. The
    // extra "ping" is missing from the manifest — drift on the
    // manifest side. Verify must refuse.
    let describe_path = plugin.path().join("describe.json");
    std::fs::write(
        &describe_path,
        r#"{"request_types": ["echo", "ping"], "course_correct_verbs": [], "accepts_custody": false, "flags": {}}"#,
    )
    .unwrap();

    let o = run_tool(&[
        "verify",
        plugin.path().to_str().unwrap(),
        "--allow-unsigned",
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
        "--describe-json",
        describe_path.to_str().unwrap(),
    ]);
    assert_eq!(exit_code(&o), 1, "stderr: {}", stderr_str(&o));
    assert!(
        stderr_str(&o).contains("manifest drift detected"),
        "expected drift message in stderr, got: {}",
        stderr_str(&o)
    );
    assert!(
        stderr_str(&o).contains("ping"),
        "expected the offending verb \"ping\" in stderr, got: {}",
        stderr_str(&o)
    );
}

#[test]
fn verify_drift_refuses_when_implementation_is_missing_a_verb() {
    let plugin = tempfile::tempdir().unwrap();
    let t = TrustFixture::new();
    write_bundle(plugin.path());

    // Manifest declares request_types = ["echo"]; describe()
    // claims the implementation provides nothing. Drift in the
    // implementation direction.
    let describe_path = plugin.path().join("describe.json");
    std::fs::write(
        &describe_path,
        r#"{"request_types": [], "course_correct_verbs": [], "accepts_custody": false, "flags": {}}"#,
    )
    .unwrap();

    let o = run_tool(&[
        "verify",
        plugin.path().to_str().unwrap(),
        "--allow-unsigned",
        "--trust-dir-opt",
        t.opt_path(),
        "--trust-dir-etc",
        t.etc_path(),
        "--revocations",
        t.revs_path(),
        "--describe-json",
        describe_path.to_str().unwrap(),
    ]);
    assert_eq!(exit_code(&o), 1, "stderr: {}", stderr_str(&o));
    assert!(
        stderr_str(&o).contains("missing in implementation"),
        "expected missing-in-implementation message in stderr, got: {}",
        stderr_str(&o)
    );
    assert!(
        stderr_str(&o).contains("echo"),
        "expected the missing verb \"echo\" in stderr, got: {}",
        stderr_str(&o)
    );
}
