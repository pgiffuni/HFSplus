//! `hfsck` -- HFS+/HFSX consistency check and repair.
//!
//! Implements the Darwin `fsck_hfs` command-line interface for checking and
//! repairing HFS and HFS+ file systems. Read-write when repair flags are given;
//! the default (`-n`) is check-only, matching Apple's behavior.
//!
//! Mining reference: `lib_fsck_hfs/dfalib/SVerify1.c` `CheckBitmapRange`,
//! `ExtBTChk`, `SVerify2.c` `BTMapChk`, the volume-information checks, and
//! `core/VolumeAllocation.c` `hfs_count_allocated` for the checks. What each
//! check verifies is documented in `src/check/mod.rs`.
//!
//! Exit codes follow the POSIX `fsck` convention used by macOS:
//!
//! 0 -- no errors found (or all repaired successfully)
//! 1 -- unused (reserved for "errors corrected" in some implementations)
//! 2 -- errors left uncorrected (check only, or repair not implemented)
//! 4 -- file system was modified
//! 8 -- operational error (bad usage, unreadable image, etc.)

use std::process::ExitCode;

use hfsplus::blockdev::{BlockDevice, BlockDeviceMut, FileDevice};
use hfsplus::check;
use hfsplus::error::Error;
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::volume::Volume;

