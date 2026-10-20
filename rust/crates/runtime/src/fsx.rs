//! Small filesystem helpers shared by the runtime crate.
//!
//! These mirror the 2.1 helpers that the carrier and CC Switch code relied on
//! (`ensurePrivateDir`, `atomicWriteJson`, `readJson`) without pulling in the
//! core crate.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use pihub_contracts::normalize_lexical;
use serde_json::Value;

/// Create a directory tree with owner-only permissions.
pub fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn read_json(path: &Path) -> std::io::Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => {
            let value = serde_json::from_slice(&bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            Ok(Some(value))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Write `value` (compact, newline-terminated) to a temp sibling then rename,
/// so a reader never observes a partial JSON document.
pub fn write_json_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let temp = temp_sibling(path);
    {
        let mut file = private_file(&temp)?;
        let text = serde_json::to_string(value)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
    }
    fs::rename(&temp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn write_text_atomic(path: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let temp = temp_sibling(path);
    {
        let mut file = private_file(&temp)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
    }
    fs::rename(&temp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn private_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn temp_sibling(path: &Path) -> PathBuf {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "value".to_string());
    directory.join(format!(".{name}.tmp-{}-{}", std::process::id(), nonce()))
}

fn nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}

/// Lexical `relative()` equivalent: true when `target` is strictly inside `root`.
/// The lexical normalization itself lives in `pihub_contracts`.
pub fn is_inside(root: &Path, target: &Path) -> bool {
    let root = normalize_lexical(root);
    let target = normalize_lexical(target);
    match target.strip_prefix(&root) {
        Ok(rest) => !rest.as_os_str().is_empty(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{ensure_private_dir, is_inside, read_json, write_json_atomic};
    use pihub_contracts::absolute_normalized;
    use serde_json::json;

    #[test]
    fn atomic_json_round_trips_and_is_owner_only() {
        let root = std::env::temp_dir().join(format!("pihub-fsx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        ensure_private_dir(&root).expect("private dir");
        let path = root.join("nested").join("value.json");
        write_json_atomic(&path, &json!({"sessionId": "abc"})).expect("write");
        let value = read_json(&path).expect("read").expect("present");
        assert_eq!(value["sessionId"], "abc");
        assert!(read_json(&root.join("missing.json")).expect("missing ok").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(root.join("nested"))
                .expect("stat dir")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn containment_and_absolute_resolution_are_lexical() {
        assert!(is_inside(
            std::path::Path::new("/tmp/home"),
            std::path::Path::new("/tmp/home/a/b")
        ));
        assert!(!is_inside(
            std::path::Path::new("/tmp/home"),
            std::path::Path::new("/tmp/home")
        ));
        assert!(!is_inside(
            std::path::Path::new("/tmp/home"),
            std::path::Path::new("/tmp/home-other/a")
        ));
        assert!(!is_inside(
            std::path::Path::new("/tmp/home"),
            std::path::Path::new("/tmp")
        ));
        assert_eq!(
            absolute_normalized(std::path::Path::new("/tmp/./a/../b")),
            std::path::PathBuf::from("/tmp/b")
        );
    }
}
