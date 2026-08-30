//! Content hashing, used to tell a duplicate from a genuine collision.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const CHUNK: usize = 64 * 1024;

/// Streams a file through SHA-256. Never loads the whole file into memory, so
/// it is safe against the multi-gigabyte files that turn up in a downloads
/// folder.
pub fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn identical_content_hashes_identically() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let c = dir.path().join("c");
        File::create(&a).unwrap().write_all(b"hello").unwrap();
        File::create(&b).unwrap().write_all(b"hello").unwrap();
        File::create(&c).unwrap().write_all(b"world").unwrap();

        assert_eq!(file_sha256(&a).unwrap(), file_sha256(&b).unwrap());
        assert_ne!(file_sha256(&a).unwrap(), file_sha256(&c).unwrap());
    }

    #[test]
    fn hashes_files_larger_than_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        let payload = vec![7u8; CHUNK * 2 + 13];
        File::create(&big).unwrap().write_all(&payload).unwrap();
        assert_eq!(file_sha256(&big).unwrap().len(), 64);
    }
}
