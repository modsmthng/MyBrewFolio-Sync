// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::model::IndexEntry;

const INDEX_MAGIC: u32 = 0x5844_4953;
const INDEX_HEADER_SIZE: usize = 32;
const INDEX_ENTRY_SIZE: usize = 128;
const SHOT_MAGIC: u32 = 0x544f_4853;
const MAX_SHOT_SAMPLES: usize = 4096;

#[derive(Debug, Error)]
pub enum BinaryError {
    #[error("history file is truncated")]
    Truncated,
    #[error("history file has an unsupported format")]
    Unsupported,
    #[error("history file uses unsupported format version {0}")]
    UnsupportedVersion(u8),
    #[error("history file contains too many samples")]
    TooManySamples,
}

fn u16_le(bytes: &[u8], offset: usize) -> Result<u16, BinaryError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(BinaryError::Truncated)?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn i16_le(bytes: &[u8], offset: usize) -> Result<i16, BinaryError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(BinaryError::Truncated)?;
    Ok(i16::from_le_bytes([value[0], value[1]]))
}

fn u32_le(bytes: &[u8], offset: usize) -> Result<u32, BinaryError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(BinaryError::Truncated)?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn c_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

pub fn parse_index(bytes: &[u8]) -> Result<Vec<IndexEntry>, BinaryError> {
    if bytes.len() < INDEX_HEADER_SIZE || u32_le(bytes, 0)? != INDEX_MAGIC {
        return Err(BinaryError::Unsupported);
    }
    if u16_le(bytes, 6)? as usize != INDEX_ENTRY_SIZE {
        return Err(BinaryError::Unsupported);
    }
    let count = u32_le(bytes, 8)? as usize;
    let expected = INDEX_HEADER_SIZE
        .checked_add(
            count
                .checked_mul(INDEX_ENTRY_SIZE)
                .ok_or(BinaryError::Truncated)?,
        )
        .ok_or(BinaryError::Truncated)?;
    if expected > bytes.len() {
        return Err(BinaryError::Truncated);
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let base = INDEX_HEADER_SIZE + index * INDEX_ENTRY_SIZE;
        let flags = *bytes.get(base + 15).ok_or(BinaryError::Truncated)?;
        if flags & 0x02 != 0 {
            continue;
        }
        let volume = u16_le(bytes, base + 12)?;
        let rating = *bytes.get(base + 14).ok_or(BinaryError::Truncated)?;
        entries.push(IndexEntry {
            id: u32_le(bytes, base)?,
            timestamp: u32_le(bytes, base + 4)?,
            duration: u32_le(bytes, base + 8)?,
            volume: (volume > 0).then_some(volume as f64 / 10.0),
            rating: (rating > 0).then_some(rating),
            profile_id: c_string(&bytes[base + 16..base + 48]),
            profile_name: c_string(&bytes[base + 48..base + 96]),
            incomplete: flags & 0x01 == 0,
        });
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.timestamp));
    Ok(entries)
}

#[derive(Clone, Copy)]
enum FieldKind {
    Unsigned,
    Signed,
}

struct Field {
    name: &'static str,
    kind: FieldKind,
    scale: f64,
}

fn field(bit: u32) -> Option<Field> {
    Some(match bit {
        0 => Field {
            name: "t",
            kind: FieldKind::Unsigned,
            scale: 0.0,
        },
        1 => Field {
            name: "tt",
            kind: FieldKind::Unsigned,
            scale: 10.0,
        },
        2 => Field {
            name: "ct",
            kind: FieldKind::Unsigned,
            scale: 10.0,
        },
        3 => Field {
            name: "tp",
            kind: FieldKind::Unsigned,
            scale: 10.0,
        },
        4 => Field {
            name: "cp",
            kind: FieldKind::Unsigned,
            scale: 10.0,
        },
        5 => Field {
            name: "fl",
            kind: FieldKind::Signed,
            scale: 100.0,
        },
        6 => Field {
            name: "tf",
            kind: FieldKind::Signed,
            scale: 100.0,
        },
        7 => Field {
            name: "pf",
            kind: FieldKind::Signed,
            scale: 100.0,
        },
        8 => Field {
            name: "vf",
            kind: FieldKind::Signed,
            scale: 100.0,
        },
        9 => Field {
            name: "v",
            kind: FieldKind::Unsigned,
            scale: 10.0,
        },
        10 => Field {
            name: "ev",
            kind: FieldKind::Unsigned,
            scale: 10.0,
        },
        11 => Field {
            name: "pr",
            kind: FieldKind::Unsigned,
            scale: 100.0,
        },
        12 => Field {
            name: "systemInfo",
            kind: FieldKind::Unsigned,
            scale: 0.0,
        },
        _ => return None,
    })
}

