//! `hnewfs` -- construct a new HFS+ file system.
//!
//! Implements the Darwin `newfs_hfs` command-line interface for creating
//! HFS+ and HFSX volumes on a regular file or block device.
//!
//! Mining reference: Apple `newfs_hfs/makehfs.c` (`MakeHFS`, `initVolume`) and
//! `newfs_hfs/newfs_hfs.tproj/newfs_hfs.c` for the command-line parsing.

use std::path::PathBuf;
use std::process::ExitCode;

use hfsplus::blockdev::FileDevice;
use hfsplus::format::volume_header::{K_HFSX_SIG_WORD, K_HFS_PLUS_SIG_WORD};
use hfsplus::format::writer::format_volume;

/// Exit codes match POSIX fsck conventions.
const EXIT_CLEAN: u8 = 0;
const EXIT_OPERATIONAL: u8 = 8;

/// Parse a size suffix: bare number = bytes, `K`=KiB, `M`=MiB, `G`=GiB, `T`=TiB.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.trim_end_matches(|c: char| c.is_ascii_digit()).len());
    let n: u64 = num.parse().map_err(|_| format!("invalid size: {s}"))?;
    let multiplier: u64 = match unit.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" => 1024,
        "m" => 1024 * 1024,
        "g" => 1024 * 1024 * 1024,
        "t" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("unknown size unit: {unit}")),
    };
    n.checked_mul(multiplier)
        .ok_or_else(|| "size overflow".to_string())
}

#[derive(Default)]
struct Options {
    volume_name: String,
    block_size: Option<u32>,
    node_size: Option<u16>,
    case_sensitive: bool,
    journaled: bool,
    journal_size: Option<u64>,
    no_create: bool,
    uid: Option<u32>,
    gid: Option<u32>,
    umask: Option<u16>,
    total_bytes: Option<u64>,
}

fn usage() {
    eprintln!(
        "usage: hnewfs [options] special-device\n\
         \n\
         Construct an HFS+ file system.\n\
         \n\
         Options:\n\
         \x20 -N              Print the volume parameters without creating.\n\
         \x20 -v name         Volume name (max 255 UTF-16 units).\n\
         \x20 -b size         Allocation block size (512..32768).\n\
         \x20 -c count        Journal size in megabytes (default 8M).\n\
         \x20 -J [size]       Create a journaled volume.\n\
         \x20 -s              Case-sensitive (HFSX).\n\
         \x20 -w              Create HFS X (same as -s).\n\
         \x20 -U uid          Root directory owner UID.\n\
         \x20 -G gid          Root directory group GID.\n\
         \x20 -M mask         Root directory permission mask (octal).\n\
         \x20 -0              Use 0 as root directory permissions.\n\
         \x20 -h              Help."
    );
}

