use std::path::PathBuf;

use crate::util;

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-rs-{}-{}", util::short_id(), name));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub fn temp_file(name: &str, content: &str) -> String {
    let dir = temp_dir(name);
    let path = dir.join(format!("{name}.txt"));
    std::fs::write(&path, content).expect("write temp file");
    path.to_string_lossy().into_owned()
}
