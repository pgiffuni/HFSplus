// SPDX-License-Identifier: BSD-2-Clause

//! Library-level tests for `format_volume` — the in-tree HFS+ formatter.
//!
//! These tests create volumes in memory, verify the parser can read them, and
//! confirm the in-tree checker (`check::check`) finds them consistent. This is
//! the library path `hnewfs` wraps, so a failure here is a bug in `format_volume`
//! itself rather than in argument handling.

mod common;

use std::path::PathBuf;

use hfsplus::blockdev::FileDevice;
use hfsplus::check;
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::format::writer::format_volume;
use hfsplus::volume::Volume;

/// Create a zeroed image file of `size_mb` MiB and return its path.
fn make_image(size_mb: u64) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("format-test-{}-{}.img", std::process::id(), n));
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(size_mb * 1024 * 1024).unwrap();
    path
}

/// Format an image, then run the in-tree checker on it.
fn format_and_check(args: &[(&str, &str)], size_mb: u64, expect_sig: &[u8; 2]) {
    let img = make_image(size_mb);

    let mut dev = FileDevice::open_writable(&img).unwrap();
    let total_bytes = std::fs::metadata(&img).unwrap().len();

    let mut volume_name = "TestVolume";
    let mut block_size: u32 = 4096;
    let mut node_size: u16 = 4096;
    let mut case_sensitive = false;
    let mut journaled = false;
    let mut journal_size: Option<u64> = None;

    for (key, val) in args {
        match *key {
            "name" => volume_name = val,
            "block_size" => block_size = val.parse().unwrap(),
            "node_size" => node_size = val.parse().unwrap(),
            "case_sensitive" => case_sensitive = *val == "true",
            "journaled" => journaled = *val == "true",
            "journal_size" => journal_size = Some(val.parse().unwrap()),
            _ => panic!("unknown arg {key}"),
        }
    }

    format_volume(
        &mut dev,
        volume_name,
        total_bytes,
        block_size,
        node_size,
        case_sensitive,
        journaled,
        journal_size,
        0,
        0,
        0o177,
    )
    .unwrap();
    drop(dev);

    // The parser must accept it.
    let dev = FileDevice::open(&img).unwrap();
    let vol = Volume::open(&dev).unwrap();

    // The signature must match expectations.
    let header = vol.header();
    assert_eq!(
        header.signature,
        u16::from_be_bytes(*expect_sig),
        "expected signature {:02x?}",
        expect_sig
    );
    assert_eq!(header.block_size, block_size);
    assert_eq!(
        header.total_blocks,
        (total_bytes / u64::from(block_size)) as u32
    );

    // The checker must find it consistent.
    let report = check::check(&vol, None).unwrap();
    assert!(
        report.is_clean(),
        "volume should be clean, got: {:?}",
        report.describe()
    );

    let _ = std::fs::remove_file(&img);
}

#[test]
fn a_basic_hfs_plus_volume_round_trips_and_checks_clean() {
    format_and_check(&[("name", "TestVolume")], 1, &[0x48, 0x2b]);
}

#[test]
fn a_case_sensitive_hfsx_volume_round_trips_and_checks_clean() {
    format_and_check(
        &[("name", "CaseX"), ("case_sensitive", "true")],
        1,
        &[0x48, 0x58],
    );
}

#[test]
fn a_journaled_volume_round_trips_and_checks_clean() {
    format_and_check(
        &[("name", "Journaled"), ("journaled", "true")],
        2,
        &[0x48, 0x2b],
    );
}

#[test]
fn a_1k_block_volume_round_trips_and_checks_clean() {
    format_and_check(
        &[("name", "SmallBlock"), ("block_size", "1024")],
        1,
        &[0x48, 0x2b],
    );
}

#[test]
fn a_16k_block_volume_round_trips_and_checks_clean() {
    format_and_check(
        &[("name", "BigBlock"), ("block_size", "16384")],
        2,
        &[0x48, 0x2b],
    );
}

#[test]
fn the_root_folder_is_cnid_2_and_has_the_volume_name() {
    let img = make_image(1);
    let mut dev = FileDevice::open_writable(&img).unwrap();
    let total_bytes = std::fs::metadata(&img).unwrap().len();
    format_volume(
        &mut dev,
        "MyVolume",
        total_bytes,
        4096,
        4096,
        false,
        false,
        None,
        0,
        0,
        0o177,
    )
    .unwrap();
    drop(dev);

    let dev = FileDevice::open(&img).unwrap();
    let vol = Volume::open(&dev).unwrap();

    // Root folder is CNID 2; look it up by CNID.
    let root = vol
        .lookup_cnid(hfsplus::catalog::cnid::ROOT_FOLDER_ID)
        .unwrap();
    assert!(root.is_some(), "root folder (CNID 2) must exist");
    let root = root.unwrap();
    let name = root.name_string();
    assert_eq!(name, "MyVolume", "root folder must carry the volume name");

    let _ = std::fs::remove_file(&img);
}

