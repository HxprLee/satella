//! The UniFFI build script.
//!
//! It generates the UniFFI scaffolding from `src/satella.udl`, which the
//! `uniffi::include_scaffolding!` call in `src/lib.rs` picks up.

fn main() {
    uniffi::generate_scaffolding("src/satella.udl").expect("the UniFFI scaffolding must build");
}
