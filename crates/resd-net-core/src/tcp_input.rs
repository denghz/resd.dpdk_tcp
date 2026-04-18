//! Inbound TCP segment processing. Entry point is `tcp_input_dispatch`;
//! it parses the segment, looks up the flow, and dispatches to the
//! per-state handler. Per-state handlers are in this file but live
//! in `handle_syn_sent`, `handle_established`, etc.
//!
//! Per-segment ACK policy (spec §6.4): every segment that advances
//! `rcv_nxt` or transitions state triggers an ACK on the same poll
//! iteration (wired in the handlers via `TxAction::Ack`).

use crate::flow_table::FourTuple;
use crate::tcp_conn::TcpConn;
use crate::tcp_output::{TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN};
use crate::tcp_state::TcpState;

#[derive(Debug, Clone, Copy)]
pub struct ParsedSegment<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub header_len: usize, // bytes including options
    pub payload: &'a [u8],
    /// The raw options-bytes region, if any. A3 only peeks for MSS
    /// (RFC 6691); unknown options are skipped.
    pub options: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpParseError {
    Short,
    BadDataOffset,
    BadFlags,
    Csum,
}

/// Parse a TCP segment from `tcp_bytes` (starts at the TCP header).
/// `src_ip`/`dst_ip` are from the IPv4 header in host byte order and
/// are used for the pseudo-header checksum verification. Caller can
/// skip verification by passing `nic_csum_ok=true` when the NIC has
/// already verified the TCP checksum.
pub fn parse_segment<'a>(
    tcp_bytes: &'a [u8],
    src_ip: u32,
    dst_ip: u32,
    nic_csum_ok: bool,
) -> Result<ParsedSegment<'a>, TcpParseError> {
    if tcp_bytes.len() < 20 {
        return Err(TcpParseError::Short);
    }
    let src_port = u16::from_be_bytes([tcp_bytes[0], tcp_bytes[1]]);
    let dst_port = u16::from_be_bytes([tcp_bytes[2], tcp_bytes[3]]);
    let seq = u32::from_be_bytes([tcp_bytes[4], tcp_bytes[5], tcp_bytes[6], tcp_bytes[7]]);
    let ack = u32::from_be_bytes([tcp_bytes[8], tcp_bytes[9], tcp_bytes[10], tcp_bytes[11]]);
    let data_off_words = (tcp_bytes[12] >> 4) as usize;
    if data_off_words < 5 {
        return Err(TcpParseError::BadDataOffset);
    }
    let header_len = data_off_words * 4;
    if tcp_bytes.len() < header_len {
        return Err(TcpParseError::BadDataOffset);
    }
    let flags = tcp_bytes[13];
    // Reject obviously-broken flag combinations per RFC 9293 §3.5
    // (SYN+FIN is nonsensical; RST+SYN likewise).
    if (flags & TCP_SYN) != 0 && (flags & TCP_FIN) != 0 {
        return Err(TcpParseError::BadFlags);
    }
    if (flags & TCP_RST) != 0 && (flags & TCP_SYN) != 0 {
        return Err(TcpParseError::BadFlags);
    }
    let window = u16::from_be_bytes([tcp_bytes[14], tcp_bytes[15]]);
    let options = &tcp_bytes[20..header_len];
    let payload = &tcp_bytes[header_len..];

    if !nic_csum_ok {
        let stored = u16::from_be_bytes([tcp_bytes[16], tcp_bytes[17]]);
        let mut scratch = tcp_bytes.to_vec();
        scratch[16] = 0;
        scratch[17] = 0;
        let csum = tcp_pseudo_csum(src_ip, dst_ip, scratch.len() as u32, &scratch);
        // Folded result of header-with-zero-csum + stored-csum should sum to 0.
        if csum != stored {
            return Err(TcpParseError::Csum);
        }
    }

    Ok(ParsedSegment {
        src_port,
        dst_port,
        seq,
        ack,
        flags,
        window,
        header_len,
        payload,
        options,
    })
}

