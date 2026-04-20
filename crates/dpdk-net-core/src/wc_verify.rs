//! Write-Combining BAR-mapping verification for AWS ENA. Mirrors the
//! pattern of `llq_verify.rs`: bring-up-time check, slow-path counter
//! bump on negative result, WARN-only (no fail-hard by default).
//!
//! Source: docs/references/ena-dpdk-readme.md §6.1 (mandatory WC for
//! ENAv2 + LLQ), §6.2.3 (verification recipe), §15 (DPDK 21.11
//! regression). On a misconfigured igb_uio (loaded without
//! `wc_activate=1`) or affected vfio-pci, LLQ activates but the BAR
//! falls back to uncached-minus → ena_com_prep_pkts dominates the
//! flame graph (perf-FAQ Q1).

/// Parse `/sys/kernel/debug/x86/pat_memtype_list` and return whether
/// the prefetchable BAR at `bar_phys_addr` (e.g. 0xfe900000) has a
/// `write-combining` mapping. Per the README §6.2.3 the file format is:
///
///     PAT: [mem 0x00000000fe900000-0x00000000fea00000] write-combining
///
/// Match the second numeric — the start of the range — against
/// `bar_phys_addr` and confirm the trailing token is `write-combining`.
///
/// `allow(dead_code)` covers Task 2 — the engine bring-up wiring that
/// will call this function lands in Task 3, mirroring the same gating
/// pattern used by `llq_verify::verify_llq_activation`. Visibility is
/// `pub` (not `pub(crate)`) so the Task 12 pure-unit smoke at
/// `tests/ena_obs_smoke.rs` can exercise the parser across the
/// integration-test crate boundary.
#[allow(dead_code)]
pub fn parse_pat_memtype_list(
    pat_contents: &str,
    bar_phys_addr: u64,
) -> WcVerdict {
    let needle_lo = format!("{:016x}", bar_phys_addr); // 16-hex zero-pad
    for line in pat_contents.lines() {
        // Line shape: "PAT: [mem 0x00000000fe900000-0x00000000fea00000] write-combining"
        let Some(after_mem) = line.find("[mem 0x") else { continue };
        let rest = &line[after_mem + "[mem 0x".len()..];
        let Some(dash) = rest.find('-') else { continue };
        let lo_hex = &rest[..dash];
        if !lo_hex.eq_ignore_ascii_case(&needle_lo) {
            continue;
        }
        // Found a matching BAR-start address. Decide WC vs other based
        // on the trailing token. The post-loop fall-through path means
        // "no matching range found at all" → NotFound.
        if line.contains("write-combining") {
            return WcVerdict::WriteCombining;
        } else {
            return WcVerdict::OtherMapping;
        }
    }
    WcVerdict::NotFound
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum WcVerdict {
    WriteCombining,
    OtherMapping,
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
PAT: [mem 0x00000000fe800000-0x00000000fe900000] write-combining
PAT: [mem 0x00000000fe900000-0x00000000fea00000] write-combining
PAT: [mem 0x00000000fea00000-0x00000000feb00000] uncached-minus
";

    #[test]
    fn matches_write_combining_bar() {
        assert_eq!(
            parse_pat_memtype_list(SAMPLE, 0xfe900000),
            WcVerdict::WriteCombining,
        );
    }

    #[test]
    fn detects_uncached_bar() {
        assert_eq!(
            parse_pat_memtype_list(SAMPLE, 0xfea00000),
            WcVerdict::OtherMapping,
        );
    }

    #[test]
    fn missing_bar_is_not_found() {
        assert_eq!(
            parse_pat_memtype_list(SAMPLE, 0xdeadbeef),
            WcVerdict::NotFound,
        );
    }

    #[test]
    fn empty_input_is_not_found() {
        assert_eq!(
            parse_pat_memtype_list("", 0xfe900000),
            WcVerdict::NotFound,
        );
    }

    #[test]
    fn case_insensitive_hex() {
        let upper = "PAT: [mem 0x00000000FE900000-0x00000000FEA00000] write-combining\n";
        assert_eq!(
            parse_pat_memtype_list(upper, 0xfe900000),
            WcVerdict::WriteCombining,
        );
    }
}
