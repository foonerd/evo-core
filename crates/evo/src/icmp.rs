//! Unprivileged ICMP echo prober.
//!
//! Used by the presence correlator to distinguish a `Stalled`
//! peer (hardware on the network, `evo` crashed or restarting)
//! from an `Absent` peer (hardware off the network) when the
//! heartbeat substrate has been silent for the device for more
//! than the `Quiet → Stalled` threshold.
//!
//! Implementation: `socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)`
//! is the Linux unprivileged ICMP socket type. The host's
//! `net.ipv4.ping_group_range` sysctl must admit the steward's
//! group (default-wide on Debian / Pi OS / Raspbian — full
//! `0 2147483647`). On distributions that restrict it, the
//! prober's `new()` returns an error and the correlator falls
//! back to ARP-only Stalled detection.
//!
//! Wire shape: ICMPv4 type 8 (echo request) with framework-
//! supplied id + monotonically-increasing sequence, payload
//! is the literal bytes `evo` (4 bytes; identification +
//! debugging convenience). Reply matching is by id + sequence
//! within a per-probe deadline.
//!
//! IPv6 is not addressed in this module. The framework's LAN
//! presence substrate is IPv4-first; IPv6 echo (ICMPv6 type
//! 128) is a future sibling module when an IPv6-only rig
//! enters scope.
//!
//! Resource posture:
//! - One socket per `IcmpProber` instance; reused across all
//!   probes (the correlator constructs one and shares it).
//! - Non-blocking; integrates with the tokio reactor.
//! - Per-probe cost: one packet out, one packet in (or one
//!   timeout if unreachable). No state retained between probes.

use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;

/// ICMP echo prober errors.
#[derive(Debug, thiserror::Error)]
pub enum IcmpError {
    /// Socket bind / open / configure failure. On Linux, the
    /// most common cause is `ping_group_range` not admitting
    /// the steward's group.
    #[error("ICMP socket: {0}")]
    Socket(#[from] std::io::Error),
    /// Address family mismatch (IPv6 target on IPv4 prober).
    #[error("ICMP target address family unsupported: {0}")]
    UnsupportedAddress(IpAddr),
}

/// Unprivileged ICMPv4 echo prober.
pub struct IcmpProber {
    socket: UdpSocket,
    next_seq: AtomicU16,
    id: u16,
}

impl IcmpProber {
    /// Open the unprivileged ICMPv4 socket. Returns
    /// `IcmpError::Socket` if `ping_group_range` does not
    /// admit the calling process's group, or the kernel does
    /// not support `IPPROTO_ICMP` on `SOCK_DGRAM`.
    pub fn new() -> Result<Self, IcmpError> {
        let raw = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::ICMPV4),
        )?;
        raw.set_nonblocking(true)?;
        let std_sock: std::net::UdpSocket = raw.into();
        let tok_sock = UdpSocket::from_std(std_sock)?;
        // Use process id (truncated to u16) as the ICMP
        // identifier so concurrent stewards on the same host
        // do not race their echo replies. Pure-Rust truncation
        // via `as` is fine here — we just need a stable
        // per-process id.
        let id = std::process::id() as u16;
        Ok(Self {
            socket: tok_sock,
            next_seq: AtomicU16::new(0),
            id,
        })
    }

    /// Probe `target` with an ICMPv4 echo request; return
    /// `Ok(true)` on echo reply within `deadline`, `Ok(false)`
    /// on timeout (peer unreachable). Network errors return
    /// `Err`; a `false` from timeout is NOT an error — it is
    /// the contract: "we tried, we waited, peer did not
    /// respond."
    pub async fn probe(
        &self,
        target: IpAddr,
        deadline: Duration,
    ) -> Result<bool, IcmpError> {
        let ipv4 = match target {
            IpAddr::V4(v) => v,
            IpAddr::V6(_) => return Err(IcmpError::UnsupportedAddress(target)),
        };
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let request = build_echo_request(self.id, seq);
        let dest = SocketAddr::V4(SocketAddrV4::new(ipv4, 0));
        self.socket.send_to(&request, dest).await?;

        // Wait for any matching echo reply within deadline.
        // The kernel queues all ICMP replies to this socket;
        // we filter by id + seq.
        let start = tokio::time::Instant::now();
        let mut recv_buf = vec![0u8; 256];
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(false);
            }
            let recv = tokio::time::timeout(
                remaining,
                self.socket.recv_from(&mut recv_buf),
            )
            .await;
            match recv {
                Ok(Ok((n, _addr))) => {
                    if is_matching_echo_reply(&recv_buf[..n], self.id, seq) {
                        return Ok(true);
                    }
                    // Non-matching reply (someone else's probe);
                    // loop and wait for ours within the deadline.
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => return Ok(false),
            }
        }
    }
}

