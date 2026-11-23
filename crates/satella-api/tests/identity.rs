//! The engine keeps its device identity in the directory it was given.
//!
//! The identity is the certificate the peers recognize this device by. It
//! has to survive a restart, or every LocalSend client shows this device as
//! a new one on every launch. On a platform that has no config directory of
//! its own, the app passes its own directory, and that path is the only place
//! the identity can live.

use satella::Engine;

/// The identity file of an engine built with a storage directory.
fn identity_file(storage_dir: &std::path::Path) -> std::path::PathBuf {
    storage_dir.join("localsend-identity.pem")
}

#[test]
fn the_engine_stores_its_identity_in_the_given_directory() {
    let storage_dir =
        std::env::temp_dir().join(format!("satella-engine-identity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&storage_dir);

    {
        let engine = Engine::with_storage_dir(Some(storage_dir.to_string_lossy().to_string()));
        engine.start().expect("the engine must start");
        engine.stop();
    }

    let identity = identity_file(&storage_dir);
    assert!(
        identity.exists(),
        "the engine did not store its identity in the given directory"
    );

    let contents = std::fs::read_to_string(&identity).expect("read the identity");
    assert!(
        contents.contains("BEGIN CERTIFICATE"),
        "the identity is not a certificate"
    );

    let _ = std::fs::remove_dir_all(&storage_dir);
}
