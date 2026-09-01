//! ARP table reader.
//!
//! Used by the presence correlator as the L2 last-ditch check
//! when both heartbeat and ICMP echo have failed for a peer.
//! An IP in the ARP cache with a complete MAC means the
//! kernel has recently observed the host on the L2 segment;
//! that is enough to classify the peer as `Stalled` (hardware
//! present, framework dead) rather than `Absent`.
//!
//! Implementation: parse `/proc/net/arp` (Linux). Format is
//! whitespace-separated columns:
//!
//!   IP address       HW type     Flags       HW address            Mask     Device
//!   192.0.2.41       0x1         0x2         d8:cb:8a:11:22:33    *        eth0
//!
//! Flags column meanings (Linux):
//!   0x0 — incomplete (no MAC observed)
//!   0x2 — complete (MAC observed)
//!   0x4 — permanent (static entry)
//!   0x6 — complete + permanent
//!
//! We treat any non-zero flag as "complete enough"; only
//! 0x0 / missing MAC is rejected.
//!
//! Platform note: `/proc/net/arp` is Linux-specific. On other
//! Unix tiers the reader returns `false` (treated as "no
//! information"); the correlator falls through to `Absent`.
//! A platform-native ARP query (BSD `sysctl PF_ROUTE`, macOS
//! `arp -an` parse) is a future sibling implementation when
//! a non-Linux tier admits to the rig.
//!
//! Resource posture:
//! - One `std::fs::read_to_string` per ARP-check.
//! - Caches: none — the kernel already caches; re-reading
//!   `/proc/net/arp` per probe is microsecond-cheap and
//!   always-fresh.
//! - Per-check cost: O(ARP table size), which is bounded by
//!   the LAN's host count (typically < 100).

use std::net::IpAddr;
use std::path::Path;

/// Path to the Linux ARP table. Overridable for tests via
/// [`is_in_arp_cache_at`].
const ARP_PROC_PATH: &str = "/proc/net/arp";

/// True iff `ip` appears in the kernel's ARP cache with a
/// complete MAC address. False if the IP is not in the cache,
/// or the entry is incomplete (no MAC observed), or the ARP
/// table is unreadable (non-Linux platform, permissions
/// stripped). False on parse error.
///
/// The function is intentionally tolerant: a false return is
/// "no L2 evidence that this peer is on the segment" — which
/// is the correct correlator behaviour for both "host is gone"
/// and "we cannot determine".
pub fn is_in_arp_cache(ip: IpAddr) -> bool {
    is_in_arp_cache_at(Path::new(ARP_PROC_PATH), ip)
}

/// Path-injectable variant for testing.
pub fn is_in_arp_cache_at(path: &Path, ip: IpAddr) -> bool {
    let Ok(table) = std::fs::read_to_string(path) else {
        return false;
    };
    for entry in parse_arp_table(&table) {
        if entry.ip == ip
            && entry.complete
            && !entry.mac.is_empty()
            && entry.mac != "00:00:00:00:00:00"
        {
            return true;
        }
    }
    false
}

/// Parsed ARP entry. Internal — exposed in tests only.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArpEntry {
    ip: IpAddr,
    /// Flags interpreted as "complete": any non-zero flag.
    complete: bool,
    mac: String,
}

/// Parse `/proc/net/arp` content into entries. Skips the
/// header row. Lines with too few columns or unparseable IPs
/// are silently skipped.
fn parse_arp_table(content: &str) -> impl Iterator<Item = ArpEntry> + '_ {
    content.lines().skip(1).filter_map(|line| {
        let mut fields = line.split_whitespace();
        let ip_str = fields.next()?;
        let _hw_type = fields.next()?;
        let flags_str = fields.next()?;
        let mac = fields.next()?.to_string();
        let ip: IpAddr = ip_str.parse().ok()?;
        let flags =
            u32::from_str_radix(flags_str.trim_start_matches("0x"), 16).ok()?;
        Some(ArpEntry {
            ip,
            complete: flags != 0,
            mac,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const SAMPLE_ARP_TABLE: &str = "\
IP address       HW type     Flags       HW address            Mask     Device
192.0.2.41       0x1         0x2         d8:cb:8a:11:22:33     *        eth0
192.0.2.40       0x1         0x2         52:54:00:aa:bb:cc     *        eth0
192.0.2.99       0x1         0x0         00:00:00:00:00:00     *        eth0
192.0.2.10       0x1         0x6         aa:bb:cc:dd:ee:ff     *        eth0
";

    fn write_temp_arp(content: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file
    }

    #[test]
    fn parse_table_extracts_every_complete_entry() {
        let entries: Vec<_> = parse_arp_table(SAMPLE_ARP_TABLE).collect();
        assert_eq!(entries.len(), 4);
        assert!(entries
            .iter()
            .any(|e| e.ip == IpAddr::V4(Ipv4Addr::new(192, 0, 2, 41))
                && e.complete));
        assert!(entries
            .iter()
            .any(|e| e.ip == IpAddr::V4(Ipv4Addr::new(192, 0, 2, 40))
                && e.complete));
        assert!(entries
            .iter()
            .any(|e| e.ip == IpAddr::V4(Ipv4Addr::new(192, 0, 2, 99))
                && !e.complete));
        assert!(entries
            .iter()
            .any(|e| e.ip == IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
                && e.complete));
    }

    #[test]
    fn is_in_cache_returns_true_for_complete_entry() {
        let f = write_temp_arp(SAMPLE_ARP_TABLE);
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 41));
        assert!(is_in_arp_cache_at(f.path(), ip));
    }

    #[test]
    fn is_in_cache_returns_false_for_incomplete_entry() {
        let f = write_temp_arp(SAMPLE_ARP_TABLE);
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 99));
        assert!(!is_in_arp_cache_at(f.path(), ip));
    }

    #[test]
    fn is_in_cache_returns_false_for_unknown_ip() {
        let f = write_temp_arp(SAMPLE_ARP_TABLE);
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 200));
        assert!(!is_in_arp_cache_at(f.path(), ip));
    }

    #[test]
    fn is_in_cache_returns_true_for_permanent_complete_entry() {
        let f = write_temp_arp(SAMPLE_ARP_TABLE);
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
        // Flag 0x6 = complete + permanent.
        assert!(is_in_arp_cache_at(f.path(), ip));
    }

    #[test]
    fn missing_arp_file_returns_false_silently() {
        let path = std::path::Path::new("/nonexistent/arp/file");
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 41));
        assert!(!is_in_arp_cache_at(path, ip));
    }

    #[test]
    fn empty_arp_table_returns_no_entries() {
        let entries: Vec<_> = parse_arp_table("").collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn header_only_arp_table_returns_no_entries() {
        let content = "IP address       HW type     Flags       HW address            Mask     Device\n";
        let entries: Vec<_> = parse_arp_table(content).collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn malformed_row_silently_skipped() {
        let content = "\
IP address       HW type     Flags       HW address            Mask     Device
not.an.ip        0x1         0x2         de:ad:be:ef:00:00     *        eth0
192.0.2.41       0x1         0x2         d8:cb:8a:11:22:33     *        eth0
";
        let entries: Vec<_> = parse_arp_table(content).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ip, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 41)));
    }
}
