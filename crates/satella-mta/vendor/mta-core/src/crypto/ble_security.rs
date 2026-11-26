//! Cattysend Crypto Module
//!
//! Implements encryption logic fully compatible with CatShare (Android):
//! - ECDH (P-256) key exchange
//! - AES-256-CTR encryption (with a fixed IV)
//!
//! # Key Compatibility Notes
//!
//! 1. **Key format**: CatShare uses Java's `ECPublicKey.getEncoded()`,
//!    which returns the X.509 SubjectPublicKeyInfo (SPKI) format. We must use the same format.
//!
//! 2. **Key derivation**: CatShare uses `KeyAgreement.generateSecret("TlsPremasterSecret")`,
//!    which returns the raw ECDH shared secret (32 bytes) and does **not** use HKDF.
//!
//! 3. **AES IV**: this is the **ASCII bytes** of the string `"0102030405060708"`, not hexadecimal.

use aes::cipher::{KeyIvInit, StreamCipher};
use base64::{Engine as _, engine::general_purpose};
use log::{debug, trace, warn};
use p256::pkcs8::EncodePublicKey;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use p256::{PublicKey, ecdh::EphemeralSecret};
use rand::rngs::OsRng;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

/// Fixed IV used by CatShare: the ASCII bytes of the string "0102030405060708"
/// Actual bytes: [0x30, 0x31, 0x30, 0x32, 0x30, 0x33, 0x30, 0x34, 0x30, 0x35, 0x30, 0x36, 0x30, 0x37, 0x30, 0x38]
const AES_IV: &[u8; 16] = b"0102030405060708";

/// Generate a sender ID from random data.
fn sender_id_from_random_data(random_data: &[u8; 2]) -> String {
    format!("{:02x}{:02x}", random_data[0], random_data[1])
}

/// BLE security context - manages an ECDH key pair
///
/// # Lifecycle
///
/// Each `BleSecurity` instance holds an ephemeral ECDH key pair.
/// After calling `derive_session_key`, the private key is consumed (moved), and the instance can no longer be used.
///
/// # Compatibility with CatShare
///
/// - The public key is encoded in X.509 SPKI DER format, compatible with Java `ECPublicKey.getEncoded()`
/// - The private key is used for ECDH negotiation; the resulting shared secret is used directly for AES (no HKDF)
pub struct BleSecurity {
    secret: EphemeralSecret,
    public_key_b64: String,
}

/// Session cipher - performs AES encryption/decryption using a shared secret derived via ECDH
///
/// # Encryption Algorithm
///
/// - Algorithm: AES-256-CTR (NoPadding)
/// - IV: fixed ASCII string `"0102030405060708"` (16 bytes)
/// - Key: raw ECDH shared secret (32 bytes)
pub struct SessionCipher {
    key: [u8; 32],
}

impl BleSecurity {
    /// Generate a local ECDH key pair
    ///
    /// # Public Key Format
    ///
    /// The public key uses the X.509 SubjectPublicKeyInfo (SPKI) DER format,
    /// matching the format returned by Java `ECPublicKey.getEncoded()`.
    /// This is the key to ensuring interoperability with CatShare.
    ///
    /// # Errors
    ///
    /// Returns an error if SPKI encoding fails (very rare).
    pub fn new() -> anyhow::Result<Self> {
        let secret = EphemeralSecret::random(&mut OsRng);
        let public_key = secret.public_key();

        // Encode the public key using the X.509 SPKI DER format
        // This matches the format returned by Java ECPublicKey.getEncoded()
        let spki_der = public_key
            .to_public_key_der()
            .map_err(|e| anyhow::anyhow!("Failed to encode public key as SPKI: {}", e))?;
        let public_key_b64 = general_purpose::STANDARD.encode(spki_der.as_bytes());

        debug!(
            "Generated ECDH key pair, public key (SPKI) length: {} bytes",
            spki_der.as_bytes().len()
        );

        Ok(Self {
            secret,
            public_key_b64,
        })
    }

    /// Get the Base64-encoded public key (used for DeviceInfo.key)
    ///
    /// The returned string can be used directly in the DeviceInfo JSON in the BLE GATT STATUS characteristic.
    pub fn get_public_key(&self) -> &str {
        &self.public_key_b64
    }

