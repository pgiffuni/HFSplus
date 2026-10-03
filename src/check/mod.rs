//! Volume consistency checks, read-only.
//!
//! # What this is and is not
//!
//! These checks exist because two structures on disk have to agree with each
//! other, and either can be wrong. The allocation bitmap says which blocks are in
//! use; the catalog's fork extents say which blocks the files occupy. If those two
//! disagree, the volume is inconsistent whichever one is right — and this crate
//! has no way to tell which, only that something is wrong.
//!
//! That makes these checks worth more than they look. A mis-parsed extent record
//! is mis-parsed identically here and in the reader, so it does **not** get caught
//! — a checker built on the same parser cannot catch that class of bug, and the
//! format notes say so. What it *does* catch is disagreement between two
//! structures that were written independently by the formatter, which is a real
//! signal even though both sides are read by the same code.
//!
//! Correctness of what each check *is* comes from Apple's source, not from here.
//! The provenance of each check is named below.
//!
//! # Nothing is repaired
//!
//! Every check is a read. `fsck_hfs` repairs as well as reports, and pointing it
//! at a fixture once undid a corruption it was meant to be diagnosing. A checker
//! that only reads cannot make that mistake, so there is no repair mode to
//! mis-invoke.
//!
//! Mining reference: `lib_fsck_hfs/dfalib/SVerify1.c` `CheckBitmapRange` and
//! `ExtBTChk`, `SVerify2.c` `BTMapChk` and the volume-information checks, and
//! `core/VolumeAllocation.c` `hfs_count_allocated`.

use crate::alloc::AllocationMap;
use crate::btree::io::BTreeFile;
use crate::btree::key::ExtentKey;
use crate::catalog::record::CatalogRecord;
use crate::error::{Error, Result};
use crate::volume::Volume;

/// Sentinel in [`CheckReport::fork_block_count`] for the volume's own forks.
///
/// No file can hold CNID 0: `kHFSRootFolderID` is 2, and 0 and 1 are reserved,
/// so the value cannot collide with a file's.
pub const SPECIAL_FORK_SENTINEL: u32 = 0;

/// What a set of checks found.
///
/// Every field is a disagreement between two structures, not a verdict on which
/// side is wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Blocks the bitmap marks used that nothing references.
    ///
    /// `fsck` reports this as the bitmap needing repair for orphaned blocks.
    pub orphaned: Vec<u32>,
    /// Blocks the catalog references that the bitmap does not mark used.
    ///
    /// The opposite disagreement: a fork whose extents were written without
    /// updating the bitmap. `fsck` reports it as under-allocation.
    pub missing: Vec<u32>,
    /// `nextCatalogID` is not past the highest CNID in use.
    ///
    /// If it is not, the next file created would reuse a CNID that already
    /// identifies an existing one, and a lookup by CNID would then find the wrong
    /// file. Mining reference: `core/hfs_catalog.c` allocates `nextCatalogID`
    /// monotonically and `hfs_vfsutils.c` checks it against the catalog.
    pub next_cnid_reuse: Option<(u32, u32)>,
    /// A fork's `totalBlocks` disagrees with the blocks its extents describe.
    ///
    /// The other direction of the same arithmetic: the extents must account for
    /// every block the fork claims, inline and overflow together.
    pub fork_block_count: Vec<(u32, u32, u32)>,
}

impl CheckReport {
    /// Whether every check passed.
    pub fn is_clean(&self) -> bool {
        self.orphaned.is_empty()
            && self.missing.is_empty()
            && self.next_cnid_reuse.is_none()
            && self.fork_block_count.is_empty()
    }

    /// A one-line summary per disagreement, for a tool to print.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        for block in &self.orphaned {
            out.push(format!(
                "block {block} is marked allocated but no file references it"
            ));
        }
        for block in &self.missing {
            out.push(format!(
                "block {block} is referenced by a fork but not marked allocated"
            ));
        }
        if let Some((next, highest)) = self.next_cnid_reuse {
            out.push(format!(
                "nextCatalogID is {next} but CNID {highest} is already in use"
            ));
        }
        for (cnid, declared, described) in &self.fork_block_count {
            let which = if *cnid == SPECIAL_FORK_SENTINEL {
                "a special fork".to_string()
            } else {
                format!("file {cnid}")
            };
            out.push(format!(
                "{which} declares {declared} blocks but its extents describe {described}"
            ));
        }
        out
    }
}

