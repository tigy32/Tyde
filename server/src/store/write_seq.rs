use std::fs::File;
use std::io::Read;
use std::path::Path;

pub(crate) const CAS_ATTEMPTS: usize = 4;
const PEEK_BYTES: usize = 256;

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
    Ok(parse_leading_write_seq(&buf[..n]))
}

fn parse_leading_write_seq(bytes: &[u8]) -> u64 {
    let mut index = skip_ws(bytes, 0);
    if bytes.get(index) != Some(&b'{') {
        return 0;
    }
    index = skip_ws(bytes, index + 1);
    let key = b"\"write_seq\"";
    if !bytes.get(index..).is_some_and(|rest| rest.starts_with(key)) {
        return 0;
    }
    index = skip_ws(bytes, index + key.len());
    if bytes.get(index) != Some(&b':') {
        return 0;
    }
    index = skip_ws(bytes, index + 1);
    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if start == index {
        return 0;
    }
    std::str::from_utf8(&bytes[start..index])
        .ok()
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

fn skip_ws(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\n' | b'\r') {
        index += 1;
    }
    index
}