    /// Derive the session key using the peer's public key
    ///
    /// # Parameters
    ///
    /// - `peer_pub_key_b64`: Base64 encoding of the peer's public key (SPKI or SEC1 format accepted)
    ///
    /// # Compatibility Notes
    ///
    /// - Supports parsing the X.509 SPKI format (Java ECPublicKey)
    /// - Also supports the SEC1 uncompressed format (65 bytes, 0x04 prefix) as a fallback
    /// - Uses the raw ECDH shared secret directly, with no HKDF processing
    ///
    /// # Consumption
    ///
    /// This method consumes `self`, because the ECDH private key should only be used once.
    pub fn derive_session_key(self, peer_pub_key_b64: &str) -> anyhow::Result<SessionCipher> {
        let peer_pub_bytes = general_purpose::STANDARD.decode(peer_pub_key_b64)?;

        trace!(
            "Parsing peer public key, length: {} bytes, first byte: 0x{:02x}",
            peer_pub_bytes.len(),
            peer_pub_bytes.first().unwrap_or(&0)
        );

        // Try parsing the public key in both formats
        let peer_public = Self::parse_public_key(&peer_pub_bytes)?;

        // ECDH key agreement
        let shared_secret = self.secret.diffie_hellman(&peer_public);

        // **Key point**: use the raw shared secret directly, with no HKDF processing
        // CatShare's Java code: agreement.generateSecret("TlsPremasterSecret")
        // returns the 32-byte raw ECDH shared secret
        let raw_secret = shared_secret.raw_secret_bytes();

        let mut key = [0u8; 32];
        key.copy_from_slice(raw_secret.as_slice());

        debug!("ECDH key agreement completed, derived 32-byte session key");

        Ok(SessionCipher { key })
    }

    /// Parse the peer's public key (supports SPKI and SEC1 formats)
    fn parse_public_key(bytes: &[u8]) -> anyhow::Result<PublicKey> {
        // First try the SPKI format (Java ECPublicKey.getEncoded())
        // The SPKI format usually starts with 0x30 (SEQUENCE)
        if bytes.first() == Some(&0x30) {
            use p256::pkcs8::DecodePublicKey;
            if let Ok(pk) = PublicKey::from_public_key_der(bytes) {
                trace!("Parsed public key as X.509 SPKI format");
                return Ok(pk);
            }
        }

        // Then try the SEC1 uncompressed format (65 bytes, 0x04 prefix)
        if bytes.len() == 65
            && bytes[0] == 0x04
            && let Ok(pk) = PublicKey::from_sec1_bytes(bytes)
        {
            trace!("Parsed public key as SEC1 uncompressed format");
            return Ok(pk);
        }

        // Finally try as raw SPKI (some implementations may not start with 0x30)
        use p256::pkcs8::DecodePublicKey;
        PublicKey::from_public_key_der(bytes)
            .map_err(|e| anyhow::anyhow!("Invalid public key format: {}", e))
    }
}

/// Session cipher reference (for borrowed scenarios)
pub struct SessionCipherRef<'a> {
    key: &'a [u8; 32],
}

impl SessionCipher {
    /// Create a session cipher from a raw key (for testing)
    #[cfg(test)]
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Get a borrowed cipher reference
    pub fn as_ref(&self) -> SessionCipherRef<'_> {
        SessionCipherRef { key: &self.key }
    }

    /// Encrypt data using AES-256-CTR
    ///
    /// # Parameters
    ///
    /// - `data`: the plaintext string to encrypt
    ///
    /// # Returns
    ///
    /// Base64-encoded ciphertext, ready for JSON transport.
    ///
    /// # Compatibility
    ///
    /// - Algorithm: AES/CTR/NoPadding (consistent with Java Cipher)
    /// - IV: the ASCII bytes of the string "0102030405060708"
    pub fn encrypt(&self, data: &str) -> anyhow::Result<String> {
        let mut buffer = data.as_bytes().to_vec();

        let mut cipher = Aes256Ctr::new(&self.key.into(), AES_IV.into());
        cipher.apply_keystream(&mut buffer);

        let result = general_purpose::STANDARD.encode(buffer);
        trace!(
            "Encrypted {} bytes -> {} bytes base64",
            data.len(),
            result.len()
        );
        Ok(result)
    }

    /// Decrypt data using AES-256-CTR
    ///
    /// # Parameters
    ///
    /// - `encoded_data`: Base64-encoded ciphertext
    ///
    /// # Returns
    ///
    /// The decrypted plaintext string.
    ///
    /// # Errors
    ///
    /// - Base64 decoding failure
    /// - The decrypted data is not valid UTF-8
    pub fn decrypt(&self, encoded_data: &str) -> anyhow::Result<String> {
        let mut buffer = general_purpose::STANDARD.decode(encoded_data)?;

        let mut cipher = Aes256Ctr::new(&self.key.into(), AES_IV.into());
        cipher.apply_keystream(&mut buffer);

        let result = String::from_utf8(buffer)?;
        trace!(
            "Decrypted {} bytes base64 -> {} chars",
            encoded_data.len(),
            result.len()
        );
        Ok(result)
    }
}

