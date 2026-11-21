extern crate prost_build;

// Vendored from rquickshare (GPL-3.0), upstream commit
// 378d8ae969941bee4bf60ad34ac9cf8bb7005eb7.
//
// Upstream also writes a TypeScript binding index into ./bindings for its
// JavaScript frontend. Satella has no JavaScript frontend, so that half of the
// build script is dropped; the protobuf compilation is unchanged.

fn main() {
    prost_build::compile_protos(
        &[
            "src/proto_src/device_to_device_messages.proto",
            "src/proto_src/offline_wire_formats.proto",
            "src/proto_src/securegcm.proto",
            "src/proto_src/securemessage.proto",
            "src/proto_src/ukey.proto",
            "src/proto_src/wire_format.proto",
        ],
        &["src/proto_src"],
    )
    .unwrap();
}
