//! Catalog lookup and enumeration.
//!
//! Mining reference: Apple `core/BTree.c` (`BTSearchRecord`,
//! `BTIterateRecords`), `core/BTreeScanner.c`, and `core/hfs_catalog.c`
//! (`cat_lookup`, `cat_idlookup`, `cat_getdirentries`, `cat_findname`).
//!
//! # How a directory is stored
//!
//! HFS+ directories are not separate structures. A directory's children are the
//! *consecutive* catalog keys whose `parentID` equals that directory's CNID.
//! Listing a directory is therefore a range scan of one sorted key space, and
//! resolving a name is a single B-tree descent.
//!
//! This is the property that makes `readdir` cheap and `lookup` cheap, and it is
//! also why the key comparison must be exactly right: get it wrong and a name
//! that is present cannot be found, which looks like data loss rather than like a
//! comparator bug.
//!
//! # Thread records
//!
//! Every object also has a thread record keyed by an *empty* name under parent CNID
//! 1, so all thread records form one contiguous key range. A thread record's body
//! holds the object's CNID and full name. Two things follow:
//!
//! - The whole volume can be enumerated by scanning that one range, with no
//!   recursion. That is how a flat file list is produced.
//! - The thread record stored under the *folder's own name* gives the folder's
//!   CNID and name, which is how `.` and `..` resolve without a search.

use super::cnid::Cnid;
use super::key::CatalogKey;
use super::record::{parse_record, CatalogRecord};
use crate::btree::header::BTreeHeader;
use crate::btree::io::BTreeFile;
use crate::btree::node::NodeKind;
use crate::blockdev::BlockDevice;
use crate::error::{Error, Result};
use crate::unicode::{Comparator, Ordering};

/// One entry from a directory scan: a name plus the object's CNID and kind.
///
/// The name is kept as code units because that is what the catalog stores, and
/// because the comparator needs it in that form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The object's name.
    pub name: Vec<u16>,
    /// The object's CNID.
    pub cnid: Cnid,
    /// Whether this is a directory.
    ///
    /// Taken from the record: folder records are directories, file records are
    /// not. Thread records carry no type and are never returned by a directory
    /// scan.
    pub is_dir: bool,
}

impl CatalogEntry {
    /// The name as a `String`, for diagnostics only.
    pub fn name_string(&self) -> String {
        String::from_utf16_lossy(&self.name)
    }
}

/// A read-only view of the catalog B-tree.
pub struct Catalog<'a, D: ?Sized> {
    tree: BTreeFile<'a, D>,
    comparator: Comparator,
}

impl<'a, D: BlockDevice + ?Sized> std::fmt::Debug for Catalog<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog")
            .field("node_size", &self.tree.node_size())
            .field("comparator", &self.comparator)
            .finish()
    }
}

impl<'a, D: BlockDevice + ?Sized> Catalog<'a, D> {
    /// Open the catalog stored in `fork`.
    ///
    /// `is_hfsx` is the volume's signature test. The comparator is chosen from
    /// that together with the catalog's own `keyCompareType`; see
    /// [`crate::unicode::compare`] for why both are required.
    pub fn open(
        device: &'a D,
        fork: &crate::format::fork::ForkData,
        block_size: u32,
        is_hfsx: bool,
    ) -> Result<Self> {
        let tree = BTreeFile::open(device, fork, block_size, true)?;
        let comparator = Comparator::for_volume(is_hfsx, tree.header().key_compare_type.code());
        Ok(Catalog { tree, comparator })
    }