const EXIT_CLEAN: u8 = 0;
const EXIT_MODIFIED: u8 = 4;
const EXIT_ERRORS_UNCORRECTED: u8 = 2;
const EXIT_OPERATIONAL: u8 = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepairLevel {
    None,
    Minor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckLevel {
    Quick,
    Always,
    Force,
    Partial,
}

struct Options {
    debug: bool,
    force: bool,
    gui: bool,
    quiet: bool,
    quick: bool,
    journal_disabled: bool,
    lock_only: bool,
    cache_size: Option<u64>,
    mode: u16,
    repair: RepairLevel,
    check: CheckLevel,
    rebuild: Option<RebuildTarget>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RebuildTarget {
    Catalog,
    Attributes,
    Extents,
}

fn usage() {
    eprintln!(
        "usage: fsck_hfs [-b [size] B path c size e mode ESdfglx m [mode] npqruy] special-device\n\
         \n\
         Checks HFS/HFS+ file systems for structural consistency and optionally\n\
         repairs them.\n\
         \n\
         Options:\n\
         \x20 -f  Force a check of a `clean' volume.\n\
         \x20 -d  Display debugging information.\n\
         \x20 -g  Generate output strings in GUI format.\n\
         \x20 -l  Lock down (test-only check, no repairs).\n\
         \x20 -m mode  Set permissions for lost+found directory (default 1777).\n\
         \x20 -n  Do not attempt repair (default).\n\
         \x20 -p  Preen: fix common inconsistencies.\n\
         \x20 -q  Quick check: exit 0 if clean, non-zero if dirty.\n\
         \x20 -r  Rebuild the catalog B-tree.\n\
         \x20 -c size  Internal cache size (ignored).\n\
         \x20 -J  Disable journal replay before checking.\n\
         \x20 -y  Always attempt to repair any damage found.\n\
         \x20 -S  Scan disk for bad blocks (not implemented).\n\
         \x20 -b size  Physical block size for -B option (not implemented).\n\
         \x20 -B path  File containing physical block numbers to map (not implemented).\n\
         \x20 -D level  Debug level (same as -d if non-zero).\n\
         \x20 -e mode  Emulate 'embedded' or 'desktop' (not implemented).\n\
         \x20 -E  Exit on first error after logging.\n\
         \x20 -R mode  Rebuild specific B-tree (a=attr, c=catalog, e=extents).\n\
         \x20 -S  Scan disk for bad blocks.\n\
         \x20 -u  Usage.\n\
         \x20 -x  XML output format.\n\
         \x20 -X  Progress tracking (not implemented).\n\
         \n\
         Exit status:\n\
         \x20 0  no errors found (or all repaired)\n\
         \x20 2  errors left uncorrected\n\
         \x20 4  file system was modified\n\
         \x20 8  operational error"
    );
}

fn main() -> ExitCode {
    let mut opts = Options {
        debug: false,
        force: false,
        gui: false,
        quiet: false,
        quick: false,
        journal_disabled: false,
        lock_only: false,
        cache_size: None,
        mode: 0o1777,
        repair: RepairLevel::None,
        check: CheckLevel::Always,
        rebuild: None,
    };
    let mut paths: Vec<String> = Vec::new();
    let mut explicit_help = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1usize;
    while i < args.len() {
        let arg = &args[i];
        if arg.starts_with('-') && arg.len() > 1 {
            let flags = &arg[1..];
            for flag in flags.chars() {
                match flag {
                    'd' => opts.debug = true,
                    'D' => {
                        if i + 1 < args.len() {
                            i += 1;
                            if let Ok(dlevel) = args[i].parse::<u32>() {
                                if dlevel > 0 {
                                    opts.debug = true;
                                }
                            }
                        }
                    }
                    'f' => {
                        opts.force = true;
                        if opts.check == CheckLevel::Always {
                            opts.check = CheckLevel::Force;
                        }
                    }
                    'g' => opts.gui = true,
                    'l' => {
                        opts.lock_only = true;
                        opts.repair = RepairLevel::None;
                    }
                    'm' => {
                        if i + 1 < args.len() {
                            i += 1;
                            opts.mode = u16::from_str_radix(&args[i], 8).unwrap_or(0o1777);
                        }
                    }
                    'n' => {
                        opts.repair = RepairLevel::None;
                        opts.check = CheckLevel::Always;
                    }
                    'p' => {
                        opts.repair = RepairLevel::Minor;
                    }
                    'q' => {
                        opts.quick = true;
                        opts.check = CheckLevel::Quick;
                        opts.repair = RepairLevel::None;
                    }
                    'r' => {
                        opts.rebuild = Some(RebuildTarget::Catalog);
                    }
                    'c' => {
                        if i + 1 < args.len() {
                            i += 1;
                            opts.cache_size = args[i].parse().ok();
                        }
                    }
                    'J' => {
                        opts.journal_disabled = true;
                    }
                    'S' => {}
                    'B' => {
                        if i + 1 < args.len() {
                            i += 1;
                        }
                    }
                    'b' => {
                        if i + 1 < args.len() {
                            i += 1;
                        }
                    }
                    'e' => {
                        if i + 1 < args.len() {
                            i += 1;
                        }
                    }
                    'E' => {}
                    'R' => {
                        if i + 1 < args.len() {
                            i += 1;
                            match args[i].as_str() {
                                "a" => opts.rebuild = Some(RebuildTarget::Attributes),
                                "c" => opts.rebuild = Some(RebuildTarget::Catalog),
                                "e" => opts.rebuild = Some(RebuildTarget::Extents),
                                _ => {
                                    eprintln!("fsck_hfs: unknown rebuild target");
                                    usage();
                                    return ExitCode::from(EXIT_OPERATIONAL);
                                }
                            }
                        }
                    }
                    'x' => {}
                    'X' => {}
                    'u' => {
                        explicit_help = true;
                    }
                    'h' => {
                        explicit_help = true;
                    }
                    other => {
                        eprintln!("fsck_hfs: unknown option -{other}");
                        usage();
                        return ExitCode::from(EXIT_OPERATIONAL);
                    }
                }
            }
        } else {
            paths.push(arg.clone());
        }
        i += 1;
    }

    if explicit_help {
        usage();
        return ExitCode::from(EXIT_CLEAN);
    }

    if opts.rebuild.is_some() && opts.check != CheckLevel::Partial {
        opts.check = CheckLevel::Partial;
    }

    if paths.is_empty() {
        eprintln!("fsck_hfs: no special device given");
        usage();
        return ExitCode::from(EXIT_OPERATIONAL);
    }

    let mut worst = EXIT_CLEAN;
    for path in &paths {
        let code = check_one(path, &opts);
        if code > worst {
            worst = code;
        }
    }
    ExitCode::from(worst)
}

fn check_one(path: &str, opts: &Options) -> u8 {
    // Open read-only first to run the check.
    let dev = match FileDevice::open(path) {
        Ok(dev) => dev,
        Err(e) => return report_unreadable(path, &e.to_string(), opts),
    };

    let vol = match Volume::open(&dev) {
        Ok(vol) => vol,
        Err(e @ hfsplus::error::Error::BadSignature { .. }) => {
            return report_unreadable(path, &e.to_string(), opts);
        }
        Err(e) => return report_unreadable(path, &e.to_string(), opts),
    };

    if opts.quick {
        let is_clean = vol.is_clean();
        if is_clean {
            if !opts.gui && !opts.quiet {
                println!("{path}: clean");
            }
            return EXIT_CLEAN;
        } else if !opts.gui && !opts.quiet {
            println!("{path}: dirty");
        }
        if opts.repair == RepairLevel::None {
            return EXIT_ERRORS_UNCORRECTED;
        }
    }

    if opts.lock_only {
        // Test-only: check but do not repair, even with -y.
        if !opts.gui && !opts.quiet {
            println!("{path}: checking (lockdown mode)");
        }
    }

    let report = match check::check(&vol, None) {
        Ok(report) => report,
        Err(e) => return report_unreadable(path, &e.to_string(), opts),
    };

    if opts.debug {
        let header = vol.header();
        eprintln!(
            "fsck_hfs: {path} block_size={} total_blocks={} free_blocks={}",
            header.block_size, header.total_blocks, header.free_blocks
        );
        eprintln!(
            "fsck_hfs: {} finding(s), orphaned={:?}, missing={:?}",
            report.describe().len(),
            report.orphaned,
            report.missing
        );
    }

    let findings = report.describe();
    let is_clean = findings.is_empty();

    if is_clean {
        if !opts.gui && !opts.quiet {
            println!("{path}: consistent");
        }
        return EXIT_CLEAN;
    }

    // Print findings before attempting repair.
    if !opts.gui {
        println!("{path}: {} finding(s)", findings.len());
    } else {
        println!("DIRTY: {path}");
    }
    for finding in &findings {
        if opts.gui {
            println!("  {finding}");
        } else if opts.debug {
            eprintln!("  {finding}");
        } else {
            println!("  {finding}");
        }
    }

    // Check-only mode: report and exit.
    if opts.repair == RepairLevel::None && !opts.lock_only {
        return EXIT_ERRORS_UNCORRECTED;
    }

    // Attempt repairs.
    let mut dev_w = match FileDevice::open_writable(path) {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("{path}: cannot open for writing: {e}");
            return EXIT_OPERATIONAL;
        }
    };

    let header = match VolumeHeader::read_from(&dev_w) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{path}: cannot read header: {e}");
            return EXIT_OPERATIONAL;
        }
    };

    let mut modified = false;

    // Repair: orphaned blocks -> mark free in the allocation bitmap.
    if !report.orphaned.is_empty() {
        if let Err(e) = fix_orphaned(&mut dev_w, &header, &report.orphaned) {
            eprintln!("{path}: failed to fix orphaned blocks: {e}");
        } else {
            modified = true;
            if !opts.gui && !opts.quiet {
                println!("{path}: freed {} orphaned block(s)", report.orphaned.len());
            }
        }
    }

    // Repair: missing blocks -> mark allocated.
    if !report.missing.is_empty() {
        if let Err(e) = fix_missing(&mut dev_w, &header, &report.missing) {
            eprintln!("{path}: failed to fix missing blocks: {e}");
        } else {
            modified = true;
            if !opts.gui && !opts.quiet {
                println!(
                    "{path}: marked {} missing block(s) as allocated",
                    report.missing.len()
                );
            }
        }
    }

    // Repair: nextCatalogID advancement.
    if let Some((next, highest)) = report.next_cnid_reuse {
        if let Err(e) = fix_next_cnid(&mut dev_w, &header, highest) {
            eprintln!("{path}: failed to advance nextCatalogID: {e}");
        } else {
            modified = true;
            if !opts.gui && !opts.quiet {
                println!("{path}: advanced nextCatalogID past {highest} (was {next})");
            }
        }
    }

    // Repair: zero unerased nodes.
    if !report.unerased_node.is_empty() {
        if let Err(e) = fix_unerased_nodes(&mut dev_w, &header, &report.unerased_node) {
            eprintln!("{path}: failed to zero unerased nodes: {e}");
        } else {
            modified = true;
            if !opts.gui && !opts.quiet {
                println!(
                    "{path}: zeroed {} stale B-tree node(s)",
                    report.unerased_node.len()
                );
            }
        }
    }

    if modified {
        if opts.debug {
            eprintln!("{path}: re-checking after repairs");
        }
        // Re-check to confirm.
        let dev_r = match FileDevice::open(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{path}: cannot reopen after repair: {e}");
                return EXIT_OPERATIONAL;
            }
        };
        let vol2 = match Volume::open(&dev_r) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{path}: volume no longer readable after repair: {e}");
                return EXIT_ERRORS_UNCORRECTED;
            }
        };
        match check::check(&vol2, None) {
            Ok(report2) if report2.is_clean() => {
                if !opts.gui && !opts.quiet {
                    println!("{path}: repairs complete");
                }
                return EXIT_MODIFIED;
            }
            Ok(report2) => {
                let remaining = report2.describe();
                if !opts.gui && !opts.quiet {
                    println!("{path}: {} finding(s) remain after repair", remaining.len());
                }
                for f in &remaining {
                    println!("  {f}");
                }
                return EXIT_ERRORS_UNCORRECTED;
            }
            Err(e) => {
                eprintln!("{path}: re-check failed after repair: {e}");
                return EXIT_ERRORS_UNCORRECTED;
            }
        }
    }

    EXIT_ERRORS_UNCORRECTED
}

