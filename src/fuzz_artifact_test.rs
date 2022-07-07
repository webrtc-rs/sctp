use bytes::Bytes;

#[test]
fn param_crash_artifacts() {
    for artifact in std::fs::read_dir("fuzz/artifacts/param").unwrap() {
        let artifact = artifact.unwrap();
        if artifact.file_name().into_string().unwrap().starts_with("crash-") {
            let artifact = std::fs::read(artifact.path()).unwrap();
            crate::param::build_param(&Bytes::from(artifact)).ok();
        }
    }
}

#[test]
fn packet_crash_artifacts() {
    for artifact in std::fs::read_dir("fuzz/artifacts/packet").unwrap() {
        let artifact = artifact.unwrap();
        if artifact.file_name().into_string().unwrap().starts_with("crash-") {
            let artifact = std::fs::read(artifact.path()).unwrap();
            crate::packet::Packet::unmarshal(&Bytes::from(artifact)).ok();
        }
    }
}