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

pub fn parse_shot(bytes: &[u8], id: u32) -> Result<Value, BinaryError> {
    if bytes.len() < 28 || u32_le(bytes, 0)? != SHOT_MAGIC {
        return Err(BinaryError::Unsupported);
    }
    let version = bytes[4];
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
    if active_bits.len() * 2 != sample_size {
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

    let mut transitions = Vec::new();
    let mut phase_transitions = Vec::new();
    if version >= 5 {
        let count = bytes.get(458).copied().unwrap_or(0).min(12);
        for index in 0..count as usize {
            let offset = 110 + index * 29;
            let sample_index = u16_le(bytes, offset)? as usize;
            let phase_number = bytes[offset + 2];
            let transition_reason = bytes[offset + 3];
            let phase_name = c_string(&bytes[offset + 4..offset + 29]);
            transitions.push((sample_index, phase_number, phase_name.clone()));
            phase_transitions.push(json!({
                "sampleIndex": sample_index,
                "phaseNumber": phase_number,
                "transitionReason": transition_reason,
                "phaseName": phase_name
            }));
        }
    }

    let mut samples = Vec::with_capacity(sample_count);
    for sample_index in 0..sample_count {
        let base = header_size + sample_index * sample_size;
        let mut sample = Map::new();
        for (field_index, bit) in active_bits.iter().enumerate() {
            let offset = base + field_index * 2;
            let known = field(*bit);
            let (name, raw, scale) = match known {
                Some(value) => {
                    let raw = match value.kind {
                        FieldKind::Signed => i16_le(bytes, offset)? as f64,
                        FieldKind::Unsigned => u16_le(bytes, offset)? as f64,
                    };
                    (value.name.to_string(), raw, value.scale)
                }
                None => (format!("unknown_{bit}"), u16_le(bytes, offset)? as f64, 0.0),
            };
            let number = if name == "t" {
                raw * sample_interval as f64
            } else if scale > 0.0 {
                raw / scale
            } else {
                raw
            };
            if name == "systemInfo" {
                sample.insert(name, system_info(number as u16));
            } else {
                sample.insert(name, json!(number));
            }
        }
        if version >= 5 {
            let mut phase_number = 0_u8;
            let mut phase_name = "Phase 1".to_string();
            for (transition_sample, transition_phase, transition_name) in &transitions {
                if sample_index < *transition_sample {
                    break;
                }
                phase_number = *transition_phase;
                phase_name = transition_name.clone();
            }
            sample.insert("phaseNumber".into(), json!(phase_number));
            sample.insert("phaseDisplayNumber".into(), json!(phase_number + 1));
            sample.insert("phaseName".into(), json!(phase_name));
        }
        samples.push(Value::Object(sample));
    }
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
}