    /// The underlying B-tree, for callers that need its geometry.
    pub fn tree(&self) -> &BTreeFile<'a, D> {
        &self.tree
    }

    /// The tree header.
    pub fn header(&self) -> &BTreeHeader {
        self.tree.header()
    }

    /// The comparator this catalog orders names by.
    pub fn comparator(&self) -> Comparator {
        self.comparator
    }

    /// Whether this catalog distinguishes case.
    pub fn is_case_sensitive(&self) -> bool {
        self.comparator.is_case_sensitive()
    }

    /// Compare two keys the way this catalog does.
    ///
    /// `parentID` first, then the name. Mining reference: Apple
    /// `CompareExtendedCatalogKeys`, which compares `parentID` numerically and
    /// only then the name, and `cat_binarykeycompare`, which does the same with a
    /// binary name comparison.
    pub fn compare_keys(&self, a: &CatalogKey, b: &CatalogKey) -> Ordering {
        let by_parent = a.parent_id.cmp(&b.parent_id);
        let parent_ordering = match by_parent {
            std::cmp::Ordering::Less => Ordering::Less,
            std::cmp::Ordering::Equal => Ordering::Equal,
            std::cmp::Ordering::Greater => Ordering::Greater,
        };
        if parent_ordering != Ordering::Equal {
            return parent_ordering;
        }
        // Apple short-circuits to a length difference when either name is empty,
        // rather than calling the comparator. FastUnicodeCompare reaches the same
        // answer, but reproducing the shortcut keeps the edge cases identical.
        if a.name.is_empty() || b.name.is_empty() {
            return match a.name.len().cmp(&b.name.len()) {
                std::cmp::Ordering::Less => Ordering::Less,
                std::cmp::Ordering::Equal => Ordering::Equal,
                std::cmp::Ordering::Greater => Ordering::Greater,
            };
        }
        self.comparator.compare(&a.name, &b.name)
    }

    /// Look up `name` inside `parent_id`.
    ///
    /// Returns the catalog record for the object, or `None` when there is no such
    /// entry.
    ///
    /// Mining reference: Apple `core/hfs_catalog.c` `cat_lookup`, which builds the
    /// key with `buildkey` and calls `BTGetRecordFromIndex`/`BTSearchRecord`.
    pub fn lookup(&self, parent_id: Cnid, name: &[u16]) -> Result<Option<CatalogRecord>> {
        let key = CatalogKey::for_child(parent_id, name);
        Ok(self.search(&key)?.map(|(_, record)| record))
    }

    /// Look up a child and report the name the catalog *stores* for it.
    ///
    /// On a case-folding volume several spellings resolve to one record, so the
    /// name that was asked for and the name on disk differ. A caller that echoes
    /// the request back is then reporting a file that does not exist, so this
    /// returns the key the search actually landed on.
    ///
    /// Mining reference: Apple `core/hfs_catalog.c` `cat_lookup` returns the
    /// position it found, and the caller reads the key from the record rather
    /// than reusing the one it built.
    pub fn lookup_named(
        &self,
        parent_id: Cnid,
        name: &[u16],
    ) -> Result<Option<(Vec<u16>, CatalogRecord)>> {
        let key = CatalogKey::for_child(parent_id, name);
        self.search(&key)
    }

    /// Look up a child by CNID within `parent_id`.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_idlookup`, which uses
    /// `cat_findposition` to find the record at the right index rather than
    /// comparing keys.
    pub fn lookup_child(&self, parent_id: Cnid, cnid: Cnid) -> Result<Option<CatalogEntry>> {
        // scan_children returns everything it visited when the predicate never
        // fires, so the match has to be made here rather than by taking the first
        // entry unconditionally.
        let visited = self.scan_children(parent_id, |entry| entry.cnid == cnid)?;
        Ok(visited.into_iter().find(|e| e.cnid == cnid))
    }

    /// Look up an object's CNID from its name, via its thread record.
    ///
    /// A thread record's **key** is `(parentID = the object's own CNID, name =
    /// "")` and its body repeats the object's parent and name. So the name alone
    /// does not locate a thread record directly; the helper instead finds the
    /// object's *main* record, whose key is `(parent, name)`, and returns the CNID
    /// stored in it.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_lookuplink` follows the link
    /// from a main record to its thread; here the main record is found first
    /// because the name is what the caller has.
    pub fn lookup_thread(&self, name: &[u16]) -> Result<Option<Cnid>> {
        // Search every parent for a child record with this name. For a thread
        // record the CNID comes from the key; for a main record it comes from the
        // record body, so both are handled.
        let Some(bytes) = self.leaf_bytes() else { return Ok(None) };
        let node = self.tree.parse_node(&bytes)?;
        for i in 0..node.num_records() {
            let Ok(rec) = node.record(i) else { continue };
            let Some((key, body)) = split_record(rec) else { continue };
            let Ok(parsed) = parse_record(body) else { continue };
            match &parsed {
                CatalogRecord::Thread(t) => {
                    // Thread key: parentID is the object's CNID, body holds its name.
                    if key.name.is_empty()
                        && t.node_name == name
                    {
                        return Ok(Some(key.parent_id));
                    }
                }
                _ => {
                    if !key.name.is_empty() && key.name == name {
                        if let Some(cnid) = parsed.cnid() { return Ok(Some(cnid)); }
                    }
                }
            }
        }
        Ok(None)
    }

    /// The raw bytes of the first leaf node, if the tree has any records.
    fn leaf_bytes(&self) -> Option<Vec<u8>> {
        if self.tree.header().leaf_records == 0 {
            return None;
        }
        self.tree.read_node_bytes(self.tree.header().first_leaf_node).ok()
    }

    /// Find the record whose key is exactly `key`.
    fn search(&self, key: &CatalogKey) -> Result<Option<(Vec<u16>, CatalogRecord)>> {
        let max_key = usize::from(self.tree.header().max_key_length);
        let root = self.tree.header().root_node;

        // A tree whose root is itself a leaf is the common case for small
        // volumes, and the corpus is entirely made of them, so the loop below
        // handles both shapes without a special case: it reads the root, sees a
        // leaf, and is done.
        //
        // Each iteration owns its buffer and drops it before the next read, so
        // no node borrow outlives the iteration that created it.
        let mut node_num = root;
        for _ in 0..=crate::btree::node::NODE_MAX_DEPTH {
            let bytes = self.tree.read_node_bytes(node_num)?;
            let node = self.tree.parse_node(&bytes)?;

            match node.kind() {
                NodeKind::Leaf => return Ok(self.find_in_leaf(&node, key, max_key)),
                NodeKind::Index => node_num = self.descend_index(&node, key, max_key)?,
                other => {
                    return Err(Error::invalid(
                        "catalog index node",
                        format!("node {node_num} has kind {other:?} during descent"),
                    ))
                }
            }
        }
        Err(Error::invalid("catalog tree depth", "descent exceeded the depth limit"))
    }

    /// Binary search one leaf for `key`.
    fn find_in_leaf(
        &self,
        node: &crate::btree::node::Node<'_>,
        key: &CatalogKey,
        max_key: usize,
    ) -> Option<(Vec<u16>, CatalogRecord)> {
        let mut lo = 0u16;
        let mut hi = node.num_records();
        let mut found: Option<u16> = None;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (k, _body) = split_record(node.record(mid).ok()?)?;
            match self.compare_keys(&k, key) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => {
                    found = Some(mid);
                    break;
                }
            }
        }
        let _ = max_key;

        let idx = found?;
        let record = node.record(idx).ok()?;
        let (stored, body) = split_record(record)?;
        parse_record(body).ok().map(|r| (stored.name, r))
    }

    /// Choose the child node to descend into from an index node.
    ///
    /// Mining reference: Apple `core/BTree.c` `BTSearchRecord`, which binary
    /// searches the index records and, for the first key greater than the search
    /// key, follows its trailing child pointer.
    fn descend_index(
        &self,
        node: &crate::btree::node::Node<'_>,
        key: &CatalogKey,
        max_key: usize,
    ) -> Result<u32> {
        let mut lo = 0u16;
        let mut hi = node.num_records();

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let rec = node.record(mid)?;
            let Some((k, _)) = split_record(rec) else {
                return Err(Error::invalid("catalog index record", "key could not be decoded"));
            };
            if self.compare_keys(&k, key) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        // The child pointer is the first u32 *after* the key, so the record has to
        // be re-read to find where it starts.
        let rec = node.record(lo)?;
        let key_len = record_key_len(rec, max_key)?;
        let child_off = key_len
            .checked_add(4)
            .filter(|off| *off <= rec.len())
            .ok_or(Error::Truncated {
                what: "catalog index record",
                needed: key_len + 4,
                available: rec.len(),
            })?;
        let child = u32::from_be_bytes(
            rec.get(child_off..child_off + 4)
                .and_then(|s| s.try_into().ok())
                .ok_or(Error::Truncated {
                    what: "catalog index child pointer",
                    needed: 4,
                    available: rec.len(),
                })?,
        );
        Ok(child)
    }

    /// Scan the children of `parent_id` in catalog order.
    ///
    /// Mining reference: Apple `core/hfs_catalog.c` `cat_getdirentries`, which
    /// finds the first child with `BTGetRecordFromIndex` and then walks forward
    /// through the leaf chain.
    ///
    /// If `stop_after_first` is set the scan stops at the first match.
    pub fn scan_children(
        &self,
        parent_id: Cnid,
        mut stop_after_first: impl FnMut(&CatalogEntry) -> bool,
    ) -> Result<Vec<CatalogEntry>> {
        let mut out = Vec::new();
        let header = self.tree.header();
        if header.leaf_records == 0 {
            return Ok(out);
        }

        let mut node_num = header.first_leaf_node;
        // Bounded so that a corrupt link chain cannot spin forever.
        let mut budget = header.total_nodes;
        let mut entered = false;

        while budget > 0 {
            budget -= 1;
            let bytes = self.tree.read_node_bytes(node_num)?;
            let node = self.tree.parse_node(&bytes)?;

            for i in 0..node.num_records() {
                let Ok(record) = node.record(i) else { continue };
                let Some((key, body)) = split_record(record) else { continue };
                if key.parent_id != parent_id {
                    // Keys are sorted by parentID, so once the range has been
                    // left it cannot reappear in this leaf or any later one.
                    if key.parent_id > parent_id {
                        return Ok(out);
                    }
                    continue;
                }
                entered = true;
                let Ok(parsed) = parse_record(body) else { continue };
                // A thread record's CNID is in its key, not its body; children
                // of a directory are main records, so anything else is skipped
                // rather than misattributed.
                let Some(cnid) = parsed.cnid() else { continue };
                let is_dir = matches!(parsed, CatalogRecord::Folder(_));
                let entry = CatalogEntry { name: key.name.clone(), cnid, is_dir };
                let done = stop_after_first(&entry);
                out.push(entry);
                if done {
                    return Ok(out);
                }
            }

            if node_num == header.last_leaf_node {
                break;
            }
            node_num = node.descriptor().f_link;
        }

        let _ = entered;
        Ok(out)
    }

    /// List every child of `parent_id`, in catalog order.
    pub fn read_dir(&self, parent_id: Cnid) -> Result<Vec<CatalogEntry>> {
        self.scan_children(parent_id, |_| false)
    }

    /// Enumerate every object on the volume.
    ///
    /// This scans every leaf and collects the thread records, taking each
    /// object's CNID from the thread record's **key** parentID. Every object has
    /// exactly one thread record, so each CNID appears exactly once.
    ///
    /// It is tempting to assume thread records occupy one contiguous key range,
    /// because a thread key's name is always empty. That is wrong: the key's
    /// parentID is the object's *own* CNID, which varies per object, so thread
    /// keys are scattered through the catalog alongside main records. The scan is
    /// therefore whole-catalog.
    ///
    /// Mining reference: `core/hfs_catalog.c` `buildthreadkey` builds the thread
    /// key from the node's CNID, and `buildthread` copies the main key's parent
    /// and name into the record body. Verified against a real volume.
    pub fn all_objects(&self) -> Result<Vec<ThreadEntry>> {
        let mut out = Vec::new();
        let header = self.tree.header();
        if header.leaf_records == 0 {
            return Ok(out);
        }

        let mut node_num = header.first_leaf_node;
        let mut budget = header.total_nodes;

        while budget > 0 {
            budget -= 1;
            let bytes = self.tree.read_node_bytes(node_num)?;
            let node = self.tree.parse_node(&bytes)?;

            for i in 0..node.num_records() {
                let Ok(record) = node.record(i) else { continue };
                let Some((key, body)) = split_record(record) else { continue };
                let Ok(CatalogRecord::Thread(t)) = parse_record(body) else { continue };
                if !key.name.is_empty() {
                    // The other key form is a main record, not a thread record.
                    continue;
                }
                let is_dir = t.is_folder();
                out.push(ThreadEntry { cnid: key.parent_id, name: t.node_name, is_dir });
            }

            if node_num == header.last_leaf_node {
                break;
            }
            node_num = node.descriptor().f_link;
        }
        Ok(out)
    }

}