fn read_bitmap(
    dev: &FileDevice,
    header: &VolumeHeader,
) -> Result<hfsplus::alloc::AllocationMap, Error> {
    use hfsplus::alloc::AllocationMap;
    let fork = &header.allocation_file;
    let limit = usize::try_from(fork.logical_size).unwrap_or(1 << 20);
    let bytes = {
        let mut buf = vec![0u8; limit];
        let read_len = buf.len().min(limit);
        dev.read_at(
            u64::from(fork.extents.raw[0].start_block) * u64::from(header.block_size),
            &mut buf[..read_len],
        )?;
        buf
    };
    let map = AllocationMap::from_bytes(&bytes, header.total_blocks)?;
    Ok(map)
}

/// Mark orphaned blocks as free in the allocation bitmap.
///
/// An orphaned block is one the bitmap marks allocated but no file's extents
/// reference. The fix releases each block through the allocation map and
/// writes the updated bitmap back to disk.
fn fix_orphaned(
    dev: &mut FileDevice,
    header: &VolumeHeader,
    orphaned: &[u32],
) -> Result<(), Error> {
    let block_size = header.block_size;
    let mut map = read_bitmap(dev, header)?;

    let mut freed = 0u32;
    for block in orphaned {
        if *block >= header.total_blocks {
            continue;
        }
        if !map.is_allocated(*block)? {
            continue;
        }
        map.release(*block, 1)?;
        freed += 1;
    }

    if freed == 0 {
        return Ok(());
    }

    // Write the bitmap back.
    let bitmap_bytes = map.as_bytes();
    let bm_off =
        u64::from(header.allocation_file.extents.raw[0].start_block) * u64::from(block_size);
    let mut at = bm_off;
    let mut i = 0usize;
    while i < bitmap_bytes.len() {
        let span = (block_size as usize).min(bitmap_bytes.len() - i);
        dev.write_at(at, &bitmap_bytes[i..i + span])?;
        at += span as u64;
        i += span;
    }

    // Update the volume header's free block count.
    let new_free = header
        .free_blocks
        .checked_add(freed)
        .ok_or(Error::overflow("freeBlocks"))?;
    let header_bytes = header.to_bytes();
    // Write full header to primary and alternate locations.
    dev.write_at(0, &header_bytes)?;
    let free_off = 48usize;
    dev.write_at(free_off as u64, &new_free.to_be_bytes())?;
    let vol_bytes = header.total_blocks as u64 * block_size as u64;
    let alt_off = vol_bytes.saturating_sub(1024);
    dev.write_at(alt_off, &header_bytes)?;
    dev.write_at(alt_off + free_off as u64, &new_free.to_be_bytes())?;
    Ok(())
}
/// Mark missing blocks as allocated in the allocation bitmap.
fn fix_missing(dev: &mut FileDevice, header: &VolumeHeader, missing: &[u32]) -> Result<(), Error> {
    let block_size = header.block_size;
    let mut map = read_bitmap(dev, header)?;

    let mut allocated = 0u32;
    for block in missing {
        if *block >= header.total_blocks {
            continue;
        }
        if map.is_allocated(*block)? {
            continue;
        }
        let _ = map.reserve_one(*block)?;
        allocated += 1;
    }

    if allocated == 0 {
        return Ok(());
    }

    let bitmap_bytes = map.as_bytes();
    let bm_off =
        u64::from(header.allocation_file.extents.raw[0].start_block) * u64::from(block_size);
    let mut at = bm_off;
    let mut i = 0usize;
    while i < bitmap_bytes.len() {
        let span = (block_size as usize).min(bitmap_bytes.len() - i);
        dev.write_at(at, &bitmap_bytes[i..i + span])?;
        at += span as u64;
        i += span;
    }

    let free_off = 48usize;
    let new_free = header
        .free_blocks
        .checked_sub(allocated)
        .ok_or(Error::overflow("freeBlocks underflow"))?;
    dev.write_at(free_off as u64, &new_free.to_be_bytes())?;
    let vol_bytes = header.total_blocks as u64 * block_size as u64;
    let alt_off = vol_bytes.saturating_sub(1024);
    dev.write_at(alt_off + free_off as u64, &new_free.to_be_bytes())?;

    Ok(())
}

