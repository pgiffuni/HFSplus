// SPDX-License-Identifier: BSD-2-Clause

//! Entry point for the HFS+ FUSE adapter.
//!
//! Usage:
//!
//! ```text
//! hfsplus-fuse <image> <mountpoint> [options]
//! ```
//!
//! In the initial implementation, the mount is read-only.

use std::path::PathBuf;
use std::process::ExitCode;

use fuser::Config;
use fuser::MountOption;
use hfsplus_fuse::HfsPlusFilesystem;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <image> <mountpoint>", args[0]);
        return ExitCode::from(1);
    }

    let image = &args[1];
    let mountpoint = &args[2];

    let fs = match HfsPlusFilesystem::open(image) {
        Ok(fs) => fs,
        Err(e) => {
            eprintln!("hfsplus-fuse: {}: {}", image, e);
            return ExitCode::from(2);
        }
    };

    let mountpoint_path = PathBuf::from(mountpoint);
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::AutoUnmount,
        MountOption::FSName("hfsplus".to_string()),
    ];

    match fuser::mount(fs, &mountpoint_path, &config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hfsplus-fuse: mount failed: {}", e);
            ExitCode::from(1)
        }
    }
}
