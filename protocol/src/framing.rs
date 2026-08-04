use std::collections::HashMap;
use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(not(target_arch = "wasm32"))]
use tokio::time::timeout as reassembly_timeout;
#[cfg(target_arch = "wasm32")]
use wasmtimer::tokio::timeout as reassembly_timeout;

use crate::Envelope;
use crate::types::SeqMismatch;

pub const RECORD_MAGIC: [u8; 4] = *b"TYD2";
pub const RECORD_VERSION: u8 = 1;
pub const MAX_RECORD_HEADER: usize = 1024 * 1024;
pub const MAX_RECORD_BODY: usize = 64 * 1024;
pub const MAX_LOGICAL_HEADER: usize = 16 * 1024 * 1024;
pub const MAX_FRAGMENT_BODY: usize = 48 * 1024;
pub const MAX_ACTIVE_REASSEMBLIES: usize = 16;
pub const MAX_REASSEMBLY_BYTES: usize = 32 * 1024 * 1024;
pub const REASSEMBLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const FIXED_HEADER_LEN: usize = 20;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Json(serde_json::Error),
    Protocol(String),
}

impl From<io::Error> for FrameError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for FrameError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<SeqMismatch> for FrameError {
    fn from(value: SeqMismatch) -> Self {
        Self::Protocol(value.to_string())
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Json(err) => write!(f, "json error: {err}"),
            Self::Protocol(msg) => write!(f, "protocol violation: {msg}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Protocol(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProtocolFrame {
    pub envelope: Envelope,
    pub binary: Vec<u8>,
}

impl ProtocolFrame {
    pub fn json(envelope: Envelope) -> Self {
        Self {
            envelope,
            binary: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum RecordKind {
    Json = 1,
    Binary = 2,
    Fragment = 3,
}

impl TryFrom<u8> for RecordKind {
    type Error = FrameError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Json),
            2 => Ok(Self::Binary),
            3 => Ok(Self::Fragment),
            _ => Err(FrameError::Protocol(format!("unknown record kind {value}"))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EncodedRecord {
    pub bytes: Vec<u8>,
    pub fragment_index: u32,
    pub fragment_count: u32,
}

#[derive(Serialize, Deserialize)]
struct FragmentHeader {
    message_id: u64,
    index: u32,
    count: u32,
    logical_header_len: u32,
    binary_len: u32,
}

struct PartialFrame {
    count: u32,
    next: u32,
    logical_header_len: usize,
    binary_len: usize,
    bytes: Vec<u8>,
}

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize()
}

fn encode_record(kind: RecordKind, header: &[u8], body: &[u8]) -> Result<Vec<u8>, FrameError> {
    if header.len() > MAX_RECORD_HEADER {
        return Err(FrameError::Protocol("record header exceeds 1 MiB".into()));
    }
    if body.len() > MAX_RECORD_BODY {
        return Err(FrameError::Protocol("record body exceeds 64 KiB".into()));
    }
    let header_len = u32::try_from(header.len())
        .map_err(|_| FrameError::Protocol("header length overflow".into()))?;
    let body_len = u32::try_from(body.len())
        .map_err(|_| FrameError::Protocol("body length overflow".into()))?;
    let checksum = crc32(&[header, body]);
    let mut out = Vec::with_capacity(FIXED_HEADER_LEN + header.len() + body.len());
    out.extend_from_slice(&RECORD_MAGIC);
    out.push(RECORD_VERSION);
    out.push(kind as u8);
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&body_len.to_be_bytes());
    out.extend_from_slice(&checksum.to_be_bytes());
    out.extend_from_slice(header);
    out.extend_from_slice(body);
    Ok(out)
}

pub fn encode_frame(
    frame: &ProtocolFrame,
    message_id: u64,
) -> Result<Vec<EncodedRecord>, FrameError> {
    let header = serde_json::to_vec(&frame.envelope)?;
    if header.len() > MAX_LOGICAL_HEADER {
        return Err(FrameError::Protocol(
            "logical envelope exceeds 16 MiB".into(),
        ));
    }
    if frame.binary.len() > MAX_RECORD_BODY {
        return Err(FrameError::Protocol("binary payload exceeds 64 KiB".into()));
    }
    if header.len() <= MAX_RECORD_HEADER {
        let kind = if frame.binary.is_empty() {
            RecordKind::Json
        } else {
            RecordKind::Binary
        };
        return Ok(vec![EncodedRecord {
            bytes: encode_record(kind, &header, &frame.binary)?,
            fragment_index: 0,
            fragment_count: 1,
        }]);
    }

    if !frame.binary.is_empty() {
        return Err(FrameError::Protocol(
            "large binary envelopes cannot be fragmented".into(),
        ));
    }
    let count = header.len().div_ceil(MAX_FRAGMENT_BODY);
    let count =
        u32::try_from(count).map_err(|_| FrameError::Protocol("fragment count overflow".into()))?;
    let mut records = Vec::with_capacity(count as usize);
    for (index, chunk) in header.chunks(MAX_FRAGMENT_BODY).enumerate() {
        let meta = FragmentHeader {
            message_id,
            index: index as u32,
            count,
            logical_header_len: header.len() as u32,
            binary_len: 0,
        };
        records.push(EncodedRecord {
            bytes: encode_record(RecordKind::Fragment, &serde_json::to_vec(&meta)?, chunk)?,
            fragment_index: index as u32,
            fragment_count: count,
        });
    }
    Ok(records)
}

async fn read_record<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<(RecordKind, Vec<u8>, Vec<u8>)>, FrameError> {
    let mut fixed = [0u8; FIXED_HEADER_LEN];
    match reader.read_exact(&mut fixed[..1]).await {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    reader.read_exact(&mut fixed[1..]).await?;
    if fixed[..4] != RECORD_MAGIC {
        return Err(FrameError::Protocol("invalid TYD2 record magic".into()));
    }
    if fixed[4] != RECORD_VERSION {
        return Err(FrameError::Protocol(format!(
            "unsupported record version {}",
            fixed[4]
        )));
    }
    if u16::from_be_bytes([fixed[6], fixed[7]]) != 0 {
        return Err(FrameError::Protocol(
            "record reserved flags are nonzero".into(),
        ));
    }
    let kind = RecordKind::try_from(fixed[5])?;
    let header_len = u32::from_be_bytes(fixed[8..12].try_into().unwrap()) as usize;
    let body_len = u32::from_be_bytes(fixed[12..16].try_into().unwrap()) as usize;
    let expected_crc = u32::from_be_bytes(fixed[16..20].try_into().unwrap());
    if header_len > MAX_RECORD_HEADER || body_len > MAX_RECORD_BODY {
        return Err(FrameError::Protocol("record length exceeds bound".into()));
    }
    let mut header = vec![0; header_len];
    let mut body = vec![0; body_len];
    reader.read_exact(&mut header).await?;
    reader.read_exact(&mut body).await?;
    if crc32(&[&header, &body]) != expected_crc {
        return Err(FrameError::Protocol("record checksum mismatch".into()));
    }
    Ok(Some((kind, header, body)))
}

pub struct FrameReader<R> {
    reader: R,
    partial: HashMap<u64, PartialFrame>,
    reassembly_bytes: usize,
}

impl<R> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            partial: HashMap::new(),
            reassembly_bytes: 0,
        }
    }
    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub async fn read_envelope(&mut self) -> Result<Option<Envelope>, FrameError> {
        match self.read_frame().await? {
            Some(frame) if frame.binary.is_empty() => Ok(Some(frame.envelope)),
            Some(_) => Err(FrameError::Protocol(
                "binary frame requires read_frame".into(),
            )),
            None => Ok(None),
        }
    }

    pub async fn read_frame(&mut self) -> Result<Option<ProtocolFrame>, FrameError> {
        loop {
            let record = if self.partial.is_empty() {
                read_record(&mut self.reader).await?
            } else {
                reassembly_timeout(REASSEMBLY_TIMEOUT, read_record(&mut self.reader))
                    .await
                    .map_err(|_| FrameError::Protocol("fragment reassembly timed out".into()))??
            };
            let Some((kind, header, body)) = record else {
                if self.partial.is_empty() {
                    return Ok(None);
                }
                return Err(FrameError::Protocol(
                    "EOF during fragmented envelope".into(),
                ));
            };
            match kind {
                RecordKind::Json | RecordKind::Binary => {
                    let envelope: Envelope = serde_json::from_slice(&header)?;
                    if kind == RecordKind::Json && !body.is_empty() {
                        return Err(FrameError::Protocol("JSON record has a binary body".into()));
                    }
                    return Ok(Some(ProtocolFrame {
                        envelope,
                        binary: body,
                    }));
                }
                RecordKind::Fragment => {
                    let meta: FragmentHeader = serde_json::from_slice(&header)?;
                    if meta.count == 0 || meta.index >= meta.count || meta.binary_len != 0 {
                        return Err(FrameError::Protocol("invalid fragment metadata".into()));
                    }
                    let logical_len = meta.logical_header_len as usize;
                    if logical_len > MAX_LOGICAL_HEADER || body.is_empty() {
                        return Err(FrameError::Protocol(
                            "invalid fragmented envelope length".into(),
                        ));
                    }
                    if !self.partial.contains_key(&meta.message_id) {
                        if meta.index != 0
                            || self.partial.len() >= MAX_ACTIVE_REASSEMBLIES
                            || self.reassembly_bytes + logical_len > MAX_REASSEMBLY_BYTES
                        {
                            return Err(FrameError::Protocol(
                                "fragment reassembly bound/order violation".into(),
                            ));
                        }
                        self.reassembly_bytes += logical_len;
                        self.partial.insert(
                            meta.message_id,
                            PartialFrame {
                                count: meta.count,
                                next: 0,
                                logical_header_len: logical_len,
                                binary_len: 0,
                                bytes: Vec::with_capacity(logical_len),
                            },
                        );
                    }
                    let partial = self.partial.get_mut(&meta.message_id).unwrap();
                    if partial.count != meta.count
                        || partial.next != meta.index
                        || partial.logical_header_len != logical_len
                    {
                        return Err(FrameError::Protocol("fragment sequence mismatch".into()));
                    }
                    partial.bytes.extend_from_slice(&body);
                    partial.next += 1;
                    if partial.bytes.len() > partial.logical_header_len {
                        return Err(FrameError::Protocol(
                            "fragment data exceeds declared length".into(),
                        ));
                    }
                    if partial.next == partial.count {
                        let partial = self.partial.remove(&meta.message_id).unwrap();
                        self.reassembly_bytes -= partial.logical_header_len;
                        if partial.bytes.len() != partial.logical_header_len
                            || partial.binary_len != 0
                        {
                            return Err(FrameError::Protocol(
                                "fragmented envelope length mismatch".into(),
                            ));
                        }
                        let envelope = serde_json::from_slice(&partial.bytes)?;
                        return Ok(Some(ProtocolFrame::json(envelope)));
                    }
                }
            }
        }
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &ProtocolFrame,
) -> Result<(), FrameError> {
    for record in encode_frame(frame, 0)? {
        writer.write_all(&record.bytes).await?;
    }
    writer.flush().await?;
    Ok(())
}

pub async fn write_envelope<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &Envelope,
) -> Result<(), FrameError> {
    write_frame(writer, &ProtocolFrame::json(envelope.clone())).await
}

pub async fn read_envelope<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Envelope>, FrameError> {
    let mut framed = FrameReader::new(reader);
    let frame = framed.read_frame().await?;
    match frame {
        Some(frame) if frame.binary.is_empty() => Ok(Some(frame.envelope)),
        Some(_) => Err(FrameError::Protocol(
            "binary frame requires read_frame".into(),
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameKind, StreamPath};

    fn envelope(payload: serde_json::Value) -> Envelope {
        Envelope {
            stream: StreamPath("/host/test".into()),
            kind: FrameKind::Heartbeat,
            seq: 7,
            payload,
        }
    }

    #[tokio::test]
    async fn rejects_corruption_and_oversize_before_allocation() {
        let mut bytes = encode_frame(&ProtocolFrame::json(envelope(serde_json::json!({}))), 1)
            .unwrap()
            .remove(0)
            .bytes;
        *bytes.last_mut().unwrap() ^= 1;
        let error = FrameReader::new(bytes.as_slice())
            .read_frame()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("checksum"));

        let mut fixed = vec![0; FIXED_HEADER_LEN];
        fixed[..4].copy_from_slice(&RECORD_MAGIC);
        fixed[4] = RECORD_VERSION;
        fixed[5] = RecordKind::Json as u8;
        fixed[8..12].copy_from_slice(&((MAX_RECORD_HEADER as u32) + 1).to_be_bytes());
        let error = FrameReader::new(fixed.as_slice())
            .read_frame()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds bound"));

        let error = FrameReader::new(&RECORD_MAGIC[..2])
            .read_frame()
            .await
            .unwrap_err();
        assert!(
            matches!(error, FrameError::Io(ref error) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
    }

    #[tokio::test]
    async fn reassembles_large_json_and_preserves_binary() {
        let large = "x".repeat(MAX_RECORD_HEADER + 1);
        let records = encode_frame(
            &ProtocolFrame::json(envelope(serde_json::json!({"large": large}))),
            9,
        )
        .unwrap();
        assert!(records.len() > 1);
        let bytes = records
            .into_iter()
            .flat_map(|record| record.bytes)
            .collect::<Vec<_>>();
        let decoded = FrameReader::new(bytes.as_slice())
            .read_frame()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decoded.envelope.seq, 7);

        let frame = ProtocolFrame {
            envelope: envelope(serde_json::json!({"codec":"opus"})),
            binary: vec![1, 2, 3],
        };
        let bytes = encode_frame(&frame, 10).unwrap().remove(0).bytes;
        let decoded = FrameReader::new(bytes.as_slice())
            .read_frame()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decoded, frame);
    }

    #[tokio::test]
    async fn rejects_unknown_version_kind_flags_and_fragment_order() {
        let base = encode_frame(&ProtocolFrame::json(envelope(serde_json::json!({}))), 1)
            .unwrap()
            .remove(0)
            .bytes;
        for (offset, value, expected) in [(4, 99, "version"), (5, 99, "kind"), (7, 1, "reserved")] {
            let mut bytes = base.clone();
            bytes[offset] = value;
            let error = FrameReader::new(bytes.as_slice())
                .read_frame()
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected));
        }
        let records = encode_frame(
            &ProtocolFrame::json(envelope(
                serde_json::json!({"large":"x".repeat(MAX_RECORD_HEADER+1)}),
            )),
            2,
        )
        .unwrap();
        let bytes = records
            .into_iter()
            .skip(1)
            .flat_map(|record| record.bytes)
            .collect::<Vec<_>>();
        assert!(
            FrameReader::new(bytes.as_slice())
                .read_frame()
                .await
                .unwrap_err()
                .to_string()
                .contains("order")
        );
        let oversized = ProtocolFrame {
            envelope: envelope(serde_json::json!({})),
            binary: vec![0; MAX_RECORD_BODY + 1],
        };
        assert!(
            encode_frame(&oversized, 3)
                .unwrap_err()
                .to_string()
                .contains("64 KiB")
        );
    }
}