fn tcp_pseudo_csum(src_ip: u32, dst_ip: u32, tcp_seg_len: u32, tcp_bytes: &[u8]) -> u16 {
    let mut buf = Vec::with_capacity(12 + tcp_bytes.len());
    buf.extend_from_slice(&src_ip.to_be_bytes());
    buf.extend_from_slice(&dst_ip.to_be_bytes());
    buf.push(0);
    buf.push(crate::l3_ip::IPPROTO_TCP);
    buf.extend_from_slice(&(tcp_seg_len as u16).to_be_bytes());
    buf.extend_from_slice(tcp_bytes);
    crate::l3_ip::internet_checksum(&buf)
}

/// Parse the TCP options field for a MSS value. Returns 536 (RFC 9293
/// §3.7.1 default) when absent. Unknown options are skipped by `len`.
pub fn parse_mss_option(options: &[u8]) -> u16 {
    let mut i = 0;
    while i < options.len() {
        match options[i] {
            0 => return 536, // End of options
            1 => { i += 1; } // NOP
            2 => {
                // MSS option
                if i + 4 > options.len() || options[i + 1] != 4 {
                    return 536;
                }
                return u16::from_be_bytes([options[i + 2], options[i + 3]]);
            }
            _ => {
                if i + 1 >= options.len() {
                    return 536;
                }
                let olen = options[i + 1] as usize;
                if olen < 2 {
                    return 536;
                }
                i += olen;
            }
        }
    }
    536
}

/// What the engine should do next after processing a segment. Emitted
/// by the per-state handlers and consumed by the engine's dispatch code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxAction {
    None,
    Ack,
    Rst,
}

/// Outcome of dispatching a segment to a per-state handler.
#[derive(Debug, Clone, Copy)]
pub struct Outcome {
    pub tx: TxAction,
    pub new_state: Option<TcpState>,
    /// Number of payload bytes delivered to recv queue this segment.
    /// `> 0` implies the engine should enqueue a Readable event.
    pub delivered: u32,
    /// True iff this segment completed a handshake (SYN_SENT → ESTABLISHED).
    pub connected: bool,
    /// True iff this segment completed a clean close (→ CLOSED or
    /// entered TIME_WAIT which reaps to CLOSED).
    pub closed: bool,
}

impl Outcome {
    pub fn none() -> Self {
        Self { tx: TxAction::None, new_state: None, delivered: 0, connected: false, closed: false }
    }
    pub fn rst() -> Self {
        Self { tx: TxAction::Rst, new_state: Some(TcpState::Closed), delivered: 0, connected: false, closed: true }
    }
}

/// Per-state dispatcher. Stubs for now; concrete handlers land in
/// Tasks 11–13.
pub fn dispatch(conn: &mut TcpConn, seg: &ParsedSegment) -> Outcome {
    match conn.state {
        TcpState::SynSent => handle_syn_sent(conn, seg),
        TcpState::Established => handle_established(conn, seg),
        TcpState::FinWait1
        | TcpState::FinWait2
        | TcpState::Closing
        | TcpState::LastAck
        | TcpState::CloseWait
        | TcpState::TimeWait => handle_close_path(conn, seg),
        _ => Outcome::none(),
    }
}

// Stubs filled in by subsequent tasks.
fn handle_syn_sent(conn: &mut TcpConn, seg: &ParsedSegment) -> Outcome {
    use crate::tcp_seq::seq_le;

    // RFC 9293 §3.10.7.3 — SYN_SENT processing.
    // RST without a valid ACK of our SYN → drop silently. With a valid
    // ACK (snd_una < ack <= snd_nxt) → close.
    if (seg.flags & TCP_RST) != 0 {
        if (seg.flags & TCP_ACK) != 0
            && seq_le(conn.snd_una.wrapping_add(1), seg.ack)
            && seq_le(seg.ack, conn.snd_nxt)
        {
            return Outcome {
                tx: TxAction::None,
                new_state: Some(TcpState::Closed),
                delivered: 0,
                connected: false,
                closed: true,
            };
        }
        return Outcome::none();
    }

    // Must have SYN to advance from SYN_SENT. Simultaneous-open (SYN
    // without ACK) transitions to SYN_RECEIVED per RFC 9293 — deferred
    // to A4. We drop it here.
    if (seg.flags & TCP_SYN) == 0 {
        return Outcome::rst();
    }

    if (seg.flags & TCP_ACK) == 0 {
        // SYN-only (simultaneous-open): deferred.
        return Outcome::none();
    }

    // ACK must cover exactly iss+1 (our SYN). Accept only when
    // snd_una+1 <= ack <= snd_nxt (RFC 9293 §3.10.7.3).
    if !seq_le(conn.snd_una.wrapping_add(1), seg.ack)
        || !seq_le(seg.ack, conn.snd_nxt)
    {
        return Outcome::rst();
    }

    // Update state per RFC 9293.
    conn.irs = seg.seq;
    conn.rcv_nxt = seg.seq.wrapping_add(1);
    conn.snd_una = seg.ack;
    conn.snd_wnd = seg.window as u32;
    conn.snd_wl1 = seg.seq;
    conn.snd_wl2 = seg.ack;
    conn.peer_mss = parse_mss_option(seg.options);

    Outcome {
        tx: TxAction::Ack,
        new_state: Some(TcpState::Established),
        delivered: 0,
        connected: true,
        closed: false,
    }
}

