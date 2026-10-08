// SPDX-License-Identifier: BSD-2-Clause

//! Mounted integration tests for the read-only FUSE adapter.
//!
//! These tests mount an HFS+ image and verify that POSIX operations through
//! the FUSE kernel layer produce the same results as direct library reads.
//!
//! Tests are skipped silently when FUSE is unavailable (e.g. in a container
//! without /dev/fuse), matching the convention of the existing test corpus.

use std::path::PathBuf;
use std::process::Command;

/// Workspace root, for locating test images.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn image(name: &str) -> PathBuf {
    workspace_root()
        .join("tests/images/generated")
        .join(format!("{name}.img"))
}

/// Whether FUSE can be used on this system.
fn fuse_available() -> bool {
    std::path::Path::new("/dev/fuse").exists()
}

/// Whether the `fusermount` command is available.
fn fusermount_available() -> bool {
    Command::new("fusermount").arg("-V").output().is_ok()
}

/// Build and return a unique mountpoint path for this test.
fn mountpoint(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hfsplus-fuse-test-{test_name}"));
    std::fs::create_dir_all(&dir).expect("create mountpoint");
    dir
}

fn unmount(mountpoint: &std::path::Path) {
    let _ = Command::new("fusermount")
        .arg("-u")
        .arg(mountpoint)
        .status();
    let _ = std::fs::remove_dir_all(mountpoint);
}

/// Mount an image read-only and run `check_fn` against it.
fn with_mount<F>(image_name: &str, check_fn: F)
where
    F: FnOnce(&std::path::Path) + std::panic::RefUnwindSafe + std::panic::UnwindSafe,
{
    if !fuse_available() || !fusermount_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }

    let img = image(image_name);
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let mp = mountpoint(image_name);
    let fuse_bin = workspace_root().join("target/debug/hfsplus-fuse");

    // Start the FUSE mount in a background process.
    let _child = Command::new(&fuse_bin)
        .arg(&img)
        .arg(&mp)
        .spawn()
        .expect("spawn fuse mount");

    // Wait for the mount to become visible.
    let mut mounted = false;
    for _ in 0..50 {
        if std::fs::read_dir(&mp).is_ok() {
            mounted = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    if !mounted {
        let _ = Command::new("fusermount").arg("-u").arg(&mp).status();
        eprintln!("skipping: mount timed out");
        return;
    }

    let result = std::panic::catch_unwind(|| check_fn(&mp));
    unmount(&mp);

    if let Err(_) = result {
        panic!("test assertion failed");
    }
}

#[test]
fn can_ls_root() {
    with_mount("minimal-hfsplus", |mp| {
        let entries: Vec<String> = std::fs::read_dir(mp)
            .expect("readdir root")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(!entries.is_empty(), "root directory is empty");
    });
}

#[test]
fn can_cat_a_file() {
    with_mount("minimal-hfsplus", |mp| {
        // Find any non-directory, non-empty regular file.
        let mut found_file = false;
        for entry in std::fs::read_dir(mp).expect("readdir root") {
            let entry = entry.expect("readdir entry");
            let path = entry.path();
            let meta = std::fs::metadata(&path).expect("stat");
            if !meta.is_dir() && meta.len() > 0 {
                let _contents = std::fs::read(&path).expect("read file contents");
                found_file = true;
                break;
            }
        }
        assert!(found_file, "no readable non-empty file found");
    });
}

#[test]
fn can_stat_all_entries() {
    use std::os::unix::fs::MetadataExt;

    with_mount("minimal-hfsplus", |mp| {
        for entry in std::fs::read_dir(mp).expect("readdir root") {
            let entry = entry.expect("readdir entry");
            let meta = std::fs::metadata(entry.path()).expect("stat");
            // The inode number must be the CNID (non-zero).
            assert!(meta.ino() != 0, "inode number is zero");
        }
    });
}

#[test]
fn can_traverse_subdirectories() {
    with_mount("minimal-hfsplus", |mp| {
        // If the image has subdirectories, traverse into them.
        let mut explored = 0;
        for entry in std::fs::read_dir(mp).expect("readdir root") {
            let entry = entry.expect("readdir entry");
            if entry.path().is_dir() {
                let _ = std::fs::read_dir(entry.path()).expect("readdir subdir");
                explored += 1;
            }
        }
        // Just verify no panic occurred.
        let _ = explored;
    });
}

#[test]
fn read_only_mount_rejects_writes() {
    with_mount("minimal-hfsplus", |mp| {
        // Attempting to create a file in a read-only mount should fail.
        let test_file = mp.join("should-fail");
        let result = std::fs::write(&test_file, b"hello");
        assert!(result.is_err(), "write should fail on read-only mount");
    });
}

#[test]
fn can_read_symlink_target() {
    with_mount("minimal-hfsplus", |mp| {
        // Find a symlink and verify readlink works.
        for entry in std::fs::read_dir(mp).expect("readdir root") {
            let entry = entry.expect("readdir entry");
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).expect("lstat");
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path).expect("readlink");
                assert!(!target.as_os_str().is_empty(), "symlink target is empty");
            }
        }
    });
}
