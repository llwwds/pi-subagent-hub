//! Shared test helpers: isolated temporary homes and small file fixtures.
//!
//! Each integration test binary includes only the helpers it uses, so the
//! unused ones are allowed here instead of duplicated per test.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

pub fn temp_home(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("pihub-core-{label}-"))
        .tempdir()
        .expect("temporary home")
}

pub fn write_file(path: &Path, content: &str) {
    write_file_mode(path, content, 0o644);
}

pub fn write_file_mode(path: &Path, content: &str, mode: u32) {
    if let Some(parent) = path.parent() {
        mkdir(parent);
    }
    fs::write(path, content).expect("write fixture file");
    set_mode(path, mode);
}

pub fn mkdir(path: &Path) {
    fs::create_dir_all(path).expect("create fixture directory");
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

pub fn read_json(path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(path).expect("read json fixture");
    serde_json::from_str(&text).expect("parse json fixture")
}

pub fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A minimal Skill package: `SKILL.md` plus an optional extra file.
pub fn write_skill_package(root: &Path, name: &str, body: &str) -> PathBuf {
    let directory = root.join(name);
    mkdir(&directory);
    write_file(
        &directory.join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {name}\n---\n{body}\n"),
    );
    directory
}

/// Remove a read-only Skill tree (frozen objects are 0500).
pub fn thaw(root: &Path) {
    let _ = fs::set_permissions(
        root,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    );
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => thaw(&path),
            Ok(_) => {
                let _ = fs::set_permissions(
                    &path,
                    <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
                );
            }
            Err(_) => {}
        }
    }
}

/// Remove a temporary tree, thawing frozen Skill objects first.
pub fn remove_tree(path: &Path) {
    if path.exists() {
        thaw(path);
        let _ = fs::remove_dir_all(path);
    }
}
