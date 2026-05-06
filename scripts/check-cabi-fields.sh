#!/usr/bin/env bash
# scripts/check-cabi-fields.sh
#
# Pattern P5 — C-ABI dead-field audit (Task A6 of the cross-phase fixes plan).
#
# # The class of bug this script ends
#
# Each Stage 1 phase has accreted new fields onto the public C-ABI struct
# `dpdk_net_engine_config_t` (in `crates/dpdk-net/src/api.rs`) without
# wiring every one through to the core-side `EngineConfig`. The compile-
# time `size_of` assertions catch *layout* drift but NOT per-field
# *semantics* — a brand-new `pub field: T,` line happily compiles even if
# nothing ever reads it on the Rust side. The result is a zombie knob:
# C callers set it, the engine ignores it, the field stays in the public
# header forever (ABI stability), and over multiple phases the surface
# grows a long tail of inert configuration.
#
# Concrete examples (Part 1 BLOCK-A11 #1, surfaced by the cross-phase
# meta-synthesis):
#   - tcp_min_rto_ms      (superseded by tcp_min_rto_us in A5 Task 21)
#   - tcp_timestamps      (declared at A2; never wired)
#   - tcp_sack            (declared at A2; never wired)
#   - tcp_ecn             (declared at A2; never wired)
#
# # What the script does
#
# 1. Parse `crates/dpdk-net/src/api.rs` and extract every `pub <field>: <ty>`
#    declared inside `pub struct dpdk_net_engine_config_t { ... }`.
#    Doc-comments (`///`) and attributes (`#[...]`) attached to a field
#    are scanned as part of that field's "leading lines" so an opt-out
#    marker can be found there.
# 2. For each field name `f`, run ripgrep across the workspace
#    (excluding api.rs itself) for any read of `\.f\b`. The pattern
#    intentionally also catches struct-init writes (`f: value` or
#    `cfg.f`), but we filter the writes back out by skipping matches
#    where the immediate next non-whitespace char is `:` AND the line
#    matches the struct-literal init shape (`<ws>f: <expr>,`). What's
#    left is a real read.
#    NOTE: api.rs itself contains the struct *declaration* (the field
#    name appears as `pub f: T,` — that pattern does NOT match `\.f\b`
#    so api.rs would not contribute spurious hits anyway, but we
#    exclude it explicitly for clarity).
# 3. Allow opt-out: if the field's leading lines (preceding doc-comments
#    or attributes) contain a marker of the form
#       // REMOVE-BY: A<N>
#    or
#       /// REMOVE-BY: A<N>
#    the field is treated as a deliberate carry-over awaiting removal in
#    the named task. The marker is mandatory machinery: bare comments
#    like "TODO: remove" are not honoured — you have to commit to a task
#    number.
# 4. Print a structured report and exit:
#    - 0 if every field has a reader OR carries a REMOVE-BY marker;
#    - 1 if any field is dead (no reader, no marker). In that case the
#      output names every offender with the file:line of the declaration
#      and the actionable remediation hint.
#
# # Expected state at this script's introduction
#
# This script is **RED on master at introduction time, by design**. The
# four fields enumerated above (tcp_min_rto_ms, tcp_timestamps, tcp_sack,
# tcp_ecn) are all dead at HEAD. Task A1 of the cross-phase fixes plan
# either deletes them or wires them; the gate flips to GREEN when A1
# lands. (Surfacing them as RED is the proof-of-life that A6 actually
# works — if A6 lands and the gate is GREEN at HEAD, either someone
# already did A1's work, or A6's parser missed them.)
#
# # Self-test
#
# `bash scripts/check-cabi-fields.sh --self-test` runs the parser
# logic against synthetic struct bodies and asserts dead-vs-alive
# detection. Run by CI before the real gate so a parser regression
# can't silently turn the gate green.
#
# # Operation
#
# Pure bash + `rg`. No cargo, no compile, no network. Sub-second.
#
# Exit codes:
#   0 — every config field is alive or carries an explicit REMOVE-BY marker
#   1 — one or more dead fields surfaced (printed with remediation hint)
#   2 — tool error (rg missing, api.rs missing, regex parse failure)

set -euo pipefail

readonly SCRIPT_NAME="check-cabi-fields.sh"
readonly API_FILE_REL="crates/dpdk-net/src/api.rs"
readonly TARGET_STRUCT="dpdk_net_engine_config_t"

