//! `hfsinspect` — print HFS family volume header facts for an image.
//!
//! A standalone entry point to the format layer, so that the library can be
//! exercised and debugged without a FUSE mount. It is also the tool the
//! differential test harness uses to extract machine-readable volume facts for
//! comparison against `fsck.hfsplus` and `hfsfuse`.
//!
//! # Usage
//!
//! ```text
//! hfsinspect [--json] [--verbose] <image>...
//! ```
//!
//! Exit status is 0 if every image inspected cleanly, 1 on a usage error, and 2
//! if any image failed to parse. A malformed image is a *result*, not a crash,
//! so the tool reports it and keeps going rather than aborting.

use std::process::ExitCode;

use hfsplus::blockdev::{BlockDevice, FileDevice, ViewDevice, VOLUME_HEADER_OFFSET};
use hfsplus::error::{Error, Result};
use hfsplus::format::extents::ExtentDescriptor;
use hfsplus::format::fork::ForkData;
use hfsplus::format::volume_header::{FileSystemKind, VolumeHeader, VOLUME_HEADER_SIZE};

fn main() -> ExitCode {
    let Args { opts, paths } = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            print_usage();
            return ExitCode::from(1);
        }
        Err(msg) => {
            eprintln!("hfsinspect: {msg}");
            print_usage();
            return ExitCode::from(1);
        }
    };

    if paths.is_empty() {
        print_usage();
        return ExitCode::from(1);
    }

    let mut any_failed = false;
    for path in &paths {
        match inspect(path, opts) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                any_failed = true;
                if opts.json {
                    println!(
                        "{{\"path\":{},\"ok\":false,\"error\":{}}}",
                        json_string(path),
                        json_string(&e.to_string())
                    );
                } else {
                    eprintln!("hfsinspect: {path}: {e}");
                }
            }
        }
    }

    if any_failed {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

#[derive(Debug, Clone, Copy)]
struct Options {
    json: bool,
    verbose: bool,
}

struct Args {
    opts: Options,
    paths: Vec<String>,
}

fn parse_args() -> std::result::Result<Option<Args>, String> {
    let mut opts = Options { json: false, verbose: false };
    let mut paths = Vec::new();
    let it = std::env::args().skip(1);
    for arg in it {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--verbose" | "-v" => opts.verbose = true,
            "--help" | "-h" => return Ok(None),
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option `{s}`"))
            }
            s => paths.push(s.to_string()),
        }
    }
    Ok(Some(Args { opts, paths }))
}

fn print_usage() {
    eprintln!(
        "usage: hfsinspect [--json] [--verbose] <image>...\n\
         \n\
         Prints HFS+/HFSX volume header facts: signature, version, allocation\n\
         block size, block counts, special-file fork geometry, and journal state."
    );
}

fn inspect(path: &str, opts: Options) -> Result<String> {
    let dev = FileDevice::open(path)?;
    inspect_device(&dev, path, opts)
}

fn inspect_device<D: BlockDevice + ?Sized>(
    dev: &D,
    path: &str,
    opts: Options,
) -> Result<String> {
    let vh = VolumeHeader::read_from(dev)?;
    if opts.json {
        Ok(render_json(vh, path))
    } else {
        Ok(render_text(vh, dev, path, opts))
    }
}

