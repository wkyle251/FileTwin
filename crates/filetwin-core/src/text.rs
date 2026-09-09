use crate::{
    Error, ErrorCode, Result,
    profile::{DIMENSIONS, FNV_OFFSET, FNV_PRIME},
};
use sha2::{Digest, Sha256};
use std::io::Read;
use unicode_normalization::{UnicodeNormalization, char::canonical_combining_class};

pub(crate) struct EncodedText {
    pub vector: Vec<f32>,
    pub bytes_read: u64,
    pub characters: u64,
}

struct Scalars<'a, R> {
    reader: R,
    buffer: [u8; 8192],
    position: usize,
    length: usize,
    first: bool,
    previous_cr: bool,
    nonstarters: usize,
    bytes_read: u64,
    hash: Option<Sha256>,
    error: Option<Error>,
    check: &'a mut dyn FnMut() -> Result<()>,
}

impl<R: Read> Scalars<'_, R> {
    fn byte(&mut self) -> Option<u8> {
        if self.position == self.length {
            if let Err(e) = (self.check)() {
                self.error = Some(e);
                return None;
            }
            let n = loop {
                match self.reader.read(&mut self.buffer) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        self.error = Some(e.into());
                        return None;
                    }
                }
            };
            if n == 0 {
                return None;
            }
            self.bytes_read += n as u64;
            if let Some(h) = &mut self.hash {
                h.update(&self.buffer[..n]);
            }
            self.position = 0;
            self.length = n;
        }
        let b = self.buffer[self.position];
        self.position += 1;
        Some(b)
    }

    fn raw_char(&mut self) -> Option<char> {
        let first = self.byte()?;
        let width = match first {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        let mut bytes = [0u8; 4];
        bytes[0] = first;
        for b in bytes.iter_mut().take(width).skip(1) {
            match self.byte() {
                Some(v) => *b = v,
                None => {
                    if self.error.is_none() {
                        self.error = Some(Error::new(
                            ErrorCode::InvalidText,
                            "encoding",
                            "Truncated UTF-8 sequence",
                        ));
                    }
                    return None;
                }
            }
        }
        let c = if width == 0 {
            None
        } else {
            std::str::from_utf8(&bytes[..width])
                .ok()
                .and_then(|s| s.chars().next())
        };
        match c {
            Some(c) if c != '\0' => Some(c),
            _ => {
                self.error = Some(Error::new(
                    ErrorCode::InvalidText,
                    "encoding",
                    "Input is not valid NUL-free UTF-8 text",
                ));
                None
            }
        }
    }
}

impl<R: Read> Iterator for Scalars<'_, R> {
    type Item = char;
    fn next(&mut self) -> Option<char> {
        if self.error.is_some() {
            return None;
        }
        loop {
            let mut c = self.raw_char()?;
            if self.first {
                self.first = false;
                if c == '\u{feff}' {
                    continue;
                }
            }
            if c == '\n' && self.previous_cr {
                self.previous_cr = false;
                continue;
            }
            self.previous_cr = c == '\r';
            if self.previous_cr {
                c = '\n';
            }
            if canonical_combining_class(c) == 0 {
                self.nonstarters = 0;
            } else {
                self.nonstarters += 1;
                if self.nonstarters > 1024 {
                    self.error = Some(Error::new(
                        ErrorCode::InvalidText,
                        "encoding",
                        "Text exceeds the profile's 1024 consecutive combining-mark limit",
                    ));
                    return None;
                }
            }
            return Some(c);
        }
    }
}

pub(crate) fn encode<R: Read>(
    reader: R,
    compute_digest: bool,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<EncodedText> {
    let mut scalars = Scalars {
        reader,
        buffer: [0; 8192],
        position: 0,
        length: 0,
        first: true,
        previous_cr: false,
        nonstarters: 0,
        bytes_read: 0,
        hash: compute_digest.then(Sha256::new),
        error: None,
        check,
    };
    let mut counts = vec![0.0f64; DIMENSIONS];
    let mut window = ['\0'; 5];
    let (mut used, mut characters, mut features, mut content) = (0usize, 0u64, 0u64, false);
    for c in scalars.by_ref().nfc() {
        characters += 1;
        content |= !c.is_whitespace();
        if used == 5 {
            window.copy_within(1..5, 0);
        } else {
            used += 1;
        }
        window[used - 1] = c;
        for n in 1..=used {
            let mut h = (FNV_OFFSET ^ n as u64).wrapping_mul(FNV_PRIME);
            for scalar in &window[used - n..used] {
                let mut buf = [0; 4];
                for b in scalar.encode_utf8(&mut buf).bytes() {
                    h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
                }
            }
            counts[(h as usize) & (DIMENSIONS - 1)] += if h >> 63 == 0 { 1.0 } else { -1.0 };
            features += 1;
        }
        if features > 9_007_199_254_740_991 {
            scalars.error = Some(Error::new(
                ErrorCode::InvalidText,
                "encoding",
                "Text exceeds the profile's exact feature-count range",
            ));
            break;
        }
    }
    if let Some(mut e) = scalars.error {
        e.details = Box::new(
            serde_json::json!({"bytes_read":scalars.bytes_read,"bytes_hashed":if compute_digest{scalars.bytes_read}else{0}}),
        );
        return Err(e);
    }
    let norm = counts.iter().map(|v| v * v).sum::<f64>().sqrt();
    if !content || norm == 0.0 || !norm.is_finite() {
        let mut e = Error::new(
            ErrorCode::InsufficientContent,
            "encoding",
            "No usable text features",
        );
        e.details = Box::new(
            serde_json::json!({"bytes_read":scalars.bytes_read,"bytes_hashed":if compute_digest{scalars.bytes_read}else{0},"digest":scalars.hash.map(|h|format!("{:x}",h.finalize()))}),
        );
        return Err(e);
    }
    Ok(EncodedText {
        vector: counts.into_iter().map(|v| (v / norm) as f32).collect(),
        bytes_read: scalars.bytes_read,
        characters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vector(s: &str) -> Vec<f32> {
        encode(s.as_bytes(), false, &mut || Ok(())).unwrap().vector
    }
    #[test]
    fn normalization_survives_boundaries() {
        let a = "a".repeat(8190) + "é\r\nend";
        let b = "\u{feff}".to_owned() + &"a".repeat(8190) + "e\u{301}\nend";
        assert_eq!(vector(&a), vector(&b));
    }
    #[test]
    fn malformed_empty_and_excessive_combining_marks_fail() {
        assert_eq!(
            encode(&b"a\xff"[..], false, &mut || Ok(()))
                .err()
                .unwrap()
                .code,
            ErrorCode::InvalidText
        );
        assert_eq!(
            encode(&b" \n"[..], false, &mut || Ok(()))
                .err()
                .unwrap()
                .code,
            ErrorCode::InsufficientContent
        );
        let marks = "a".to_owned() + &"\u{301}".repeat(1025);
        assert_eq!(
            encode(marks.as_bytes(), false, &mut || Ok(()))
                .err()
                .unwrap()
                .code,
            ErrorCode::InvalidText
        );
    }
    #[test]
    fn small_edits_score_above_unrelated_text() {
        let a = vector("Im human!");
        assert!(
            crate::profile::cosine(&a, &vector("I'm human!"))
                > crate::profile::cosine(&a, &vector("A distant mountain lake."))
        );
        assert_ne!(vector("price 1.25"), vector("price 125"));
    }
}
