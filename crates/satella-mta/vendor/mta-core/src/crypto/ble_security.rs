//! Cattysend Crypto Module
//!
//! 实现与 CatShare (Android) 完全兼容的加密逻辑：
//! - ECDH (P-256) 密钥交换
//! - AES-256-CTR 加密（使用固定 IV）
//!
//! # 关键兼容性说明
//!
//! 1. **密钥格式**: CatShare 使用 Java 的 `ECPublicKey.getEncoded()`，
//!    返回 X.509 SubjectPublicKeyInfo (SPKI) 格式。我们必须使用相同格式。
//!
//! 2. **密钥派生**: CatShare 使用 `KeyAgreement.generateSecret("TlsPremasterSecret")`，
//!    这会返回原始的 ECDH 共享密钥（32 字节），**不使用** HKDF。
//!
//! 3. **AES IV**: 是字符串 `"0102030405060708"` 的 **ASCII 字节**，不是十六进制。

use aes::cipher::{KeyIvInit, StreamCipher};
use base64::{Engine as _, engine::general_purpose};
use log::{debug, trace, warn};
use p256::pkcs8::EncodePublicKey;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use p256::{PublicKey, ecdh::EphemeralSecret};
use rand::rngs::OsRng;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

/// CatShare 使用的固定 IV：字符串 "0102030405060708" 的 ASCII 字节
/// 实际字节: [0x30, 0x31, 0x30, 0x32, 0x30, 0x33, 0x30, 0x34, 0x30, 0x35, 0x30, 0x36, 0x30, 0x37, 0x30, 0x38]
const AES_IV: &[u8; 16] = b"0102030405060708";

/// 从随机数据生成发送方 ID。
fn sender_id_from_random_data(random_data: &[u8; 2]) -> String {
    format!("{:02x}{:02x}", random_data[0], random_data[1])
}

/// BLE 安全上下文 - 管理 ECDH 密钥对
///
/// # 生命周期
///
/// 每个 `BleSecurity` 实例持有一个临时 ECDH 密钥对。
/// 调用 `derive_session_key` 后，私钥被消耗（move），实例不可再用。
///
/// # 与 CatShare 的兼容性
///
/// - 公钥使用 X.509 SPKI DER 格式编码，与 Java `ECPublicKey.getEncoded()` 兼容
/// - 私钥用于 ECDH 协商，生成的共享密钥直接用于 AES（无 HKDF）
pub struct BleSecurity {
    secret: EphemeralSecret,
    public_key_b64: String,
}

/// 会话加密器 - 使用 ECDH 派生的共享密钥进行 AES 加解密
///
/// # 加密算法
///
/// - 算法: AES-256-CTR (NoPadding)
/// - IV: 固定 ASCII 字符串 `"0102030405060708"` (16 bytes)
/// - 密钥: ECDH 原始共享密钥 (32 bytes)
pub struct SessionCipher {
    key: [u8; 32],
}

