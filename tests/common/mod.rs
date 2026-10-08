// SPDX-License-Identifier: BSD-2-Clause

//! Shared helpers for the integration test suite.
//!
//! The integration tests deliberately drive the *real* corpus through the
//! *public* library API rather than through internal modules, so that what is
//! being asserted is the same surface the FUSE frontend will use.

#![allow(dead_code)]

pub mod manifest;

use std::path::{Path, PathBuf};

use hfsplus::blockdev::FileDevice;
use hfsplus::error::Result;
use hfsplus::format::volume_header::VolumeHeader;

/// Repository root, derived from this file's location at compile time.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Directory holding generated images.
pub fn generated_dir() -> PathBuf {
    repo_root().join("tests/images/generated")
}

/// Directory holding manifests.
pub fn manifests_dir() -> PathBuf {
    repo_root().join("tests/images/manifests")
}

/// Absolute path of a generated image by stem name.
pub fn image(name: &str) -> PathBuf {
    generated_dir().join(format!("{name}.img"))
}

/// Absolute path of a manifest by stem name.
pub fn manifest(name: &str) -> PathBuf {
    manifests_dir().join(format!("{name}.toml"))
}

/// Every generated image stem present in the corpus directory.
pub fn generated_names() -> Vec<String> {
    let dir = generated_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("img") {
                return None;
            }
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    names.sort();
    names
}

/// Read the volume header of an image, or `None` when the image is absent.
///
/// A missing image is not a test failure: `tools/genimages.sh` may not have run,
/// and the harness reports that once as a skip rather than as dozens of
/// individual errors.
pub fn read_header(path: &Path) -> Result<Option<VolumeHeader>> {
    if !path.exists() {
        return Ok(None);
    }
    let dev = FileDevice::open(path)?;
    Ok(Some(VolumeHeader::read_from(&dev)?))
}

/// Whether an external checker is available on this machine.
pub fn fsck_available() -> Option<String> {
    for candidate in ["/usr/sbin/fsck.hfsplus", "/sbin/fsck.hfsplus"] {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Whether the in-tree `hfsck` binary is available (always present in debug
/// builds, since it is compiled from this crate).
pub fn hfsck_available() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let root = exe.parent()?;
    let candidates = [root.join("hfsck"), root.join("..").join("hfsck")];
    candidates.into_iter().find(|c| c.exists())
}

/// Run `hfsck` on an image and return whether it was clean.
pub fn run_hfsck(hfsck: &Path, image: &Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new(hfsck).arg(image).output()
}

/// Run `fsck.hfsplus` on an image and return its combined output.
pub fn run_fsck(fsck: &str, image: &Path) -> std::process::Output {
    std::process::Command::new(fsck)
        .arg(image)
        .output()
        .expect("failed to spawn fsck.hfsplus")
}
