//! Mac OS timestamps and Apple's "expanded times" mode.
//!
//! # Why this is not just a subtraction
//!
//! Classic HFS timestamps are `u32` seconds since 1904-01-01 00:00:00 UTC. The
//! Unix epoch is 2,082,844,800 seconds later. It is tempting to model that as
//! a constant offset, and for well-behaved post-1970 timestamps that is
//! numerically correct. It is nevertheless wrong as a *specification*, for
//! three reasons that this crate preserves deliberately:
//!
//! 1. **Pre-epoch timestamps clamp rather than go negative.** Apple's
//!    `to_bsd_time` compares with a strict `>` against the epoch factor and
//!    assigns `0` otherwise. A file stamped 1960 therefore reports as the Unix
//!    epoch, not as a negative time. Silently allowing a negative `time_t`
//!    would produce different `stat` output from Apple's, and would make
//!    round-trip writes lossy in a way Apple's own code is not.
//! 2. **Zero is not the same as the epoch in classic mode.** `to_hfs_time`
//!    deliberately does not add the factor to zero, because zero in a classic
//!    HFS timestamp means "never set" rather than 1904-01-01. Applying the
//!    offset to zero would fabricate a date for an unstamped field.
//! 3. **Expanded times change the epoch entirely.** A volume with
//!    `kHFSVolumeHasExpandedTimesMask` set stores timestamps that are *already*
//!    Unix seconds. On such a volume the factor must not be applied at all,
//!    and zero *is* the legitimate value 1970-01-01.
//!
//! Mining reference: Apple `core/MacOSStubs.c` implements `to_bsd_time` and
//! `to_hfs_time` with exactly these rules (`expanded` short-circuits, classic
//! mode uses a strict comparison against `MAC_GMT_FACTOR`, and negative inputs
//! are clipped to zero in expanded mode). The same functions are duplicated in
//! `livefiles_hfs_plugin/lf_hfs_utils.c`, which is useful corroboration that
//! the behaviour is intentional and stable rather than incidental. The
//! `kHFSExpandedTimesBit` / `kHFSExpandedTimesMask` constants come from
//! `core/hfs_format.h`, and `core/hfs_vfsutils.c`
//! (`hfs_MountHFSPlusVolume`) passes `attributes & kHFSExpandedTimesMask` into
//! the conversion for volume-header timestamps.

/// Seconds between the Mac OS epoch (1904-01-01) and the Unix epoch
/// (1970-01-01).
///
/// The interval is 24107 days: 66 years of 365 days plus 17 leap days
/// (1904, 1908, ... 1968 all fall inside the span).
///
/// Mining reference: `MAC_GMT_FACTOR` in Apple `core/hfs.h`, which defines it
/// as `2082844800UL`. `core/MacOSStubs.c` consumes it in `to_bsd_time` and
/// `to_hfs_time`.
pub const MAC_GMT_FACTOR: u32 = 2_082_844_800;

/// Bit 29 of the volume attribute word: the volume uses expanded, non-MacOS
/// native timestamps.
///
/// Mining reference: `kHFSExpandedTimesBit` in Apple `core/hfs_format.h`.
pub const K_HFS_EXPANDED_TIMES_BIT: u32 = 29;

/// Mask form of [`K_HFS_EXPANDED_TIMES_BIT`].
///
/// Mining reference: `kHFSExpandedTimesMask` in Apple `core/hfs_format.h`.
pub const K_HFS_EXPANDED_TIMES_MASK: u32 = 0x2000_0000;

/// A Mac OS timestamp together with the epoch interpretation that applies to
/// it.
///
/// The epoch choice is part of the value, not a global setting, because it is
/// decided by the volume that owns the record. Keeping them together makes it
/// impossible to decode a timestamp with the wrong epoch by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HfsTimestamp {
    /// Raw on-disk seconds.
    pub raw: u32,
    /// Whether the owning volume is in expanded-timestamps mode.
    pub expanded: bool,
}

impl HfsTimestamp {
    /// Wrap a raw value for a volume with the given expanded-times flag.
    pub const fn new(raw: u32, expanded: bool) -> Self {
        HfsTimestamp { raw, expanded }
    }

    /// Interpret the value as Unix seconds and return a `time_t`-style
    /// `i64`.
    ///
    /// Mirrors Apple's `to_bsd_time(hfs_time, expanded)`. In classic mode a
    /// value at or below [`MAC_GMT_FACTOR`] clamps to `0`; in expanded mode the
    /// value is returned unchanged.
    pub fn to_unix(self) -> i64 {
        to_bsd_time(self.raw, self.expanded)
    }

    /// Whether this is the "never set" sentinel of a classic HFS timestamp.
    ///
    /// In classic mode zero means *unset*, whereas in expanded mode zero is a
    /// real date (1970-01-01). Mining reference: `to_hfs_time` in
    /// `core/MacOSStubs.c`, whose comment states "don't adjust zero - treat as
    /// uninitialzed".
    pub fn is_unset(self) -> bool {
        !self.expanded && self.raw == 0
    }

    /// Format as RFC 3339 in UTC, or `None` for the classic unset sentinel.
    ///
    /// Diagnostics only. Formatting is kept out of the library's semantics so
    /// that no consumer accidentally compares formatted strings.
    pub fn to_rfc3339(self) -> Option<String> {
        if self.is_unset() {
            return None;
        }
        let secs = self.to_unix();
        format_utc(secs)
    }
}