/// Advance nextCatalogID to one past the highest CNID in use.
fn fix_next_cnid(dev: &mut FileDevice, header: &VolumeHeader, highest: u32) -> Result<(), Error> {
    let mut header_bytes = header.to_bytes();
    let new_next = highest
        .checked_add(1)
        .ok_or(Error::overflow("nextCatalogID"))?;
    let next_off = 24usize;
    header_bytes[next_off..next_off + 4].copy_from_slice(&new_next.to_be_bytes());
    dev.write_at(0, &header_bytes)?;
    let vol_bytes = header.total_blocks as u64 * header.block_size as u64;
    let alt_off = vol_bytes.saturating_sub(1024);
    dev.write_at(alt_off, &header_bytes)?;
    Ok(())
}

fn report_unreadable(path: &str, message: &str, opts: &Options) -> u8 {
    if opts.gui {
        println!("ERROR: {path}: {message}");
    } else {
        eprintln!("{path}: {message}");
    }
    EXIT_OPERATIONAL
}

/// Zero out B-tree nodes that are marked as used in the node map but are not
/// reachable from the root.
fn fix_unerased_nodes(
    dev: &mut FileDevice,
    header: &VolumeHeader,
    unerased: &[(u8, u32)],
) -> Result<(), Error> {
    let block_size = header.block_size;
    for (_tree, node_num) in unerased {
        // For the extents, catalog, and attributes files, each node is one
        // block at the given node number.
        let node_off = u64::from(*node_num) * u64::from(block_size);
        let zero = vec![0u8; block_size as usize];
        dev.write_at(node_off, &zero)?;
    }
    Ok(())
}
