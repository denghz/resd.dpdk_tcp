//! AD-A7-dup-syn-in-syn-rcvd-silent-drop + mTCP AD-4 promotion (S1(d) / A8 T14).
//!
//! RFC 9293 §3.10.7.4 Fourth + §3.8.1 (mTCP reading):
//!   - dup-SYN with SEG.SEQ == IRS → benign loss-retransmit case →
//!     retransmit SYN-ACK (peer didn't see our first SYN-ACK, re-sent
//!     its own SYN). Reuses T11's SynRetrans wheel entry; no re-arm.
//!   - dup-SYN with SEG.SEQ != IRS → in-window SYN → RST (peer confused
//!     or malicious; RFC 9293 §3.10.7.4 Fourth strict reading).
//!
//! Retires AD-A7-dup-syn-in-syn-rcvd-silent-drop + mTCP AD-4 (the
//! fourth and final S1 AD-A7 promotion task: T11 S1(a), T12 S1(b),
//! T13 S1(c), T14 S1(d)).

#![cfg(feature = "test-server")]

mod common;

use common::{is_rst, is_syn_ack, parse_syn_seq, parse_tcp_seq_ack, CovHarness};

#[test]
fn dup_syn_in_syn_rcvd_same_iss_triggers_syn_ack_retransmit() {
    let mut h = CovHarness::new();

    // Peer SYN → we SYN-ACK → peer's SYN retransmit (same ISS) lands
    // while we're in SYN_RCVD (peer didn't see our SYN-ACK).
    h.do_listen_then_peer_syn();
    let synack1 = h.drain_tx_frames();
    assert_eq!(synack1.len(), 1, "initial peer SYN → one SYN-ACK");
    assert!(is_syn_ack(&synack1[0]), "initial emission must be SYN-ACK");

    // Peer retransmits the SAME SYN (same peer_iss; represents peer's
    // loss-retransmit because our SYN-ACK was dropped).
    h.inject_duplicate_peer_syn_same_iss();
    let synack2 = h.drain_tx_frames();
    assert_eq!(
        synack2.len(),
        1,
        "dup-SYN with seq==IRS → SYN-ACK retransmit per mTCP AD-4 / RFC 9293 §3.8.1"
    );
    assert!(
        is_syn_ack(&synack2[0]),
        "retransmit must be SYN-ACK, not RST"
    );

    // SYN-ACK retransmit must use the SAME ISS we emitted first time
    // (the peer expects a consistent ACK of their SYN; a different ISS
    // would break the handshake on peer's second SYN-ACK reception).
    assert_eq!(
        parse_syn_seq(&synack1[0]),
        parse_syn_seq(&synack2[0]),
        "retransmitted SYN-ACK must carry the same ISS as the first emission"
    );
}

#[test]
fn dup_syn_in_syn_rcvd_different_iss_triggers_rst() {
    let mut h = CovHarness::new();
    h.do_listen_then_peer_syn();
    // Drain the initial SYN-ACK.
    let _ = h.drain_tx_frames();

    // Peer sends a "new" SYN on the SAME 4-tuple with a DIFFERENT ISS.
    // RFC 9293 §3.10.7.4 Fourth: in-window SYN with SEG.SEQ != IRS → RST.
    h.inject_duplicate_peer_syn_different_iss(0x5000);
    let out = h.drain_tx_frames();
    assert_eq!(
        out.len(),
        1,
        "dup-SYN with SEG.SEQ != IRS must emit exactly one RST"
    );
    assert!(
        is_rst(&out[0]),
        "in-window SYN with SEG.SEQ != IRS → RST per RFC 9293 §3.10.7.4 Fourth"
    );
}

/// B2 regression: matched-flow `emit_rst` must include the SYN flag's
/// 1-byte sequence-space contribution in the RST's ACK field per
/// RFC 9293 §3.10.7.2. Pre-B2, `emit_rst` computed
/// `ack = incoming.seq + payload_len`, ignoring SYN/FIN. Sister
/// `send_rst_unmatched` already added `+syn_len+fin_len`; B2 ports the
/// same arithmetic into `emit_rst`.
///
/// Scenario: dup-SYN-in-SYN_RCVD with a different ISS (the same path
/// `dup_syn_in_syn_rcvd_different_iss_triggers_rst` exercises) drives
/// `TxAction::Rst` → `emit_rst` with a SYN-bearing zero-payload incoming.
/// Expected ACK = `new_iss + 0 (payload) + 1 (SYN) + 0 (FIN) = new_iss + 1`.
/// Pre-fix this would have been `new_iss`, off-by-one.
#[test]
fn rst_for_dup_syn_includes_syn_byte_in_ack() {
    let mut h = CovHarness::new();
    h.do_listen_then_peer_syn();
    let _ = h.drain_tx_frames();

    let new_iss: u32 = 0x5000;
    h.inject_duplicate_peer_syn_different_iss(new_iss);
    let out = h.drain_tx_frames();
    assert_eq!(out.len(), 1, "exactly one RST expected");
    assert!(is_rst(&out[0]), "frame must be RST");

    let (_seq, ack) = parse_tcp_seq_ack(&out[0]);
    assert_eq!(
        ack,
        new_iss.wrapping_add(1),
        "RFC 9293 §3.10.7.2: RST ACK must advance past SYN's 1 seq number \
         (expected new_iss+1, got {ack:#x})"
    );
}
