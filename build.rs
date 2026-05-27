fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Recompile uniquement si un fichier proto change.
    println!("cargo:rerun-if-changed=proto/vector_router/v1/router.proto");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/vector_router/v1/router.proto"], &["proto"])?;

    Ok(())
}
