#[cfg(target_os = "macos")]
use swift_rs::SwiftLinker;

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "macos")]
fn link_macos_swift_runtime_rpaths() {
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}

fn go_target_from_rust_target(target: &str) -> Option<(&'static str, &'static str)> {
    let goos = if target.contains("windows") {
        "windows"
    } else if target.contains("apple-darwin") {
        "darwin"
    } else if target.contains("linux") {
        "linux"
    } else {
        return None;
    };

    let goarch = if target.starts_with("x86_64") {
        "amd64"
    } else if target.starts_with("aarch64") {
        "arm64"
    } else if target.starts_with("i686") {
        "386"
    } else if target.starts_with("armv7") {
        "arm"
    } else {
        return None;
    };

    Some((goos, goarch))
}

fn should_skip_sidecar_build(output: &Path, env_name: &str) -> bool {
    std::env::var(env_name).ok().as_deref() == Some("1") && output.exists()
}

fn emit_sidecar_rerun_inputs(path: &Path) {
    if path.file_name().and_then(|name| name.to_str()) == Some("bin") {
        return;
    }

    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };

    if metadata.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            emit_sidecar_rerun_inputs(&entry.path());
        }
        return;
    }

    let should_track = matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("go.mod") | Some("go.sum")
    ) || path.extension().and_then(|extension| extension.to_str()) == Some("go");

    if should_track {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn build_go_sidecar(
    sidecar_dir: &Path,
    output_dir: &Path,
    binary_name: &str,
    package: &str,
    skip_env: &str,
    rust_target: &str,
    goos: &str,
    goarch: &str,
) -> PathBuf {
    let extension = if goos == "windows" { ".exe" } else { "" };
    let output = output_dir.join(format!("{binary_name}-{rust_target}{extension}"));
    if should_skip_sidecar_build(&output, skip_env) {
        return output;
    }

    let status = Command::new("go")
        .current_dir(sidecar_dir)
        .env("GOOS", goos)
        .env("GOARCH", goarch)
        .env("CGO_ENABLED", "0")
        .arg("build")
        .arg("-trimpath")
        .arg("-ldflags")
        .arg("-s -w")
        .arg("-o")
        .arg(&output)
        .arg(package)
        .status()
        .unwrap_or_else(|error| panic!("failed to start go build for {binary_name}: {error}"));

    if !status.success() {
        panic!("go build for {binary_name} failed with status: {status}");
    }

    output
}

fn build_macos_universal_sidecar(
    sidecar_dir: &Path,
    output_dir: &Path,
    binary_name: &str,
    package: &str,
    skip_env: &str,
) {
    let output = output_dir.join(format!("{binary_name}-universal-apple-darwin"));
    if should_skip_sidecar_build(&output, skip_env) {
        return;
    }

    let x86_64_output = build_go_sidecar(
        sidecar_dir,
        output_dir,
        binary_name,
        package,
        skip_env,
        "x86_64-apple-darwin",
        "darwin",
        "amd64",
    );
    let aarch64_output = build_go_sidecar(
        sidecar_dir,
        output_dir,
        binary_name,
        package,
        skip_env,
        "aarch64-apple-darwin",
        "darwin",
        "arm64",
    );

    let status = Command::new("lipo")
        .arg("-create")
        .arg(&x86_64_output)
        .arg(&aarch64_output)
        .arg("-output")
        .arg(&output)
        .status()
        .unwrap_or_else(|error| panic!("failed to start lipo for {binary_name}: {error}"));

    if !status.success() {
        panic!("lipo for {binary_name} universal sidecar failed with status: {status}");
    }
}

fn build_cockpit_cliproxy_sidecar() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is required"));
    let target = std::env::var("TARGET").expect("TARGET is required");
    println!("cargo:rustc-env=COCKPIT_RUST_TARGET={target}");
    let sidecar_dir = manifest_dir.join("../sidecars/cockpit-cliproxy");
    let output_dir = sidecar_dir.join("bin");

    println!("cargo:rerun-if-env-changed=COCKPIT_SKIP_CLIPROXY_BUILD");
    emit_sidecar_rerun_inputs(&sidecar_dir);
    std::fs::create_dir_all(&output_dir).expect("failed to create cockpit-cliproxy bin dir");

    if cfg!(target_os = "macos") && target == "universal-apple-darwin" {
        build_macos_universal_sidecar(
            &sidecar_dir,
            &output_dir,
            "cockpit-cliproxy",
            ".",
            "COCKPIT_SKIP_CLIPROXY_BUILD",
        );
        return;
    }

    let Some((goos, goarch)) = go_target_from_rust_target(&target) else {
        panic!("unsupported sidecar build target: {target}");
    };
    build_go_sidecar(
        &sidecar_dir,
        &output_dir,
        "cockpit-cliproxy",
        ".",
        "COCKPIT_SKIP_CLIPROXY_BUILD",
        &target,
        goos,
        goarch,
    );
    if cfg!(target_os = "macos") && target.contains("apple-darwin") {
        build_macos_universal_sidecar(
            &sidecar_dir,
            &output_dir,
            "cockpit-cliproxy",
            ".",
            "COCKPIT_SKIP_CLIPROXY_BUILD",
        );
    }
}

fn build_grok2api_sidecar() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is required"));
    let target = std::env::var("TARGET").expect("TARGET is required");
    let sidecar_dir = manifest_dir.join("../sidecars/grok2api/backend");
    let output_dir = manifest_dir.join("../sidecars/grok2api/bin");
    let skip_env = "COCKPIT_SKIP_GROK2API_BUILD";

    println!("cargo:rerun-if-env-changed={skip_env}");
    emit_sidecar_rerun_inputs(&sidecar_dir);
    std::fs::create_dir_all(&output_dir).expect("failed to create grok2api bin dir");

    if cfg!(target_os = "macos") && target == "universal-apple-darwin" {
        build_macos_universal_sidecar(
            &sidecar_dir,
            &output_dir,
            "cockpit-grok2api",
            "./cmd/grok2api",
            skip_env,
        );
        return;
    }
    let Some((goos, goarch)) = go_target_from_rust_target(&target) else {
        panic!("unsupported grok2api sidecar build target: {target}");
    };
    build_go_sidecar(
        &sidecar_dir,
        &output_dir,
        "cockpit-grok2api",
        "./cmd/grok2api",
        skip_env,
        &target,
        goos,
        goarch,
    );
    if cfg!(target_os = "macos") && target.contains("apple-darwin") {
        build_macos_universal_sidecar(
            &sidecar_dir,
            &output_dir,
            "cockpit-grok2api",
            "./cmd/grok2api",
            skip_env,
        );
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    build_cockpit_cliproxy_sidecar();
    build_grok2api_sidecar();

    #[cfg(target_os = "macos")]
    {
        SwiftLinker::new("12.0")
            .with_package("MacosNativeMenuSwift", "native/macos-native-menu")
            .link();
        link_macos_swift_runtime_rpaths();
    }

    tauri_build::build()
}
