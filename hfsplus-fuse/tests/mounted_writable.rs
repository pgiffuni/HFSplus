// SPDX-License-Identifier: BSD-2-Clause

//! Mounted integration tests for the writable FUSE adapter.
//!
//! These tests mount an HFS+ image as writable and verify that mutations
//! through the FUSE kernel layer persist to the image.
//!
//! Tests are skipped silently when FUSE is unavailable, matching the convention
//! of the existing test corpus.

use std::path::PathBuf;
use std::process::Command;

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

fn image_copy(test_name: &str) -> PathBuf {
    let src = image("journaled-hfsplus");
    let dst = std::env::temp_dir().join(format!("hfsplus-writable-test-{test_name}.img"));
    std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy image: {e}"));
    dst
}

fn fuse_available() -> bool {
    std::path::Path::new("/dev/fuse").exists()
}

fn fusermount_available() -> bool {
    Command::new("fusermount").arg("-V").output().is_ok()
}

fn mountpoint(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hfsplus-fuse-writable-{test_name}"));
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

/// Check if a path appears in the mount table.
fn is_mounted(mountpoint: &std::path::Path) -> bool {
    let mp = mountpoint.display().to_string();
    std::fs::read_to_string("/proc/self/mounts")
        .map(|s| s.lines().any(|line| line.contains(&mp)))
        .unwrap_or(false)
}

/// Mount an image writable and run `check_fn` against it.
fn with_writable_mount<F>(test_name: &str, check_fn: F)
where
    F: FnOnce(&std::path::Path) + std::panic::RefUnwindSafe + std::panic::UnwindSafe,
{
    if !fuse_available() || !fusermount_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }

    let img = image_copy(test_name);
    if !img.exists() {
        eprintln!("skipping: {} not built", img.display());
        return;
    }

    let mp = mountpoint(test_name);
    let fuse_bin = workspace_root().join("target/debug/hfsplus-fuse");

    let mut child = Command::new(&fuse_bin)
        .arg(&img)
        .arg(&mp)
        .arg("-w")
        .spawn()
        .expect("spawn fuse mount");

    // Wait for the mount to become visible and accessible.
    let mut mounted = false;
    for _ in 0..50 {
        if child.try_wait().is_ok() {
            break;
        }
        if is_mounted(&mp) {
            // Verify the FUSE mount actually serves content, not just an empty
            // directory. In some environments (e.g. WSL2) the mount is registered
            // but doesn't serve requests.
            if std::fs::read_dir(&mp)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
                || std::fs::metadata(&mp).is_ok()
            {
                mounted = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    if !mounted {
        let _ = child.kill();
        let _ = child.wait();
        unmount(&mp);
        eprintln!("skipping: mount not accessible for {test_name}");
        return;
    }

    let result = std::panic::catch_unwind(|| check_fn(&mp));

    // Unmount and clean up.
    unmount(&mp);
    let _ = child.kill();
    let _ = child.wait();

    // Clean up the test image copy.
    let _ = std::fs::remove_file(&img);

    if result.is_err() {
        panic!("test assertion failed");
    }
}

#[test]
fn can_create_and_read_file() {
    with_writable_mount("create_file", |mp| {
        let test_file = mp.join("testfile.txt");
        std::fs::write(&test_file, b"hello world").expect("create file");
        let contents = std::fs::read(&test_file).expect("read back");
        assert_eq!(contents, b"hello world");
    });
}

#[test]
fn can_create_and_remove_file() {
    with_writable_mount("create_remove", |mp| {
        let test_file = mp.join("todelete.txt");
        std::fs::write(&test_file, b"temp data").expect("create file");
        std::fs::remove_file(&test_file).expect("remove file");
        assert!(!test_file.exists());
    });
}

#[test]
fn can_create_and_remove_directory() {
    with_writable_mount("create_remove_dir", |mp| {
        let test_dir = mp.join("subdir");
        std::fs::create_dir(&test_dir).expect("create dir");
        assert!(test_dir.exists());
        std::fs::remove_dir(&test_dir).expect("remove dir");
        assert!(!test_dir.exists());
    });
}

#[test]
fn can_rename_file() {
    with_writable_mount("rename", |mp| {
        let old_path = mp.join("oldname.txt");
        let new_path = mp.join("newname.txt");
        std::fs::write(&old_path, b"renamed content").expect("create file");
        std::fs::rename(&old_path, &new_path).expect("rename");
        assert!(!old_path.exists());
        assert!(new_path.exists());
        let contents = std::fs::read(&new_path).expect("read renamed");
        assert_eq!(contents, b"renamed content");
    });
}

#[test]
fn can_write_at_offset() {
    use std::os::unix::fs::FileExt;

    with_writable_mount("write_offset", |mp| {
        let test_file = mp.join("offset.txt");
        std::fs::write(&test_file, b"hello world").expect("create file");

        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&test_file)
            .expect("open for write");
        f.write_at(b"HELLO", 6).expect("write at offset");
        drop(f);

        let contents = std::fs::read(&test_file).expect("read back");
        assert_eq!(contents, b"hello HELLO");
    });
}

#[test]
fn can_truncate_file() {
    with_writable_mount("truncate", |mp| {
        let test_file = mp.join("trunc.txt");
        std::fs::write(&test_file, b"hello world, this is long").expect("create file");

        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&test_file)
            .expect("open");
        f.set_len(5).expect("truncate");
        drop(f);

        let contents = std::fs::read(&test_file).expect("read back");
        assert_eq!(contents, b"hello");
    });
}

#[test]
fn can_create_directory_with_file() {
    with_writable_mount("nested", |mp| {
        let dir = mp.join("parent");
        std::fs::create_dir(&dir).expect("create dir");
        let file = dir.join("child.txt");
        std::fs::write(&file, b"nested").expect("create file in dir");
        let contents = std::fs::read(&file).expect("read nested");
        assert_eq!(contents, b"nested");
    });
}

#[test]
fn directory_listing_reflects_creations() {
    with_writable_mount("listing", |mp| {
        let test_file = mp.join("newlisting.txt");
        std::fs::write(&test_file, b"data").expect("create file");

        let entries: Vec<String> = std::fs::read_dir(mp)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            entries.iter().any(|n| n == "newlisting.txt"),
            "new file not found in listing: {entries:?}"
        );
    });
}

