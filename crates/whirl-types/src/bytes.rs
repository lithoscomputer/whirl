//! Bytes literals (SPEC 3.1): `hex,DIGITS;` and `base64,TEXT;`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// True for text shaped like a bytes literal: `hex,…;` or `base64,…;`.
pub fn is_bytes_literal_shape(text: &str) -> bool {
    (text.starts_with("hex,") || text.starts_with("base64,")) && text.ends_with(';')
}

/// Decodes a bytes literal: `hex,DIGITS;` or `base64,TEXT;`.
pub fn bytes_literal(text: &str) -> Option<Vec<u8>> {
    let body = text.strip_suffix(';')?;
    if let Some(hex) = body.strip_prefix("hex,") {
        if hex.len() % 2 != 0 {
            return None;
        }
        return (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
            .collect();
    }
    body.strip_prefix("base64,")
        .and_then(|encoded| STANDARD.decode(encoded).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_bytes_literals() {
        assert_eq!(bytes_literal("hex,beef;"), Some(vec![0xbe, 0xef]));
        assert_eq!(
            bytes_literal("base64,PDw/Pz8+Pg==;"),
            Some(b"<<???>>".to_vec())
        );
        assert_eq!(bytes_literal("hex,abc;"), None);
        assert_eq!(bytes_literal("hex,beef"), None);
    }
}
