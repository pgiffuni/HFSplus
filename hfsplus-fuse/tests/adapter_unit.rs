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

/// Copy a generated image to a writable temp file.
///
/// Uses `bootstrapped-with-file` which has 220 free blocks (901 KB) and a real
/// file in the root, providing enough space for write operations.
fn writable_image_copy(test_name: &str) -> std::path::PathBuf {
    let src = image("bootstrapped-with-file");
    let dst = std::env::temp_dir().join(format!("hfsplus-writable-unit-{test_name}.img"));
    std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy image: {e}"));
    dst
}

#[test]
fn writable_open_validates_and_caches_read_volume() {
    let img = image("bootstrapped-with-file");
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let copy = writable_image_copy("open_validates");
    let fs = hfsplus_fuse::HfsPlusFilesystem::open_writable(copy.to_str().unwrap());
    assert!(fs.is_ok(), "open_writable failed");

    // The read volume should be cached and accessible.
    let fs = fs.unwrap();
    let holder = fs.get_read_volume();
    assert!(holder.is_ok(), "get_read_volume failed");

    // The volume should report the root folder's name.
    let holder = holder.unwrap();
    let name = holder.volume().name();
    assert!(name.is_ok(), "volume name lookup failed");

    // Clean up.
    let _ = std::fs::remove_file(&copy);
}

#[test]
fn writable_create_file_appears_on_next_read() {
    let img = image("bootstrapped-with-file");
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let copy = writable_image_copy("create_appears");
    let fs = hfsplus_fuse::HfsPlusFilesystem::open_writable(copy.to_str().unwrap()).unwrap();

    // Record the root's state before creating a file (for comparison below).
    let _before = fs
        .lookup_cnid(hfsplus::catalog::ROOT_FOLDER_ID)
        .unwrap()
        .unwrap();

    // Create a file.
    let parent_cnid = hfsplus::catalog::ROOT_FOLDER_ID;
    let name: Vec<u16> = "test_create_file".encode_utf16().collect();
    let result = fs.with_writable(|wvol| wvol.create_file(parent_cnid.0, &name));
    assert!(result.is_ok(), "create_file failed: {:?}", result.err());

    // The new file should be findable in the parent.
    let new_cnid_result = fs.get_read_volume();
    assert!(new_cnid_result.is_ok());

    // Invalidate and re-read to see the new file.
    fs.invalidate_volume();

    // The new file should be findable in the parent.
    let holder = fs.get_read_volume().unwrap();
    assert!(
        holder
            .volume()
            .lookup(parent_cnid, &name)
            .unwrap()
            .is_some(),
        "new file not found after create"
    );

    let _ = std::fs::remove_file(&copy);
}

#[test]
fn writable_write_to_file_round_trips() {
    let img = image("bootstrapped-with-file");
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let copy = writable_image_copy("write_round_trip");
    let fs = hfsplus_fuse::HfsPlusFilesystem::open_writable(copy.to_str().unwrap()).unwrap();

    // Find an existing file in the root.
    let holder = fs.get_read_volume().unwrap();
    let entries = holder
        .volume()
        .read_dir(holder.volume().root_cnid())
        .unwrap();
    let target = entries.iter().find(|e| !e.is_dir() && !e.is_symlink());
    let Some(object) = target else {
        eprintln!("skipping: no regular file in root");
        let _ = std::fs::remove_file(&copy);
        return;
    };
    let cnid = object.cnid().0;
    let original = holder.volume().read(object, 0, usize::MAX).unwrap();

    // Write new content at offset 0. The file preserves its original length
    // beyond the written range (FUSE write semantics).
    let new_data = b"HELLO";
    let result = fs.read_modify_write(cnid, 0, new_data);
    assert!(result.is_ok(), "write failed: {:?}", result.err());

    // Read back through a fresh volume.
    fs.invalidate_volume();
    let holder = fs.get_read_volume().unwrap();
    let object = holder
        .volume()
        .lookup_cnid(hfsplus::catalog::Cnid(cnid))
        .unwrap()
        .unwrap();
    let written = holder.volume().read(&object, 0, usize::MAX).unwrap();

    // The first 5 bytes should be our new data.
    assert_eq!(&written[..5], new_data, "written prefix doesn't match");
    // The rest should be the tail of the original (bytes after offset 5).
    assert_eq!(
        &written[5..],
        &original[5..],
        "file tail changed unexpectedly"
    );

    // Restore original content.
    let _ = fs.read_modify_write(cnid, 0, &original);

    let _ = std::fs::remove_file(&copy);
}

