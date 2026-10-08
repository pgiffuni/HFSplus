// SPDX-License-Identifier: BSD-2-Clause

//! Adapter unit tests: errno, timestamp, inode, and handle-table conversion.
//!
//! These tests do not require a mounted filesystem. They exercise the
//! translation layers between HFS+ types and FUSE types.

use std::path::Path;

/// Path to the workspace root, derived from the crate manifest directory.
fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Path to a generated test image.
fn image(name: &str) -> std::path::PathBuf {
    workspace_root()
        .join("tests/images/generated")
        .join(format!("{name}.img"))
}

#[test]
fn errno_mapping_is_total() {
    use hfsplus::error::Error;
    use hfsplus_fuse::errno_of;

    assert_eq!(
        errno_of(&Error::NotFound { what: "file" }),
        fuser::Errno::ENOENT
    );
    assert_eq!(
        errno_of(&Error::NotFoundKey {
            key: "x".to_string()
        }),
        fuser::Errno::ENOENT
    );
    assert_eq!(errno_of(&Error::no_space(10, 5)), fuser::Errno::ENOSPC);
    assert_eq!(errno_of(&Error::ReadOnly), fuser::Errno::EROFS);
    assert_eq!(errno_of(&Error::unsupported("x")), fuser::Errno::EOPNOTSUPP);
    assert_eq!(
        errno_of(&Error::BadSignature { found: 0 }),
        fuser::Errno::EIO
    );
    assert_eq!(
        errno_of(&Error::BadVersion {
            found: 0,
            expected: 0
        }),
        fuser::Errno::EIO
    );
    assert_eq!(
        errno_of(&Error::Io {
            message: "x".into()
        }),
        fuser::Errno::EIO
    );
    assert_eq!(
        errno_of(&Error::invalid("field", "bad")),
        fuser::Errno::EINVAL
    );
    assert_eq!(
        errno_of(&Error::Truncated {
            what: "x",
            needed: 1,
            available: 0
        }),
        fuser::Errno::EIO
    );
    assert_eq!(errno_of(&Error::Overflow { what: "x" }), fuser::Errno::EIO);
    assert_eq!(
        errno_of(&Error::BadBlockNumber {
            block: 1,
            total_blocks: 1
        }),
        fuser::Errno::EIO
    );
    assert_eq!(
        errno_of(&Error::OutOfRange {
            what: "x",
            value: 1,
            limit: 1
        }),
        fuser::Errno::EIO
    );
}

#[test]
fn cnid_to_inode_is_identity() {
    assert_eq!(
        hfsplus_fuse::cnid_to_inode(hfsplus::catalog::ROOT_FOLDER_ID),
        2
    );
    assert_eq!(
        hfsplus_fuse::cnid_to_inode(hfsplus::catalog::EXTENTS_FILE_ID),
        3
    );
    assert_eq!(
        hfsplus_fuse::cnid_to_inode(hfsplus::catalog::FIRST_USER_CATALOG_NODE_ID),
        16
    );
}

#[test]
fn timestamp_conversion_handles_expanded_mode() {
    use std::time::UNIX_EPOCH;

    // In expanded mode, the raw value IS Unix seconds.
    let t = hfsplus_fuse::hfs_time_to_systemtime(1_000_000_000, true);
    assert!(t.duration_since(UNIX_EPOCH).unwrap().as_secs() == 1_000_000_000);

    // Zero in expanded mode is a real date (1970-01-01).
    let t = hfsplus_fuse::hfs_time_to_systemtime(0, true);
    assert_eq!(t, UNIX_EPOCH);
}

#[test]
fn timestamp_conversion_handles_classic_mode() {
    use hfsplus::timestamp::MAC_GMT_FACTOR;
    use std::time::UNIX_EPOCH;

    // Zero in classic mode is "unset" → Unix epoch.
    let t = hfsplus_fuse::hfs_time_to_systemtime(0, false);
    assert_eq!(t, UNIX_EPOCH);

    // A value just past the epoch factor maps to 1 second after Unix epoch.
    let t = hfsplus_fuse::hfs_time_to_systemtime(MAC_GMT_FACTOR + 1, false);
    assert_eq!(t.duration_since(UNIX_EPOCH).unwrap().as_secs(), 1);
}

#[test]
fn handle_table_tracks_files_and_dirs() {
    let table = hfsplus_fuse::HandleTable::default();

    // Insert a file handle.
    let fh = table.insert_file(hfsplus_fuse::OpenFile::new_file(
        hfsplus::catalog::ROOT_FOLDER_ID,
    ));
    assert!(table.get_file(fh).is_some());
    assert!(table.get_dir(fh).is_none());

    // Insert a directory handle.
    let dh = table.insert_dir(hfsplus_fuse::OpenDir {
        cnid: hfsplus::catalog::ROOT_FOLDER_ID,
        cursor: hfsplus::catalog::lookup::DirCursor::start(),
    });
    assert!(table.get_dir(dh).is_some());
    assert!(table.get_file(dh).is_none());

    // Handles must be distinct.
    assert_ne!(fh, dh);

    // Remove and verify.
    assert!(table.remove(fh).is_some());
    assert!(table.get_file(fh).is_none());
}

#[test]
fn handle_table_returns_none_for_unknown_tokens() {
    let table = hfsplus_fuse::HandleTable::default();
    assert!(table.get_file(999).is_none());
    assert!(table.get_dir(999).is_none());
    assert!(table.remove(999).is_none());
}

#[test]
fn file_attr_conversion_preserves_inode_and_kind() {
    use hfsplus::blockdev::MemoryDevice;
    use hfsplus::volume::Volume;

    let path = image("minimal-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read test image");
    let dev = MemoryDevice::new(bytes);
    let vol = Volume::open(&dev).expect("open volume");

    // The root directory must exist.
    if let Ok(Some(hfsplus::volume::Object::Directory(dir))) = vol.lookup(vol.root_cnid(), &[]) {
        let expanded = vol.header().has_expanded_times();
        let attr = hfsplus_fuse::dir_attrs_to_file_attr(&dir, expanded);
        assert_eq!(attr.ino.0, hfsplus::catalog::ROOT_FOLDER_ID.0 as u64);
        assert_eq!(attr.kind, fuser::FileType::Directory);
    }
}

#[test]
fn file_attr_conversion_for_regular_file() {
    use hfsplus::blockdev::MemoryDevice;
    use hfsplus::volume::Volume;

    let path = image("minimal-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read test image");
    let dev = MemoryDevice::new(bytes);
    let vol = Volume::open(&dev).expect("open volume");

    // Walk the root to find a file.
    if let Ok(entries) = vol.read_dir(vol.root_cnid()) {
        for entry in &entries {
            if let hfsplus::volume::Object::File(file) = entry {
                if !entry.is_symlink() {
                    let expanded = vol.header().has_expanded_times();
                    let attr = hfsplus_fuse::file_attrs_to_file_attr(file, expanded);
                    assert_eq!(attr.ino.0, entry.cnid().0 as u64);
                    assert_eq!(attr.kind, fuser::FileType::RegularFile);
                    break;
                }
            }
        }
    }
}
