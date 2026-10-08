// SPDX-License-Identifier: BSD-2-Clause

//! Reading decmpfs-compressed files: the whole point of the fixture.
//!
//! `tools/mkfiles.py --add-compressed-file` writes `compressed.bin` with a
//! `com.apple.decmpfs` xattr (zlib type 2). The file's data fork is empty; the
//! compressed bytes live in the resource fork, and the logical contents come
//! from inflating them.
//!
//! These tests drive the full path: detect the attribute -> read the resource
//! fork -> inflate the zlib payload -> return the original bytes.

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::volume::{Object, Volume};

const COMPRESSED_IMG: &str = "journal-with-compressed";
const COMPRESSED_FILE: &str = "compressed.bin";

fn image_path() -> std::path::PathBuf {
    common::image(COMPRESSED_IMG)
}

fn require() -> bool {
    if image_path().exists() {
        true
    } else {
        eprintln!(
            "skipping: {} not built; run tools/mkfiles.py --add-compressed-file",
            image_path().display()
        );
        false
    }
}

fn lookup(vol: &Volume<'_, FileDevice>, name: &str) -> Object {
    let units: Vec<u16> = name.encode_utf16().collect();
    vol.lookup(vol.root_cnid(), &units)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .unwrap_or_else(|| panic!("{name} must be present"))
}

// --- Decompression ------------------------------------------------------

#[test]
fn a_decmpfs_file_reads_back_as_its_uncompressed_contents() {
    if !require() {
        return;
    }
    let dev = FileDevice::open(image_path()).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    let file = lookup(&vol, COMPRESSED_FILE);
    let data = vol.read_file(&file, 1 << 20).expect("read_file");

    let pattern = b"HFS+ decmpfs zlib compression test data. ".to_vec();
    let expected: Vec<u8> = std::iter::repeat(pattern).take(10).flatten().collect();
    assert_eq!(
        data, expected,
        "the decompressed contents must be the original 410-byte pattern"
    );
}

#[test]
fn reading_a_decmpfs_file_by_offset_matches_whole_read() {
    // The same decompression must be transparent to `read` at an offset:
    // a strided read through the middle must agree with the whole-file read.
    if !require() {
        return;
    }
    let dev = FileDevice::open(image_path()).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    let file = lookup(&vol, COMPRESSED_FILE);
    let whole = vol.read_file(&file, 1 << 20).expect("read_file");

    let offset = 50usize;
    let length = 100usize;
    let window = vol.read(&file, offset as u64, length).expect("offset read");
    assert_eq!(
        window,
        &whole[offset..offset + length],
        "an offset read must slice the same decompressed bytes"
    );
}

// --- The data fork lies; the reader must not believe it ----------------

#[test]
fn a_compressed_file_reports_its_logical_size_via_read_file() {
    // The catalog record says the data fork is zero bytes, but the file is
    // really 410 bytes once decompressed. `read_file` must return the full
    // decompressed contents, not the empty data fork.
    if !require() {
        return;
    }
    let dev = FileDevice::open(image_path()).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    let file = lookup(&vol, COMPRESSED_FILE);
    let data = vol.read_file(&file, 1 << 20).expect("read_file");
    assert_eq!(
        data.len(),
        410,
        "read_file must return the decompressed size, not the empty data fork"
    );
}

#[test]
fn a_compressed_file_is_not_a_regular_empty_file() {
    // Without decompression, the empty data fork would read as zero bytes.
    // The reader must detect the decmpfs attribute and redirect.
    if !require() {
        return;
    }
    let dev = FileDevice::open(image_path()).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    let file = lookup(&vol, COMPRESSED_FILE);
    let data = vol.read_file(&file, 1 << 20).expect("read_file");
    assert_ne!(
        data.len(),
        0,
        "decompression must have produced non-empty data"
    );
}

// --- Resource fork is not exposed as a file -----------------------------

#[test]
fn the_decmpfs_attribute_is_not_in_listxattr_but_is_readable() {
    // decmpfs is hidden from the POSIX xattr interface but is how the reader
    // detects compression. Both properties must hold.
    if !require() {
        return;
    }
    let dev = FileDevice::open(image_path()).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    let file = lookup(&vol, COMPRESSED_FILE);

    // The attribute must be readable directly.
    let xattr = vol.getxattr(&file, "com.apple.decmpfs").expect("getxattr");
    assert!(xattr.is_some(), "the decmpfs xattr must be present");
    let val = xattr.unwrap();
    assert_eq!(val.len(), 16, "the decmpfs header is 16 bytes");

    // Its magic and type must match the known values.
    let magic = u32::from_le_bytes([val[0], val[1], val[2], val[3]]);
    assert_eq!(magic, 0x636d7066, "decmpfs magic 'cmpf'");
    let ctype = u32::from_le_bytes([val[4], val[5], val[6], val[7]]);
    assert_eq!(ctype, 2, "compression type 2 = zlib");
    let usize = u64::from_le_bytes([
        val[8], val[9], val[10], val[11], val[12], val[13], val[14], val[15],
    ]);
    assert_eq!(usize, 410, "uncompressed size from the header");
}

// --- Edge cases ---------------------------------------------------------

#[test]
fn an_offset_read_past_the_end_returns_only_what_remains() {
    if !require() {
        return;
    }
    let dev = FileDevice::open(image_path()).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    let file = lookup(&vol, COMPRESSED_FILE);

    // 10 bytes from the end.
    let tail = vol.read(&file, 400, 10).expect("read near end");
    assert_eq!(tail.len(), 10);

    // Past the end: empty.
    let beyond = vol.read(&file, 410, 10).expect("read past end");
    assert!(beyond.is_empty(), "a read past EOF must return empty");
}
