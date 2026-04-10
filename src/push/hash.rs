use sha1::{Digest, Sha1};

pub fn blob_sha1(content: &[u8]) -> String {
    let header = format!("blob {}\0", content.len());
    let mut hasher = Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(content);
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut acc, b| {
            use std::fmt::Write;
            write!(acc, "{b:02x}").unwrap();
            acc
        })
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
}