fn handle_established(_conn: &mut TcpConn, _seg: &ParsedSegment) -> Outcome {
    Outcome::none()
}

fn handle_close_path(_conn: &mut TcpConn, _seg: &ParsedSegment) -> Outcome {
    Outcome::none()
}

/// Build the 4-tuple from a parsed segment's ports + the IPv4 header's
/// source/destination. Caller owns the IP fields. HBO throughout.
pub fn tuple_from_segment(src_ip: u32, dst_ip: u32, seg: &ParsedSegment) -> FourTuple {
    // RX: the segment arrives FROM peer TO us. Our tuple has
    // local = our side, peer = their side.
    FourTuple {
        local_ip: dst_ip,
        local_port: seg.dst_port,
        peer_ip: src_ip,
        peer_port: seg.src_port,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tcp_output::{build_segment, SegmentTx};

    fn build_test_segment(flags: u8, mss: Option<u16>, payload: &[u8]) -> Vec<u8> {
        let seg = SegmentTx {
            src_mac: [0x02, 0, 0, 0, 0, 1],
            dst_mac: [0x02, 0, 0, 0, 0, 2],
            src_ip: 0x0a_00_00_01,
            dst_ip: 0x0a_00_00_02,
            src_port: 5000,
            dst_port: 40000,
            seq: 100,
            ack: 200,
            flags,
            window: 65535,
            mss_option: mss,
            payload,
        };
        let mut out = vec![0u8; 256];
        let n = build_segment(&seg, &mut out).unwrap();
        out.truncate(n);
        out
    }

    #[test]
    fn parse_ack_segment_with_payload() {
        let frame = build_test_segment(TCP_ACK | TCP_PSH, None, b"hello");
        let tcp = &frame[14 + 20..];
        let p = parse_segment(tcp, 0x0a_00_00_01, 0x0a_00_00_02, false).unwrap();
        assert_eq!(p.src_port, 5000);
        assert_eq!(p.dst_port, 40000);
        assert_eq!(p.seq, 100);
        assert_eq!(p.ack, 200);
        assert_eq!(p.payload, b"hello");
        assert_eq!(p.flags, TCP_ACK | TCP_PSH);
    }

    #[test]
    fn parse_rejects_short_segment() {
        let err = parse_segment(&[0u8; 10], 0, 0, true).unwrap_err();
        assert_eq!(err, TcpParseError::Short);
    }

    #[test]
    fn parse_rejects_syn_fin_combo() {
        let frame = build_test_segment(TCP_SYN | TCP_FIN, None, &[]);
        let tcp = &frame[14 + 20..];
        let err = parse_segment(tcp, 0x0a_00_00_01, 0x0a_00_00_02, true).unwrap_err();
        assert_eq!(err, TcpParseError::BadFlags);
    }

    #[test]
    fn parse_mss_option_present() {
        let frame = build_test_segment(TCP_SYN | TCP_ACK, Some(1460), &[]);
        let tcp = &frame[14 + 20..];
        let p = parse_segment(tcp, 0x0a_00_00_01, 0x0a_00_00_02, false).unwrap();
        assert_eq!(parse_mss_option(p.options), 1460);
    }

    #[test]
    fn parse_mss_absent_returns_default() {
        assert_eq!(parse_mss_option(&[]), 536);
    }

    #[test]
    fn bad_tcp_csum_rejected() {
        let mut frame = build_test_segment(TCP_ACK, None, b"hi");
        // Flip a payload bit — csum must now mismatch.
        frame[14 + 20 + 20] ^= 0xff;
        let tcp = &frame[14 + 20..];
        let err = parse_segment(tcp, 0x0a_00_00_01, 0x0a_00_00_02, false).unwrap_err();
        assert_eq!(err, TcpParseError::Csum);
    }

    #[test]
    fn tuple_from_segment_swaps_src_and_dst() {
        let frame = build_test_segment(TCP_ACK, None, &[]);
        let tcp = &frame[14 + 20..];
        let p = parse_segment(tcp, 0x0a_00_00_01, 0x0a_00_00_02, false).unwrap();
        let t = tuple_from_segment(0x0a_00_00_01, 0x0a_00_00_02, &p);
        assert_eq!(t.local_ip, 0x0a_00_00_02);
        assert_eq!(t.local_port, 40000);
        assert_eq!(t.peer_ip, 0x0a_00_00_01);
        assert_eq!(t.peer_port, 5000);
    }

    #[test]
    fn syn_sent_syn_ack_transitions_to_established() {
        use crate::flow_table::FourTuple;
        use crate::tcp_conn::TcpConn;

        let t = FourTuple { local_ip: 0x0a_00_00_02, local_port: 40000,
                            peer_ip: 0x0a_00_00_01, peer_port: 5000 };
        let mut c = TcpConn::new_client(t, 1000, 1460, 1024, 2048);
        c.state = TcpState::SynSent;
        c.snd_nxt = c.snd_nxt.wrapping_add(1); // after SYN TX
        // Craft a SYN-ACK: their seq=5000, their ack=1001 (our iss+1), MSS=1400.
        let seg = ParsedSegment {
            src_port: 5000, dst_port: 40000,
            seq: 5000, ack: 1001,
            flags: TCP_SYN | TCP_ACK,
            window: 65535,
            header_len: 24,
            payload: &[],
            options: &[2, 4, 0x05, 0x78], // MSS=1400
        };
        let out = dispatch(&mut c, &seg);
        assert_eq!(out.new_state, Some(TcpState::Established));
        assert_eq!(out.tx, TxAction::Ack);
        assert!(out.connected);
        assert_eq!(c.rcv_nxt, 5001);
        assert_eq!(c.snd_una, 1001);
        assert_eq!(c.irs, 5000);
        assert_eq!(c.peer_mss, 1400);
    }

    #[test]
    fn syn_sent_plain_ack_wrong_seq_sends_rst() {
        use crate::flow_table::FourTuple;
        use crate::tcp_conn::TcpConn;

        let t = FourTuple { local_ip: 0x0a_00_00_02, local_port: 40000,
                            peer_ip: 0x0a_00_00_01, peer_port: 5000 };
        let mut c = TcpConn::new_client(t, 1000, 1460, 1024, 2048);
        c.state = TcpState::SynSent;
        c.snd_nxt = c.snd_nxt.wrapping_add(1);
        // Bogus: ACK-only with an ack that doesn't cover our SYN.
        let seg = ParsedSegment {
            src_port: 5000, dst_port: 40000,
            seq: 5000, ack: 999,
            flags: TCP_ACK,
            window: 65535,
            header_len: 20,
            payload: &[], options: &[],
        };
        let out = dispatch(&mut c, &seg);
        assert_eq!(out.tx, TxAction::Rst);
    }

    #[test]
    fn syn_sent_rst_matching_our_ack_closes() {
        use crate::flow_table::FourTuple;
        use crate::tcp_conn::TcpConn;

        let t = FourTuple { local_ip: 0x0a_00_00_02, local_port: 40000,
                            peer_ip: 0x0a_00_00_01, peer_port: 5000 };
        let mut c = TcpConn::new_client(t, 1000, 1460, 1024, 2048);
        c.state = TcpState::SynSent;
        c.snd_nxt = c.snd_nxt.wrapping_add(1);
        let seg = ParsedSegment {
            src_port: 5000, dst_port: 40000,
            seq: 0, ack: 1001,
            flags: TCP_RST | TCP_ACK,
            window: 0,
            header_len: 20,
            payload: &[], options: &[],
        };
        let out = dispatch(&mut c, &seg);
        assert_eq!(out.new_state, Some(TcpState::Closed));
        assert!(out.closed);
        assert_eq!(out.tx, TxAction::None);
    }
}