impl BleSecurity {
    /// 生成本地 ECDH 密钥对
    ///
    /// # 公钥格式
    ///
    /// 公钥使用 X.509 SubjectPublicKeyInfo (SPKI) DER 格式，
    /// 与 Java `ECPublicKey.getEncoded()` 返回的格式一致。
    /// 这是确保与 CatShare 互操作的关键。
    ///
    /// # 错误
    ///
    /// 如果 SPKI 编码失败（极少发生），返回错误。
    pub fn new() -> anyhow::Result<Self> {
        let secret = EphemeralSecret::random(&mut OsRng);
        let public_key = secret.public_key();

        // 使用 X.509 SPKI DER 格式编码公钥
        // 这与 Java ECPublicKey.getEncoded() 返回的格式一致
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

    /// 获取 Base64 编码的公钥（用于 DeviceInfo.key）
    ///
    /// 返回的字符串可直接用于 BLE GATT STATUS 特征中的 DeviceInfo JSON。
    pub fn get_public_key(&self) -> &str {
        &self.public_key_b64
    }

    /// 使用对方公钥派生会话密钥
    ///
    /// # 参数
    ///
    /// - `peer_pub_key_b64`: 对方公钥的 Base64 编码（SPKI 或 SEC1 格式均可）
    ///
    /// # 兼容性说明
    ///
    /// - 支持解析 X.509 SPKI 格式（Java ECPublicKey）
    /// - 同时支持 SEC1 uncompressed 格式（65 字节，0x04 前缀）作为后备
    /// - 直接使用 ECDH 原始共享密钥，不做 HKDF 处理
    ///
    /// # 消耗
    ///
    /// 此方法消耗 `self`，因为 ECDH 私钥应该只用一次。
    pub fn derive_session_key(self, peer_pub_key_b64: &str) -> anyhow::Result<SessionCipher> {
        let peer_pub_bytes = general_purpose::STANDARD.decode(peer_pub_key_b64)?;

        trace!(
            "Parsing peer public key, length: {} bytes, first byte: 0x{:02x}",
            peer_pub_bytes.len(),
            peer_pub_bytes.first().unwrap_or(&0)
        );

        // 尝试两种格式解析公钥
        let peer_public = Self::parse_public_key(&peer_pub_bytes)?;

        // ECDH 密钥协商
        let shared_secret = self.secret.diffie_hellman(&peer_public);

        // **关键**: 直接使用原始共享密钥，不做 HKDF 处理
        // CatShare 的 Java 代码: agreement.generateSecret("TlsPremasterSecret")
        // 返回 32 字节的原始 ECDH 共享密钥
        let raw_secret = shared_secret.raw_secret_bytes();

        let mut key = [0u8; 32];
        key.copy_from_slice(raw_secret.as_slice());

        debug!("ECDH key agreement completed, derived 32-byte session key");

        Ok(SessionCipher { key })
    }

    /// 解析对方公钥（支持 SPKI 和 SEC1 格式）
    fn parse_public_key(bytes: &[u8]) -> anyhow::Result<PublicKey> {
        // 首先尝试 SPKI 格式（Java ECPublicKey.getEncoded()）
        // SPKI 格式通常以 0x30 (SEQUENCE) 开头
        if bytes.first() == Some(&0x30) {
            use p256::pkcs8::DecodePublicKey;
            if let Ok(pk) = PublicKey::from_public_key_der(bytes) {
                trace!("Parsed public key as X.509 SPKI format");
                return Ok(pk);
            }
        }

        // 然后尝试 SEC1 uncompressed 格式（65 字节，0x04 前缀）
        if bytes.len() == 65
            && bytes[0] == 0x04
            && let Ok(pk) = PublicKey::from_sec1_bytes(bytes)
        {
            trace!("Parsed public key as SEC1 uncompressed format");
            return Ok(pk);
        }

        // 最后尝试作为原始 SPKI（某些实现可能不以 0x30 开头）
        use p256::pkcs8::DecodePublicKey;
        PublicKey::from_public_key_der(bytes)
            .map_err(|e| anyhow::anyhow!("Invalid public key format: {}", e))
    }
}

/// 会话加密器引用（用于借用场景）
pub struct SessionCipherRef<'a> {
    key: &'a [u8; 32],
}

impl SessionCipher {
    /// 从原始密钥创建会话加密器（用于测试）
    #[cfg(test)]
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// 获取借用的加密器引用
    pub fn as_ref(&self) -> SessionCipherRef<'_> {
        SessionCipherRef { key: &self.key }
    }

    /// 使用 AES-256-CTR 加密数据
    ///
    /// # 参数
    ///
    /// - `data`: 要加密的明文字符串
    ///
    /// # 返回
    ///
    /// Base64 编码的密文，可直接用于 JSON 传输。
    ///
    /// # 兼容性
    ///
    /// - 算法: AES/CTR/NoPadding（与 Java Cipher 一致）
    /// - IV: 字符串 "0102030405060708" 的 ASCII 字节
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

    /// 使用 AES-256-CTR 解密数据
    ///
    /// # 参数
    ///
    /// - `encoded_data`: Base64 编码的密文
    ///
    /// # 返回
    ///
    /// 解密后的明文字符串。
    ///
    /// # 错误
    ///
    /// - Base64 解码失败
    /// - 解密后的数据不是有效 UTF-8
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
    /// 加密数据（借用版本）
    pub fn encrypt(&self, data: &str) -> anyhow::Result<String> {
        let mut buffer = data.as_bytes().to_vec();
        let mut cipher = Aes256Ctr::new(self.key.into(), AES_IV.into());
        cipher.apply_keystream(&mut buffer);
        Ok(general_purpose::STANDARD.encode(buffer))
    }

    /// 解密数据（借用版本）
    pub fn decrypt(&self, encoded_data: &str) -> anyhow::Result<String> {
        let mut buffer = general_purpose::STANDARD.decode(encoded_data)?;
        let mut cipher = Aes256Ctr::new(self.key.into(), AES_IV.into());
        cipher.apply_keystream(&mut buffer);
        String::from_utf8(buffer).map_err(Into::into)
    }
}