fn render_text(vh: VolumeHeader, dev: &(impl BlockDevice + ?Sized), path: &str, opts: Options) -> String {
    let kind = vh.kind().unwrap_or(FileSystemKind::HfsPlus);
    let mut out = String::new();

    out.push_str(&format!("image:          {path}\n"));
    out.push_str(&format!("device length:  {} bytes\n", dev.len().unwrap_or(0)));
    out.push_str(&format!(
        "signature:      0x{:04x} ({})\n",
        vh.signature,
        match kind {
            FileSystemKind::HfsPlus => "H+",
            FileSystemKind::HfsX => "HX",
            FileSystemKind::ClassicHfs => "BD",
        }
    ));
    out.push_str(&format!("filesystem:     {}\n", kind_name(kind)));
    out.push_str(&format!("version:        0x{:04x}\n", vh.version));
    // The volume name is not a volume-header field: it is the name of the root
    // folder (CNID 2) in the catalog B-tree. Mining reference: Apple
    // core/hfs_vfsutils.c (hfs_MountHFSPlusVolume) calls
    // cat_idlookup(kHFSRootFolderID) and copies cd_nameptr into vcb->vcbVN.
    out.push_str(&format!("root folder id: {}\n", vh.root_folder_id()));
    out.push_str(&format!(
        "block size:     {} bytes\n",
        vh.block_size
    ));
    out.push_str(&format!("total blocks:   {}\n", vh.total_blocks));
    out.push_str(&format!("free blocks:    {}\n", vh.free_blocks));
    match vh.volume_bytes() {
        Ok(b) => out.push_str(&format!(
            "volume size:    {b} bytes ({:.2} MiB)\n",
            b as f64 / (1024.0 * 1024.0)
        )),
        Err(_) => out.push_str("volume size:    <overflow>\n"),
    }
    out.push_str(&format!("next catalog id:{}\n", vh.next_catalog_id));
    out.push_str(&format!(
        "file/folder cnt:{} / {}\n",
        vh.file_count, vh.folder_count
    ));

    // Journal presence: the attribute bit is authoritative; the info block is
    // only meaningful when the bit is set. Apple core/hfs_vfsutils.c
    // (hfs_MountHFSPlusVolume) makes the same distinction.
    out.push_str(&format!(
        "attributes:     0x{:08x}{}{}{}{}{}{}\n",
        vh.attributes,
        if vh.is_clean() { " unmounted" } else { " DIRTY" },
        if vh.is_journaled() { " journaled" } else { "" },
        if vh.has_expanded_times() { " expanded-times" } else { "" },
        if vh.attribute_flags().has_content_protection() { " content-protection" } else { "" },
        if vh.attribute_flags().is_software_locked() { " software-locked" } else { "" },
        if vh.attribute_flags().is_inconsistent() { " inconsistent" } else { "" },
    ));
    if vh.is_journaled() {
        out.push_str(&format!(
            "journal:        present (info block {})\n",
            vh.journal_info_block
        ));
    } else {
        out.push_str("journal:        absent\n");
    }
    out.push_str(&format!(
        "clean:          {}\n",
        if vh.is_clean() { "yes" } else { "no (needs fsck)" }
    ));

    // Timestamps, decoded with the volume's own epoch convention.
    for (label, ts) in [
        ("created", vh.create_time()),
        ("modified", vh.modify_time()),
        ("backed up", vh.backup_time()),
        ("checked", vh.checked_time()),
    ] {
        match ts.to_rfc3339() {
            Some(s) => out.push_str(&format!("{label} time:   {s} (raw {})\n", ts.raw)),
            None => out.push_str(&format!("{label} time:   unset (raw 0)\n")),
        }
    }

    out.push_str("\nspecial files:\n");
    for (name, fork) in [
        ("allocationFile (bitmap)", vh.allocation_file),
        ("extentsFile   (overflow B-tree)", vh.extents_file),
        ("catalogFile   (catalog B-tree)", vh.catalog_file),
        ("attributesFile(xattr B-tree)", vh.attributes_file),
        ("startupFile", vh.startup_file),
    ] {
        out.push_str(&format!(
            "  {name}\n\
             \x20   logical size {:>10}  clump {:>8}  total blocks {:>6}  inline extents {}\n",
            fork.logical_size,
            fork.clump_size,
            fork.total_blocks,
            fork.extents.used()
        ));
        if opts.verbose {
            for (i, d) in fork.iter_inline().enumerate() {
                out.push_str(&format!(
                    "     [{i}] start {} count {}\n",
                    d.start_block, d.block_count
                ));
            }
            if fork.inline_blocks() != u64::from(fork.total_blocks) {
                out.push_str(&format!(
                    "     note: inline extents cover {} blocks but fork claims {}; \
                     the remainder must be in the extents overflow B-tree\n",
                    fork.inline_blocks(),
                    fork.total_blocks
                ));
            }
        }
    }

    // Catalog presence is the minimum precondition for a read-only mount.
    if vh.catalog_file.logical_size == 0 {
        out.push_str("\nnote: catalogFile is empty; the volume has no catalog B-tree\n");
    }

    if opts.verbose {
        out.push_str(&format!(
            "\nheader location: byte {} ({} sectors of 512)\n",
            VOLUME_HEADER_OFFSET,
            VOLUME_HEADER_OFFSET / 512
        ));
        match dev.len() {
            Ok(len) if len < VOLUME_HEADER_SIZE as u64 + VOLUME_HEADER_OFFSET => {
                out.push_str("warning: device is smaller than one volume header\n");
            }
            _ => {}
        }
        match vh.alternate_header_offset() {
            Ok(off) => out.push_str(&format!(
                "alternate header: byte {off} (last allocation block)\n"
            )),
            Err(e) => out.push_str(&format!("alternate header: unavailable ({e})\n")),
        }
        if let Err(e) = check_extent_ranges(&vh) {
            out.push_str(&format!("warning: {e}\n"));
        }
    }

    out
}

