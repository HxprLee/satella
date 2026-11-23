//! The device identity has to survive a restart.
//!
//! Peers deduplicate by the certificate fingerprint, so a fresh certificate
//! on every launch makes every LocalSend client show this device as a new
//! one, over and over. This is the check that fails if the identity is
//! regenerated instead of loaded.

use std::path::PathBuf;

use satella_localsend::identity;

/// A temporary directory that cleans itself up.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("satella-identity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the temp dir");
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The fingerprint of a stored identity is the same on every load.
#[test]
fn the_identity_survives_a_restart() {
    let dir = TempDir::new("restart");
    let path = dir.file("identity.pem");

    let first = identity::load_or_create(&path).expect("the first load");
    let second = identity::load_or_create(&path).expect("the second load");

    assert_eq!(
        first.fingerprint, second.fingerprint,
        "the fingerprint changed on reload, so every peer sees a new device"
    );
    assert_eq!(first.certificate_pem, second.certificate_pem);
    assert_eq!(first.private_key_pem, second.private_key_pem);
}

/// The fingerprint of a loaded identity is the one the peer sees.
///
/// The fingerprint has to be the SHA-256 of the certificate, computed the
/// same way the vendor computes it, or the peers deduplicate by a different
/// value than we announce.
#[test]
fn the_loaded_fingerprint_matches_the_certificate() {
    let dir = TempDir::new("fingerprint");
    let path = dir.file("identity.pem");

    let stored = identity::load_or_create(&path).expect("the load");

    let expected = satella_localsend_vendor::crypto::cert::fingerprint_from_cert_der(
        &pem::parse(&stored.certificate_pem)
            .expect("the certificate parses")
            .into_contents(),
    );

    assert_eq!(
        stored.fingerprint, expected,
        "the loaded fingerprint does not match the certificate"
    );
}

/// A file that is not a valid identity is replaced, not fatal.
#[test]
fn a_broken_identity_file_is_replaced() {
    let dir = TempDir::new("broken");
    let path = dir.file("identity.pem");

    std::fs::write(&path, "this is not a certificate").expect("write the broken file");

    let identity = identity::load_or_create(&path).expect("a broken file must not be fatal");
    assert!(!identity.fingerprint.is_empty());

    // The broken file was replaced by a usable one.
    let reloaded = identity::load_or_create(&path).expect("the reload");
    assert_eq!(identity.fingerprint, reloaded.fingerprint);
}

/// The stored file holds the private key, so it is not world readable.
#[test]
fn the_stored_identity_is_private() {
    let dir = TempDir::new("perms");
    let path = dir.file("identity.pem");

    identity::load_or_create(&path).expect("the load");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o077, 0, "the identity is readable by others");
    }
}

/// Two instances that start at the same time leave a usable identity behind.
///
/// The write is atomic, so a reader never sees a half-written file.
#[test]
fn two_instances_starting_at_once_leave_a_usable_identity() {
    let dir = TempDir::new("concurrent");
    let path = dir.file("identity.pem");

    std::thread::scope(|scope| {
        for _ in 0..4 {
            let path = path.clone();
            scope.spawn(move || {
                identity::load_or_create(&path).expect("the load");
            });
        }
    });

    // The file is a valid identity, whichever instance won the race.
    let reloaded = identity::load_or_create(&path).expect("the reload");
    assert!(!reloaded.fingerprint.is_empty());

    // And it is the one on disk, not a fresh one.
    let stored = std::fs::read_to_string(&path).expect("read");
    assert!(stored.contains("BEGIN CERTIFICATE"), "the file is torn");
}
