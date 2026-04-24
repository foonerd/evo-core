#!/usr/bin/env bash
# Sourced by CI before `cargo check --target ...`. Exports cc-rs / cargo linker
# variables so C build scripts (e.g. ring, lzma-sys) find the right cross tools.
# See .github/workflows/ci.yml msrv job.
TARGET="${TARGET:?Set TARGET to a Rust triple (e.g. aarch64-unknown-linux-gnu)}"

case "$TARGET" in
x86_64-unknown-linux-gnu)
	# Host: default toolchain
	;;
aarch64-unknown-linux-gnu)
	export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
	export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
	;;
armv7-unknown-linux-gnueabihf)
	export CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
	export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
	;;
arm-unknown-linux-gnueabihf)
	export CC_arm_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
	export CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
	;;
i686-unknown-linux-gnu)
	export CC_i686_unknown_linux_gnu=i686-linux-gnu-gcc
	export CARGO_TARGET_I686_UNKNOWN_LINUX_GNU_LINKER=i686-linux-gnu-gcc
	;;
x86_64-unknown-linux-musl)
	# /usr/bin/x86_64-linux-musl-gcc from the musl-tools package
	export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
	export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc
	;;
aarch64-unknown-linux-musl)
	# musl.cc tarball adds …/bin to PATH in CI; compiler name is fixed
	export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
	export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar
	export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc
	;;
armv7-unknown-linux-musleabihf)
	export CC_armv7_unknown_linux_musleabihf=armv7l-linux-musleabihf-gcc
	export AR_armv7_unknown_linux_musleabihf=armv7l-linux-musleabihf-ar
	export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=armv7l-linux-musleabihf-gcc
	;;
*)
	echo "ci-msrv-cargo-env: unknown target $TARGET" >&2
	exit 1
	;;
esac