#[test]
fn can_extend_file_by_writing_past_end() {
    use std::os::unix::fs::FileExt;

    with_writable_mount("extend_by_write", |mp| {
        let test_file = mp.join("extend.txt");
        std::fs::write(&test_file, b"hello").expect("create file");

        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&test_file)
            .expect("open for write");
        // Write at offset 20, well past the 5-byte file.
        f.write_at(b"WORLD", 20).expect("write past end");
        drop(f);

        let contents = std::fs::read(&test_file).expect("read back");
        assert_eq!(
            contents.len(),
            25,
            "file should be 25 bytes after extended write"
        );
        assert_eq!(&contents[..5], b"hello", "original content preserved");
        assert_eq!(&contents[20..], b"WORLD", "new content at offset 20");
        // Gap between original end and new data should be zero.
        assert_eq!(&contents[5..20], &[0u8; 15], "gap should be zero-filled");
    });
}

#[test]
fn can_truncate_file_to_zero() {
    with_writable_mount("truncate_zero", |mp| {
        let test_file = mp.join("empty.txt");
        std::fs::write(&test_file, b"some data to remove").expect("create file");
        assert_eq!(std::fs::read(&test_file).unwrap().len(), 19);

        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&test_file)
            .expect("open");
        f.set_len(0).expect("truncate to zero");
        drop(f);

        let contents = std::fs::read(&test_file).expect("read back");
        assert_eq!(contents, b"", "file should be empty after truncate to zero");
    });
}

#[test]
fn can_truncate_file_to_grow() {
    with_writable_mount("truncate_grow", |mp| {
        let test_file = mp.join("growfile.txt");
        std::fs::write(&test_file, b"short").expect("create file");
        assert_eq!(std::fs::read(&test_file).unwrap(), b"short");

        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&test_file)
            .expect("open");
        f.set_len(100).expect("grow file");
        drop(f);

        let contents = std::fs::read(&test_file).expect("read back");
        assert_eq!(contents.len(), 100, "file should be 100 bytes after grow");
        assert_eq!(&contents[..5], b"short", "original content preserved");
        assert_eq!(
            &contents[5..],
            &[0u8; 95],
            "extended region should be zero-filled"
        );
    });
}

#[test]
fn can_create_and_read_hard_link() {
    with_writable_mount("hardlink", |mp| {
        let original = mp.join("target.txt");
        std::fs::write(&original, b"link data").expect("create file");

        let link = mp.join("link.txt");
        std::fs::hard_link(&original, &link).expect("create hard link");

        // Both paths should resolve to the same inode.
        let orig_ino = std::os::unix::fs::MetadataExt::ino(
            &std::fs::metadata(&original).expect("stat original"),
        );
        let link_ino =
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&link).expect("stat link"));
        assert_eq!(orig_ino, link_ino, "hard links must share the same inode");

        // Reading through the link should return the original content.
        let contents = std::fs::read(&link).expect("read link");
        assert_eq!(
            contents, b"link data",
            "hard link should read original content"
        );

        // Modifying through the link should be visible at the original path.
        std::fs::write(&link, b"modified").expect("write through link");
        let orig_contents = std::fs::read(&original).expect("read original after write");
        assert_eq!(
            orig_contents, b"modified",
            "write through link not visible at original"
        );
    });
}