fn system_info(raw: u16) -> Value {
    json!({
        "shotStartedVolumetric": raw & 0x0001 != 0,
        "currentlyVolumetric": raw & 0x0002 != 0,
        "bluetoothScaleConnected": raw & 0x0004 != 0,
        "volumetricAvailable": raw & 0x0008 != 0,
        "extendedRecording": raw & 0x0010 != 0
    })
}

fn field_width(version: u8, bit: u32) -> usize {
    if version == 6 && bit == 0 {
        4
    } else {
        2
    }
}

type PhaseTransition = (usize, u8, String);

fn parse_phase_transitions(
    bytes: &[u8],
    version: u8,
) -> Result<(Vec<PhaseTransition>, Vec<Value>), BinaryError> {
    if version < 5 {
        return Ok((Vec::new(), Vec::new()));
    }
    let count = bytes.get(458).copied().unwrap_or(0).min(12);
    let mut transitions = Vec::with_capacity(count as usize);
    let mut details = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let offset = 110 + index * 29;
        let sample_index = u16_le(bytes, offset)? as usize;
        let phase_number = bytes[offset + 2];
        let transition_reason = bytes[offset + 3];
        let phase_name = c_string(&bytes[offset + 4..offset + 29]);
        transitions.push((sample_index, phase_number, phase_name.clone()));
        details.push(json!({
            "sampleIndex": sample_index,
            "phaseNumber": phase_number,
            "transitionReason": transition_reason,
            "phaseName": phase_name
        }));
    }
    Ok((transitions, details))
}

fn field_value(
    bytes: &[u8],
    version: u8,
    bit: u32,
    offset: usize,
    sample_interval: u32,
) -> Result<(String, Value), BinaryError> {
    let known = field(bit);
    let (name, raw, scale) = match known {
        Some(value) => {
            let raw = if version == 6 && bit == 0 {
                u32_le(bytes, offset)? as f64
            } else {
                match value.kind {
                    FieldKind::Signed => i16_le(bytes, offset)? as f64,
                    FieldKind::Unsigned => u16_le(bytes, offset)? as f64,
                }
            };
            (value.name.to_string(), raw, value.scale)
        }
        None => (format!("unknown_{bit}"), u16_le(bytes, offset)? as f64, 0.0),
    };
    let number = if name == "t" && version < 6 {
        raw * sample_interval as f64
    } else if scale > 0.0 {
        raw / scale
    } else {
        raw
    };
    let value = if name == "systemInfo" {
        system_info(number as u16)
    } else {
        json!(number)
    };
    Ok((name, value))
}

fn phase_at(sample_index: usize, transitions: &[PhaseTransition]) -> (u8, String) {
    let mut phase = (0_u8, "Phase 1".to_string());
    for (transition_sample, number, name) in transitions {
        if sample_index < *transition_sample {
            break;
        }
        phase = (*number, name.clone());
    }
    phase
}

