//! `hfsls` — list an HFS+ or HFSX image without mounting it.
//!
//! The point of this tool is that it exercises the same library a FUSE mount
//! would, without FUSE. That makes it both a debugging aid and the differential
//! test harness's reference reader.
//!
//! ```text
//! hfsls [options] <image> [path]
//!   -l, --long        show mode, size, dates, CNID and Finder codes
//!   -a, --all          include entries whose names begin with a dot
//!   -R, --recursive    descend into directories
//!   -s, --stat         print volume statistics and exit
//!   -b, --bits         print the allocation bitmap summary
//!   --json             machine-readable output
//! ```
//!
//! A path is resolved one component at a time through `Volume::lookup`, using
//! whatever name comparison the volume itself declares, so a path that this tool
//! resolves is a path the filesystem actually has.

use std::process::ExitCode;

use hfsplus::blockdev::FileDevice;
use hfsplus::error::{Error, Result};
use hfsplus::volume::{Object, Volume};

fn main() -> ExitCode {
    let mut long = false;
    let mut all = false;
    let mut recursive = false;
    let mut stat_only = false;
    let mut bitmap = false;
    let mut journal = false;
    let mut json = false;
    let mut args: Vec<String> = Vec::new();

    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-l" | "--long" => long = true,
            "-a" | "--all" => all = true,
            "-R" | "--recursive" => recursive = true,
            "-s" | "--stat" => stat_only = true,
            "-b" | "--bits" => bitmap = true,
            "-j" | "--journal" => journal = true,
            "--json" => json = true,
            "-h" | "--help" => {
                usage();
                return ExitCode::SUCCESS;
            }
            s if s.starts_with('-') && s.len() > 1 => {
                eprintln!("hfsls: unknown option `{s}`");
                usage();
                return ExitCode::from(1);
            }
            s => args.push(s.to_string()),
        }
    }

    if args.is_empty() {
        usage();
        return ExitCode::from(1);
    }

    let image = &args[0];
    let dev = match FileDevice::open(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hfsls: {image}: {e}");
            return ExitCode::from(2);
        }
    };
    let vol = match Volume::open(&dev) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("hfsls: {image}: {e}");
            return ExitCode::from(2);
        }
    };

    if json {
        match run_json(&vol) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                eprintln!("hfsls: {image}: {e}");
                return ExitCode::from(2);
            }
        }
        return ExitCode::SUCCESS;
    }

    if stat_only {
        match render_stat(&vol) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                eprintln!("hfsls: {image}: {e}");
                return ExitCode::from(2);
            }
        }
        return ExitCode::SUCCESS;
    }

    if journal {
        match render_journal(&vol) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                eprintln!("hfsls: {image}: {e}");
                return ExitCode::from(2);
            }
        }
        return ExitCode::SUCCESS;
    }

    if bitmap {
        match render_bitmap(&vol) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                eprintln!("hfsls: {image}: {e}");
                return ExitCode::from(2);
            }
        }
        return ExitCode::SUCCESS;
    }

    let path = args.get(1).map(|s| s.as_str()).unwrap_or("/");
    match walk(&vol, path, all, recursive, long, "") {
        Ok(text) => print!("{text}"),
        Err(e) => {
            eprintln!("hfsls: {image}: {path}: {e}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

fn usage() {
    eprintln!(
        "usage: hfsls [-l] [-a] [-R] [-s] [-b] [--json] <image> [path]\n\
         \n\
         Lists an HFS+ or HFSX image without mounting it. Paths are resolved\n\
         through the volume's own name comparison."
    );
}

/// Resolve `path` component by component, then list it.
fn walk(
    vol: &Volume<'_, FileDevice>,
    path: &str,
    all: bool,
    recursive: bool,
    long: bool,
    indent: &str,
) -> Result<String> {
    let object = resolve(vol, path)?;
    let Object::Directory(dir) = object else {
        // A file path lists the file itself.
        let mut s = format!("{indent}{}\n", object.name_string());
        if long {
            s.push_str(&long_line(&object));
        }
        return Ok(s);
    };

    let mut out = String::new();
    if path != "/" && path != "." {
        out.push_str(&format!("{indent}{path}:\n"));
    }
    let inner = format!("{indent}  ");

    let mut entries = vol.read_dir(dir.cnid)?;
    entries.sort_by(|a, b| a.name().cmp(b.name()));
    for entry in entries {
        let name = entry.name_string();
        if !all && name.starts_with('.') {
            continue;
        }
        out.push_str(&format!("{inner}{name}{}\n", if entry.is_dir() { "/" } else { "" }));
        if long {
            out.push_str(&format!("{inner}  {}", long_line(&entry)));
        }
        if recursive && entry.is_dir() {
            let sub = format!("{path}/{name}");
            out.push_str(&walk(vol, &sub, all, recursive, long, &inner)?);
        }
    }
    Ok(out)
}

/// Resolve a path one component at a time.
///
/// Each step goes through `Volume::lookup`, so the volume's own comparator
/// decides whether a name matches. That is what makes this tool agree with a
/// mount instead of imposing its own idea of equality.
fn resolve(vol: &Volume<'_, FileDevice>, path: &str) -> Result<Object> {
    let mut current = vol.lookup_cnid(vol.root_cnid())?;
    for part in path.split('/').filter(|s| !s.is_empty() && *s != ".") {
        let Some(object) = current else {
            return Err(Error::NotFoundKey { key: part.to_string() });
        };
        if !object.is_dir() {
            return Err(Error::NotFoundKey { key: part.to_string() });
        }
        let units: Vec<u16> = part.encode_utf16().collect();
        current = vol.lookup(object.cnid(), &units)?;
        if current.is_none() {
            return Err(Error::NotFoundKey { key: part.to_string() });
        }
    }
    current.ok_or(Error::NotFound { what: "root folder" })
}

fn long_line(object: &Object) -> String {
    let mode = object.bsd_info().permissions() & 0o7777;
    let kind = if object.is_dir() { 'd' } else { '-' };
    let t = object.times();
    let modified = t
        .modified
        .to_rfc3339()
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{kind}0{mode:o} {:>3} {:>3} {modified} {:>10} CNID {} {}\n",
        object.bsd_info().owner_id,
        object.bsd_info().group_id,
        object.data_size(),
        object.cnid(),
        object.name_string(),
    )
}

fn render_stat(vol: &Volume<'_, FileDevice>) -> Result<String> {
    let st = vol.statfs()?;
    let name = vol.name().unwrap_or_else(|_| "<unknown>".to_string());
    let mut s = String::new();
    s.push_str(&format!("volume name:     {name}\n"));
    s.push_str(&format!("filesystem:      {:?}\n", vol.kind()));
    s.push_str(&format!("block size:      {}\n", st.block_size));
    s.push_str(&format!("total blocks:    {}\n", st.total_blocks));
    s.push_str(&format!("free blocks:     {}\n", st.free_blocks));
    s.push_str(&format!(
        "total size:      {} bytes ({:.2} MiB)\n",
        st.total_bytes,
        st.total_bytes as f64 / (1024.0 * 1024.0)
    ));
    s.push_str(&format!(
        "free size:       {} bytes ({:.2} MiB)\n",
        st.free_bytes,
        st.free_bytes as f64 / (1024.0 * 1024.0)
    ));
    s.push_str(&format!("files/folders:   {} / {}\n", st.file_count, st.folder_count));
    s.push_str(&format!("journaled:       {}\n", st.journaled));
    s.push_str(&format!("case sensitive:  {}\n", vol.is_case_sensitive()));
    s.push_str(&format!("clean:           {}\n", vol.is_clean()));
    Ok(s)
}

fn render_journal(vol: &Volume<'_, FileDevice>) -> Result<String> {
    let mut s = String::new();
    s.push_str(&format!("journaled:       {}\n", vol.is_journaled()));
    s.push_str(&format!(
        "journalInfoBlock:{}\n",
        vol.header().journal_info_block
    ));

    let Some(j) = vol.journal()? else {
        s.push_str("journal:         none\n");
        return Ok(s);
    };

    let info = j.info();
    s.push_str(&format!("journal offset:  {}\n", info.offset));
    s.push_str(&format!("journal size:    {}\n", info.size));
    let flags = info.flag_set();
    s.push_str(&format!(
        "journal flags:   0x{:08x}{}{}{}\n",
        info.flags,
        if flags.in_filesystem() { " in-filesystem" } else { "" },
        if flags.on_other_device() { " other-device" } else { "" },
        if flags.needs_init() { " needs-init" } else { "" },
    ));
    s.push_str(&format!("uninitialised:   {}\n", j.is_uninitialized()));
    match j.header() {
        None => s.push_str("journal header:  none (never written)\n"),
        Some(h) => {
            s.push_str(&format!(
                "header start/end:{}/{}  sequence {}  blhdr {}  jhdr {}\n",
                h.start, h.end, h.sequence_num, h.blhdr_size, h.jhdr_size
            ));
            // Apple reports a stale header checksum but still mounts, so this is
            // printed as a warning rather than treated as a failure.
            match j.header_checksum_ok() {
                None => s.push_str("header checksum: not checked (legacy magic)\n"),
                Some(true) => s.push_str("header checksum: ok\n"),
                Some(false) => s.push_str(
                    "header checksum: MISMATCH -- a stale checksum is not fatal; \
                     Apple mounts anyway\n",
                ),
            }
        }
    }
    if let Some((at, why)) = j.truncation() {
        s.push_str(&format!(
            "replay truncated at journal offset {at}: {why}\n"
        ));
    }
    s.push_str(&format!("transactions:    {}\n", j.transactions().len()));
    s.push_str(&format!("replayed blocks: {}\n", j.replayed_blocks().len()));
    if j.replayed_blocks().is_empty() {
        s.push_str("note:            an uninitialised journal replays nothing, so the\n");
        s.push_str("                 filesystem is used as-is\n");
    }
    Ok(s)
}

fn render_bitmap(vol: &Volume<'_, FileDevice>) -> Result<String> {
    let bm = vol.allocation_bitmap()?;
    let allocated = bm.count_all_allocated()?;
    let total = u64::from(bm.total_blocks());
    let mut s = String::new();
    s.push_str(&format!("bitmap bytes:    {}\n", bm.bitmap_bytes()));
    s.push_str(&format!("blocks tracked:  {total}\n"));
    s.push_str(&format!("allocated:       {allocated}\n"));
    s.push_str(&format!("free:            {}\n", total - allocated));
    s.push_str(&format!(
        "header freeBlocks:{}\n",
        u64::from(vol.header().free_blocks)
    ));
    // The first few blocks are where a formatter's structure lives, so showing
    // them makes an unexpected layout obvious at a glance.
    let show = bm.total_blocks().min(64);
    s.push_str("first blocks:     ");
    for b in 0..show {
        s.push(if bm.is_allocated(b)? { '#' } else { '.' });
    }
    s.push('\n');
    Ok(s)
}

fn run_json(vol: &Volume<'_, FileDevice>) -> Result<String> {
    let st = vol.statfs()?;
    let objects = vol.catalog().all_objects()?;
    let mut out = String::from("{\n  \"volume\": ");
    out.push_str(&format!(
        "{{\"name\":{},\"filesystem\":\"{:?}\",\"block_size\":{},\"total_blocks\":{},\
         \"free_blocks\":{},\"total_bytes\":{},\"free_bytes\":{},\"journaled\":{},\
         \"case_sensitive\":{},\"clean\":{}}}",
        quote(&vol.name()?),
        vol.kind(),
        st.block_size,
        st.total_blocks,
        st.free_blocks,
        st.total_bytes,
        st.free_bytes,
        st.journaled,
        vol.is_case_sensitive(),
        vol.is_clean(),
    ));
    out.push_str(",\n  \"objects\": [\n");
    for (i, o) in objects.iter().enumerate() {
        // Only what a thread record carries: CNID, name and whether it is a
        // directory. Sizes live in the main records, which a whole-volume scan
        // does not load, and inventing zeros for them would be worse than
        // omitting the field.
        let mut s = format!(
            "    {{\"cnid\":{},\"name\":{},\"is_dir\":{}}}",
            o.cnid.0,
            quote(&String::from_utf16_lossy(&o.name)),
            o.is_dir
        );
        if i + 1 < objects.len() {
            s.push(',');
        }
        out.push_str(&s);
        out.push('\n');
    }
    out.push_str("  ]\n}\n");
    Ok(out)
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}