//! Keeps the pinned Rust toolchain in one agreed state.
//!
//! `src-tauri/rust-toolchain.toml` is what rustup obeys locally; the
//! workflows also name a version so the setup action installs it up front
//! instead of letting the first `cargo` call download it. Several places,
//! one fact, which is the shape that drifts. When they disagree, CI lints
//! with a different clippy than a local checkout, and a lint that does not
//! exist locally cannot be caught locally.

use std::fs;
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workflows_dir() -> PathBuf {
    manifest_dir().join("..").join(".github").join("workflows")
}

/// The `channel = "..."` value from `rust-toolchain.toml`.
fn pinned_channel() -> String {
    let path = manifest_dir().join("rust-toolchain.toml");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("channel") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        return rest.trim().trim_matches('"').to_owned();
    }
    panic!("rust-toolchain.toml has no `channel` line");
}

/// Every workflow file that installs a Rust toolchain.
fn rust_workflows() -> Vec<(PathBuf, String)> {
    let dir = workflows_dir();
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
    {
        let path = entry.expect("readable directory entry").path();
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yml") | Some("yaml")
        );
        if !is_yaml {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        if text.contains("setup-rust-toolchain") || text.contains("dtolnay/rust-toolchain") {
            found.push((path, text));
        }
    }
    found
}

/// Every `toolchain: "..."` a workflow asks the setup action to install.
fn workflow_toolchains(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("toolchain:"))
        .map(|rest| rest.trim().trim_matches('"').trim_matches('\'').to_owned())
        .collect()
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[test]
fn toolchain_is_pinned_to_an_exact_release() {
    let pinned = pinned_channel();
    let parts: Vec<&str> = pinned.split('.').collect();
    assert!(
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        "rust-toolchain.toml pins `{pinned}`; use an exact MAJOR.MINOR.PATCH release so local and CI \
         toolchains cannot drift onto different patch releases"
    );
}

#[test]
fn every_workflow_installs_the_pinned_toolchain() {
    let pinned = pinned_channel();
    let workflows = rust_workflows();
    assert!(
        !workflows.is_empty(),
        "no workflow installs a Rust toolchain; this test is looking in the wrong place"
    );

    for (path, text) in workflows {
        let workflow = name(&path);
        let setups = text.matches("setup-rust-toolchain").count()
            + text.matches("dtolnay/rust-toolchain").count();
        let declared = workflow_toolchains(&text);
        assert_eq!(
            declared.len(),
            setups,
            "{workflow} has {setups} toolchain setup step(s) but {} `toolchain:` line(s); every \
             setup step must name the version explicitly",
            declared.len()
        );
        for version in declared {
            assert_eq!(
                version, pinned,
                "{workflow} installs Rust {version} but rust-toolchain.toml pins {pinned}, so CI \
                 would build and lint with a different compiler than a local checkout"
            );
        }
    }
}

#[test]
fn no_workflow_installs_a_floating_stable_toolchain() {
    for (path, text) in rust_workflows() {
        assert!(
            !text.contains("rust-toolchain@stable") && !text.contains("toolchain: stable"),
            "{} installs a floating stable toolchain; pin it to the version in rust-toolchain.toml",
            name(&path)
        );
    }
}