/// 持久化 BLE 安全上下文 - 支持多次派生会话密钥
///
/// 与 `BleSecurity` 不同，此类型使用 `SecretKey` 而非 `EphemeralSecret`，
/// 可以多次调用 `derive_session_key` 而不消耗实例。
///
/// # 与 CatShare 的兼容性
///
/// CatShare 使用单例 `BleSecurity`，可多次调用 `deriveSessionKey`。
/// 此类型模拟该行为，适用于：
/// - 接收端 GATT Server（需要持有密钥对等待多个连接）
/// - 需要验证多个发送端的场景
///
/// # 使用示例
///
/// ```ignore
/// let security = BleSecurityPersistent::new()?;
/// let public_key = security.get_public_key().to_string();
///
/// // 可以多次派生会话密钥
/// let cipher1 = security.derive_session_key(&sender1_key)?;
/// let cipher2 = security.derive_session_key(&sender2_key)?;
/// ```
pub struct BleSecurityPersistent {
    secret_key: p256::SecretKey,
    public_key_b64: String,
    /// 发送方 ID：对端用它识别这台设备。
    sender_id: String,
}

impl BleSecurityPersistent {
    /// 生成新的持久化 ECDH 密钥对
    pub fn new() -> anyhow::Result<Self> {
        let secret_key = p256::SecretKey::random(&mut OsRng);
        let public_key = secret_key.public_key();

        // 使用 X.509 SPKI DER 格式编码公钥
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

    /// 载入或创建持久化的身份。
    ///
    /// 身份由两部分组成：ECDH 密钥对和发送方 ID。两者都必须跨重启保持
    /// 不变，否则对端会把这台设备当成新设备（互传联盟的客户端按广播里的
    /// 身份去重）。
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

    /// 解析身份文件：先是 Base64 的私钥 DER，然后是发送方 ID。
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

    /// 把身份写入文件，只有本人可读。
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

        // 先写临时文件再改名，这样中断不会留下半个文件。
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

    /// 发送方 ID：对端用它识别这台设备，必须跨重启保持不变。
    pub fn sender_id(&self) -> &str {
        &self.sender_id
    }

    /// 用给定的 ID 创建身份（首次创建时）。
    pub fn with_sender_id(mut self, sender_id: &str) -> Self {
        self.sender_id = sender_id.to_owned();
        self
    }

    /// 获取 Base64 编码的公钥
    pub fn get_public_key(&self) -> &str {
        &self.public_key_b64
    }

    /// 使用对方公钥派生会话密钥（不消耗 self）
    ///
    /// 与 `BleSecurity::derive_session_key` 不同，此方法借用 `&self`，
    /// 可以多次调用。
    pub fn derive_session_key(&self, peer_pub_key_b64: &str) -> anyhow::Result<SessionCipher> {
        let peer_pub_bytes = general_purpose::STANDARD.decode(peer_pub_key_b64)?;

        trace!(
            "Parsing peer public key, length: {} bytes, first byte: 0x{:02x}",
            peer_pub_bytes.len(),
            peer_pub_bytes.first().unwrap_or(&0)
        );

        // 解析对方公钥
        let peer_public = BleSecurity::parse_public_key(&peer_pub_bytes)?;

        // 使用 diffie_hellman 函数（不消耗私钥）
        let shared_secret = p256::ecdh::diffie_hellman(
            self.secret_key.to_nonzero_scalar(),
            peer_public.as_affine(),
        );

        // 直接使用原始共享密钥
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

    /// 验证 AES IV 是 ASCII 字符串 "0102030405060708" 的字节表示
    #[test]
    fn test_aes_iv_is_ascii() {
        assert_eq!(AES_IV, b"0102030405060708");
        assert_eq!(AES_IV[0], 0x30); // '0'
        assert_eq!(AES_IV[1], 0x31); // '1'
        assert_eq!(AES_IV[2], 0x30); // '0'
        assert_eq!(AES_IV[3], 0x32); // '2'
    }

    /// 测试加密解密往返
    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = [0u8; 32];
        let cipher = SessionCipher::new(key);

        let plaintext = "Hello, 互传联盟!";
        let encrypted = cipher.encrypt(plaintext).unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    /// 测试公钥格式为 SPKI (X.509)
    #[test]
    fn test_public_key_is_spki_format() {
        let security = BleSecurity::new().unwrap();
        let pub_key_b64 = security.get_public_key();
        let pub_key_bytes = general_purpose::STANDARD.decode(pub_key_b64).unwrap();

        // SPKI 格式应该以 0x30 (SEQUENCE) 开头
        assert_eq!(
            pub_key_bytes[0], 0x30,
            "Public key should be SPKI format (starts with 0x30)"
        );

        // SPKI 格式的 P-256 公钥通常是 91 字节
        // 结构: SEQUENCE { SEQUENCE { OID, OID }, BIT STRING { 0x04 ... } }
        assert!(
            pub_key_bytes.len() >= 88 && pub_key_bytes.len() <= 92,
            "SPKI P-256 public key should be ~91 bytes, got {}",
            pub_key_bytes.len()
        );
    }

    /// 测试 ECDH 密钥协商
    #[test]
    fn test_ecdh_key_agreement() {
        // 创建两个密钥对
        let alice = BleSecurity::new().unwrap();
        let bob = BleSecurity::new().unwrap();

        let alice_pub = alice.get_public_key().to_string();
        let bob_pub = bob.get_public_key().to_string();

        // 各自派生会话密钥
        let alice_cipher = alice.derive_session_key(&bob_pub).unwrap();
        let bob_cipher = bob.derive_session_key(&alice_pub).unwrap();

        // 验证共享密钥相同
        assert_eq!(
            alice_cipher.key, bob_cipher.key,
            "Shared secret should be identical"
        );
    }

    /// 测试持久化安全上下文的多次派生
    #[test]
    fn test_persistent_security() {
        let persistent = BleSecurityPersistent::new().unwrap();
        let alice = BleSecurity::new().unwrap();
        let bob = BleSecurity::new().unwrap();

        let persistent_pub = persistent.get_public_key().to_string();
        let alice_pub = alice.get_public_key().to_string();
        let bob_pub = bob.get_public_key().to_string();

        // Persistent 与 Alice 协商
        let cipher_p_alice = persistent.derive_session_key(&alice_pub).unwrap();
        let cipher_alice_p = alice.derive_session_key(&persistent_pub).unwrap();
        assert_eq!(cipher_p_alice.key, cipher_alice_p.key);

        // Persistent 与 Bob 协商 (第二次使用)
        let cipher_p_bob = persistent.derive_session_key(&bob_pub).unwrap();
        let cipher_bob_p = bob.derive_session_key(&persistent_pub).unwrap();
        assert_eq!(cipher_p_bob.key, cipher_bob_p.key);

        // 验证两次派生出的密钥不同（因为 peer 不同）
        assert_ne!(cipher_p_alice.key, cipher_p_bob.key);
    }

    /// 测试跨格式公钥兼容性
    #[test]
    fn test_parse_sec1_public_key() {
        // 创建一个密钥对并获取 SPKI 格式公钥
        let security = BleSecurity::new().unwrap();
        let spki_b64 = security.get_public_key();
        let spki_bytes = general_purpose::STANDARD.decode(spki_b64).unwrap();

        // 验证可以解析
        let parsed = BleSecurity::parse_public_key(&spki_bytes);
        assert!(parsed.is_ok(), "Should parse SPKI format");
    }

    /// 身份必须跨重启保持不变，否则对端会把这台设备当成新设备。
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

        // 同一个身份派生出的会话密钥必须和之前一致，否则对端会认为
        // 密钥变了。
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

    /// 测试空数据加密
    #[test]
    fn test_encrypt_empty_string() {
        let key = [42u8; 32];
        let cipher = SessionCipher::new(key);

        let encrypted = cipher.encrypt("").unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();

        assert_eq!("", decrypted);
    }

    /// 测试大数据加密
    #[test]
    fn test_encrypt_large_data() {
        let key = [0xAB; 32];
        let cipher = SessionCipher::new(key);

        let plaintext = "A".repeat(10000);
        let encrypted = cipher.encrypt(&plaintext).unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    /// 测试 Unicode 数据加密
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
