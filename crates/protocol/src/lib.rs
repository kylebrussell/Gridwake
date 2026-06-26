use std::fmt;

use gridwake_core::{ClientId, EntityId, SnapshotId, Tick};
use gridwake_snapshot::{DeltaOp, DeltaSnapshot};

pub const PROTOCOL_MAGIC: [u8; 2] = *b"GW";
pub const PROTOCOL_VERSION: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMessage {
    AckSnapshot { sequence: SnapshotId },
    Input { payload: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerMessage {
    SnapshotDelta(DeltaSnapshot),
    SnapshotFragment(SnapshotFragment),
    Metrics(MetricsFrame),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFragment {
    pub sequence: SnapshotId,
    pub baseline: Option<SnapshotId>,
    pub fragment_index: u16,
    pub fragment_count: u16,
    pub ops: Vec<DeltaOp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetricsFrame {
    pub tick: Tick,
    pub clients: usize,
    pub entities: usize,
    pub aoi_candidates: usize,
    pub selected_updates: usize,
    pub selected_full_lod_updates: usize,
    pub selected_reduced_lod_updates: usize,
    pub selected_minimal_lod_updates: usize,
    pub deferred_updates: usize,
    pub bytes_scheduled: usize,
    pub deferred_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedClientMessage {
    pub client: ClientId,
    pub message: ClientMessage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeConfig {
    pub max_payload_len: usize,
    pub max_delta_ops: usize,
}

impl Default for DecodeConfig {
    fn default() -> Self {
        Self {
            max_payload_len: 1024 * 1024,
            max_delta_ops: 65_536,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    InvalidMagic,
    UnsupportedVersion(u8),
    UnknownMessageTag(u8),
    UnknownDeltaOpTag(u8),
    UnexpectedEof,
    TrailingBytes(usize),
    PayloadTooLarge { len: usize, max: usize },
    TooManyDeltaOps { len: usize, max: usize },
    LengthOverflow { value: usize },
    CountOverflow { value: usize },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic => write!(f, "invalid Gridwake protocol magic"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported Gridwake protocol version {version}")
            }
            Self::UnknownMessageTag(tag) => write!(f, "unknown message tag {tag}"),
            Self::UnknownDeltaOpTag(tag) => write!(f, "unknown delta op tag {tag}"),
            Self::UnexpectedEof => write!(f, "unexpected end of protocol buffer"),
            Self::TrailingBytes(bytes) => write!(f, "{bytes} trailing protocol bytes"),
            Self::PayloadTooLarge { len, max } => {
                write!(f, "payload length {len} exceeds configured maximum {max}")
            }
            Self::TooManyDeltaOps { len, max } => {
                write!(f, "delta op count {len} exceeds configured maximum {max}")
            }
            Self::LengthOverflow { value } => {
                write!(f, "length {value} cannot be represented on the wire")
            }
            Self::CountOverflow { value } => {
                write!(f, "count {value} cannot be represented on the wire")
            }
        }
    }
}

impl std::error::Error for CodecError {}

const TAG_CLIENT_ACK_SNAPSHOT: u8 = 0x01;
const TAG_CLIENT_INPUT: u8 = 0x02;
const TAG_SERVER_SNAPSHOT_DELTA: u8 = 0x81;
const TAG_SERVER_METRICS: u8 = 0x82;
const TAG_SERVER_SNAPSHOT_FRAGMENT: u8 = 0x83;

const OP_SPAWN_OR_ENTER: u8 = 0x01;
const OP_UPDATE: u8 = 0x02;
const OP_DESPAWN_OR_EXIT: u8 = 0x03;

pub fn encode_client_message(message: &ClientMessage) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::with_capacity(encoded_client_message_len(message)?);
    write_header(
        &mut out,
        match message {
            ClientMessage::AckSnapshot { .. } => TAG_CLIENT_ACK_SNAPSHOT,
            ClientMessage::Input { .. } => TAG_CLIENT_INPUT,
        },
    );

    match message {
        ClientMessage::AckSnapshot { sequence } => write_u64(&mut out, sequence.raw()),
        ClientMessage::Input { payload } => write_bytes(&mut out, payload)?,
    }

    Ok(out)
}

pub fn decode_client_message(bytes: &[u8]) -> Result<ClientMessage, CodecError> {
    decode_client_message_with_config(bytes, DecodeConfig::default())
}

pub fn decode_client_message_with_config(
    bytes: &[u8],
    config: DecodeConfig,
) -> Result<ClientMessage, CodecError> {
    let mut reader = Reader::new(bytes);
    let tag = reader.read_header()?;
    let message = match tag {
        TAG_CLIENT_ACK_SNAPSHOT => ClientMessage::AckSnapshot {
            sequence: SnapshotId::new(reader.read_u64()?),
        },
        TAG_CLIENT_INPUT => ClientMessage::Input {
            payload: reader.read_bytes(config.max_payload_len)?,
        },
        _ => return Err(CodecError::UnknownMessageTag(tag)),
    };
    reader.finish()?;
    Ok(message)
}

pub fn encode_server_message(message: &ServerMessage) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::with_capacity(encoded_server_message_len(message)?);
    write_header(
        &mut out,
        match message {
            ServerMessage::SnapshotDelta(_) => TAG_SERVER_SNAPSHOT_DELTA,
            ServerMessage::SnapshotFragment(_) => TAG_SERVER_SNAPSHOT_FRAGMENT,
            ServerMessage::Metrics(_) => TAG_SERVER_METRICS,
        },
    );

    match message {
        ServerMessage::SnapshotDelta(delta) => write_delta_snapshot(&mut out, delta)?,
        ServerMessage::SnapshotFragment(fragment) => write_snapshot_fragment(&mut out, fragment)?,
        ServerMessage::Metrics(metrics) => write_metrics(&mut out, metrics),
    }

    Ok(out)
}

pub fn encoded_client_message_len(message: &ClientMessage) -> Result<usize, CodecError> {
    let body_len = match message {
        ClientMessage::AckSnapshot { .. } => U64_BYTES,
        ClientMessage::Input { payload } => encoded_bytes_len(payload)?,
    };
    Ok(HEADER_BYTES + body_len)
}

pub fn encoded_server_message_len(message: &ServerMessage) -> Result<usize, CodecError> {
    let body_len = match message {
        ServerMessage::SnapshotDelta(delta) => encoded_delta_snapshot_len(delta)?,
        ServerMessage::SnapshotFragment(fragment) => encoded_snapshot_fragment_len(fragment)?,
        ServerMessage::Metrics(_) => METRICS_BODY_BYTES,
    };
    Ok(HEADER_BYTES + body_len)
}

pub fn decode_server_message(bytes: &[u8]) -> Result<ServerMessage, CodecError> {
    decode_server_message_with_config(bytes, DecodeConfig::default())
}

pub fn decode_server_message_with_config(
    bytes: &[u8],
    config: DecodeConfig,
) -> Result<ServerMessage, CodecError> {
    let mut reader = Reader::new(bytes);
    let tag = reader.read_header()?;
    let message = match tag {
        TAG_SERVER_SNAPSHOT_DELTA => {
            ServerMessage::SnapshotDelta(reader.read_delta_snapshot(config)?)
        }
        TAG_SERVER_SNAPSHOT_FRAGMENT => {
            ServerMessage::SnapshotFragment(reader.read_snapshot_fragment(config)?)
        }
        TAG_SERVER_METRICS => ServerMessage::Metrics(reader.read_metrics()?),
        _ => return Err(CodecError::UnknownMessageTag(tag)),
    };
    reader.finish()?;
    Ok(message)
}

fn write_header(out: &mut Vec<u8>, tag: u8) {
    out.extend_from_slice(&PROTOCOL_MAGIC);
    out.push(PROTOCOL_VERSION);
    out.push(tag);
}

const HEADER_BYTES: usize = PROTOCOL_MAGIC.len() + 2;
const U16_BYTES: usize = 2;
const U32_BYTES: usize = 4;
const U64_BYTES: usize = 8;
const METRICS_BODY_BYTES: usize = U64_BYTES * 11;

fn encoded_delta_snapshot_len(delta: &DeltaSnapshot) -> Result<usize, CodecError> {
    checked_count(delta.ops.len())?;
    let mut len = U64_BYTES + 1 + U32_BYTES;
    if delta.baseline.is_some() {
        len += U64_BYTES;
    }

    for op in &delta.ops {
        len += match op {
            DeltaOp::SpawnOrEnter { payload, .. } | DeltaOp::Update { payload, .. } => {
                1 + U64_BYTES + encoded_bytes_len(payload)?
            }
            DeltaOp::DespawnOrExit { .. } => 1 + U64_BYTES,
        };
    }

    Ok(len)
}

fn encoded_snapshot_fragment_len(fragment: &SnapshotFragment) -> Result<usize, CodecError> {
    checked_count(fragment.ops.len())?;
    let mut len = U64_BYTES + 1 + U16_BYTES + U16_BYTES + U32_BYTES;
    if fragment.baseline.is_some() {
        len += U64_BYTES;
    }

    for op in &fragment.ops {
        len += encoded_delta_op_len(op)?;
    }

    Ok(len)
}

fn encoded_delta_op_len(op: &DeltaOp) -> Result<usize, CodecError> {
    match op {
        DeltaOp::SpawnOrEnter { payload, .. } | DeltaOp::Update { payload, .. } => {
            Ok(1 + U64_BYTES + encoded_bytes_len(payload)?)
        }
        DeltaOp::DespawnOrExit { .. } => Ok(1 + U64_BYTES),
    }
}

fn encoded_bytes_len(bytes: &[u8]) -> Result<usize, CodecError> {
    checked_len(bytes.len())?;
    Ok(U32_BYTES + bytes.len())
}

fn write_delta_snapshot(out: &mut Vec<u8>, delta: &DeltaSnapshot) -> Result<(), CodecError> {
    write_u64(out, delta.sequence.raw());
    match delta.baseline {
        Some(baseline) => {
            out.push(1);
            write_u64(out, baseline.raw());
        }
        None => out.push(0),
    }

    write_u32(out, checked_count(delta.ops.len())?);
    for op in &delta.ops {
        match op {
            DeltaOp::SpawnOrEnter { entity, payload } => {
                out.push(OP_SPAWN_OR_ENTER);
                write_u64(out, entity.raw());
                write_bytes(out, payload)?;
            }
            DeltaOp::Update { entity, payload } => {
                out.push(OP_UPDATE);
                write_u64(out, entity.raw());
                write_bytes(out, payload)?;
            }
            DeltaOp::DespawnOrExit { entity } => {
                out.push(OP_DESPAWN_OR_EXIT);
                write_u64(out, entity.raw());
            }
        }
    }

    Ok(())
}

fn write_snapshot_fragment(
    out: &mut Vec<u8>,
    fragment: &SnapshotFragment,
) -> Result<(), CodecError> {
    write_u64(out, fragment.sequence.raw());
    match fragment.baseline {
        Some(baseline) => {
            out.push(1);
            write_u64(out, baseline.raw());
        }
        None => out.push(0),
    }
    write_u16(out, fragment.fragment_index);
    write_u16(out, fragment.fragment_count);
    write_u32(out, checked_count(fragment.ops.len())?);
    for op in &fragment.ops {
        write_delta_op(out, op)?;
    }

    Ok(())
}

fn write_metrics(out: &mut Vec<u8>, metrics: &MetricsFrame) {
    write_u64(out, metrics.tick.raw());
    write_u64(out, metrics.clients as u64);
    write_u64(out, metrics.entities as u64);
    write_u64(out, metrics.aoi_candidates as u64);
    write_u64(out, metrics.selected_updates as u64);
    write_u64(out, metrics.selected_full_lod_updates as u64);
    write_u64(out, metrics.selected_reduced_lod_updates as u64);
    write_u64(out, metrics.selected_minimal_lod_updates as u64);
    write_u64(out, metrics.deferred_updates as u64);
    write_u64(out, metrics.bytes_scheduled as u64);
    write_u64(out, metrics.deferred_bytes as u64);
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CodecError> {
    write_u32(out, checked_len(bytes.len())?);
    out.extend_from_slice(bytes);
    Ok(())
}

fn write_delta_op(out: &mut Vec<u8>, op: &DeltaOp) -> Result<(), CodecError> {
    match op {
        DeltaOp::SpawnOrEnter { entity, payload } => {
            out.push(OP_SPAWN_OR_ENTER);
            write_u64(out, entity.raw());
            write_bytes(out, payload)?;
        }
        DeltaOp::Update { entity, payload } => {
            out.push(OP_UPDATE);
            write_u64(out, entity.raw());
            write_bytes(out, payload)?;
        }
        DeltaOp::DespawnOrExit { entity } => {
            out.push(OP_DESPAWN_OR_EXIT);
            write_u64(out, entity.raw());
        }
    }
    Ok(())
}

fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn checked_len(value: usize) -> Result<u32, CodecError> {
    value
        .try_into()
        .map_err(|_| CodecError::LengthOverflow { value })
}

fn checked_count(value: usize) -> Result<u32, CodecError> {
    value
        .try_into()
        .map_err(|_| CodecError::CountOverflow { value })
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_header(&mut self) -> Result<u8, CodecError> {
        let magic = self.read_exact(PROTOCOL_MAGIC.len())?;
        if magic != PROTOCOL_MAGIC {
            return Err(CodecError::InvalidMagic);
        }

        let version = self.read_u8()?;
        if version != PROTOCOL_VERSION {
            return Err(CodecError::UnsupportedVersion(version));
        }

        self.read_u8()
    }

    fn read_delta_snapshot(&mut self, config: DecodeConfig) -> Result<DeltaSnapshot, CodecError> {
        let sequence = SnapshotId::new(self.read_u64()?);
        let baseline = match self.read_u8()? {
            0 => None,
            1 => Some(SnapshotId::new(self.read_u64()?)),
            tag => return Err(CodecError::UnknownDeltaOpTag(tag)),
        };
        let op_count = self.read_u32()? as usize;
        let ops = self.read_delta_ops(op_count, config)?;

        Ok(DeltaSnapshot {
            sequence,
            baseline,
            ops,
        })
    }

    fn read_snapshot_fragment(
        &mut self,
        config: DecodeConfig,
    ) -> Result<SnapshotFragment, CodecError> {
        let sequence = SnapshotId::new(self.read_u64()?);
        let baseline = match self.read_u8()? {
            0 => None,
            1 => Some(SnapshotId::new(self.read_u64()?)),
            tag => return Err(CodecError::UnknownDeltaOpTag(tag)),
        };
        let fragment_index = self.read_u16()?;
        let fragment_count = self.read_u16()?;
        let op_count = self.read_u32()? as usize;
        let ops = self.read_delta_ops(op_count, config)?;

        Ok(SnapshotFragment {
            sequence,
            baseline,
            fragment_index,
            fragment_count,
            ops,
        })
    }

    fn read_delta_ops(
        &mut self,
        op_count: usize,
        config: DecodeConfig,
    ) -> Result<Vec<DeltaOp>, CodecError> {
        if op_count > config.max_delta_ops {
            return Err(CodecError::TooManyDeltaOps {
                len: op_count,
                max: config.max_delta_ops,
            });
        }

        let mut ops = Vec::with_capacity(op_count);
        for _ in 0..op_count {
            let tag = self.read_u8()?;
            let entity = EntityId::new(self.read_u64()?);
            let op = match tag {
                OP_SPAWN_OR_ENTER => DeltaOp::SpawnOrEnter {
                    entity,
                    payload: self.read_bytes(config.max_payload_len)?,
                },
                OP_UPDATE => DeltaOp::Update {
                    entity,
                    payload: self.read_bytes(config.max_payload_len)?,
                },
                OP_DESPAWN_OR_EXIT => DeltaOp::DespawnOrExit { entity },
                _ => return Err(CodecError::UnknownDeltaOpTag(tag)),
            };
            ops.push(op);
        }

        Ok(ops)
    }

    fn read_metrics(&mut self) -> Result<MetricsFrame, CodecError> {
        Ok(MetricsFrame {
            tick: Tick::new(self.read_u64()?),
            clients: self.read_u64()? as usize,
            entities: self.read_u64()? as usize,
            aoi_candidates: self.read_u64()? as usize,
            selected_updates: self.read_u64()? as usize,
            selected_full_lod_updates: self.read_u64()? as usize,
            selected_reduced_lod_updates: self.read_u64()? as usize,
            selected_minimal_lod_updates: self.read_u64()? as usize,
            deferred_updates: self.read_u64()? as usize,
            bytes_scheduled: self.read_u64()? as usize,
            deferred_bytes: self.read_u64()? as usize,
        })
    }

    fn read_bytes(&mut self, max_len: usize) -> Result<Vec<u8>, CodecError> {
        let len = self.read_u32()? as usize;
        if len > max_len {
            return Err(CodecError::PayloadTooLarge { len, max: max_len });
        }
        Ok(self.read_exact(len)?.to_vec())
    }

    fn read_u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, CodecError> {
        let mut bytes = [0; 2];
        bytes.copy_from_slice(self.read_exact(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32, CodecError> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64, CodecError> {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], CodecError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(CodecError::UnexpectedEof)?;
        if end > self.bytes.len() {
            return Err(CodecError::UnexpectedEof);
        }
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn finish(self) -> Result<(), CodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(CodecError::TrailingBytes(self.bytes.len() - self.offset))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ack_round_trips() {
        let message = ClientMessage::AckSnapshot {
            sequence: SnapshotId::new(42),
        };

        let encoded = encode_client_message(&message).unwrap();

        assert_eq!(decode_client_message(&encoded).unwrap(), message);
    }

    #[test]
    fn client_input_round_trips() {
        let message = ClientMessage::Input {
            payload: b"move:north".to_vec(),
        };

        let encoded = encode_client_message(&message).unwrap();

        assert_eq!(decode_client_message(&encoded).unwrap(), message);
    }

    #[test]
    fn server_snapshot_delta_round_trips() {
        let message = ServerMessage::SnapshotDelta(DeltaSnapshot {
            sequence: SnapshotId::new(10),
            baseline: Some(SnapshotId::new(7)),
            ops: vec![
                DeltaOp::SpawnOrEnter {
                    entity: EntityId::new(1),
                    payload: b"spawn".to_vec(),
                },
                DeltaOp::Update {
                    entity: EntityId::new(2),
                    payload: b"update".to_vec(),
                },
                DeltaOp::DespawnOrExit {
                    entity: EntityId::new(3),
                },
            ],
        });

        let encoded = encode_server_message(&message).unwrap();

        assert_eq!(decode_server_message(&encoded).unwrap(), message);
    }

    #[test]
    fn server_snapshot_fragment_round_trips() {
        let message = ServerMessage::SnapshotFragment(SnapshotFragment {
            sequence: SnapshotId::new(10),
            baseline: Some(SnapshotId::new(7)),
            fragment_index: 1,
            fragment_count: 3,
            ops: vec![
                DeltaOp::SpawnOrEnter {
                    entity: EntityId::new(1),
                    payload: b"spawn".to_vec(),
                },
                DeltaOp::Update {
                    entity: EntityId::new(2),
                    payload: b"update".to_vec(),
                },
                DeltaOp::DespawnOrExit {
                    entity: EntityId::new(3),
                },
            ],
        });

        let encoded = encode_server_message(&message).unwrap();

        assert_eq!(encoded_server_message_len(&message).unwrap(), encoded.len());
        assert_eq!(decode_server_message(&encoded).unwrap(), message);
    }

    #[test]
    fn encoded_server_snapshot_len_matches_wire_bytes() {
        let message = ServerMessage::SnapshotDelta(DeltaSnapshot {
            sequence: SnapshotId::new(10),
            baseline: None,
            ops: vec![
                DeltaOp::SpawnOrEnter {
                    entity: EntityId::new(1),
                    payload: vec![0; 24],
                },
                DeltaOp::Update {
                    entity: EntityId::new(2),
                    payload: vec![0; 36],
                },
                DeltaOp::DespawnOrExit {
                    entity: EntityId::new(3),
                },
            ],
        });

        let encoded = encode_server_message(&message).unwrap();

        assert_eq!(encoded_server_message_len(&message).unwrap(), encoded.len());
        assert_eq!(
            encoded.len(),
            HEADER_BYTES
                + U64_BYTES
                + 1
                + U32_BYTES
                + 1
                + U64_BYTES
                + U32_BYTES
                + 24
                + 1
                + U64_BYTES
                + U32_BYTES
                + 36
                + 1
                + U64_BYTES
        );
    }

    #[test]
    fn server_metrics_round_trips() {
        let message = ServerMessage::Metrics(MetricsFrame {
            tick: Tick::new(123),
            clients: 10,
            entities: 20,
            aoi_candidates: 30,
            selected_updates: 40,
            selected_full_lod_updates: 11,
            selected_reduced_lod_updates: 12,
            selected_minimal_lod_updates: 13,
            deferred_updates: 50,
            bytes_scheduled: 60,
            deferred_bytes: 70,
        });

        let encoded = encode_server_message(&message).unwrap();

        assert_eq!(encoded_server_message_len(&message).unwrap(), encoded.len());
        assert_eq!(decode_server_message(&encoded).unwrap(), message);
    }

    #[test]
    fn decoder_rejects_bad_magic_and_trailing_bytes() {
        assert_eq!(
            decode_client_message(&[b'B', b'W', PROTOCOL_VERSION, TAG_CLIENT_ACK_SNAPSHOT]),
            Err(CodecError::InvalidMagic)
        );

        let mut encoded = encode_client_message(&ClientMessage::AckSnapshot {
            sequence: SnapshotId::new(1),
        })
        .unwrap();
        encoded.push(99);

        assert_eq!(
            decode_client_message(&encoded),
            Err(CodecError::TrailingBytes(1))
        );
    }

    #[test]
    fn decoder_enforces_payload_limit() {
        let encoded = encode_client_message(&ClientMessage::Input {
            payload: vec![1, 2, 3, 4],
        })
        .unwrap();
        let config = DecodeConfig {
            max_payload_len: 3,
            ..DecodeConfig::default()
        };

        assert_eq!(
            decode_client_message_with_config(&encoded, config),
            Err(CodecError::PayloadTooLarge { len: 4, max: 3 })
        );
    }

    #[test]
    fn decoder_enforces_delta_op_limit() {
        let message = ServerMessage::SnapshotDelta(DeltaSnapshot {
            sequence: SnapshotId::new(1),
            baseline: None,
            ops: vec![DeltaOp::DespawnOrExit {
                entity: EntityId::new(1),
            }],
        });
        let encoded = encode_server_message(&message).unwrap();
        let config = DecodeConfig {
            max_delta_ops: 0,
            ..DecodeConfig::default()
        };

        assert_eq!(
            decode_server_message_with_config(&encoded, config),
            Err(CodecError::TooManyDeltaOps { len: 1, max: 0 })
        );
    }
}
