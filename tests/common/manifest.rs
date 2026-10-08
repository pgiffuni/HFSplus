// SPDX-License-Identifier: BSD-2-Clause

//! Minimal TOML subset reader for the test-image manifests.
//!
//! The manifest format is part of the test specification, so it gets a parser —
//! but pulling in a full TOML dependency for `key = value`, `[table]` and
//! `[[array of tables]]` would add supply-chain surface to a crate whose whole
//! point is that it parses untrusted input itself. The subset supported here is
//! exactly what `tools/genmanifests.sh` emits.
//!
//! Supported:
//!   - `# comment` and blank lines
//!   - `[table.sub]` table headers (nested by dots)
//!   - `[[array.of.tables]]` array-of-table headers
//!   - `key = value`, where value is a quoted string, a bare token, a number,
//!     a boolean, or an inline array of the same
//!
//! Anything else is an error rather than a silently skipped line, so that a
//! manifest typo cannot quietly weaken a test.

use std::collections::BTreeMap;

/// One parsed manifest.
#[derive(Debug, Default, Clone)]
pub struct Manifest {
    /// Values keyed by dotted path, e.g. `volume.block_size` or `forks.catalogFile.logical_size`.
    pub values: BTreeMap<String, String>,
    /// Current table path at end of file, for diagnostics.
    pub last_table: String,
}

impl Manifest {
    /// Parse a manifest from TOML text.
    ///
    /// # Panics
    ///
    /// Panics on a malformed manifest. That is deliberate and confined to test
    /// code: a manifest is a checked-in test fixture, so an unparseable one is
    /// a bug in the repository rather than an attack on the parser under test.
    /// Runtime parsing of untrusted images never goes through this path.
    pub fn parse(text: &str) -> Self {
        let mut values: BTreeMap<String, String> = BTreeMap::new();
        let mut path: Vec<String> = Vec::new();
        let mut array_len: BTreeMap<String, usize> = BTreeMap::new();

        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(inner) = line.strip_prefix("[[") {
                let name = inner
                    .strip_suffix("]]")
                    .unwrap_or_else(|| panic!("line {}: unterminated [[table]] header", lineno + 1))
                    .trim();
                let idx = array_len.entry(name.to_string()).or_insert(0);
                *idx += 1;
                // Each [[array]] entry gets its own index so that entries do not
                // overwrite one another: [[files]] #1 stores under files.1.*.
                path = name
                    .split('.')
                    .map(|s| s.to_string())
                    .chain(std::iter::once(idx.to_string()))
                    .collect();
                continue;
            }

            if let Some(inner) = line.strip_prefix('[') {
                let name = inner
                    .strip_suffix(']')
                    .unwrap_or_else(|| panic!("line {}: unterminated [table] header", lineno + 1))
                    .trim();
                path = name.split('.').map(|s| s.to_string()).collect();
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                panic!(
                    "line {}: expected `key = value`, got `{}`",
                    lineno + 1,
                    line
                );
            };
            let key = key.trim();
            let value = value.trim();

            let full = if path.is_empty() {
                key.to_string()
            } else {
                format!("{}.{}", path.join("."), key)
            };
            values.insert(full, unquote(value, lineno + 1));
        }

