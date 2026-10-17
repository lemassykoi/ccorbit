//! Follows an append-only JSONL file by byte offset.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;

const CHUNK: usize = 64 * 1024;

pub struct Tail {
    pub path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl Tail {
    pub fn new(path: PathBuf) -> Self {
        Tail { path, offset: 0, partial: Vec::new() }
    }

    /// Calls `f` for every complete line appended since the last call and returns whether
    /// anything was read. A trailing line without its newline is held back until the writer
    /// finishes it. A missing file is not an error: transcripts appear lazily.
    pub fn poll(&mut self, mut f: impl FnMut(&str)) -> io::Result<bool> {
        let len = match std::fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if len < self.offset {
            // Rewritten from scratch: start over. Steps are keyed by id, so replays are harmless.
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return Ok(false);
        }
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = vec![0u8; CHUNK];
        let mut remaining = len - self.offset;
        while remaining > 0 {
            let want = remaining.min(CHUNK as u64) as usize;
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            remaining -= n as u64;
            self.offset += n as u64;
            let mut start = 0;
            for i in 0..n {
                if buf[i] != b'\n' {
                    continue;
                }
                if self.partial.is_empty() {
                    emit(&mut f, &buf[start..i]);
                } else {
                    self.partial.extend_from_slice(&buf[start..i]);
                    emit(&mut f, &self.partial);
                    self.partial.clear();
                }
                start = i + 1;
            }
            self.partial.extend_from_slice(&buf[start..n]);
        }
        if self.partial.capacity() > CHUNK && self.partial.is_empty() {
            self.partial.shrink_to(0);
        }
        Ok(true)
    }
}

fn emit(f: &mut impl FnMut(&str), bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => f(s),
        Err(_) => f(&String::from_utf8_lossy(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn holds_back_partial_lines() {
        let dir = std::env::temp_dir().join(format!("ccorbit-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut file = File::create(&path).unwrap();
        let mut tail = Tail::new(path.clone());
        let mut got = Vec::new();

        file.write_all(b"{\"a\":1}\n{\"b\":").unwrap();
        tail.poll(|l| got.push(l.to_string())).unwrap();
        assert_eq!(got, vec!["{\"a\":1}"]);

        file.write_all(b"2}\n").unwrap();
        tail.poll(|l| got.push(l.to_string())).unwrap();
        assert_eq!(got, vec!["{\"a\":1}", "{\"b\":2}"]);

        assert!(!tail.poll(|_| panic!("nothing new")).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