#[test]
fn the_alternate_header_sits_at_volume_size_minus_1024() {
    let img = make_image(1);
    let mut dev = FileDevice::open_writable(&img).unwrap();
    let total_bytes = std::fs::metadata(&img).unwrap().len();
    format_volume(
        &mut dev,
        "TestVol",
        total_bytes,
        4096,
        4096,
        false,
        false,
        None,
        0,
        0,
        0o177,
    )
    .unwrap();
    drop(dev);

    let bytes = std::fs::read(&img).unwrap();
    let primary = u16::from_be_bytes([bytes[1024], bytes[1025]]);
    let alt_off = (total_bytes - 1024) as usize;
    let alt = u16::from_be_bytes([bytes[alt_off], bytes[alt_off + 1]]);
    assert_eq!(
        primary, alt,
        "primary header ({:04x}) and alternate ({:04x}) must agree",
        primary, alt
    );

    let _ = std::fs::remove_file(&img);
}

#[test]
fn a_journaled_volume_has_journal_pseudo_files_in_the_catalog() {
    let img = make_image(2);
    let mut dev = FileDevice::open_writable(&img).unwrap();
    let total_bytes = std::fs::metadata(&img).unwrap().len();
    format_volume(
        &mut dev,
        "Journaled",
        total_bytes,
        4096,
        4096,
        false,
        true,
        None,
        0,
        0,
        0o177,
    )
    .unwrap();
    drop(dev);

    let dev = FileDevice::open(&img).unwrap();
    let vol = Volume::open(&dev).unwrap();
    let header = vol.header();

    // nextCatalogID must be past the journal pseudo-files (CNIDs 16 and 17).
    assert!(
        header.next_catalog_id >= 18,
        "nextCatalogID must be past the journal files, got {}",
        header.next_catalog_id
    );

    // fileCount must include the two pseudo-files.
    assert_eq!(
        header.file_count, 2,
        "journaled volume should have 2 cataloged files (.journal, .journal_info_block)"
    );

    let _ = std::fs::remove_file(&img);
}

#[test]
fn a_non_journaled_volume_has_no_journal_files() {
    let img = make_image(1);
    let mut dev = FileDevice::open_writable(&img).unwrap();
    let total_bytes = std::fs::metadata(&img).unwrap().len();
    format_volume(
        &mut dev,
        "Plain",
        total_bytes,
        4096,
        4096,
        false,
        false,
        None,
        0,
        0,
        0o177,
    )
    .unwrap();
    drop(dev);

    let dev = FileDevice::open(&img).unwrap();
    let vol = Volume::open(&dev).unwrap();
    let header = vol.header();
    assert_eq!(
        header.file_count, 0,
        "non-journaled volume should have no files"
    );
    assert_eq!(
        header.next_catalog_id,
        hfsplus::catalog::cnid::FIRST_USER_CATALOG_NODE_ID.0,
        "nextCatalogID must be the first user CNID"
    );

    let _ = std::fs::remove_file(&img);
}

#[test]
fn journal_info_block_address_is_within_the_volume() {
    let img = make_image(2);
    let mut dev = FileDevice::open_writable(&img).unwrap();
    let total_bytes = std::fs::metadata(&img).unwrap().len();
    format_volume(
        &mut dev,
        "Journaled",
        total_bytes,
        4096,
        4096,
        false,
        true,
        None,
        0,
        0,
        0o177,
    )
    .unwrap();
    drop(dev);

    let dev = FileDevice::open(&img).unwrap();
    let vol = Volume::open(&dev).unwrap();
    let header = vol.header();
    assert!(
        header.journal_info_block > 0 && header.journal_info_block < header.total_blocks,
        "journalInfoBlock {} must be within {} blocks",
        header.journal_info_block,
        header.total_blocks
    );

    let _ = std::fs::remove_file(&img);
}

#[test]
fn the_volume_header_encodes_all_five_special_forks() {
    let img = make_image(1);
    let mut dev = FileDevice::open_writable(&img).unwrap();
    let total_bytes = std::fs::metadata(&img).unwrap().len();
    format_volume(
        &mut dev,
        "TestVol",
        total_bytes,
        4096,
        4096,
        false,
        false,
        None,
        0,
        0,
        0o177,
    )
    .unwrap();
    drop(dev);

    let dev = FileDevice::open(&img).unwrap();
    let header = VolumeHeader::read_from(&dev).unwrap();

    // The five special forks must all have at least one extent.
    assert!(
        header.allocation_file.extents.raw[0].block_count > 0,
        "allocation file must have blocks"
    );
    assert!(
        header.extents_file.extents.raw[0].block_count > 0,
        "extents file must have blocks"
    );
    assert!(
        header.catalog_file.extents.raw[0].block_count > 0,
        "catalog file must have blocks"
    );
    assert!(
        header.attributes_file.extents.raw[0].block_count > 0,
        "attributes file must have blocks"
    );
    assert!(
        header.startup_file.extents.raw[0].block_count == 0,
        "startup file should be empty"
    );

    let _ = std::fs::remove_file(&img);
}