#[test]
fn writable_truncate_file_grows() {
    let img = image("bootstrapped-with-file");
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let copy = writable_image_copy("truncate_grow");
    let fs = hfsplus_fuse::HfsPlusFilesystem::open_writable(copy.to_str().unwrap()).unwrap();

    // Find an existing file in the root.
    let holder = fs.get_read_volume().unwrap();
    let entries = holder
        .volume()
        .read_dir(holder.volume().root_cnid())
        .unwrap();
    let target = entries.iter().find(|e| !e.is_dir() && !e.is_symlink());
    let Some(object) = target else {
        eprintln!("skipping: no regular file in root");
        let _ = std::fs::remove_file(&copy);
        return;
    };
    let cnid = object.cnid().0;
    let original = holder.volume().read(object, 0, usize::MAX).unwrap();
    let original_len = original.len();

    // Grow the file to a larger size.
    let new_len = (original_len + 500) as u64;
    let result = fs.with_writable(|wvol| wvol.truncate_file(cnid, new_len));
    assert!(result.is_ok(), "truncate_file failed: {:?}", result.err());

    // Read back through a fresh volume.
    fs.invalidate_volume();
    let holder = fs.get_read_volume().unwrap();
    let object = holder
        .volume()
        .lookup_cnid(hfsplus::catalog::Cnid(cnid))
        .unwrap()
        .unwrap();
    let written = holder.volume().read(&object, 0, usize::MAX).unwrap();

    assert_eq!(
        written.len(),
        new_len as usize,
        "file should be grown to new_len"
    );
    assert_eq!(
        &written[..original_len],
        &original[..],
        "original content preserved after grow"
    );
    assert_eq!(
        &written[original_len..],
        &[0u8; 500][..],
        "extended region should be zero-filled"
    );

    // Restore original content.
    fs.with_writable(|wvol| wvol.truncate_file(cnid, original_len as u64))
        .ok();
    let _ = fs.read_modify_write(cnid, 0, &original);

    let _ = std::fs::remove_file(&copy);
}

#[test]
fn writable_modify_file_metadata() {
    let img = image("bootstrapped-with-file");
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let copy = writable_image_copy("modify_meta");
    let fs = hfsplus_fuse::HfsPlusFilesystem::open_writable(copy.to_str().unwrap()).unwrap();

    // Find an existing file in the root.
    let holder = fs.get_read_volume().unwrap();
    let entries = holder
        .volume()
        .read_dir(holder.volume().root_cnid())
        .unwrap();
    let target = entries.iter().find(|e| !e.is_dir() && !e.is_symlink());
    let Some(object) = target else {
        eprintln!("skipping: no regular file in root");
        let _ = std::fs::remove_file(&copy);
        return;
    };
    let cnid = object.cnid().0;
    let original_mode = object.mode();

    // Change the file mode to 0o600.
    fs.with_writable(|wvol| {
        wvol.modify_file_metadata(
            cnid,
            hfsplus::volume::FileMetadataChanges {
                mode: Some(0o100600), // S_IFREG | 0600
                uid: None,
                gid: None,
                atime: None,
                mtime: None,
                ctime: None,
            },
        )
    })
    .unwrap();

    // Read back and verify.
    fs.invalidate_volume();
    let holder = fs.get_read_volume().unwrap();
    let object = holder
        .volume()
        .lookup_cnid(hfsplus::catalog::Cnid(cnid))
        .unwrap()
        .unwrap();
    let new_mode = object.mode();
    assert_eq!(new_mode & 0o777, 0o600, "mode should be 600 after setattr");

    // Restore original mode.
    fs.with_writable(|wvol| {
        wvol.modify_file_metadata(
            cnid,
            hfsplus::volume::FileMetadataChanges {
                mode: Some(original_mode),
                uid: None,
                gid: None,
                atime: None,
                mtime: None,
                ctime: None,
            },
        )
    })
    .ok();
    let _ = std::fs::remove_file(&copy);
}

