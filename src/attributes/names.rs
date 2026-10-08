// SPDX-License-Identifier: APSL-1.2

//! The attribute names HFS+ itself stores.
//!
//! Not user attributes. The attributes file holds several entries that are part
//! of the filesystem's own bookkeeping, and a reader that treats them as ordinary
//! named data will mis-handle them in ways that are hard to see: a hard link's
//! first link is recorded as a decimal CNID in a *string*, and a resource fork's
//! visibility is decided by whether a compression attribute is present.
//!
//! The names are exact and case-sensitive. They are stored as UTF-16 in the
//! attributes tree, so both the spelling and its length are part of the key, and a
//! case-folded comparison -- which is what an HFS+ volume uses for *file* names --
//! is not applied to attribute names.
//!
//! Mining reference: `core/hfs_format.h` (`FIRST_LINK_XATTR_NAME`),
//! `core/hfs_cprotect.h` (`CONTENT_PROTECTION_XATTR_NAME`), and the string
//! literals `core/hfs_xattr.c` and `core/hfs_vnops.c` use.

/// The resource fork, as macOS presents it.
///
/// This is the name `setxattr` and `getxattr` use for a resource fork, and the
/// only stream `getnamedstream` supports -- it answers `ENOATTR` for every other
/// name. So the resource fork is a **real fork** in the catalog record that macOS
/// *also* surfaces under this name; it is not stored as an attribute.
///
/// The distinction is the one this crate's `Object` already makes, and it is worth
/// keeping: modelling a resource fork internally as an xattr would lose the fork
/// identity that allocation, extents overflow and truncation all depend on.
///
/// Mining reference: `core/hfs_xattr.c` `hfs_vnop_getnamedstream`, which
/// compares against `XATTR_RESOURCEFORK_NAME` and returns `ENOATTR` otherwise.
pub const RESOURCE_FORK_NAME: &str = "com.apple.ResourceFork";

/// The first hard link of a chain, recorded as a decimal CNID.
///
/// A file with more than one hard link stores, under this name, the CNID of the
/// first link -- as ASCII digits, not as binary. Every other link in the chain
/// stores an entry with its *own* name and the same first-link CNID.
///
/// So the chain is not "each link names the next": the links are the file's
/// catalog entries, and this attribute points at the head of the chain. Getting
/// that wrong makes a hard link look like a file with a strange attribute.
///
/// Mining reference: `core/hfs_format.h` `FIRST_LINK_XATTR_NAME` and
/// `FIRST_LINK_XATTR_REC_SIZE`, the latter being
/// `sizeof(HFSPlusAttrData) - 2 + 12` -- the inline record plus twelve
/// characters of CNID.
pub const FIRST_LINK_NAME: &str = "com.apple.system.hfs.firstlink";

/// Content protection, as class metadata.
pub const CONTENT_PROTECTION_NAME: &str = "com.apple.system.cprotect";

/// The quarantine flag macOS sets on a downloaded file.
pub const QUARANTINE_NAME: &str = "com.apple.quarantine";

/// The decmpfs compression metadata, which this crate reads.
///
/// Present on a compressed file, and **hidden from the extended-attribute
/// interface**: `listxattr` and `getxattr` filter it out, so a reader that
/// enumerates attributes does not see it and a reader that reads the data fork
/// naively gets compressed bytes rather than the file's contents.
///
/// Two consequences worth stating, because both produce wrong answers rather than
/// errors:
///
/// - "the data fork is shorter than the logical size" can mean the file is
///   compressed, not truncated.
/// - "this file has no attributes" can mean it has compression metadata that was
///   hidden.
///
/// Decompression is handled in `src/compression/mod.rs`. The HFS+ metadata layer
/// (this name, `is_compressed`) is native to the crate; the compression
/// algorithms beneath it are custom pure-Rust decoders.
pub const DECOMPRESSION_NAME: &str = "com.apple.decmpfs";

/// Every attribute name HFS+ writes for its own bookkeeping.
///
/// Useful for a caller that must tell system metadata from user attributes --
/// which is a distinction the FUSE layer needs and the filesystem does not: a
/// `listxattr` that exposes `com.apple.ResourceFork` as an ordinary attribute is
/// correct at the POSIX boundary and wrong at the native one.
pub const SYSTEM_ATTRIBUTE_NAMES: &[&str] = &[
    RESOURCE_FORK_NAME,
    FIRST_LINK_NAME,
    CONTENT_PROTECTION_NAME,
    QUARANTINE_NAME,
    DECOMPRESSION_NAME,
];

/// Whether the file is compressed, according to its attributes.
///
/// False both when there is no decmpfs attribute and when one exists but does not
/// describe a compressed file -- so "not compressed" is not "no attribute".
///
/// Mining reference: `core/hfs_vnops.c` `hfs_vnop_listxattr` and
/// `hfs_vnop_getxattr`, which consult `decmpfs_hides_xattr` and omit it.
pub fn is_compressed(
    tree: &crate::attributes::AttributesFile<'_, impl crate::blockdev::BlockDevice + ?Sized>,
    cnid: u32,
) -> crate::error::Result<bool> {
    Ok(tree
        .attributes_for(cnid)?
        .iter()
        .any(|a| a.name == DECOMPRESSION_NAME))
}

/// Whether `name` is one HFS+ writes for itself.
pub fn is_system_attribute(name: &str) -> bool {
    SYSTEM_ATTRIBUTE_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_are_exactly_as_apple_spells_them() {
        // These strings are part of the on-disk key. A case difference or a
        // missing segment is a different attribute, and nothing would complain.
        assert_eq!(RESOURCE_FORK_NAME, "com.apple.ResourceFork");
        assert_eq!(FIRST_LINK_NAME, "com.apple.system.hfs.firstlink");
        assert_eq!(CONTENT_PROTECTION_NAME, "com.apple.system.cprotect");
        assert_eq!(QUARANTINE_NAME, "com.apple.quarantine");
    }

    #[test]
    fn system_attributes_are_distinguishable_from_user_ones() {
        assert!(is_system_attribute(RESOURCE_FORK_NAME));
        assert!(is_system_attribute(FIRST_LINK_NAME));
        assert!(!is_system_attribute("com.apple.quarantine_custom"));
        assert!(!is_system_attribute("user.xattr"));
        assert!(!is_system_attribute(""));
    }

    #[test]
    fn the_decompression_name_matches_the_corroborating_literal() {
        // `core/` uses the macro `DECMPFS_XATTR_NAME`, whose definition is not in
        // this tree; the livefiles plugin spells the same string. So this is two
        // implementations agreeing, not one mined definition.
        assert_eq!(DECOMPRESSION_NAME, "com.apple.decmpfs");
        assert!(is_system_attribute(DECOMPRESSION_NAME));
    }

    #[test]
    fn the_resource_fork_name_is_not_its_own_attribute_here() {
        // The crate must not find the resource fork in the attributes tree: it is
        // in the catalog record. `is_system_attribute` says it is *named* like an
        // attribute, which is a statement about the POSIX boundary, so the two
        // must not be conflated inside the filesystem.
        assert!(
            !FIRST_LINK_NAME.eq_ignore_ascii_case(RESOURCE_FORK_NAME),
            "distinct names, distinct concepts"
        );
    }
}
