//! The device identity: the certificate that proves this device to its peers.
//!
//! LocalSend identifies a device by the SHA-256 fingerprint of its
//! certificate. Peers deduplicate by that fingerprint, so the certificate has
//! to survive a restart: a fresh certificate on every launch makes every
//! other LocalSend client show this device as a new one, over and over.
//!
//! The identity is a PEM file holding the private key, the certificate and
//! the public key. Upstream LocalSend stores the same thing in its
//! preferences and reloads it on every start.

use std::path::Path;

use satella_localsend_vendor::crypto::cert::{generate_self_signed, SelfSignedCert};

/// Loads the identity from `path`, or creates and stores a new one.
///
/// A file that cannot be read or parsed is replaced by a fresh identity: a
/// broken file must not keep the device from starting. The peers see a new
/// fingerprint in that case, which is the same as an identity that was never
/// stored.
pub fn load_or_create(path: &Path) -> anyhow::Result<SelfSignedCert> {
    match std::fs::read_to_string(path) {
        Ok(contents) => match parse(&contents) {
            Some(identity) => Ok(identity),
            None => {
                tracing::warn!(
                    "the stored identity at {} is unusable, creating a new one",
                    path.display()
                );
                let identity = generate_self_signed()?;
                store(path, &identity)?;
                Ok(identity)
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let identity = generate_self_signed()?;
            store(path, &identity)?;
            Ok(identity)
        }
        Err(err) => {
            // The directory may not exist yet, or may not be writable. That
            // must not stop the device from running, it only means the
            // identity does not survive a restart.
            tracing::warn!("could not read the identity at {}: {err}", path.display());
            let identity = generate_self_signed()?;
            let _ = store(path, &identity);
            Ok(identity)
        }
    }
}

/// Parses a stored identity file.
fn parse(contents: &str) -> Option<SelfSignedCert> {
    let mut private_key_pem = String::new();
    let mut certificate_pem = String::new();
    let mut public_key_pem = String::new();

    for section in pem::parse_many(contents).ok()? {
        let tag = section.tag().to_owned();
        let der = section.into_contents();
        // LF, the line ending the vendor writes, so that reloading an
        // identity gives back exactly what was stored.
        let encoded = pem::encode_config(
            &pem::Pem::new(tag.clone(), der),
            pem::EncodeConfig::new().set_line_ending(pem::LineEnding::LF),
        );
        match tag.as_str() {
            "PRIVATE KEY" => private_key_pem = encoded,
            "CERTIFICATE" => certificate_pem = encoded,
            "PUBLIC KEY" => public_key_pem = encoded,
            _ => {}
        }
    }

    if private_key_pem.is_empty() || certificate_pem.is_empty() {
        return None;
    }

    // The fingerprint is the SHA-256 of the certificate in DER form, which is
    // what the vendor computes from the certificate it generates.
    let fingerprint = fingerprint_of(&certificate_pem)?;

    Some(SelfSignedCert {
        private_key_pem,
        public_key_pem,
        certificate_pem,
        fingerprint,
    })
}

/// The SHA-256 fingerprint of a PEM-encoded certificate.
fn fingerprint_of(certificate_pem: &str) -> Option<String> {
    use sha2::{Digest, Sha256};

    // The DER form is what the vendor hashes, and it is what the PEM encodes.
    let der = pem::parse(certificate_pem).ok()?.into_contents();

    let mut hasher = Sha256::new();
    hasher.update(&der);
    Some(
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect(),
    )
}

/// Writes the identity to `path`.
///
/// The write goes to a temporary file that is then renamed, so a reader
/// never sees a half-written identity, even when two instances start at the
/// same time.
fn store(path: &Path, identity: &SelfSignedCert) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let contents = format!(
        "{}{}{}",
        identity.private_key_pem, identity.certificate_pem, identity.public_key_pem
    );

    // The file holds the private key, so it is readable by its owner only.
    let temporary = path.with_extension("pem.new");
    std::fs::write(&temporary, contents)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }

    std::fs::rename(&temporary, path)?;

    Ok(())
}