die()  { echo "ERROR [$SCRIPT_NAME]: $*" >&2; exit 2; }
fail() { echo "FAIL  [$SCRIPT_NAME]: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# parse_struct_fields <api_file> <struct_name>
#
# Emits, on stdout, one line per field of the named struct in the form:
#
#   <line_no>\t<field_name>\t<remove_by_marker_or_-->
#
# where <remove_by_marker_or_-> is the task-number string from a
# `REMOVE-BY: A<N>` comment in the field's leading docs/attributes,
# or `-` if no marker is present.
#
# Implementation: stream the file with awk, track whether we're inside
# `pub struct <struct_name> {` ... `}`, accumulate doc-comment / attr
# lines as `pending`, and when we hit a `pub <field>: ...,` line emit
# the tuple and clear `pending`.
# ---------------------------------------------------------------------------
parse_struct_fields() {
    local api_file="$1" struct_name="$2"
    awk -v target="$struct_name" '
        BEGIN { inside = 0; depth = 0; pending = ""; }
        # Open brace of the target struct: the line that contains
        #   pub struct <target> {
        # We require both `pub struct <target>` and an opening `{` on
        # that line (true for the current api.rs; if a future refactor
        # splits the brace onto its own line, the parser falls back to
        # detecting `{` on a subsequent line while inside == 1).
        {
            if (!inside) {
                if ($0 ~ ("pub[ \t]+struct[ \t]+" target "[ \t]*\\{")) {
                    inside = 1; depth = 1; pending = ""; next;
                }
                if ($0 ~ ("pub[ \t]+struct[ \t]+" target "([ \t]|$)")) {
                    inside = 1; depth = 0; pending = ""; next;
                }
                next;
            }
            # We are inside the struct. Track brace depth so a nested
            # block (e.g. a const expression as a default) does not
            # close the scan early. The current api.rs has no nested
            # blocks inside the config struct, but be defensive.
            n_open = gsub(/\{/, "{");
            # gsub returned count via the assignment trick above is
            # unreliable across awks; recompute with a portable loop.
            tmp = $0; n_open = 0;
            while ((idx = index(tmp, "{")) > 0) { n_open++; tmp = substr(tmp, idx + 1); }
            tmp = $0; n_close = 0;
            while ((idx = index(tmp, "}")) > 0) { n_close++; tmp = substr(tmp, idx + 1); }
            depth += n_open - n_close;
            if (depth <= 0) { inside = 0; pending = ""; next; }
        }
        # Doc-comment, attribute, or blank → accumulate as leading lines.
        /^[[:space:]]*\/\// {
            pending = pending $0 "\n"; next;
        }
        /^[[:space:]]*#\[/ {
            pending = pending $0 "\n"; next;
        }
        /^[[:space:]]*$/ { pending = ""; next; }
        # Field declaration: `    pub <name>: <type>,`
        # Capture the field name and emit the tuple, then clear pending.
        # We deliberately accept only `pub` fields; private fields cannot
        # appear on the C ABI anyway.
        /^[[:space:]]*pub[[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]*:/ {
            line_no = NR;
            # Extract the field name: strip leading "pub" + ws, then
            # everything from the first ":" onward.
            s = $0;
            sub(/^[[:space:]]*pub[[:space:]]+/, "", s);
            colon = index(s, ":");
            if (colon == 0) { pending = ""; next; }
            name = substr(s, 1, colon - 1);
            sub(/[[:space:]]+$/, "", name);
            # Look for a REMOVE-BY: A<N> marker in pending lines.
            marker = "-";
            if (match(pending, /REMOVE-BY:[[:space:]]*A[0-9A-Za-z_.]+/) > 0) {
                m = substr(pending, RSTART, RLENGTH);
                sub(/^REMOVE-BY:[[:space:]]*/, "", m);
                marker = m;
            }
            print line_no "\t" name "\t" marker;
            pending = "";
            next;
        }
        # Anything else inside the struct (e.g. a stray comment block
        # without `///` prefix) just clears pending so it does not stick
        # to the next field by accident.
        { pending = ""; }
    ' "$api_file"
}

# ---------------------------------------------------------------------------
# field_has_reader <field_name> <repo_root> <api_file_rel>
#
# Returns 0 if `\.<field>\b` matches anywhere in the workspace OUTSIDE
# of the api.rs declaration site. Struct-literal initializers
# (`<field>: <expr>`) are NOT matched by `\.<field>\b` so the test set
# need not filter writes explicitly.
#
# We exclude:
#   - the api.rs file itself (it only contains the declaration);
#   - the `target/` build directory and `.git/`;
# ---------------------------------------------------------------------------
field_has_reader() {
    local field="$1" repo_root="$2" api_file_rel="$3"
    # `rg -q` exits 0 on first match, 1 on no match, 2 on error. We use
    # `--type rust` + `--type toml` to catch both code references and
    # any toml-ferried test fixtures (rare but cheap to include).
    rg --quiet \
        --type rust \
        --glob "!${api_file_rel}" \
        --glob '!target/**' \
        --glob '!**/target/**' \
        "\\.${field}\\b" \
        "$repo_root"
}

# ---------------------------------------------------------------------------
# Self-test (--self-test): synthetic struct + synthetic call sites + assert
# the parser + reader-detection logic agrees with hand-computed truth.
# ---------------------------------------------------------------------------
if [[ "${1:-}" == "--self-test" ]]; then
    echo "=== $SCRIPT_NAME --self-test ==="
    tmpdir=$(mktemp -d)
    trap 'rm -rf "$tmpdir"' EXIT
    fake_api="$tmpdir/api.rs"
    fake_user="$tmpdir/user.rs"
    cat >"$fake_api" <<'RUST'
//! synthetic api
#[repr(C)]
pub struct dpdk_net_engine_config_t {
    /// alive — read in user.rs
    pub port_id: u16,
    /// alive — read in user.rs (with leading attr below)
    #[allow(dead_code)]
    pub recv_buffer_bytes: u32,
    /// dead — never read anywhere
    pub tcp_zombie_knob: u32,
    /// dead but explicitly carried — REMOVE-BY: A99
    pub tcp_pending_removal: bool,
    /// dead, marker on a `//` comment style — REMOVE-BY: A99.5
    // REMOVE-BY: A99.5
    pub tcp_pending_removal_two: u8,
}

#[repr(C)]
pub struct dpdk_net_other_struct {
    pub red_herring: u32,
}
RUST
    cat >"$fake_user" <<'RUST'
fn use_cfg(cfg: &super::api::dpdk_net_engine_config_t) {
    let _ = cfg.port_id;
    let _ = cfg.recv_buffer_bytes;
    // tcp_zombie_knob is referenced ONLY as a struct-init below, no read.
    let _init = super::api::dpdk_net_engine_config_t {
        port_id: 0,
        recv_buffer_bytes: 0,
        tcp_zombie_knob: 0,
        tcp_pending_removal: false,
        tcp_pending_removal_two: 0,
    };
    let _ = _init;
}
RUST
    # Run the parser.
    parsed=$(parse_struct_fields "$fake_api" "$TARGET_STRUCT")
    # Expected fields: 5 in this order.
    expected_names="port_id recv_buffer_bytes tcp_zombie_knob tcp_pending_removal tcp_pending_removal_two"
    got_names=$(awk -F'\t' '{print $2}' <<<"$parsed" | tr '\n' ' ' | sed 's/ $//')
    if [[ "$got_names" != "$expected_names" ]]; then
        echo "self-test FAIL: parser names mismatch" >&2
        echo "  expected: $expected_names" >&2
        echo "  got     : $got_names" >&2
        exit 1
    fi
    # Expected REMOVE-BY markers.
    got_pending_removal_marker=$(awk -F'\t' '$2=="tcp_pending_removal" {print $3}' <<<"$parsed")
    if [[ "$got_pending_removal_marker" != "A99" ]]; then
        echo "self-test FAIL: expected REMOVE-BY marker 'A99' on tcp_pending_removal, got '$got_pending_removal_marker'" >&2
        exit 1
    fi
    got_pending_removal_two_marker=$(awk -F'\t' '$2=="tcp_pending_removal_two" {print $3}' <<<"$parsed")
    if [[ "$got_pending_removal_two_marker" != "A99.5" ]]; then
        echo "self-test FAIL: expected REMOVE-BY marker 'A99.5' on tcp_pending_removal_two, got '$got_pending_removal_two_marker'" >&2
        exit 1
    fi
    # Reader detection: port_id and recv_buffer_bytes alive; tcp_zombie_knob dead.
    if ! field_has_reader port_id "$tmpdir" "api.rs"; then
        echo "self-test FAIL: port_id should have a reader in user.rs" >&2; exit 1
    fi
    if ! field_has_reader recv_buffer_bytes "$tmpdir" "api.rs"; then
        echo "self-test FAIL: recv_buffer_bytes should have a reader in user.rs" >&2; exit 1
    fi
    if field_has_reader tcp_zombie_knob "$tmpdir" "api.rs"; then
        echo "self-test FAIL: tcp_zombie_knob has no reader (only a struct-init write) but field_has_reader returned alive" >&2
        exit 1
    fi
    # Field NOT in the target struct must not be confused with the target struct
    # (red_herring lives in dpdk_net_other_struct; the parser must skip it).
    if grep -qP '\tred_herring\t' <<<"$parsed"; then
        echo "self-test FAIL: parser leaked red_herring from a different struct" >&2; exit 1
    fi
    echo "self-test OK: parser + reader-detection agree on the synthetic fixture"
    exit 0
fi

# ---------------------------------------------------------------------------
# Real run.
# ---------------------------------------------------------------------------
cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"
API_FILE="$REPO_ROOT/$API_FILE_REL"

command -v rg >/dev/null 2>&1 || die "ripgrep (rg) not on PATH"
[[ -f "$API_FILE" ]] || die "api.rs not found at $API_FILE"

echo "=== $SCRIPT_NAME: C-ABI dead-field audit (Pattern P5) ==="
echo "Target file:   $API_FILE_REL"
echo "Target struct: $TARGET_STRUCT"

parsed=$(parse_struct_fields "$API_FILE" "$TARGET_STRUCT")
[[ -n "$parsed" ]] || die "parser extracted zero fields from $TARGET_STRUCT — has the struct been renamed?"

n_fields=$(wc -l <<<"$parsed" | tr -d ' ')
echo "Fields parsed: $n_fields"
echo ""

dead=()
deferred=()
alive=0
while IFS=$'\t' read -r line_no name marker; do
    # Skip any blank rows from awk edge cases.
    [[ -z "${name:-}" ]] && continue
    if [[ "$marker" != "-" ]]; then
        deferred+=("$name|$line_no|$marker")
        continue
    fi
    if field_has_reader "$name" "$REPO_ROOT" "$API_FILE_REL"; then
        alive=$((alive + 1))
    else
        dead+=("$name|$line_no")
    fi
done <<<"$parsed"

if [[ ${#deferred[@]} -gt 0 ]]; then
    echo "Deferred (REMOVE-BY marker present):"
    for entry in "${deferred[@]}"; do
        IFS='|' read -r n ln m <<<"$entry"
        echo "  - $n  ($API_FILE_REL:$ln, REMOVE-BY $m)"
    done
    echo ""
fi

echo "Alive (have at least one reader): $alive"
echo "Deferred (REMOVE-BY marker):       ${#deferred[@]}"
echo "Dead   (no reader, no marker):     ${#dead[@]}"
echo ""

if [[ ${#dead[@]} -gt 0 ]]; then
    echo "================================================================"
    echo "DEAD-FIELD ACCRETION DETECTED"
    echo "================================================================"
    echo "The following field(s) of \`$TARGET_STRUCT\` are declared on the"
    echo "C ABI but never read on the Rust side. C callers can set them,"
    echo "but the engine ignores the value — a silent ABI lie."
    echo ""
    for entry in "${dead[@]}"; do
        IFS='|' read -r n ln <<<"$entry"
        echo "  * $n  (declared at $API_FILE_REL:$ln)"
    done
    echo ""
    echo "Remediation (pick exactly one per field):"
    echo "  (a) WIRE  — read the field in dpdk_net_engine_create() (or a"
    echo "             callee) and pass it through to the core EngineConfig."
    echo "             This is the right answer for any field that names a"
    echo "             real configuration knob."
    echo "  (b) DELETE — remove the field from $TARGET_STRUCT, regenerate"
    echo "             include/dpdk_net.h via cbindgen, and bump the ABI"
    echo "             version. Right answer for a knob that was added"
    echo "             speculatively and is never going to be wired."
    echo "  (c) DEFER — add a leading doc-comment marker"
    echo "             '// REMOVE-BY: A<task-id>' on the field, identifying"
    echo "             the task that will resolve it. The audit then carries"
    echo "             the field on the deferred list until that task lands."
    echo "             Use this only when (a) and (b) are blocked on"
    echo "             coordination with another in-flight task."
    echo ""
    echo "Pattern P5 / cross-phase-fixes Task A1 enumerates the known"
    echo "fields at master HEAD; Task A6 (this script) is the mechanical"
    echo "guard that prevents the same accretion pattern from recurring."
    fail "${#dead[@]} dead field(s) on $TARGET_STRUCT"
fi

echo "OK: every $TARGET_STRUCT field has a reader or a deferred-removal marker."
exit 0