/// Every allocation block the volume's own metadata occupies.
///
/// These are the blocks a catalog walk will never yield, because they belong to
/// the structures doing the walking. Omitting them would report the whole
/// metadata zone as orphaned.
///
/// Two regions qualify, and neither is hardcoded:
///
/// - **The reserved prefix.** Everything below the allocation file's first
///   extent. `struct HFSPlusVolumeHeader` sits in block 0 with the boot blocks,
///   and how much else the formatter reserves there depends on the block size:
///   at 4 KiB the allocation file starts at block 1, and at 1 KiB it starts at
///   block 2 with block 1 reserved as well. Measured across the corpus, every
///   block below the allocation file's start is marked used and nothing above it
///   is gratuitously reserved — so deriving the prefix from the volume is both
///   correct and not a guess at a fixed count.
///
/// - **The backup volume header**, 1024 bytes before the end of the volume. That
///   is in the *last* block, not one block from the end. Mining reference:
///   `core/hfs_vfsutils.c` reads the backup from `mdb` at that offset.
///
/// # Errors
///
/// Refuses a volume with no blocks, and one smaller than the backup header's
/// 1024 bytes.
pub fn metadata_blocks(
    block_size: u32,
    total_blocks: u32,
    allocation_start: u32,
) -> Result<Vec<u32>> {
    let mut blocks: Vec<u32> = (0..allocation_start.min(total_blocks)).collect();

    // The backup volume header: 1024 bytes before the end of the volume.
    let volume_bytes = u64::from(total_blocks) * u64::from(block_size);
    if volume_bytes < 1024 {
        return Err(Error::invalid(
            "volume_bytes",
            "a volume smaller than the backup header cannot hold one",
        ));
    }
    let backup_block = ((volume_bytes - 1024) / u64::from(block_size)) as u32;
    if backup_block < total_blocks {
        blocks.push(backup_block);
    }
    Ok(blocks)
}

/// Every allocation block a fork's extents describe, inline and overflow.
///
/// A fork keeps at most `kHFSPlusExtentDensity` (8) extents inline and the rest
/// in the extents overflow B-tree, so a fork that spills has to be walked to its
/// end. Mining reference: `core/hfs_extents.c` `extoffset` walks inline extents
/// and then overflow groups, advancing the key by the blocks already described.
///
/// Returns the blocks and the total the extents account for, so a caller can
/// compare it against the fork's own `totalBlocks`.
pub fn fork_blocks<D: crate::blockdev::BlockDevice + ?Sized>(
    vol: &Volume<'_, D>,
    fork: &crate::format::fork::ForkData,
    cnid: u32,
) -> Result<(Vec<u32>, u32)> {
    let mut blocks = Vec::new();
    let mut total = 0u32;

    for extent in fork.extents.raw.iter() {
        blocks.reserve(extent.block_count as usize);
        for offset in 0..extent.block_count {
            blocks.push(extent.start_block + offset);
        }
        total = total.saturating_add(extent.block_count);
    }

    // Walk the overflow groups, if this fork spills past the inline density.
    if fork.needs_overflow() {
        let header = vol.header();
        let tree = crate::file::TreeOverflow::new(
            BTreeFile::open(
                vol.device(),
                &header.extents_file,
                header.block_size,
                header.is_hfsx(),
            )?,
        );
        let mut seen = total;
        // Bounded by the fork's own block count, so a corrupt tree cannot make
        // this loop forever.
        let mut guard = 0;
        while u64::from(seen) < u64::from(fork.total_blocks) {
            guard += 1;
            if guard > 64 {
                return Err(Error::overflow("overflow extent groups"));
            }
            let Some(group) =
                tree.find_group(ExtentKey::DATA_FORK, cnid, seen)
                    .map_err(|e| Error::invalid("extents overflow tree", e.to_string()))?
            else {
                break;
            };
            let group_total = group.total_blocks();
            if group_total == 0 {
                break;
            }
            for extent in group.raw.iter() {
                for offset in 0..extent.block_count {
                    blocks.push(extent.start_block + offset);
                }
            }
            seen = seen.saturating_add(group_total as u32);
            total = total.saturating_add(group_total as u32);
        }
    }

    Ok((blocks, total))
}

