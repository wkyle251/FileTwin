use std::path::Path;

pub(crate) fn detect_format(bytes: &[u8], path: &Path) -> (&'static str, &'static str) {
    if bytes.starts_with(b"%PDF-") {
        ("text", "pdf")
    } else if bytes.starts_with(b"PK\x03\x04") {
        ("text", "docx") // The bounded package reader verifies this is DOCX.
    } else if bytes.starts_with(b"\x1f\x8b")
        || bytes.starts_with(b"7z\xbc\xaf\x27\x1c")
        || bytes.starts_with(b"Rar!")
    {
        ("unknown", "archive")
    } else if bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"GIF8")
        || bytes.starts_with(b"II*\0")
        || bytes.starts_with(b"MM\0*")
        || bytes.starts_with(b"II+\0")
        || bytes.starts_with(b"MM\0+")
        || bytes.starts_with(b"BM")
        || bytes.starts_with(b"\0\0\x01\0")
        || bytes.starts_with(b"qoif")
        || bytes.starts_with(b"DDS ")
        || bytes.starts_with(b"v/1\x01")
        || bytes.starts_with(b"#?RADIANCE")
        || bytes.starts_with(b"#?RGBE")
        || bytes.starts_with(b"farbfeld")
        || (bytes.first() == Some(&b'P') && bytes.get(1).is_some_and(|b| (b'1'..=b'7').contains(b)))
    {
        ("image", "raster")
    } else if bytes.starts_with(b"RIFF") {
        match bytes.get(8..12) {
            Some(b"WEBP") => ("image", "raster"),
            Some(b"WAVE") => ("audio", "media"),
            Some(b"AVI ") => ("video", "media"),
            _ => ("unknown", "riff"),
        }
    } else if bytes.starts_with(b"ID3")
        || bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"caff")
        || bytes.starts_with(b"MAC ")
        || bytes.starts_with(b"wvpk")
        || bytes.starts_with(b"#!AMR")
        || bytes.starts_with(b".snd")
        || bytes.starts_with(b"MPCK")
        || bytes.starts_with(b"FORM")
        || bytes.starts_with(b"RF64")
        || (bytes.first() == Some(&0xff) && bytes.get(1).is_some_and(|b| b & 0xe0 == 0xe0))
    {
        ("audio", "media")
    } else if bytes.starts_with(b"OggS") {
        if bytes.windows(6).any(|w| w == b"theora") {
            ("video", "media")
        } else {
            ("audio", "media")
        }
    } else if bytes.get(4..8) == Some(b"ftyp") {
        // Brands are four-byte entries: the major brand at byte 8 and
        // compatible brands after the minor version. MIAF files can put the
        // decisive HEIF/AVIF brand only in their compatible-brand list.
        let box_end = bytes
            .get(..4)
            .map(|n| u32::from_be_bytes(n.try_into().unwrap()) as usize)
            .unwrap_or(0)
            .min(bytes.len());
        let image_brand = |brand: &[u8]| {
            matches!(
                brand,
                b"avif" | b"avis" | b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1"
            )
        };
        if bytes.get(8..12).is_some_and(image_brand)
            || bytes.get(16..box_end).is_some_and(|brands| {
                brands
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|brand| image_brand(brand))
            })
        {
            return ("image", "heif_avif");
        }
        match bytes.get(8..12) {
            Some(b"M4A " | b"M4B " | b"M4P ") => ("audio", "media"),
            _ => ("video", "media"),
        }
    } else if bytes.starts_with(b"\x1aE\xdf\xa3")
        || bytes.starts_with(b"FLV")
        || bytes.starts_with(b".RMF")
        || bytes.starts_with(b"\x30\x26\xb2\x75\x8e\x66\xcf\x11")
        || bytes.starts_with(b"\0\0\x01\xba")
        || bytes.get(4..8) == Some(b"moov")
        || (bytes.first() == Some(&0x47)
            && bytes.get(188) == Some(&0x47)
            && bytes.get(376) == Some(&0x47))
    {
        ("video", "media")
    } else {
        // Formats without a reliable short magic get a decoder hint, followed
        // by strict content validation. Extensions never bypass the decoder.
        match path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("tga") => ("image", "tga"),
            Some("heic" | "heif" | "hif" | "avif" | "avifs") => ("image", "heif_avif"),
            Some(
                "mts" | "m2ts" | "mpeg" | "mpg" | "mkv" | "webm" | "mov" | "mp4" | "avi" | "wmv"
                | "asf" | "flv" | "m4v" | "3gp" | "3g2" | "mxf" | "vob" | "ogv" | "rm" | "rmvb"
                | "m2v",
            ) => ("video", "media"),
            Some(
                "m4a" | "mka" | "mp3" | "aac" | "wav" | "flac" | "aiff" | "opus" | "ogg" | "oga"
                | "mp2" | "wma" | "ape" | "wv" | "caf" | "au" | "amr" | "ac3" | "eac3" | "dts"
                | "aif" | "m4b" | "ra",
            ) => ("audio", "media"),
            _ => ("text", "utf8"),
        }
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn image_brands_and_media_contents_override_extension_hints() {
        assert_eq!(
            detect_format(b"\0\0\0\x18ftypmiaf\0\0\0\0avifmif1", Path::new("copy.bin")),
            ("image", "heif_avif")
        );
        assert_eq!(
            detect_format(b"\0\0\0\x18ftyphevx\0\0\0\0mif1heic", Path::new("copy.mp4")),
            ("image", "heif_avif")
        );
        assert_eq!(
            detect_format(b"\0\0\0\x18ftypM4A \0\0\0\0isommp42", Path::new("copy.bin")),
            ("audio", "media")
        );
        assert_eq!(
            detect_format(b"\x89PNG\r\n\x1a\n", Path::new("copy.heic")),
            ("image", "raster")
        );
        assert_eq!(
            detect_format(b"const value = 42;", Path::new("code.ts")),
            ("text", "utf8")
        );
    }
}