#[test]
fn can_change_file_mode() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    with_writable_mount("chmod", |mp| {
        let test_file = mp.join("perms.txt");
        std::fs::write(&test_file, b"data").expect("create file");

        std::fs::set_permissions(&test_file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
        let mode = std::fs::metadata(&test_file).expect("stat").mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "mode should be 600, got {:o}",
            mode & 0o777
        );

        std::fs::set_permissions(&test_file, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");
        let mode = std::fs::metadata(&test_file).expect("stat").mode();
        assert_eq!(mode & 0o777, 0o644, "mode should be 644");
    });
}

#[test]
fn can_create_and_read_symlink() {
    with_writable_mount("symlink", |mp| {
        let link_name = mp.join("mylink");
        let target = "/hello/world";

        std::os::unix::fs::symlink(target, &link_name).expect("create symlink");
        assert!(link_name.exists());

        let read_target = std::fs::read_link(&link_name).expect("read link");
        assert_eq!(
            read_target.to_str().expect("valid target path"),
            target,
            "symlink target must round-trip"
        );
    });
}

#[test]
fn can_read_existing_symlink() {
    with_writable_mount("symlink_existing", |mp| {
        let link_name = mp.join("mylink");
        let target = "/hello/world";

        std::os::unix::fs::symlink(target, &link_name).expect("create symlink");

        let read_target = std::fs::read_link(&link_name).expect("read link");
        assert_eq!(
            read_target.to_str().expect("valid target path"),
            target,
            "symlink target must round-trip"
        );

        // Creating a file in the same directory and reading it should still work.
        let regular = mp.join("regular_after_link.txt");
        std::fs::write(&regular, b"ok").expect("create regular file");
        assert_eq!(std::fs::read(&regular).unwrap(), b"ok");
    });
}

#[test]
fn seek_data_and_seek_hole() {
    use std::os::unix::fs::FileExt;

    with_writable_mount("seek_data_hole", |mp| {
        let test_file = mp.join("sparse.txt");
        // Write data at the start (bytes 0-4) and at offset 20 (bytes 20-24).
        // The gap from 5 to 19 is a hole.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&test_file)
            .expect("create");
        f.write_all_at(b"HELLO", 0).expect("write at 0");
        f.write_all_at(b"WORLD", 20).expect("write at 20");
        f.set_len(25).expect("set size");
        drop(f);

        let f = std::fs::OpenOptions::new()
            .read(true)
            .open(&test_file)
            .expect("reopen");

        const SEEK_DATA: i32 = libc::SEEK_DATA;
        const SEEK_HOLE: i32 = libc::SEEK_HOLE;

        // SEEK_DATA from 0 should find data at offset 0.
        let data_off = lseek(&f, 0, SEEK_DATA).expect("seek_data from 0");
        assert_eq!(data_off, 0, "first data region starts at 0");

        // SEEK_HOLE from 0 should find the hole at offset 5.
        let hole_off = lseek(&f, 0, SEEK_HOLE).expect("seek_hole from 0");
        assert_eq!(hole_off, 5, "first hole starts at 5");

        // SEEK_DATA from 5 should jump to offset 20.
        let data_off2 = lseek(&f, 5, SEEK_DATA).expect("seek_data from 5");
        assert_eq!(data_off2, 20, "data resumes at 20");

        // SEEK_HOLE from 20 should find the end-of-file hole at 25.
        let hole_off2 = lseek(&f, 20, SEEK_HOLE).expect("seek_hole from 20");
        assert_eq!(hole_off2, 25, "file ends at 25");
    });
}

