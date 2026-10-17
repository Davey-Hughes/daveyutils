//! The binary builds, runs, and knows its name and version.

use std::process::Command;

#[test]
fn prints_its_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("subcheck {}\n", env!("CARGO_PKG_VERSION"))
    );
}