/// Build an ICMPv4 echo request packet.
///
/// Wire layout (RFC 792):
///   byte 0: type (8 = echo request)
///   byte 1: code (0)
///   bytes 2-3: checksum (computed over the full packet)
///   bytes 4-5: identifier
///   bytes 6-7: sequence number
///   bytes 8+: payload
fn build_echo_request(id: u16, seq: u16) -> Vec<u8> {
    const PAYLOAD: &[u8] = b"evo\0";
    let mut packet = Vec::with_capacity(8 + PAYLOAD.len());
    packet.push(8); // type: echo request
    packet.push(0); // code
    packet.extend_from_slice(&[0, 0]); // checksum placeholder
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(PAYLOAD);

    let checksum = icmp_checksum(&packet);
    packet[2] = (checksum >> 8) as u8;
    packet[3] = (checksum & 0xff) as u8;
    packet
}

/// RFC 1071 internet checksum.
fn icmp_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        // Odd-length tail; pad with zero.
        sum += (data[i] as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// True iff `bytes` is an ICMPv4 echo reply (type 0) with the
/// expected identifier + sequence. The kernel strips the IP
/// header on `IPPROTO_ICMP` `SOCK_DGRAM` sockets, so the
/// packet starts at the ICMP header.
fn is_matching_echo_reply(
    bytes: &[u8],
    expected_id: u16,
    expected_seq: u16,
) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    if bytes[0] != 0 || bytes[1] != 0 {
        // Not echo reply.
        return false;
    }
    let id = u16::from_be_bytes([bytes[4], bytes[5]]);
    let seq = u16::from_be_bytes([bytes[6], bytes[7]]);
    id == expected_id && seq == expected_seq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_known_vector() {
        // RFC 1071 worked example: ones-complement sum of
        // 0x0001, 0xf203, 0xf4f5, 0xf6f7 = 0x2dddc (after fold:
        // 0xddf2). ~0xddf2 = 0x220d.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(icmp_checksum(&data), 0x220d);
    }

    #[test]
    fn checksum_zero_for_zero_data() {
        let data = [0u8; 8];
        // Sum = 0, complement = 0xffff.
        assert_eq!(icmp_checksum(&data), 0xffff);
    }

    #[test]
    fn echo_request_has_correct_type_and_code() {
        let pkt = build_echo_request(42, 7);
        assert_eq!(pkt[0], 8); // echo request type
        assert_eq!(pkt[1], 0); // code
        let id = u16::from_be_bytes([pkt[4], pkt[5]]);
        let seq = u16::from_be_bytes([pkt[6], pkt[7]]);
        assert_eq!(id, 42);
        assert_eq!(seq, 7);
    }

    #[test]
    fn echo_request_checksum_is_valid() {
        // Verifier convention: checksum of a valid packet
        // (with the checksum field intact) is 0.
        let pkt = build_echo_request(42, 7);
        assert_eq!(icmp_checksum(&pkt), 0);
    }

    #[test]
    fn echo_request_payload_is_present() {
        let pkt = build_echo_request(1, 1);
        assert_eq!(&pkt[8..12], b"evo\0");
    }

    #[test]
    fn echo_reply_matching_recognises_correct_id_seq() {
        // Construct a minimal echo reply (type 0).
        let mut reply = vec![0, 0, 0, 0];
        reply.extend_from_slice(&123u16.to_be_bytes());
        reply.extend_from_slice(&456u16.to_be_bytes());
        assert!(is_matching_echo_reply(&reply, 123, 456));
    }

    #[test]
    fn echo_reply_matching_rejects_wrong_type() {
        let mut reply = vec![8, 0, 0, 0]; // type 8 = echo request, not reply
        reply.extend_from_slice(&123u16.to_be_bytes());
        reply.extend_from_slice(&456u16.to_be_bytes());
        assert!(!is_matching_echo_reply(&reply, 123, 456));
    }

    #[test]
    fn echo_reply_matching_rejects_wrong_id() {
        let mut reply = vec![0, 0, 0, 0];
        reply.extend_from_slice(&999u16.to_be_bytes());
        reply.extend_from_slice(&456u16.to_be_bytes());
        assert!(!is_matching_echo_reply(&reply, 123, 456));
    }

    #[test]
    fn echo_reply_matching_rejects_wrong_seq() {
        let mut reply = vec![0, 0, 0, 0];
        reply.extend_from_slice(&123u16.to_be_bytes());
        reply.extend_from_slice(&999u16.to_be_bytes());
        assert!(!is_matching_echo_reply(&reply, 123, 456));
    }

    #[test]
    fn echo_reply_matching_rejects_short_packet() {
        let reply = vec![0, 0, 0, 0, 0, 1, 0]; // 7 bytes
        assert!(!is_matching_echo_reply(&reply, 0, 1));
    }
}
