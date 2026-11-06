#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Png,
    Jpeg,
    Tiff,
}

/// Detects the format from magic bytes only.
pub fn detect_format(bytes: &[u8]) -> Option<Format> {
    // PDF header may be preceded by up to 1024 bytes of junk per the spec.
    let head = &bytes[..bytes.len().min(1024)];
    if head.windows(5).any(|w| w == b"%PDF-") {
        return Some(Format::Pdf);
    }
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some(Format::Png),
        [0xFF, 0xD8, 0xFF, ..] => Some(Format::Jpeg),
        [b'I', b'I', 0x2A, 0x00, ..] | [b'M', b'M', 0x00, 0x2A, ..] => Some(Format::Tiff),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_formats() {
        assert_eq!(detect_format(b"%PDF-1.7\n..."), Some(Format::Pdf));
        assert_eq!(detect_format(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]), Some(Format::Png));
        assert_eq!(detect_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Format::Jpeg));
        assert_eq!(detect_format(b"II*\0rest"), Some(Format::Tiff));
        assert_eq!(detect_format(b"PK\x03\x04"), None); // docx/zip
    }
}
