// Bake the compiler identity into the binary.
//
// Rust has no stable ABI, and this plugin hands `EntityType` / `CadDocument`
// across the dynamic-library boundary to the host. If host and plugin were
// built by different rustc versions the layouts differ and the runner dies on
// the first compound call - while simple types like `String` still work, which
// makes the failure look like anything but a toolchain mismatch.
//
// Reporting the version in `{"op":"status"}` turns a night of guesswork into a
// one-line comparison against the `rustc/<hash>` string in the host binary.

use std::process::Command;

fn main() {
    let version = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("-vV")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| {
            let line = |key: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            let release = line("release:");
            let hash = line("commit-hash:");
            if release.is_empty() {
                "unknown".to_string()
            } else if hash.is_empty() {
                release
            } else {
                format!("{release} ({hash})")
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BRIDGE_RUSTC={version}");
    println!("cargo:rerun-if-changed=build.rs");
}
