fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);
    println!("cargo:rerun-if-changed=proto/agent.proto");
    let mut config = prost_build::Config::new();
    config.enum_attribute(".", "#[allow(clippy::large_enum_variant)]");
    config
        .compile_protos(&["proto/agent.proto"], &["proto"])
        .expect("compile Cursor protobuf schema");
}