        let last_table = String::new();
        Manifest { values, last_table }
    }

    /// Read a string value, panicking if absent.
    pub fn str(&self, key: &str) -> &str {
        self.values
            .get(key)
            .map(|s| s.as_str())
            .unwrap_or_else(|| panic!("manifest is missing required key `{key}`"))
    }

    /// Read an integer value, panicking if absent or unparseable.
    pub fn int(&self, key: &str) -> i64 {
        let raw = self.str(key);
        let cleaned = raw.trim().trim_start_matches("0x");
        let parsed = if raw.trim().starts_with("0x") {
            i64::from_str_radix(cleaned, 16)
        } else {
            cleaned.parse::<i64>()
        };
        parsed.unwrap_or_else(|e| panic!("manifest key `{key}` is not an integer: {raw:?} ({e})"))
    }

    /// Read an integer parsed as hexadecimal.
    pub fn hex(&self, key: &str) -> i64 {
        let raw = self.str(key).trim();
        let cleaned = raw.strip_prefix("0x").unwrap_or(raw);
        i64::from_str_radix(cleaned, 16)
            .unwrap_or_else(|e| panic!("manifest key `{key}` is not hex: {raw:?} ({e})"))
    }

    /// Read a boolean value.
    pub fn bool(&self, key: &str) -> bool {
        match self.str(key).trim() {
            "true" => true,
            "false" => false,
            other => panic!("manifest key `{key}` is not a boolean: {other:?}"),
        }
    }

    /// Read an optional string.
    pub fn opt(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|s| s.as_str())
    }

    /// Read an optional integer.
    pub fn opt_int(&self, key: &str) -> Option<i64> {
        let raw = self.opt(key)?.trim();
        Some(
            raw.parse::<i64>().unwrap_or_else(|e| {
                panic!("manifest key `{key}` is not an integer: {raw:?} ({e})")
            }),
        )
    }

    /// Whether the manifest declares the given key.
    pub fn has(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// Number of `[[array]]` entries of the given name.
    ///
    /// Keys inside an array entry are stored under `<name>.<index>.<key>`,
    /// counting from 1, so entries never overwrite each other.
    pub fn array_len(&self, name: &str) -> usize {
        // Distinct indices, not key count: an entry with eight keys is one
        // entry, and counting keys made this report 8 for a single `[[files]]`.
        let mut indices: Vec<usize> = self
            .values
            .keys()
            .filter_map(|k| k.strip_prefix(&format!("{name}.")))
            .filter_map(|rest| rest.split('.').next())
            .filter_map(|seg| seg.parse::<usize>().ok())
            .collect();
        indices.sort_unstable();
        indices.dedup();
        indices.len()
    }

    /// Read the `key` of entry `index` (1-based) of array `name`.
    pub fn array_item(&self, name: &str, index: usize, key: &str) -> Option<&str> {
        self.values
            .get(&format!("{name}.{index}.{key}"))
            .map(|s| s.as_str())
    }

    /// The manifest's `name` field.
    pub fn name(&self) -> &str {
        self.str("name")
    }

    /// The manifest's declared filesystem, e.g. `HFS+`.
    pub fn filesystem(&self) -> &str {
        self.str("filesystem")
    }

    /// The expected outcome: `mount`, `mount-readonly` or `reject-cleanly`.
    pub fn outcome(&self) -> &str {
        self.str("expect.outcome")
    }

    /// The substring the parser's error must contain, when rejecting.
    pub fn expected_error(&self) -> &str {
        self.opt("expect.expected_error").unwrap_or("")
    }

    /// Path to the image this manifest describes, relative to the repo root.
    pub fn image_path(&self) -> &str {
        self.str("source.image")
    }

    /// The independent checker's verdict, when recorded.
    pub fn verdict(&self) -> Option<&str> {
        self.opt("verify.verdict")
    }
}

fn unquote(value: &str, lineno: usize) -> String {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        return v[1..v.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    // Strip a trailing inline comment outside quotes.
    if let Some(pos) = v.find(" #") {
        return v[..pos].trim().to_string();
    }
    if v.contains('"') {
        panic!("line {lineno}: malformed string value {v:?}");
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# a comment
name = "basic-hfsplus"
filesystem = "HFS+"

[source]
image = "tests/images/generated/basic-hfsplus.img"

[volume]
block_size = 4096
attributes = "0x80000100"
unmounted = true

[forks.catalogFile]
logical_size = 262144

[[files]]
path = "a"

[[files]]
path = "b"
"#;

    #[test]
    fn parses_scalars_tables_and_nesting() {
        let m = Manifest::parse(SAMPLE);
        assert_eq!(m.name(), "basic-hfsplus");
        assert_eq!(m.filesystem(), "HFS+");
        assert_eq!(m.image_path(), "tests/images/generated/basic-hfsplus.img");
        assert_eq!(m.int("volume.block_size"), 4096);
        assert_eq!(m.hex("volume.attributes"), 0x8000_0100);
        assert!(m.bool("volume.unmounted"));
        assert_eq!(m.int("forks.catalogFile.logical_size"), 262_144);
    }

    #[test]
    fn array_entries_are_indexed_and_do_not_overwrite_each_other() {
        // Several keys per entry, because counting keys instead of entries
        // looks correct when every entry has exactly one.
        let m = Manifest::parse(
            "[[files]]\nname = \"a\"\nsize = 1\n\n[[files]]\nname = \"b\"\nsize = 2\n",
        );
        assert_eq!(m.array_len("files"), 2);
        assert_eq!(m.array_item("files", 1, "name"), Some("a"));
        assert_eq!(m.array_item("files", 2, "name"), Some("b"));
    }

    #[test]
    fn array_len_counts_entries_not_keys() {
        let m = Manifest::parse("[[files]]\nname = \"a\"\nsize = 1\nkind = \"file\"\n");
        assert_eq!(m.array_len("files"), 1, "one entry with three keys");
        assert_eq!(m.array_len("absent"), 0);
    }

    #[test]
    fn _old_array_entries_test() {
        let m = Manifest::parse(SAMPLE);
        assert!(m.has("forks.catalogFile.logical_size"));
        // An array entry must not collide with a plain table of the same name.
        assert!(!m.has("files.path"));
        assert_eq!(m.array_len("files"), 2);
        assert_eq!(m.array_item("files", 1, "path"), Some("a"));
        assert_eq!(m.array_item("files", 2, "path"), Some("b"));
        assert_eq!(m.array_item("files", 3, "path"), None);
    }

    #[test]
    fn reports_missing_keys_distinctly() {
        let m = Manifest::parse(SAMPLE);
        assert!(!m.has("does.not.exist"));
        assert!(m.opt("does.not.exist").is_none());
        assert_eq!(m.opt_int("also.missing"), None);
    }

    #[test]
    fn strips_inline_comments_from_unquoted_values() {
        let m = Manifest::parse("a = 5 # five\n");
        assert_eq!(m.int("a"), 5);
    }
}