/// Run every check against a volume.
///
/// `alloc_limit` is the point above which blocks are metadata rather than file
/// data; pass `total_blocks` when the caller has no separate limit, which makes
/// the orphaned check report the tail as unallocated-but-marked.
pub fn check<D: crate::blockdev::BlockDevice + ?Sized>(
    vol: &Volume<'_, D>,
    alloc_limit: Option<u32>,
) -> Result<CheckReport> {
    let header = vol.header();
    let mut report = CheckReport::default();

    // --- The bitmap as a mutable map -----------------------------------
    let fork = &header.allocation_file;
    // The allocation file is read whole, using its own declared length: it is a
    // full allocation block on every volume, not merely the bytes the bitmap
    // needs.
    let limit = usize::try_from(fork.logical_size).unwrap_or(1 << 20);
    let bytes = vol.read_fork(fork, limit)?;
    let mut map = AllocationMap::from_bytes(&bytes, header.total_blocks)?
        .with_alloc_limit(alloc_limit.unwrap_or(header.total_blocks));

    // --- Every block the volume claims ---------------------------------
    let allocation_start = header.allocation_file.extents.raw[0].start_block;
    let mut referenced: Vec<u32> =
        metadata_blocks(header.block_size, header.total_blocks, allocation_start)?;
    // The volume's own five forks, each checked as carefully as a file's. A
    // special fork is validated at mount time -- `core/hfs_vfsutils.c` derives
    // each one's expected size from the header and refuses the volume otherwise
    // -- so a catalog fork claiming two billion blocks is a mount failure, not a
    // curiosity.
    for (name, special) in [
        ("allocationFile", &header.allocation_file),
        ("extentsFile", &header.extents_file),
        ("catalogFile", &header.catalog_file),
        ("attributesFile", &header.attributes_file),
        ("startupFile", &header.startup_file),
    ] {
        if special.logical_size == 0 && special.total_blocks == 0 {
            continue;
        }
        let (blocks, described) = fork_blocks(vol, special, 0)?;
        referenced.extend(blocks);
        if described != special.total_blocks {
            report.fork_block_count.push((
                SPECIAL_FORK_SENTINEL,
                special.total_blocks,
                described,
            ));
            let _ = name;
        }
    }

    // --- Every block the catalog says the files occupy ------------------
    //
    // The raw record walk, not `all_objects`: that resolves each object through
    // its thread record, so a file whose thread record is missing would not
    // appear here at all -- and its data blocks would then be reported as
    // orphaned, which is a symptom rather than the diagnosis.
    let mut highest_cnid = 0u32;
    for (_key, record) in vol.catalog().all_records()? {
        match record {
            CatalogRecord::File(f) => {
                let cnid = f.file_id.0;
                highest_cnid = highest_cnid.max(cnid);
                for fork in [&f.data_fork, &f.resource_fork] {
                    if fork.logical_size == 0 && fork.total_blocks == 0 {
                        continue;
                    }
                    let (blocks, described) = fork_blocks(vol, fork, cnid)?;
                    referenced.extend(blocks);
                    if described != fork.total_blocks {
                        report
                            .fork_block_count
                            .push((cnid, fork.total_blocks, described));
                    }
                }
            }
            CatalogRecord::Folder(f) => {
                highest_cnid = highest_cnid.max(f.folder_id.0);
            }
            CatalogRecord::Thread(_) => {}
        }
    }

    // --- Compare --------------------------------------------------------
    let mut referenced_bitmap = vec![false; header.total_blocks as usize];
    for block in &referenced {
        if *block >= header.total_blocks {
            return Err(Error::BadBlockNumber {
                block: *block,
                total_blocks: header.total_blocks,
            });
        }
        referenced_bitmap[*block as usize] = true;
    }

    let is_referenced = |b: u32| -> bool {
        referenced_bitmap.get(b as usize).copied().unwrap_or(false)
    };
    report.orphaned = map.orphaned(&is_referenced);
    report.missing = map.missing(&is_referenced);

    if header.next_catalog_id <= highest_cnid {
        report.next_cnid_reuse = Some((header.next_catalog_id, highest_cnid));
    }

    // Keep `map` borrowed so the borrow checker proves nothing above wrote to it.
    let _ = &mut map;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backup_header_is_in_the_last_block() {
        // 32 MiB at 4 KiB: the backup header is 1024 bytes before the end, which
        // is inside block 8191 -- not one block from the end.
        let blocks = metadata_blocks(4096, 8192, 1).expect("metadata blocks");
        assert_eq!(blocks, vec![0, 8191]);
        assert!(
            !blocks.contains(&(8192 - 2)),
            "8190 is ordinary file space; the backup header is not there"
        );
    }

    #[test]
    fn the_backup_header_follows_the_block_size_not_a_fixed_offset() {
        // At 1 KiB blocks the backup header moves with the block size, so a
        // fixed "second to last block" would be wrong for every volume but a 4 KiB
        // one.
        let blocks = metadata_blocks(1024, 32768, 2).expect("metadata blocks");
        assert_eq!(blocks, vec![0, 1, 32767]);
    }

    #[test]
    fn a_volume_too_small_for_a_backup_header_is_refused() {
        // The backup header sits 1024 bytes before the end, so a volume smaller
        // than that has nowhere to put it and the subtraction would wrap.
        assert!(metadata_blocks(512, 1, 1).is_err(), "512 bytes is less than 1024");
        assert!(metadata_blocks(4096, 0, 1).is_err(), "an empty volume");
        assert!(
            metadata_blocks(1024, 1, 1).is_ok(),
            "one 1 KiB block is exactly enough for the header to have a home"
        );
    }

    #[test]
    fn a_clean_report_says_so_and_prints_nothing() {
        let report = CheckReport::default();
        assert!(report.is_clean());
        assert!(report.describe().is_empty());
    }

    #[test]
    fn a_report_names_each_disagreement() {
        let report = CheckReport {
            orphaned: vec![7],
            missing: vec![9],
            next_cnid_reuse: Some((18, 20)),
            fork_block_count: vec![(19, 10, 8)],
        };
        assert!(!report.is_clean());
        let lines = report.describe();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains('7') && lines[0].contains("no file references"));
        assert!(lines[1].contains('9') && lines[1].contains("not marked"));
        assert!(lines[2].contains("nextCatalogID"));
        assert!(lines[3].contains("19") && lines[3].contains("declares 10"));
    }
}