use sha2::{Digest, Sha256};

/// The lowercase hexadecimal SHA-256 digest of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