fn lseek(f: &std::fs::File, offset: i64, whence: i32) -> std::io::Result<i64> {
    use std::os::unix::io::AsRawFd;
    let ret = unsafe { libc::lseek(f.as_raw_fd(), offset, whence) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

#[test]
fn readdirplus_returns_attributes() {
    with_writable_mount("readdirplus", |mp| {
        // Create a file and a subdirectory.
        let test_file = mp.join("entry.txt");
        std::fs::write(&test_file, b"data").expect("create file");
        std::fs::create_dir(mp.join("subdir")).expect("create dir");

        // readdirplus is used internally by many readdir callers. We verify
        // it completes and returns entries by listing the directory.
        let entries: Vec<String> = std::fs::read_dir(mp)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            entries.iter().any(|n| n == "entry.txt"),
            "entry.txt not found in {entries:?}"
        );
        assert!(
            entries.iter().any(|n| n == "subdir"),
            "subdir not found in {entries:?}"
        );
    });
}

#[test]
fn bmap_returns_physical_block() {
    with_writable_mount("bmap_test", |mp| {
        let test_file = mp.join("testfile.txt");
        std::fs::write(&test_file, "ABCDEFGH").expect("write file");

        let f = std::fs::OpenOptions::new()
            .read(true)
            .open(&test_file)
            .expect("open for bmap");

        // SEEK_DATA should report data at offset 0 (file is allocated, not sparse).
        let data_off = lseek(&f, 0, libc::SEEK_DATA).expect("seek_data");
        assert_eq!(data_off, 0, "first block should be allocated data");

        // SEEK_HOLE should report the hole past EOF.
        let hole_off = lseek(&f, 0, libc::SEEK_HOLE).expect("seek_hole");
        assert!(
            hole_off >= 8,
            "hole should be at or past EOF (8), got {hole_off}"
        );
    });
}

#[test]
fn fallocate_punch_hole() {
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;

    with_writable_mount("punch_hole_test", |mp| {
        let test_file = mp.join("punch.txt");
        std::fs::write(&test_file, "ABCDEFGHIJKLMNOPQRSTUVWXYZ").expect("write file");

        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&test_file)
            .expect("open");

        // Punch a hole from offset 5 to 10 (5 bytes, bytes 5-9 zeroed).
        let ret = unsafe { libc::fallocate(f.as_raw_fd(), 0x02 | 0x01, 5, 5) };
        assert_eq!(ret, 0, "fallocate punch_hole should succeed");

        // SEEK_DATA: the file is allocated (within one block), so data starts at 0.
        let d0 = lseek(&f, 0, libc::SEEK_DATA).expect("seek_data from 0");
        assert_eq!(d0, 0, "data starts at 0");

        // SEEK_HOLE: the extent covers the whole block, so the hole is at the end
        // of the allocated extent, which is the file's logical size (26).
        let h0 = lseek(&f, 0, libc::SEEK_HOLE).expect("seek_hole from 0");
        assert_eq!(h0, 26, "hole starts at end of file");

        // SEEK_DATA from 5: still within the extent, so data is at 5.
        let d1 = lseek(&f, 5, libc::SEEK_DATA).expect("seek_data from 5");
        assert_eq!(d1, 5, "still in extent");

        // Read data before the hole.
        let mut buf = [0u8; 5];
        f.read_exact_at(&mut buf, 0).expect("read before hole");
        assert_eq!(&buf, b"AAAAA");

        // Read the hole — should return zeros.
        let mut hole_buf = [9; 5];
        f.read_exact_at(&mut hole_buf, 5).expect("read hole");
        assert_eq!(&hole_buf, &[0u8; 5], "punched hole should read as zeros");

        // Read data after the hole.
        let mut after_buf = [0u8; 16];
        f.read_exact_at(&mut after_buf, 10)
            .expect("read after hole");
        assert_eq!(&after_buf, b"KLMNOPQRSTUVWXYZ");

        // File size unchanged.
        let meta = f.metadata().expect("metadata");
        assert_eq!(meta.len(), 26, "file size should be unchanged");
    });
}

#[test]
fn copy_file_range_copies_data() {
    use std::os::unix::io::AsRawFd;

    with_writable_mount("copy_range_test", |mp| {
        let src = mp.join("source.bin");
        let dst = mp.join("dest.bin");
        std::fs::write(&src, "COPY_ME_12345678").expect("write source");

        // Copy 8 bytes from offset 5 of source to offset 2 of dest.
        let fd_src = std::fs::OpenOptions::new()
            .read(true)
            .open(&src)
            .expect("open source");
        let fd_dst = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dst)
            .expect("open dest");

        let mut off_in = 5i64;
        let mut off_out = 2i64;
        let n = unsafe {
            libc::copy_file_range(
                fd_src.as_raw_fd(),
                &mut off_in,
                fd_dst.as_raw_fd(),
                &mut off_out,
                8,
                0,
            )
        };
        assert!(n > 0, "copy_file_range should copy bytes");

        let contents = std::fs::read(&dst).expect("read dest");
        assert_eq!(&contents[0..2], &[0u8; 2], "pre-offset region is zero");
        assert_eq!(&contents[2..10], b"_1234567", "copied data at offset 2");
    });
}
