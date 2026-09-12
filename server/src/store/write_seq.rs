use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::Path;

pub(crate) const CAS_ATTEMPTS: usize = 4;
const PEEK_BYTES: usize = 256;

#[derive(serde::Deserialize)]
struct StoreSequence {
    #[serde(default)]
    write_seq: u64,
}

pub(crate) fn peek_write_seq(path: &Path) -> Result<u64, String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => {
            return Err(format!(
                "Failed to peek write_seq in {}: {err}",
                path.display()
            ));
        }
    };
    let mut buf = [0u8; PEEK_BYTES];
    let n = file
        .read(&mut buf)
        .map_err(|err| format!("Failed to peek write_seq in {}: {err}", path.display()))?;
    if let Some(seq) = parse_leading_write_seq(&buf[..n]) {
        return Ok(seq);
    }

    // Migrations and external writers can put write_seq anywhere in the object.
    let reader = BufReader::new(Cursor::new(&buf[..n]).chain(file));
    serde_json::from_reader::<_, StoreSequence>(reader)
        .map(|store| store.write_seq)
        .map_err(|err| format!("Failed to read write_seq in {}: {err}", path.display()))
}

fn parse_leading_write_seq(bytes: &[u8]) -> Option<u64> {
    let mut index = skip_ws(bytes, 0);
    if bytes.get(index) != Some(&b'{') {
        return None;
    }
    index = skip_ws(bytes, index + 1);
    let key = b"\"write_seq\"";
    if !bytes.get(index..).is_some_and(|rest| rest.starts_with(key)) {
        return None;
    }
    index = skip_ws(bytes, index + key.len());
    if bytes.get(index) != Some(&b':') {
        return None;
    }
    index = skip_ws(bytes, index + 1);
    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if start == index || !matches!(bytes.get(skip_ws(bytes, index)), Some(b',' | b'}')) {
        return None;
    }
    std::str::from_utf8(&bytes[start..index])
        .ok()
        .and_then(|digits| digits.parse().ok())
}

fn skip_ws(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\n' | b'\r') {
        index += 1;
    }
    index
}
