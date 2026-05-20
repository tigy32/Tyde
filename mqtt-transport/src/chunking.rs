use std::collections::VecDeque;

pub const MAX_PLAINTEXT_CHUNK_LEN: usize = 64 * 1024;

#[derive(Debug, Default)]
pub struct InboundPlaintext {
    bytes: VecDeque<u8>,
}

impl InboundPlaintext {
    pub fn new() -> Self {
        Self {
            bytes: VecDeque::new(),
        }
    }

    pub fn push_chunk(&mut self, chunk: Vec<u8>) {
        self.bytes.extend(chunk);
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn read_into(&mut self, dest: &mut [u8]) -> usize {
        let mut written = 0;
        while written < dest.len() {
            let Some(byte) = self.bytes.pop_front() else {
                break;
            };
            dest[written] = byte;
            written += 1;
        }
        written
    }
}

#[derive(Debug, Default)]
pub struct OutboundPlaintext {
    buffer: Vec<u8>,
}

impl OutboundPlaintext {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn take_full_chunk(&mut self) -> Option<Vec<u8>> {
        if self.buffer.len() < MAX_PLAINTEXT_CHUNK_LEN {
            return None;
        }
        Some(self.buffer.drain(..MAX_PLAINTEXT_CHUNK_LEN).collect())
    }

    pub fn take_flush_chunk(&mut self) -> Option<Vec<u8>> {
        if self.buffer.is_empty() {
            return None;
        }
        Some(self.buffer.drain(..).collect())
    }
}

pub fn next_write_len(buffered_len: usize, incoming_len: usize) -> usize {
    if incoming_len == 0 {
        return 0;
    }

    let remaining_to_chunk = MAX_PLAINTEXT_CHUNK_LEN.saturating_sub(buffered_len);
    if remaining_to_chunk == 0 {
        incoming_len.min(MAX_PLAINTEXT_CHUNK_LEN)
    } else {
        incoming_len.min(remaining_to_chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_at_64_kib() {
        let mut outbound = OutboundPlaintext::new();
        outbound.append(&vec![1_u8; MAX_PLAINTEXT_CHUNK_LEN + 3]);
        let first = outbound.take_full_chunk();
        assert!(matches!(
            first.as_ref().map(Vec::len),
            Some(MAX_PLAINTEXT_CHUNK_LEN)
        ));
        let rest = outbound.take_flush_chunk();
        assert!(matches!(rest.as_ref().map(Vec::len), Some(3)));
    }
}