fn parse_args(args: &[String]) -> Result<(Options, PathBuf), String> {
    let mut opts = Options::default();
    let mut paths: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-N" => opts.no_create = true,
            "-v" => {
                i += 1;
                opts.volume_name = args.get(i).ok_or("option -v requires an argument")?.clone();
            }
            "-s" => opts.case_sensitive = true,
            "-w" => opts.case_sensitive = true,
            "-J" => {
                opts.journaled = true;
            }
            "-b" => {
                i += 1;
                let val = args.get(i).ok_or("option -b requires an argument")?;
                let n: u32 = val
                    .parse()
                    .map_err(|_| format!("invalid block size: {val}"))?;
                if !n.is_power_of_two() || !(512..=32768).contains(&n) {
                    return Err(
                        "block size must be a power of two between 512 and 32768".to_string()
                    );
                }
                opts.block_size = Some(n);
            }
            "-c" => {
                i += 1;
                let val = args.get(i).ok_or("option -c requires an argument")?;
                let n: u64 = parse_size(&format!("{val}M"))
                    .map_err(|e| format!("invalid clump size: {e}"))?;
                opts.journal_size = Some(n);
            }
            "-U" => {
                i += 1;
                let val = args.get(i).ok_or("option -U requires an argument")?;
                opts.uid = Some(val.parse().map_err(|_| format!("invalid uid: {val}"))?);
            }
            "-G" => {
                i += 1;
                let val = args.get(i).ok_or("option -G requires an argument")?;
                opts.gid = Some(val.parse().map_err(|_| format!("invalid gid: {val}"))?);
            }
            "-M" => {
                i += 1;
                let val = args.get(i).ok_or("option -M requires an argument")?;
                let n: u16 =
                    u16::from_str_radix(val, 8).map_err(|_| format!("invalid umask: {val}"))?;
                opts.umask = Some(n);
            }
            "-0" => {
                opts.umask = Some(0o077);
            }
            "-h" => {
                usage();
                return Err(String::new());
            }
            other => {
                if other.starts_with('-') {
                    return Err(format!("unknown option: {other}"));
                }
                paths.push(other.to_string());
            }
        }
        i += 1;
    }

    if paths.is_empty() {
        return Err("no special device given".to_string());
    }
    if paths.len() > 1 {
        return Err("only one device can be formatted".to_string());
    }

    Ok((opts, PathBuf::from(paths.remove(0))))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let (opts, path) = match parse_args(&args) {
        Ok(result) => result,
        Err(e) if e.is_empty() => return ExitCode::from(EXIT_CLEAN),
        Err(e) => {
            eprintln!("newfs_hfs: {e}");
            usage();
            return ExitCode::from(EXIT_OPERATIONAL);
        }
    };

    if opts.volume_name.is_empty() {
        eprintln!("newfs_hfs: no volume name specified (use -v)");
        return ExitCode::from(EXIT_OPERATIONAL);
    }

    // Determine total bytes and block size.
    let block_size = opts.block_size.unwrap_or(4096);
    let node_size = opts.node_size.unwrap_or(4096);

    if opts.no_create {
        let total_bytes = opts
            .total_bytes
            .unwrap_or_else(|| std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
        if total_bytes == 0 {
            eprintln!("newfs_hfs: cannot determine device size (use -N with a size argument)");
            return ExitCode::from(EXIT_OPERATIONAL);
        }
        let total_blocks = total_bytes / u64::from(block_size);
        let vol_mb = total_bytes / (1024 * 1024);
        eprintln!("newfs_hfs: {vol_mb} MB volume ({total_blocks} blocks)");
        eprintln!("  block size: {block_size}");
        eprintln!("  node size: {node_size}");
        eprintln!("  volume name: {}", opts.volume_name);
        eprintln!("  case-sensitive: {}", opts.case_sensitive);
        eprintln!("  journaled: {}", opts.journaled);
        eprintln!(
            "  signature: 0x{:04x}",
            if opts.case_sensitive {
                K_HFSX_SIG_WORD
            } else {
                K_HFS_PLUS_SIG_WORD
            }
        );
        return ExitCode::from(EXIT_CLEAN);
    }

    let total_bytes = opts
        .total_bytes
        .unwrap_or_else(|| std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0));

    if total_bytes == 0 {
        eprintln!("newfs_hfs: cannot determine device size");
        return ExitCode::from(EXIT_OPERATIONAL);
    }

    // Open the device for writing.
    let mut dev = match FileDevice::create(&path, total_bytes) {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("newfs_hfs: cannot open {}: {e}", path.display());
            return ExitCode::from(EXIT_OPERATIONAL);
        }
    };

    let journal_size = if opts.journaled {
        opts.journal_size
    } else {
        None
    };

    match format_volume(
        &mut dev,
        &opts.volume_name,
        total_bytes,
        block_size,
        node_size,
        opts.case_sensitive,
        opts.journaled,
        journal_size,
        opts.uid.unwrap_or(0),
        opts.gid.unwrap_or(0),
        opts.umask.unwrap_or(0o177),
    ) {
        Ok(()) => ExitCode::from(EXIT_CLEAN),
        Err(e) => {
            eprintln!("newfs_hfs: {}: {e}", path.display());
            ExitCode::from(EXIT_OPERATIONAL)
        }
    }
}
