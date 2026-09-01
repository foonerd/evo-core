// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Schema-driven SDK generator.
//!
//! Reads the canonical wire schema from
//! [`evo::projection_schema::canonical_schema`] and emits five
//! packaged client SDKs (TypeScript, Swift, Kotlin, Python,
//! Rust) under the supplied output directory. Each emitted SDK
//! lands in its own subdirectory matching the language name.
//!
//! ```sh
//! evo-sdk-gen --out-dir ./sdks
//! ```
//!
//! Operators invoke this binary from CI / release scripts to
//! produce the artefacts the vendor distribution publishes. The
//! generator is fully deterministic: identical schema input
//! produces byte-identical output across hosts.

use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = parse_out_dir(&args);
    let schema = evo::projection_schema::canonical_schema();

    let ts = evo_sdk_typescript::emit::render_sdk(
        &schema,
        &evo_sdk_typescript::TypeScriptConfig::default(),
    );
    write_typescript(&out_dir, &ts);

    let swift = evo_sdk_swift::emit::render_sdk(
        &schema,
        &evo_sdk_swift::SwiftConfig::default(),
    );
    write_swift(&out_dir, &swift);

    let kotlin = evo_sdk_kotlin::emit::render_sdk(
        &schema,
        &evo_sdk_kotlin::KotlinConfig::default(),
    );
    write_kotlin(&out_dir, &kotlin);

    let python = evo_sdk_python::emit::render_sdk(
        &schema,
        &evo_sdk_python::PythonConfig::default(),
    );
    write_python(&out_dir, &python);

    let rust = evo_sdk_rust::emit::render_sdk(
        &schema,
        &evo_sdk_rust::RustConfig::default(),
    );
    write_rust(&out_dir, &rust);

    println!("evo-sdk-gen: schema={} ops", schema.len());
    println!("evo-sdk-gen: out_dir={}", out_dir.display());
    println!("evo-sdk-gen: typescript: {} files", ts.files.len());
    println!("evo-sdk-gen: swift:      {} files", swift.files.len());
    println!("evo-sdk-gen: kotlin:     {} files", kotlin.files.len());
    println!("evo-sdk-gen: python:     {} files", python.files.len());
    println!("evo-sdk-gen: rust:       {} files", rust.files.len());
}

fn parse_out_dir(args: &[String]) -> PathBuf {
    let mut i = 1;
    while i < args.len() {
        if (args[i] == "--out-dir" || args[i] == "-o") && i + 1 < args.len() {
            return PathBuf::from(&args[i + 1]);
        }
        i += 1;
    }
    PathBuf::from("./sdks")
}

fn write_typescript(out_dir: &Path, sdk: &evo_sdk_typescript::RenderedSdk) {
    let dir = out_dir.join("typescript");
    for f in &sdk.files {
        write_one(&dir, &f.path, &f.content);
    }
}
fn write_swift(out_dir: &Path, sdk: &evo_sdk_swift::RenderedSdk) {
    let dir = out_dir.join("swift");
    for f in &sdk.files {
        write_one(&dir, &f.path, &f.content);
    }
}
fn write_kotlin(out_dir: &Path, sdk: &evo_sdk_kotlin::RenderedSdk) {
    let dir = out_dir.join("kotlin");
    for f in &sdk.files {
        write_one(&dir, &f.path, &f.content);
    }
}
fn write_python(out_dir: &Path, sdk: &evo_sdk_python::RenderedSdk) {
    let dir = out_dir.join("python");
    for f in &sdk.files {
        write_one(&dir, &f.path, &f.content);
    }
}
fn write_rust(out_dir: &Path, sdk: &evo_sdk_rust::RenderedSdk) {
    let dir = out_dir.join("rust");
    for f in &sdk.files {
        write_one(&dir, &f.path, &f.content);
    }
}

fn write_one(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create_dir_all {parent:?}: {e}"));
    }
    std::fs::write(&path, content)
        .unwrap_or_else(|e| panic!("write {path:?}: {e}"));
}