fn parse_samples(
    bytes: &[u8],
    header_size: usize,
    sample_size: usize,
    sample_count: usize,
    active_bits: &[u32],
    sample_interval: u32,
    version: u8,
    transitions: &[PhaseTransition],
) -> Result<Vec<Value>, BinaryError> {
    let mut samples = Vec::with_capacity(sample_count);
    for sample_index in 0..sample_count {
        let base = header_size + sample_index * sample_size;
        let mut sample = Map::new();
        let mut offset = base;
        for bit in active_bits {
            let (name, value) = field_value(bytes, version, *bit, offset, sample_interval)?;
            sample.insert(name, value);
            offset += field_width(version, *bit);
        }
        debug_assert_eq!(offset, base + sample_size);
        if version >= 5 {
            let (number, name) = phase_at(sample_index, transitions);
            sample.insert("phaseNumber".into(), json!(number));
            sample.insert("phaseDisplayNumber".into(), json!(number + 1));
            sample.insert("phaseName".into(), json!(name));
        }
        samples.push(Value::Object(sample));
    }
    Ok(samples)
}

pub fn parse_shot(bytes: &[u8], id: u32) -> Result<Value, BinaryError> {
    if bytes.len() < 28 || u32_le(bytes, 0)? != SHOT_MAGIC {
        return Err(BinaryError::Unsupported);
    }
    let version = bytes[4];
    if !(1..=6).contains(&version) {
        return Err(BinaryError::UnsupportedVersion(version));
    }
    let sample_size = bytes[5] as usize;
    let header_size = u16_le(bytes, 6)? as usize;
    let expected_header = if version <= 4 { 128 } else { 512 };
    if header_size != expected_header || bytes.len() < header_size || sample_size == 0 {
        return Err(BinaryError::Unsupported);
    }
    let sample_interval = u16_le(bytes, 8)? as u32;
    let fields_mask = u32_le(bytes, 12)?;
    let sample_count_header = u32_le(bytes, 16)? as usize;
    let duration_header = u32_le(bytes, 20)?;
    let start_epoch = u32_le(bytes, 24)?;
    let profile_id = c_string(bytes.get(28..60).ok_or(BinaryError::Truncated)?);
    let profile_name = c_string(bytes.get(60..108).ok_or(BinaryError::Truncated)?);
    let final_weight = u16_le(bytes, 108)?;
    let active_bits: Vec<u32> = (0..32)
        .filter(|bit| fields_mask & (1 << bit) != 0)
        .collect();
    let expected_sample_size = active_bits
        .iter()
        .map(|bit| field_width(version, *bit))
        .sum::<usize>();
    if expected_sample_size != sample_size {
        return Err(BinaryError::Unsupported);
    }
    let available = (bytes.len() - header_size) / sample_size;
    let sample_count = if sample_count_header == 0 {
        available
    } else {
        sample_count_header.min(available)
    };
    if sample_count > MAX_SHOT_SAMPLES {
        return Err(BinaryError::TooManySamples);
    }

    let (transitions, phase_transitions) = parse_phase_transitions(bytes, version)?;
    let samples = parse_samples(
        bytes,
        header_size,
        sample_size,
        sample_count,
        &active_bits,
        sample_interval,
        version,
        &transitions,
    )?;
    let last_t = samples
        .last()
        .and_then(|sample| sample.get("t"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let incomplete = sample_count_header == 0
        || sample_count_header > available
        || (bytes.len() - header_size) % sample_size != 0;
    let duration = if !incomplete && duration_header > 0 {
        duration_header as f64
    } else {
        last_t
    };
    let sample_volume = samples
        .last()
        .and_then(|sample| sample.get("v"))
        .and_then(Value::as_f64);
    let volume = if final_weight > 0 {
        Some(final_weight as f64 / 10.0)
    } else {
        sample_volume.filter(|value| *value > 0.0)
    };
    let final_exit_reason = (version >= 5).then(|| bytes.get(459).copied().unwrap_or(0));
    let brew_delay_ms = (version >= 5).then(|| u16_le(bytes, 460)).transpose()?;
    Ok(json!({
        "id": id.to_string(), "name": format!("Shot {id}"), "profile": profile_name,
        "profileId": profile_id, "timestamp": start_epoch, "duration": duration,
        "samples": samples, "volume": volume, "incomplete": incomplete,
        "sampleInterval": sample_interval, "fieldsMask": fields_mask,
        "phaseTransitions": phase_transitions,
        "finalExitReason": final_exit_reason,
        "brewDelayMs": brew_delay_ms
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_index_magic() {
        assert!(matches!(
            parse_index(&[0; 32]),
            Err(BinaryError::Unsupported)
        ));
    }

    #[test]
    fn parses_empty_index() {
        let mut bytes = vec![0_u8; 32];
        bytes[0..4].copy_from_slice(&INDEX_MAGIC.to_le_bytes());
        bytes[4..6].copy_from_slice(&1_u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&(INDEX_ENTRY_SIZE as u16).to_le_bytes());
        assert!(parse_index(&bytes).unwrap().is_empty());
    }

    #[test]
    fn index_ignores_deleted_entries_and_sorts_latest_first() {
        let mut bytes = vec![0_u8; INDEX_HEADER_SIZE + INDEX_ENTRY_SIZE * 3];
        bytes[0..4].copy_from_slice(&INDEX_MAGIC.to_le_bytes());
        bytes[6..8].copy_from_slice(&(INDEX_ENTRY_SIZE as u16).to_le_bytes());
        bytes[8..12].copy_from_slice(&3_u32.to_le_bytes());
        for (offset, id, timestamp, flags) in [
            (INDEX_HEADER_SIZE, 1_u32, 100_u32, 0_u8),
            (
                INDEX_HEADER_SIZE + INDEX_ENTRY_SIZE,
                2_u32,
                300_u32,
                0x02_u8,
            ),
            (
                INDEX_HEADER_SIZE + INDEX_ENTRY_SIZE * 2,
                3_u32,
                200_u32,
                0x01_u8,
            ),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&id.to_le_bytes());
            bytes[offset + 4..offset + 8].copy_from_slice(&timestamp.to_le_bytes());
            bytes[offset + 8..offset + 12].copy_from_slice(&30_u32.to_le_bytes());
            bytes[offset + 12..offset + 14].copy_from_slice(&185_u16.to_le_bytes());
            bytes[offset + 14] = 4;
            bytes[offset + 15] = flags;
            bytes[offset + 16..offset + 24].copy_from_slice(b"profile\0");
            bytes[offset + 48..offset + 53].copy_from_slice(b"Test\0");
        }

        let entries = parse_index(&bytes).expect("index is valid");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 3);
        assert_eq!(entries[0].volume, Some(18.5));
        assert_eq!(entries[0].rating, Some(4));
        assert!(!entries[0].incomplete);
        assert_eq!(entries[1].id, 1);
        assert!(entries[1].incomplete);
    }

    #[test]
    fn index_rejects_truncated_entries_and_unsupported_entry_sizes() {
        let mut truncated = vec![0_u8; INDEX_HEADER_SIZE];
        truncated[0..4].copy_from_slice(&INDEX_MAGIC.to_le_bytes());
        truncated[6..8].copy_from_slice(&(INDEX_ENTRY_SIZE as u16).to_le_bytes());
        truncated[8..12].copy_from_slice(&1_u32.to_le_bytes());
        assert!(matches!(
            parse_index(&truncated),
            Err(BinaryError::Truncated)
        ));

        truncated[6..8].copy_from_slice(&64_u16.to_le_bytes());
        assert!(matches!(
            parse_index(&truncated),
            Err(BinaryError::Unsupported)
        ));
    }

    #[test]
    fn parses_version_five_shot_samples_and_system_flags() {
        let mut bytes = vec![0_u8; 512 + 26];
        bytes[0..4].copy_from_slice(&SHOT_MAGIC.to_le_bytes());
        bytes[4] = 5;
        bytes[5] = 26;
        bytes[6..8].copy_from_slice(&512_u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&250_u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&0x1fff_u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&1_u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&250_u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&1_735_689_600_u32.to_le_bytes());
        bytes[28..40].copy_from_slice(b"profile-one\0");
        bytes[60..73].copy_from_slice(b"Test profile\0");
        bytes[108..110].copy_from_slice(&185_u16.to_le_bytes());
        bytes[110..112].copy_from_slice(&0_u16.to_le_bytes());
        bytes[112] = 0;
        bytes[114..120].copy_from_slice(b"Bloom\0");
        bytes[458] = 1;
        bytes[459] = 5;
        bytes[460..462].copy_from_slice(&750_u16.to_le_bytes());

        let sample = 512;
        let values = [
            0_u16, 930, 925, 90, 88, 220, 200, 180, 175, 25, 24, 410, 0x001d,
        ];
        for (index, value) in values.iter().enumerate() {
            let offset = sample + index * 2;
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }

        let parsed = parse_shot(&bytes, 42).unwrap();
        assert_eq!(parsed["id"], "42");
        assert_eq!(parsed["volume"], 18.5);
        assert_eq!(parsed["samples"][0]["ct"], 92.5);
        assert_eq!(
            parsed["samples"][0]["systemInfo"]["shotStartedVolumetric"],
            true
        );
        assert_eq!(
            parsed["samples"][0]["systemInfo"]["bluetoothScaleConnected"],
            true
        );
        assert_eq!(parsed["phaseTransitions"][0]["phaseName"], "Bloom");
        assert_eq!(parsed["finalExitReason"], 5);
        assert_eq!(parsed["brewDelayMs"], 750);
    }

    #[test]
    fn parses_version_six_shot_samples_with_elapsed_milliseconds() {
        let mut bytes = vec![0_u8; 512 + 2 * 28];
        bytes[0..4].copy_from_slice(&SHOT_MAGIC.to_le_bytes());
        bytes[4] = 6;
        bytes[5] = 28;
        bytes[6..8].copy_from_slice(&512_u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&250_u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&0x1fff_u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&2_u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&587_u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&1_735_689_600_u32.to_le_bytes());
        bytes[28..40].copy_from_slice(b"profile-one\0");
        bytes[60..73].copy_from_slice(b"Test profile\0");
        bytes[108..110].copy_from_slice(&185_u16.to_le_bytes());
        bytes[110..112].copy_from_slice(&0_u16.to_le_bytes());
        bytes[112] = 0;
        bytes[114..120].copy_from_slice(b"Bloom\0");
        bytes[458] = 1;
        bytes[459] = 5;
        bytes[460..462].copy_from_slice(&750_u16.to_le_bytes());

        let samples = [
            (
                0_u32,
                [
                    930_i16, 925, 90, 88, 220, 200, -180, 175, 25, 24, 410, 0x001d,
                ],
            ),
            (
                287_u32,
                [
                    930_i16, 931, 90, 88, 225, 200, -175, 180, 30, 28, 420, 0x001f,
                ],
            ),
        ];
        for (sample_index, (elapsed_ms, values)) in samples.iter().enumerate() {
            let base = 512 + sample_index * 28;
            bytes[base..base + 4].copy_from_slice(&elapsed_ms.to_le_bytes());
            for (field_index, value) in values.iter().enumerate() {
                let offset = base + 4 + field_index * 2;
                bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
            }
        }

        let parsed = parse_shot(&bytes, 42).expect("v6 shot parses");
        assert_eq!(parsed["duration"], 587.0);
        assert_eq!(parsed["samples"][1]["t"], 287.0);
        assert_eq!(parsed["samples"][1]["ct"], 93.1);
        assert_eq!(parsed["samples"][1]["pf"], -1.75);
        assert_eq!(parsed["phaseTransitions"][0]["phaseName"], "Bloom");
        assert_eq!(parsed["finalExitReason"], 5);
        assert_eq!(parsed["brewDelayMs"], 750);

        let mut incomplete = bytes.clone();
        incomplete[16..20].copy_from_slice(&0_u32.to_le_bytes());
        incomplete[20..24].copy_from_slice(&999_u32.to_le_bytes());
        let incomplete = parse_shot(&incomplete, 42).expect("incomplete v6 shot parses");
        assert_eq!(incomplete["duration"], 287.0);
        assert_eq!(incomplete["incomplete"], true);

        let mut wrong_width = bytes.clone();
        wrong_width[5] = 26;
        assert!(matches!(
            parse_shot(&wrong_width, 42),
            Err(BinaryError::Unsupported)
        ));

        let mut unknown_field = vec![0_u8; 512 + 6];
        unknown_field[0..4].copy_from_slice(&SHOT_MAGIC.to_le_bytes());
        unknown_field[4] = 6;
        unknown_field[5] = 6;
        unknown_field[6..8].copy_from_slice(&512_u16.to_le_bytes());
        unknown_field[12..16].copy_from_slice(&(1_u32 | (1 << 13)).to_le_bytes());
        unknown_field[16..20].copy_from_slice(&1_u32.to_le_bytes());
        unknown_field[512..516].copy_from_slice(&333_u32.to_le_bytes());
        unknown_field[516..518].copy_from_slice(&123_u16.to_le_bytes());
        let unknown_field = parse_shot(&unknown_field, 42).expect("v6 unknown field parses");
        assert_eq!(unknown_field["samples"][0]["t"], 333.0);
        assert_eq!(unknown_field["samples"][0]["unknown_13"], 123.0);
    }

    #[test]
    fn old_shots_use_the_last_sample_for_incomplete_duration_and_volume() {
        let mut bytes = vec![0_u8; 128 + 8];
        bytes[0..4].copy_from_slice(&SHOT_MAGIC.to_le_bytes());
        bytes[4] = 4;
        bytes[5] = 4;
        bytes[6..8].copy_from_slice(&128_u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&100_u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&(1_u32 | (1 << 9)).to_le_bytes());
        // A zero header count means the machine was still recording.
        bytes[20..24].copy_from_slice(&999_u32.to_le_bytes());
        bytes[28..40].copy_from_slice(b"profile-one\0");
        bytes[60..73].copy_from_slice(b"Test profile\0");
        bytes[128..130].copy_from_slice(&5_u16.to_le_bytes());
        bytes[130..132].copy_from_slice(&230_u16.to_le_bytes());
        bytes[132..134].copy_from_slice(&6_u16.to_le_bytes());
        bytes[134..136].copy_from_slice(&250_u16.to_le_bytes());

        let parsed = parse_shot(&bytes, 7).expect("old shot parses");

        assert_eq!(parsed["duration"], 600.0);
        assert_eq!(parsed["volume"], 25.0);
        assert_eq!(parsed["incomplete"], true);
        assert_eq!(parsed["phaseTransitions"], serde_json::json!([]));
        assert_eq!(parsed["finalExitReason"], serde_json::Value::Null);
    }

    #[test]
    fn shot_rejects_inconsistent_or_excessive_sample_data() {
        let mut inconsistent = vec![0_u8; 128];
        inconsistent[0..4].copy_from_slice(&SHOT_MAGIC.to_le_bytes());
        inconsistent[4] = 4;
        inconsistent[5] = 2;
        inconsistent[6..8].copy_from_slice(&128_u16.to_le_bytes());
        inconsistent[12..16].copy_from_slice(&3_u32.to_le_bytes());
        assert!(matches!(
            parse_shot(&inconsistent, 1),
            Err(BinaryError::Unsupported)
        ));

        let sample_count = MAX_SHOT_SAMPLES as u32 + 1;
        let mut excessive = vec![0_u8; 512 + sample_count as usize * 4];
        excessive[0..4].copy_from_slice(&SHOT_MAGIC.to_le_bytes());
        excessive[4] = 6;
        excessive[5] = 4;
        excessive[6..8].copy_from_slice(&512_u16.to_le_bytes());
        excessive[12..16].copy_from_slice(&1_u32.to_le_bytes());
        excessive[16..20].copy_from_slice(&sample_count.to_le_bytes());
        assert!(matches!(
            parse_shot(&excessive, 1),
            Err(BinaryError::TooManySamples)
        ));

        let mut future = vec![0_u8; 512];
        future[0..4].copy_from_slice(&SHOT_MAGIC.to_le_bytes());
        future[4] = 7;
        assert!(matches!(
            parse_shot(&future, 1),
            Err(BinaryError::UnsupportedVersion(7))
        ));
    }
}