impl SessionCipherRef<'_> {
    /// Encrypt data (borrowed version)
    pub fn encrypt(&self, data: &str) -> anyhow::Result<String> {
        let mut buffer = data.as_bytes().to_vec();
        let mut cipher = Aes256Ctr::new(self.key.into(), AES_IV.into());
        cipher.apply_keystream(&mut buffer);
        Ok(general_purpose::STANDARD.encode(buffer))
    }

    /// Decrypt data (borrowed version)
    pub fn decrypt(&self, encoded_data: &str) -> anyhow::Result<String> {
        let mut buffer = general_purpose::STANDARD.decode(encoded_data)?;
        let mut cipher = Aes256Ctr::new(self.key.into(), AES_IV.into());
        cipher.apply_keystream(&mut buffer);
        String::from_utf8(buffer).map_err(Into::into)
    }
}

/// Persistent BLE security context - supports deriving session keys multiple times
///
/// Unlike `BleSecurity`, this type uses `SecretKey` instead of `EphemeralSecret`,
/// so `derive_session_key` can be called multiple times without consuming the instance.
///
/// # Compatibility with CatShare
///
/// CatShare uses a singleton `BleSecurity` and can call `deriveSessionKey` multiple times.
/// This type models that behavior and is suitable for:
/// - The receiver's GATT Server (needs to hold the key pair while waiting for multiple connections)
/// - Scenarios that need to verify multiple senders
///
/// # Usage Example
///
/// ```ignore
/// let security = BleSecurityPersistent::new()?;
/// let public_key = security.get_public_key().to_string();
///
/// // The session key can be derived multiple times
/// let cipher1 = security.derive_session_key(&sender1_key)?;
/// let cipher2 = security.derive_session_key(&sender2_key)?;
/// ```
pub struct BleSecurityPersistent {
    secret_key: p256::SecretKey,
    public_key_b64: String,
    /// Sender ID: the peer uses it to identify this device.
    sender_id: String,
}

impl BleSecurityPersistent {
    /// Generate a new persistent ECDH key pair
    pub fn new() -> anyhow::Result<Self> {
        let secret_key = p256::SecretKey::random(&mut OsRng);
        let public_key = secret_key.public_key();

        // Encode the public key using the X.509 SPKI DER format
        let spki_der = public_key
            .to_public_key_der()
            .map_err(|e| anyhow::anyhow!("Failed to encode public key as SPKI: {}", e))?;
        let public_key_b64 = general_purpose::STANDARD.encode(spki_der.as_bytes());

        debug!(
            "Generated persistent ECDH key pair, public key (SPKI) length: {} bytes",
            spki_der.as_bytes().len()
        );

        Ok(Self {
            secret_key,
            public_key_b64,
            sender_id: sender_id_from_random_data(&rand::random::<[u8; 2]>()),
        })
    }

