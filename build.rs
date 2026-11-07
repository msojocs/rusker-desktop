//! Builds the VM side of the Engine proxy relay (`helpers/vm-proxy`) for the
//! WSL2 VM and embeds it in the Windows app, like Docker Desktop ships its own
//! binaries for its VM. The helper is a static Linux musl executable linked
//! with Rust's bundled `rust-lld`, so no extra toolchain is needed beyond
//! `rustup target add <arch>-unknown-linux-musl`.
//!
//! `RUSKER_VM_PROXY=<path>` embeds a prebuilt helper instead (CI, offline).

use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=helpers/vm-proxy/src");
    println!("cargo:rerun-if-changed=helpers/vm-proxy/Cargo.toml");
    println!("cargo:rerun-if-changed=helpers/vm-proxy/Cargo.lock");
    println!("cargo:rerun-if-env-changed=RUSKER_VM_PROXY");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let embedded = out_dir.join("rusker-vm-proxy");
    if let Some(prebuilt) = env::var_os("RUSKER_VM_PROXY") {
        fs::copy(&prebuilt, &embedded).unwrap_or_else(|error| {
            panic!(
                "无法复制 RUSKER_VM_PROXY={}：{error}",
                PathBuf::from(&prebuilt).display()
            )
        });
    } else {
        build_helper(&out_dir, &embedded);
    }
    println!("cargo:rustc-env=RUSKER_VM_PROXY_BIN={}", embedded.display());
}

fn build_helper(out_dir: &std::path::Path, embedded: &std::path::Path) {
    // The WSL2 VM has the host's architecture.
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let triple = match arch.as_str() {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => panic!("WSL2 不支持 {other} 架构"),
    };
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let target_dir = out_dir.join("vm-proxy-target");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let linker = format!(
        "CARGO_TARGET_{}_LINKER",
        triple.to_ascii_uppercase().replace('-', "_")
    );
    let mut command = Command::new(cargo);
    command
        .args(["build", "--release", "--locked", "--manifest-path"])
        .arg(manifest_dir.join("helpers/vm-proxy/Cargo.toml"))
        .args(["--target", triple, "--target-dir"])
        .arg(&target_dir)
        .env(linker, "rust-lld");
    // Settings meant for the desktop app's own (Windows) build.
    for name in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        command.env_remove(name);
    }
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("无法运行 cargo 构建 rusker-vm-proxy：{error}"));
    if !status.success() {
        panic!(
            "构建 rusker-vm-proxy（{triple}）失败。请先安装目标：rustup target add {triple}；\
             或设置 RUSKER_VM_PROXY 指向已构建的程序"
        );
    }
    let built = target_dir
        .join(triple)
        .join("release")
        .join("rusker-vm-proxy");
    fs::copy(&built, embedded)
        .unwrap_or_else(|error| panic!("无法复制 {}：{error}", built.display()));
}
