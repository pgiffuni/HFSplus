//! `hfsls` sibling that checks an image for structural consistency.
//!
//! Read-only, and it never writes: the checks compare structures that already
//! exist on the disk. Nothing here repairs anything, which is deliberate —
//! `fsck_hfs` repairs as well as reports, and pointing a repair tool at a
//! fixture undoes the very corruption it was meant to diagnose. There is no
//! `--fix` to mis-invoke.
//!
//! Mining reference: `lib_fsck_hfs/dfalib/VolumeBitmapCheck.c` for the bitmap
//! comparison and `SVerify1.c` for the volume-information checks. What each check
//! verifies is named on the check itself in `src/check/mod.rs`.

use std::process::ExitCode;

use hfsplus::blockdev::FileDevice;
use hfsplus::check;
use hfsplus::volume::Volume;

/// Exit codes, matching `hfsinspect` so the two tools compose in a script.
///
/// 0 when every image checked cleanly, 1 on a usage error, 2 when an image could
/// not be parsed, and **3** when an image parsed but disagreed with itself. That
/// last one is distinct from 2 on purpose: "cannot read this" and "read this and
/// found it broken" call for different responses.
const EXIT_CLEAN: u8 = 0;
const EXIT_USAGE: u8 = 1;
const EXIT_UNREADABLE: u8 = 2;
const EXIT_INCONSISTENT: u8 = 3;

struct Options {
    json: bool,
    quiet: bool,
}

fn usage() {
    eprintln!(
        "usage: hfsck [--json] [--quiet] <image>...\n\
         \n\
         Checks HFS+/HFSX images for structural consistency: the allocation\n\
         bitmap against the catalog's extents, each fork's declared block count\n\
         against what its extents describe, and nextCatalogID against the CNIDs\n\
         in use.\n\
         \n\
         Reads only. Never writes, and never repairs.\n\
         \n\
         Exit status:\n\
        \x20 0  every image was consistent\n\
        \x20 1  usage error\n\
        \x20 2  an image could not be parsed\n\
        \x20 3  an image was read and found inconsistent"
    );
}

fn main() -> ExitCode {
    let mut opts = Options {
        json: false,
        quiet: false,
    };
    let mut paths: Vec<String> = Vec::new();
    let mut explicit_help = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--quiet" | "-q" => opts.quiet = true,
            "--help" | "-h" => {
                explicit_help = true;
            }
            s if s.starts_with('-') && s.len() > 1 => {
                eprintln!("hfsck: unknown option `{s}`");
                usage();
                return ExitCode::from(EXIT_USAGE);
            }
            s => paths.push(s.to_string()),
        }
    }

    if explicit_help {
        usage();
        return ExitCode::from(EXIT_CLEAN);
    }
    if paths.is_empty() {
        eprintln!("hfsck: no image given");
        usage();
        return ExitCode::from(EXIT_USAGE);
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
    let dev = match FileDevice::open(path) {
        Ok(dev) => dev,
        Err(e) => return report_unreadable(path, &e.to_string(), opts),
    };
    let vol = match Volume::open(&dev) {
        Ok(vol) => vol,
        // A signature outside this project's scope is a clean refusal, not a
        // failure: `hfsinspect` treats it the same way.
        Err(e @ hfsplus::error::Error::BadSignature { .. }) => {
            return report_unreadable(path, &e.to_string(), opts)
        }
        Err(e) => return report_unreadable(path, &e.to_string(), opts),
    };

    let report = match check::check(&vol, None) {
        Ok(report) => report,
        Err(e) => return report_unreadable(path, &e.to_string(), opts),
    };

    let findings = report.describe();
    if opts.json {
        let header = vol.header();
        println!(
            "{{\"image\":{},\"ok\":{},\"block_size\":{},\"total_blocks\":{},\
             \"free_blocks\":{},\"orphaned\":[{}],\"missing\":[{}],\
             \"fork_block_count\":[{}],\"next_catalog_id_reuse\":{},\
             \"findings\":[{}]}}",
            json_string(path),
            findings.is_empty(),
            header.block_size,
            header.total_blocks,
            header.free_blocks,
            join_numbers(&report.orphaned),
            join_numbers(&report.missing),
            report
                .fork_block_count
                .iter()
                .map(|(c, d, n)| format!("{{\"cnid\":{c},\"declared\":{d},\"described\":{n}}}"))
                .collect::<Vec<_>>()
                .join(","),
            match report.next_cnid_reuse {
                Some((next, highest)) => format!("{{\"next\":{next},\"in_use\":{highest}}}"),
                None => "null".to_string(),
            },
            findings
                .iter()
                .map(|f| json_string(f))
                .collect::<Vec<_>>()
                .join(","),
        );
    } else if findings.is_empty() {
        if !opts.quiet {
            println!("{path}: consistent");
        }
    } else {
        println!("{path}: {} finding(s)", findings.len());
        for finding in &findings {
            println!("  {finding}");
        }
    }

    if findings.is_empty() {
        EXIT_CLEAN
    } else {
        EXIT_INCONSISTENT
    }
}

fn report_unreadable(path: &str, message: &str, opts: &Options) -> u8 {
    if opts.json {
        println!(
            "{{\"image\":{},\"ok\":false,\"error\":{}}}",
            json_string(path),
            json_string(message)
        );
    } else {
        eprintln!("{path}: {message}");
    }
    EXIT_UNREADABLE
}

fn join_numbers(values: &[u32]) -> String {
    values
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Quote a string as JSON, escaping what has to be escaped.
///
/// Written out rather than pulled in, because a diagnostic tool that formats its
/// own output by hand is easier to trust than one that carries a serialisation
/// dependency, and this crate has none.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
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
