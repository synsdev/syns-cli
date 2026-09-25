use sha1::{Digest, Sha1};

pub fn blob_sha1(content: &[u8]) -> String {
    let header = format!("blob {}\0", content.len());
    let mut hasher = Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(content);
    hasher.finalize().iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        write!(acc, "{b:02x}").unwrap();
        acc
    })
}

/// The blob hash of what `reader` yields under the header `blob_sha1`
/// writes for `declared` bytes, each piece also written to `sink`; `None`
/// where the reader yields other than `declared` bytes. The one
/// piece-by-piece hash every held-past-the-budget path takes (CR1-5).
pub(crate) fn hash_pieces<R: std::io::Read, W: std::io::Write>(
    mut reader: R,
    declared: u64,
    sink: &mut W,
) -> std::io::Result<Option<String>> {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {declared}\0").as_bytes());
    let mut buf = vec![0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if n == 0 {
            break;
        }
        read += n as u64;
        if read > declared {
            return Ok(None);
        }
        hasher.update(&buf[..n]);
        sink.write_all(&buf[..n])?;
    }
    sink.flush()?;
    if read != declared {
        return Ok(None);
    }
    Ok(Some(hasher.finalize().iter().fold(
        String::new(),
        |mut acc, b| {
            use std::fmt::Write;
            write!(acc, "{b:02x}").unwrap();
            acc
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_sha1_hello_world() {
        let hash = blob_sha1(b"hello world");
        assert_eq!(hash, "95d09f2b10159347eece71399a7e2e907ea3df4f");
    }

    #[test]
    fn blob_sha1_empty() {
        let hash = blob_sha1(b"");
        assert_eq!(hash, "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn hash_pieces_answers_blob_sha1_and_refuses_a_moved_length() {
        let mut copy = Vec::new();
        let sha = hash_pieces(&b"hello world"[..], 11, &mut copy).unwrap();
        assert_eq!(sha.as_deref(), Some(blob_sha1(b"hello world").as_str()));
        assert_eq!(copy, b"hello world");
        assert_eq!(
            hash_pieces(&b"hello world"[..], 10, &mut Vec::new()).unwrap(),
            None
        );
        assert_eq!(
            hash_pieces(&b"hello world"[..], 12, &mut Vec::new()).unwrap(),
            None
        );
    }
}