/// Convert Mac OS seconds to Unix seconds, per Apple's `to_bsd_time`.
///
/// Mining reference: Apple `core/MacOSStubs.c`, `to_bsd_time`:
///
/// ```c
/// time_t to_bsd_time(u_int32_t hfs_time, bool expanded)
/// {
///     u_int32_t gmt = hfs_time;
///     if (expanded) return (time_t) gmt;
///     if (gmt > MAC_GMT_FACTOR) gmt -= MAC_GMT_FACTOR;
///     else                       gmt = 0;  /* don't let date go negative! */
///     return (time_t) gmt;
/// }
/// ```
pub fn to_bsd_time(hfs_time: u32, expanded: bool) -> i64 {
    if expanded {
        return i64::from(hfs_time);
    }
    if hfs_time > MAC_GMT_FACTOR {
        i64::from(hfs_time - MAC_GMT_FACTOR)
    } else {
        0
    }
}

/// Convert Unix seconds to Mac OS seconds, per Apple's `to_hfs_time`.
///
/// Mining reference: Apple `core/MacOSStubs.c`, `to_hfs_time`, including the
/// two special cases: classic mode leaves zero untouched (unset sentinel), and
/// expanded mode clips negative inputs to zero.
///
/// ```
/// use hfsplus::timestamp::to_hfs_time;
/// assert_eq!(to_hfs_time(0, true), 0);              // expanded: real epoch
/// assert_eq!(to_hfs_time(0, false), 0);             // classic: "unset"
/// assert_eq!(to_hfs_time(1, false), 2_082_844_801); // classic: 1904 + 1s
/// assert_eq!(to_hfs_time(-1, true), 0);            // expanded: clipped
/// ```
pub fn to_hfs_time(bsd_time: i64, expanded: bool) -> u32 {
    let hfs_time = bsd_time as u32; // wrapping cast mirrors Apple's clip-on-write
    if expanded {
        return if bsd_time < 0 { 0 } else { hfs_time };
    }
    if hfs_time != 0 {
        hfs_time.wrapping_add(MAC_GMT_FACTOR)
    } else {
        0
    }
}

/// Extract the expanded-times flag from a volume attribute word.
///
/// Mining reference: `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) uses
/// `vcb->vcbAtrb & kHFSExpandedTimesMask` as the `expanded` argument to
/// `to_bsd_time`.
pub const fn expanded_times_from_attributes(attributes: u32) -> bool {
    attributes & K_HFS_EXPANDED_TIMES_MASK != 0
}

/// Format Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` without pulling in a date
/// library.
///
/// Implemented with a civil-from-days conversion so that the crate keeps zero
/// dependencies in the format layer. Handles the full proleptic Gregorian range
/// representable in `i64` seconds, which is far beyond any HFS timestamp.
fn format_utc(secs: i64) -> Option<String> {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    Some(format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"))
}

/// Convert a count of days since 1970-01-01 into a civil (y, m, d) date.
///
/// Howard Hinnant's `civil_from_days` algorithm, shifted to a March-based year
/// so that the leap day lands at the end.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_factor_matches_known_offset() {
        // 1904-01-01 + 2082844800s == 1970-01-01.
        assert_eq!(MAC_GMT_FACTOR, 2_082_844_800);
        assert_eq!(to_bsd_time(MAC_GMT_FACTOR, false), 0);
    }

    #[test]
    fn classic_post_epoch_subtracts_factor() {
        assert_eq!(to_bsd_time(MAC_GMT_FACTOR + 1, false), 1);
        assert_eq!(to_bsd_time(MAC_GMT_FACTOR + 86_400, false), 86_400);
    }

    #[test]
    fn classic_pre_epoch_clamps_to_zero() {
        // Apple uses a strict `>`, so exactly-at-epoch also yields 0, and
        // anything earlier clamps rather than going negative.
        assert_eq!(to_bsd_time(0, false), 0);
        assert_eq!(to_bsd_time(MAC_GMT_FACTOR - 1, false), 0);
        // 1904-01-01 itself.
        assert_eq!(to_bsd_time(1, false), 0);
    }

    #[test]
    fn expanded_times_are_already_unix() {
        assert_eq!(to_bsd_time(0, true), 0);
        assert_eq!(to_bsd_time(1_234_567_890, true), 1_234_567_890);
        // A value that would be a valid 1904 date is not shifted in expanded mode.
        assert_eq!(to_bsd_time(100, true), 100);
    }

    #[test]
    fn unset_sentinel_only_applies_in_classic_mode() {
        assert!(HfsTimestamp::new(0, false).is_unset());
        assert!(!HfsTimestamp::new(0, true).is_unset());
        assert_eq!(HfsTimestamp::new(0, false).to_rfc3339(), None);
        assert_eq!(
            HfsTimestamp::new(0, true).to_rfc3339().as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
    }

    #[test]
    fn round_trip_post_epoch() {
        for secs in [0i64, 1, 1_000_000, 1_700_000_000, 4_000_000_000] {
            let hfs = to_hfs_time(secs, true);
            assert_eq!(to_bsd_time(hfs, true), secs, "expanded round trip {secs}");
        }
    }

    #[test]
    fn expanded_flag_extracted_from_attributes() {
        assert!(expanded_times_from_attributes(K_HFS_EXPANDED_TIMES_MASK));
        assert!(!expanded_times_from_attributes(0));
        assert!(!expanded_times_from_attributes(0x8000)); // unmounted bit only
    }

    #[test]
    fn formats_known_dates() {
        assert_eq!(
            HfsTimestamp::new(MAC_GMT_FACTOR + 1, false).to_rfc3339().as_deref(),
            Some("1970-01-01T00:00:01Z")
        );
        assert_eq!(
            HfsTimestamp::new(MAC_GMT_FACTOR + 1_700_000_000, false)
                .to_rfc3339()
                .as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        // 1904-01-01, which clamps to the epoch under Apple's rule.
        assert_eq!(
            HfsTimestamp::new(0, false).to_rfc3339(),
            None
        );
    }
}