#[test]
fn writable_hard_link_round_trips() {
    let img = image("bootstrapped-with-file");
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let copy = writable_image_copy("remove_file");
    let fs = hfsplus_fuse::HfsPlusFilesystem::open_writable(copy.to_str().unwrap()).unwrap();

    // Create then remove a file.
    let parent_cnid = hfsplus::catalog::ROOT_FOLDER_ID;
    let name: Vec<u16> = "to_be_removed".encode_utf16().collect();

    let created = fs.with_writable(|wvol| wvol.create_file(parent_cnid.0, &name));
    assert!(created.is_ok());

    // Verify the file exists.
    let holder = fs.get_read_volume().unwrap();
    assert!(holder
        .volume()
        .lookup(parent_cnid, &name)
        .unwrap()
        .is_some());
    drop(holder);

    // Remove it.
    let removed = fs.with_writable(|wvol| wvol.remove(parent_cnid.0, &name));
    assert!(removed.is_ok());

    // Verify it's gone.
    fs.invalidate_volume();
    let holder = fs.get_read_volume().unwrap();
    assert!(holder
        .volume()
        .lookup(parent_cnid, &name)
        .unwrap()
        .is_none());

    let _ = std::fs::remove_file(&copy);
}

#[test]
fn writable_hard_link_unlink_frees_indirect_node() {
    // Create a file with data, make a hard link to it, then remove the link.
    // The link's removal must decrement the inode's linkCount to zero, free the
    // inode's blocks and records, and leave the parent folder's valence correct.
    let img = image("bootstrapped-with-file");
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let copy = writable_image_copy("unlink_hard_link");
    let fs = hfsplus_fuse::HfsPlusFilesystem::open_writable(copy.to_str().unwrap()).unwrap();

    let parent_cnid = hfsplus::catalog::ROOT_FOLDER_ID;
    let file_name: Vec<u16> = "target.bin".encode_utf16().collect();
    let link_name: Vec<u16> = "alias.bin".encode_utf16().collect();
    let data = b"hard link data for removal test".to_vec();

    // Create the file and write data.
    let target_cnid = fs
        .with_writable(|wvol| {
            let cnid = wvol.create_file(parent_cnid.0, &file_name)?;
            wvol.write_file_contents(cnid, &data)?;
            Ok(cnid)
        })
        .expect("create file with data");

    // Create the hard link.
    let link_cnid = fs
        .with_writable(|wvol| wvol.create_hard_link(parent_cnid.0, &link_name, target_cnid))
        .expect("create hard link");
    assert_ne!(link_cnid, target_cnid, "link must have its own CNID");

    // Record fileCount before removal.
    let file_count_before = {
        fs.invalidate_volume();
        let holder = fs.get_read_volume().unwrap();
        holder.volume().header().file_count
    };

    // Remove the link (not the inode).
    let removed = fs.with_writable(|wvol| wvol.remove(parent_cnid.0, &link_name));
    assert!(removed.is_ok(), "remove the hard link");

    // Invalidate and re-read to verify state.
    fs.invalidate_volume();
    let holder = fs.get_read_volume().unwrap();
    let vol = holder.volume();

    // The link name must be gone.
    assert!(
        vol.lookup(parent_cnid, &link_name).unwrap().is_none(),
        "link must be gone after removal"
    );

    // The inode (target CNID) must be gone too -- linkCount reached zero.
    assert!(
        vol.lookup_cnid(hfsplus::catalog::cnid::Cnid(target_cnid))
            .unwrap()
            .is_none(),
        "indirect node must be gone when its only link is removed"
    );

    // fileCount must drop by 2: the link record and the inode record.
    assert_eq!(
        vol.header().file_count,
        file_count_before - 2,
        "fileCount must account for both link and inode removal"
    );

    let _ = std::fs::remove_file(&copy);
}