    /// Load or create a persistent identity.
    ///
    /// The identity consists of two parts: an ECDH key pair and a sender ID. Both must remain
    /// unchanged across restarts, otherwise the peer will treat this device as a new device
    /// (the alliance's clients deduplicate devices by the identity in the broadcast).
    pub fn load_or_create(path: &std::path::Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => match Self::parse(&contents) {
                Some(identity) => {
                    debug!("Loaded the persistent BLE identity from {}", path.display());
                    Ok(identity)
                }
                None => {
                    warn!(
                        "the stored BLE identity at {} is unusable, creating a new one",
                        path.display()
                    );
                    let identity = Self::new()?;
                    identity.store(path)?;
                    Ok(identity)
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::new()?;
                identity.store(path)?;
                Ok(identity)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Parse the identity file: first the Base64 private key DER, then the sender ID.
    fn parse(contents: &str) -> Option<Self> {
        let mut lines = contents.lines();
        let secret_key_b64 = lines.next()?.trim();
        let sender_id = lines.next()?.trim().to_owned();

        if sender_id.is_empty() {
            return None;
        }

        let der = general_purpose::STANDARD.decode(secret_key_b64).ok()?;
        let secret_key = p256::SecretKey::from_pkcs8_der(&der).ok()?;
        let spki_der = secret_key
            .public_key()
            .to_public_key_der()
            .ok()?
            .as_bytes()
            .to_vec();

        Some(Self {
            secret_key,
            public_key_b64: general_purpose::STANDARD.encode(spki_der),
            sender_id,
        })
    }

    /// Write the identity to a file that only the owner can read.
    fn store(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let der = self.secret_key.to_pkcs8_der()?;
        let contents = format!(
            "{}\n{}\n",
            general_purpose::STANDARD.encode(der.as_bytes()),
            self.sender_id
        );

        // Write a temporary file first, then rename it, so an interruption never leaves a half-written file.
        let temp = path.with_extension("tmp");
        {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = file.metadata()?.permissions();
                perms.set_mode(0o600);
                file.set_permissions(perms)?;
            }
        }

        std::fs::rename(&temp, path)?;
        Ok(())
    }

    /// Sender ID: the peer uses it to identify this device; it must remain unchanged across restarts.
    pub fn sender_id(&self) -> &str {
        &self.sender_id
    }

    /// Create an identity with the given ID (on first creation).
    pub fn with_sender_id(mut self, sender_id: &str) -> Self {
        self.sender_id = sender_id.to_owned();
        self
    }

    /// Get the Base64-encoded public key
    pub fn get_public_key(&self) -> &str {
        &self.public_key_b64
    }

    /// Derive the session key using the peer's public key (does not consume self)
    ///
    /// Unlike `BleSecurity::derive_session_key`, this method borrows `&self`
    /// and can be called multiple times.
    pub fn derive_session_key(&self, peer_pub_key_b64: &str) -> anyhow::Result<SessionCipher> {
        let peer_pub_bytes = general_purpose::STANDARD.decode(peer_pub_key_b64)?;

        trace!(
            "Parsing peer public key, length: {} bytes, first byte: 0x{:02x}",
            peer_pub_bytes.len(),
            peer_pub_bytes.first().unwrap_or(&0)
        );

        // Parse the peer's public key
        let peer_public = BleSecurity::parse_public_key(&peer_pub_bytes)?;

        // Use the diffie_hellman function (does not consume the private key)
        let shared_secret = p256::ecdh::diffie_hellman(
            self.secret_key.to_nonzero_scalar(),
            peer_public.as_affine(),
        );

        // Use the raw shared secret directly
        let raw_secret = shared_secret.raw_secret_bytes();

        let mut key = [0u8; 32];
        key.copy_from_slice(raw_secret.as_slice());

        debug!("ECDH key agreement completed (persistent), derived 32-byte session key");

        Ok(SessionCipher { key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the AES IV is the byte representation of the ASCII string "0102030405060708"
    #[test]
    fn test_aes_iv_is_ascii() {
        assert_eq!(AES_IV, b"0102030405060708");
        assert_eq!(AES_IV[0], 0x30); // '0'
        assert_eq!(AES_IV[1], 0x31); // '1'
        assert_eq!(AES_IV[2], 0x30); // '0'
        assert_eq!(AES_IV[3], 0x32); // '2'
    }

    /// Test the encrypt/decrypt round trip
    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = [0u8; 32];
        let cipher = SessionCipher::new(key);

        let plaintext = "Hello, 互传联盟!";
        let encrypted = cipher.encrypt(plaintext).unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    /// Verify the public key format is SPKI (X.509)
    #[test]
    fn test_public_key_is_spki_format() {
        let security = BleSecurity::new().unwrap();
        let pub_key_b64 = security.get_public_key();
        let pub_key_bytes = general_purpose::STANDARD.decode(pub_key_b64).unwrap();

        // The SPKI format should start with 0x30 (SEQUENCE)
        assert_eq!(
            pub_key_bytes[0], 0x30,
            "Public key should be SPKI format (starts with 0x30)"
        );

        // A P-256 public key in SPKI format is usually 91 bytes
        // Structure: SEQUENCE { SEQUENCE { OID, OID }, BIT STRING { 0x04 ... } }
        assert!(
            pub_key_bytes.len() >= 88 && pub_key_bytes.len() <= 92,
            "SPKI P-256 public key should be ~91 bytes, got {}",
            pub_key_bytes.len()
        );
    }

    /// Test ECDH key agreement
    #[test]
    fn test_ecdh_key_agreement() {
        // Create two key pairs
        let alice = BleSecurity::new().unwrap();
        let bob = BleSecurity::new().unwrap();

        let alice_pub = alice.get_public_key().to_string();
        let bob_pub = bob.get_public_key().to_string();

        // Each side derives the session key
        let alice_cipher = alice.derive_session_key(&bob_pub).unwrap();
        let bob_cipher = bob.derive_session_key(&alice_pub).unwrap();

        // Verify the shared keys are identical
        assert_eq!(
            alice_cipher.key, bob_cipher.key,
            "Shared secret should be identical"
        );
    }

    /// Test multiple derivations with a persistent security context
    #[test]
    fn test_persistent_security() {
        let persistent = BleSecurityPersistent::new().unwrap();
        let alice = BleSecurity::new().unwrap();
        let bob = BleSecurity::new().unwrap();

        let persistent_pub = persistent.get_public_key().to_string();
        let alice_pub = alice.get_public_key().to_string();
        let bob_pub = bob.get_public_key().to_string();

        // Persistent negotiates with Alice
        let cipher_p_alice = persistent.derive_session_key(&alice_pub).unwrap();
        let cipher_alice_p = alice.derive_session_key(&persistent_pub).unwrap();
        assert_eq!(cipher_p_alice.key, cipher_alice_p.key);

        // Persistent negotiates with Bob (second use)
        let cipher_p_bob = persistent.derive_session_key(&bob_pub).unwrap();
        let cipher_bob_p = bob.derive_session_key(&persistent_pub).unwrap();
        assert_eq!(cipher_p_bob.key, cipher_bob_p.key);

        // Verify the two derived keys differ (because the peers differ)
        assert_ne!(cipher_p_alice.key, cipher_p_bob.key);
    }

    /// Test cross-format public key compatibility
    #[test]
    fn test_parse_sec1_public_key() {
        // Create a key pair and get its SPKI-format public key
        let security = BleSecurity::new().unwrap();
        let spki_b64 = security.get_public_key();
        let spki_bytes = general_purpose::STANDARD.decode(spki_b64).unwrap();

        // Verify it can be parsed
        let parsed = BleSecurity::parse_public_key(&spki_bytes);
        assert!(parsed.is_ok(), "Should parse SPKI format");
    }

    /// The identity must remain unchanged across restarts, otherwise the peer will treat this device as a new device.
    #[test]
    fn test_identity_survives_a_reload() {
        let dir =
            std::env::temp_dir().join(format!("satella-mta-identity-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity");

        let first = BleSecurityPersistent::load_or_create(&path).unwrap();
        let first_sender_id = first.sender_id().to_owned();
        let first_public_key = first.get_public_key().to_owned();

        let second = BleSecurityPersistent::load_or_create(&path).unwrap();
        assert_eq!(
            first_sender_id,
            second.sender_id(),
            "the sender ID has to survive a reload"
        );
        assert_eq!(
            first_public_key,
            second.get_public_key(),
            "the public key has to survive a reload"
        );

        // Session keys derived from the same identity must match the previous ones,
        // otherwise the peer will think the key has changed.
        let peer = BleSecurity::new().unwrap();
        assert_eq!(
            first.derive_session_key(peer.get_public_key()).unwrap().key,
            second
                .derive_session_key(peer.get_public_key())
                .unwrap()
                .key
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Test encrypting empty data
    #[test]
    fn test_encrypt_empty_string() {
        let key = [42u8; 32];
        let cipher = SessionCipher::new(key);

        let encrypted = cipher.encrypt("").unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();

        assert_eq!("", decrypted);
    }

    /// Test encrypting large data
    #[test]
    fn test_encrypt_large_data() {
        let key = [0xAB; 32];
        let cipher = SessionCipher::new(key);

        let plaintext = "A".repeat(10000);
        let encrypted = cipher.encrypt(&plaintext).unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    /// Test encrypting Unicode data
    #[test]
    fn test_encrypt_unicode() {
        let key = [0xCD; 32];
        let cipher = SessionCipher::new(key);

        let plaintext = "中文测试 🎉 日本語 العربية";
        let encrypted = cipher.encrypt(plaintext).unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();

        assert_eq!(plaintext, decrypted);
    }
}