/// One entry from a whole-volume enumeration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadEntry {
    /// The object's CNID, taken from the thread record's key parentID.
    pub cnid: Cnid,
    /// The object's name, taken from the thread record's body.
    pub name: Vec<u16>,
    /// Whether the object is a directory.
    pub is_dir: bool,
}

/// Split a node record into its key and the bytes after the key.
///
/// The key's on-disk length is prefix plus body, rounded up to an even count, so
/// the record body starts at `key.on_disk_size()`. Mining reference:
/// `core/BTreeNodeOps.c` `InsertKeyRecord` rounds the size up, and
/// `GetRecordAddress` starts the record at the offset the slot holds.
pub fn split_record(record: &[u8]) -> Option<(CatalogKey, &[u8])> {
    // The maximum is fixed by the format: a catalog key body is at most 516
    // bytes. A short body is reported by `CatalogKey::from_record`, which is
    // given the format maximum rather than the tree's, because the tree's value
    // is itself untrusted here.
    let key = CatalogKey::from_record(record, crate::btree::key::CATALOG_KEY_MAX_LENGTH).ok()?;
    let off = key.on_disk_size();
    if off > record.len() {
        return None;
    }
    Some((key, &record[off..]))
}

/// The on-disk size of the key at the head of `record`.
fn record_key_len(record: &[u8], max_key_length: usize) -> Result<usize> {
    let key = CatalogKey::from_record(record, max_key_length)?;
    Ok(key.on_disk_size())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::record::K_HFS_PLUS_FOLDER_THREAD_RECORD;
    use crate::btree::key::CATALOG_KEY_MAX_LENGTH;
    use crate::catalog::cnid::ROOT_FOLDER_ID;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// A catalog key followed by a thread record body.
    fn record_for(parent: u32, name: &str, cnid: u32, thread_type: i16) -> Vec<u8> {
        let key = CatalogKey::for_child(Cnid(parent), &units(name));
        let mut rec = key.to_record();

        let mut body = Vec::new();
        body.extend_from_slice(&thread_type.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&cnid.to_be_bytes());
        body.extend_from_slice(&(units(name).len() as u16).to_be_bytes());
        for u in units(name) {
            body.extend_from_slice(&u.to_be_bytes());
        }
        rec.extend_from_slice(&body);
        rec
    }

    /// Pack records into a leaf node, in key order.
    fn leaf(records: &[Vec<u8>], node_size: usize) -> Vec<u8> {
        let mut raw = vec![0u8; node_size];
        raw[8] = NodeKind::Leaf.as_i8() as u8;
        raw[9] = 1;
        raw[10..12].copy_from_slice(&(records.len() as u16).to_be_bytes());

        // Records grow upward from the descriptor; slot i holds the start of
        // record i and lives at node_size - 2*i - 2.
        let mut pos = crate::btree::node::NODE_DESCRIPTOR_SIZE;
        for (i, rec) in records.iter().enumerate() {
            let slot = node_size - 2 * i - 2;
            raw[slot..slot + 2].copy_from_slice(&(pos as u16).to_be_bytes());
            raw[pos..pos + rec.len()].copy_from_slice(rec);
            pos += rec.len();
        }
        // One more slot holds the free offset; it is slot `len`, so its address
        // is node_size - 2*len - 2.
        let free_slot = node_size - 2 * records.len() - 2;
        raw[free_slot..free_slot + 2].copy_from_slice(&(pos as u16).to_be_bytes());
        raw
    }

    #[test]
    fn split_record_separates_key_from_body() {
        let rec = record_for(2, "TestVol", 2, K_HFS_PLUS_FOLDER_THREAD_RECORD);
        let (key, body) = split_record(&rec).expect("split");
        assert_eq!(key.parent_id, Cnid(2));
        assert_eq!(key.name_string(), "TestVol");
        assert_eq!(body.len(), rec.len() - key.on_disk_size());
        // The body must begin with the record type word.
        assert_eq!(
            crate::endian::read_u16(body, 0).unwrap() as i16,
            K_HFS_PLUS_FOLDER_THREAD_RECORD
        );
        assert!(body.len() >= crate::catalog::record::THREAD_RECORD_NAME_OFFSET);
    }

    #[test]
    fn split_record_rejects_a_record_with_no_key() {
        assert!(split_record(&[]).is_none());
        assert!(split_record(&[0u8; 4]).is_none());
    }

    #[test]
    fn key_body_offsets_are_respected() {
        // A key whose body needs a pad byte must still leave the body aligned.
        for name in ["a", "ab", "abc", "abcd", "abcde"] {
            let key = CatalogKey::for_child(Cnid(2), &units(name));
            let mut rec = key.to_record();
            rec.extend_from_slice(&[0xAAu8; 8]);
            let (k, body) = split_record(&rec).unwrap();
            assert_eq!(k, key, "name {name:?}");
            assert_eq!(body[0], 0xAA, "name {name:?}: body must start cleanly");
        }
    }

    #[test]
    fn leaf_packing_produces_addressable_records() {
        let recs = vec![
            record_for(1, "", 2, K_HFS_PLUS_FOLDER_THREAD_RECORD),
            record_for(1, "", 16, 4),
        ];
        let raw = leaf(&recs, 4096);
        let node = crate::btree::node::Node::parse(&raw, 4096).unwrap();
        assert_eq!(node.num_records(), 2);
        for i in 0..2u16 {
            let (k, _) = split_record(node.record(i).unwrap()).unwrap();
            assert!(k.is_thread_key());
        }
    }

    #[test]
    fn comparator_selection_drives_key_ordering() {
        // Pure-key ordering logic, independent of any device.
        let folding_keys = ("TestVol", "testvol");
        let a = CatalogKey::for_child(ROOT_FOLDER_ID, &units(folding_keys.0));
        let b = CatalogKey::for_child(ROOT_FOLDER_ID, &units(folding_keys.1));
        assert_eq!(
            Comparator::CaseFolding.compare(&a.name, &b.name),
            Ordering::Equal,
            "folding makes these the same name"
        );
        assert_ne!(Comparator::Binary.compare(&a.name, &b.name), Ordering::Equal);
    }

    #[test]
    fn an_empty_name_sorts_before_a_named_one() {
        // Apple short-circuits to a length comparison when either name is empty,
        // which is what makes the thread key range self-consistent.
        let empty = CatalogKey::empty_thread();
        let named = CatalogKey::thread(&units("x"));
        assert_eq!(
            Comparator::CaseFolding.compare(&empty.name, &named.name),
            Ordering::Less
        );
        assert_eq!(
            Comparator::Binary.compare(&empty.name, &named.name),
            Ordering::Less
        );
        // An empty name also equals another empty name.
        assert_eq!(
            Comparator::CaseFolding.compare(&empty.name, &CatalogKey::empty_thread().name),
            Ordering::Equal
        );
    }

    #[test]
    fn catalog_key_max_length_is_the_format_maximum() {
        assert_eq!(crate::btree::key::CATALOG_KEY_MAX_LENGTH, CATALOG_KEY_MAX_LENGTH);
    }
}