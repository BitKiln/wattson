//! The spec and the code must not drift apart.
//!
//! `protocol/spec/frames.md` is normative, and a phase-2 C firmware implementation will be
//! written from it rather than from this crate. If someone adds a frame type to the enum and
//! forgets the spec — or renumbers one in the spec and forgets the enum — a third-party
//! implementation silently disagrees with ours. That is expensive to debug on a device with
//! no debugger attached.
//!
//! So: parse the frame-type table out of the markdown and assert it matches `FrameType`.
//! Twenty lines, bought forever.

use wattson_protocol::FrameType;

const SPEC: &str = include_str!("../../spec/frames.md");

/// Rows of the `## 2. Frame types` table, as `(code, name)`.
fn spec_frame_types() -> Vec<(u8, String)> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in SPEC.lines() {
        let t = line.trim();
        if t.starts_with("| Code | Name |") {
            in_table = true;
            continue;
        }
        if in_table {
            if !t.starts_with('|') {
                break;
            }
            let cells: Vec<&str> = t.trim_matches('|').split('|').map(str::trim).collect();
            if cells.len() < 2 || cells[0].starts_with("---") {
                continue;
            }
            let Some(hex) = cells[0].strip_prefix("0x") else {
                continue;
            };
            let code = u8::from_str_radix(hex, 16)
                .unwrap_or_else(|_| panic!("frame table has a non-hex code: {:?}", cells[0]));
            rows.push((code, cells[1].to_string()));
        }
    }
    assert!(
        !rows.is_empty(),
        "could not find the frame-type table in frames.md"
    );
    rows
}

#[test]
fn spec_table_matches_frame_type_enum() {
    let spec: Vec<(u8, String)> = spec_frame_types();
    let code: Vec<(u8, String)> = FrameType::ALL
        .iter()
        .map(|t| (t.as_u8(), t.name().to_string()))
        .collect();

    assert_eq!(
        spec, code,
        "\nframes.md and FrameType disagree.\n  spec: {spec:?}\n  code: {code:?}\n\
         Edit both or neither."
    );
}

#[test]
fn spec_documents_the_protocol_version_the_code_reports() {
    let want = format!("0x{:04X}", wattson_protocol::PROTOCOL_VERSION);
    assert!(
        SPEC.contains(&want),
        "frames.md must state the protocol version the code reports ({want})"
    );
}

#[test]
fn spec_states_the_crc_check_value() {
    assert!(
        SPEC.contains("0xCBF4_3926"),
        "frames.md must state the CRC-32/ISO-HDLC check value, so an independent \
         implementation can verify its own CRC before anything else"
    );
}

/// The reserved ranges in the spec must actually decode as unknown, not as a real type.
#[test]
fn reserved_ranges_are_not_claimed_by_the_enum() {
    for b in [0x04u8, 0x15, 0x40, 0x50, 0x6F, 0x70, 0x7E] {
        assert!(
            FrameType::from_u8(b).is_none(),
            "0x{b:02X} is reserved in frames.md but claimed by FrameType"
        );
    }
}
