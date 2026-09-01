// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Build script — compiles the `evo.v1` protobuf into Rust stubs via
//! tonic-build when the `enabled` feature is selected. Uses the
//! vendored protoc binary so the build does not depend on a
//! system-installed compiler.

fn main() {
    // Always re-run when the proto changes.
    println!("cargo:rerun-if-changed=proto/evo.v1.proto");

    // Only invoke tonic-build when the `enabled` feature is on.
    // Without it the generated module is shimmed by the
    // hand-rolled stub in `src/lib.rs` and tonic + prost are
    // excluded from the dep graph entirely.
    if std::env::var("CARGO_FEATURE_ENABLED").is_err() {
        return;
    }

    // Bundle the protoc binary so the build host does not need
    // a system-installed `protobuf-compiler`. This keeps cross-
    // compile environments + CI runners self-contained.
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("protoc-bin-vendored: locate bundled protoc");
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/evo.v1.proto"], &["proto"])
        .expect("tonic-build: compile evo.v1.proto");
}
