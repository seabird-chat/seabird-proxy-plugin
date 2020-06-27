fn main() {
    // NOTE: we use configure so we can skip generating the server code
    tonic_build::configure()
        .build_client(true)
        .build_server(false)
        .compile(&["./proto/seabird.proto"], &["./proto"])
        .unwrap();
}
