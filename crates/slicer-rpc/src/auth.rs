use rand::RngCore;
use std::sync::Arc;

/// 32-byte random token, hex-encoded for transport.
#[derive(Clone)]
pub struct AuthToken(pub Arc<String>);

impl AuthToken {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(Arc::new(hex::encode(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Constant-time compare to avoid timing-attack on token validation.
    pub fn matches(&self, other: &str) -> bool {
        if self.0.len() != other.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for (a, b) in self.0.bytes().zip(other.bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}
