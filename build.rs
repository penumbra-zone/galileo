fn main() -> anyhow::Result<()> {
    // Obtain the project root directory, and our target directory.
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_dir = root.join("src").join("proto").join("gen");
    println!("target_dir: {}", target_dir.display());

    // https://github.com/penumbra-zone/penumbra/issues/3038#issuecomment-1722534133
    // Using the "no_lfs" suffix prevents matching a catch-all LFS rule.
    let descriptor_file = target_dir.join("proto_descriptor.bin.no_lfs");

    // Configure how we should generate our code.
    let mut config = prost_build::Config::new();

    config.compile_well_known_types();
    // As recommended in pbjson_types docs.
    config.extern_path(".google.protobuf", "::pbjson_types");
    // NOTE: we need this because the rust module that defines the IBC types is external, and not
    // part of this crate.
    // See https://docs.rs/prost-build/0.5.0/prost_build/struct.Config.html#method.extern_path
    config.extern_path(".ibc", "::ibc_proto::ibc");
    // TODO: which of these is the right path?
    config.extern_path(".ics23", "::ics23");
    config.extern_path(".cosmos.ics23", "::ics23");

    config
        .out_dir(&target_dir)
        .file_descriptor_set_path(&descriptor_file)
        .enable_type_names();

    let rpc_doc_attr = r#"#[cfg(feature = "rpc")]"#;

    let protos = &[
        "./proto/galileo/faucet/v1/faucet.proto",
        // Other galileo RPC's could go here...
    ];
    let include_dirs = &["./proto/galileo/", "./proto/penumbra/"];
    tonic_build::configure()
        .out_dir(&target_dir)
        .emit_rerun_if_changed(true)
        // Feature gate the rpc's.
        .server_mod_attribute(".", rpc_doc_attr)
        .client_mod_attribute(".", rpc_doc_attr)
        .compile_with_config(config, protos, include_dirs)?;

    // Finally, build pbjson Serialize, Deserialize impls:
    let descriptor_set = std::fs::read(target_dir.join(descriptor_file))?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_set)?
        .ignore_unknown_fields()
        .out_dir(&target_dir)
        .build(&[".galileo"])?;

    Ok(())
}