/// Report extents that fall outside the volume's allocation range.
///
/// This is a cheap consistency check in the spirit of `fsck.hfsplus`'s extent
/// check, performed without any B-tree parsing.
fn check_extent_ranges(vh: &VolumeHeader) -> Result<()> {
    let total = u64::from(vh.total_blocks);
    let mut bad = 0usize;
    for fork in special_forks(vh) {
        for d in fork.iter_inline() {
            let start = u64::from(d.start_block);
            let end = start + u64::from(d.block_count);
            if end > total {
                bad += 1;
            }
        }
    }
    if bad > 0 {
        return Err(Error::OutOfRange {
            what: "inline extent end block",
            value: u64::from(vh.total_blocks),
            limit: u64::from(vh.total_blocks),
        });
    }
    Ok(())
}

fn special_forks(vh: &VolumeHeader) -> [ForkData; 5] {
    [
        vh.allocation_file,
        vh.extents_file,
        vh.catalog_file,
        vh.attributes_file,
        vh.startup_file,
    ]
}

fn render_json(vh: VolumeHeader, path: &str) -> String {
    let kind = vh.kind().unwrap_or(FileSystemKind::HfsPlus);
    let fork_json = |name: &str, f: &ForkData| {
        let extents: Vec<String> = f
            .iter_inline()
            .map(|d: &ExtentDescriptor| {
                format!("{{\"start_block\":{},\"block_count\":{}}}", d.start_block, d.block_count)
            })
            .collect();
        format!(
            "{{\"name\":{},\"logical_size\":{},\"clump_size\":{},\"total_blocks\":{},\
             \"inline_block_count\":{},\"extents\":[{}]}}",
            json_string(name),
            f.logical_size,
            f.clump_size,
            f.total_blocks,
            f.inline_blocks(),
            extents.join(",")
        )
    };
    let forks = [
        fork_json("allocationFile", &vh.allocation_file),
        fork_json("extentsFile", &vh.extents_file),
        fork_json("catalogFile", &vh.catalog_file),
        fork_json("attributesFile", &vh.attributes_file),
        fork_json("startupFile", &vh.startup_file),
    ]
    .join(",");

    let ts = |t: hfsplus::timestamp::HfsTimestamp| match t.to_rfc3339() {
        Some(s) => format!("{{\"raw\":{},\"unix\":{},\"rfc3339\":{}}}", t.raw, t.to_unix(), json_string(&s)),
        None => format!("{{\"raw\":{},\"unix\":null,\"rfc3339\":null}}", t.raw),
    };

    format!(
        "{{\"path\":{},\"ok\":true,\"signature\":{},\"signature_hex\":\"0x{:04x}\",\
         \"filesystem\":{},\"version\":{},\"root_folder_id\":{},\
         \"block_size\":{},\"total_blocks\":{},\"free_blocks\":{},\"volume_bytes\":{},\
         \"next_catalog_id\":{},\"file_count\":{},\"folder_count\":{},\
         \"attributes\":{},\"journaled\":{},\"clean\":{},\"journal_info_block\":{},\
         \"expanded_times\":{},\
         \"create_time\":{},\"modify_time\":{},\"backup_time\":{},\"checked_time\":{},\
         \"forks\":[{}]}}\n",
        json_string(path),
        vh.signature,
        vh.signature,
        json_string(kind_name(kind)),
        vh.version,
        vh.root_folder_id(),
        vh.block_size,
        vh.total_blocks,
        vh.free_blocks,
        vh.volume_bytes().unwrap_or(0),
        vh.next_catalog_id,
        vh.file_count,
        vh.folder_count,
        vh.attributes,
        vh.is_journaled(),
        vh.is_clean(),
        vh.journal_info_block,
        vh.has_expanded_times(),
        ts(vh.create_time()),
        ts(vh.modify_time()),
        ts(vh.backup_time()),
        ts(vh.checked_time()),
        forks
    )
}

fn kind_name(kind: FileSystemKind) -> &'static str {
    match kind {
        FileSystemKind::HfsPlus => "HFS+",
        FileSystemKind::HfsX => "HFSX",
        FileSystemKind::ClassicHfs => "HFS",
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Unused in the current milestone but kept to make the view type reachable
/// from this binary's tests later; `#[allow(dead_code)]` avoids a warning.
#[allow(dead_code)]
fn _view_is_available(dev: FileDevice) -> Result<ViewDevice<FileDevice>> {
    ViewDevice::from_offset(dev, VOLUME_HEADER_OFFSET)
}
