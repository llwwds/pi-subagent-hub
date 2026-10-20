//! Small, shared file-system and JavaScript-compatibility helpers.
//!
//! Every function here exists because more than one core module needs it:
//! private directory/file modes, atomic JSON writes, real-path containment,
//! and the UTF-16 ordering/trimming rules the Node reference uses.

use std::cmp::Ordering;
use std::fs::{self, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pihub_contracts::{HubError, HubResult};
use serde::Serialize;
use serde_json::Value;

pub const PRIVATE_DIR_MODE: u32 = 0o700;
pub const PRIVATE_FILE_MODE: u32 = 0o600;

pub fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// `mkdir -p` with mode 0700, then force the mode (umask may have widened it).
pub fn ensure_private_dir(path: &Path) -> HubResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        HubError::new(
            "io_error",
            format!("cannot create private directory {}: {error}", path.display()),
        )
    })?;
    chmod(path, PRIVATE_DIR_MODE).map_err(|error| {
        HubError::new(
            "io_error",
            format!("cannot set mode on {}: {error}", path.display()),
        )
    })
}

pub fn stat_no_follow(path: &Path) -> HubResult<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(HubError::from(error)),
    }
}

pub fn path_exists(path: &Path) -> bool {
    stat_no_follow(path).map(|value| value.is_some()).unwrap_or(false)
}

/// Read a JSON document; a missing file yields `None`, malformed JSON is an error.
pub fn read_json_value(path: &Path) -> HubResult<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|error| {
            HubError::new(
                "invalid_json",
                format!("{} is not valid JSON: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(HubError::from(error)),
    }
}

/// Write pretty-printed JSON through a unique temp file and an atomic rename.
pub fn atomic_write_json<T>(path: &Path, value: &T, mode: u32) -> HubResult<()>
where
    T: Serialize + ?Sized,
{
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|error| {
        HubError::new("invalid_json", format!("cannot serialize JSON: {error}"))
    })?;
    let temp = path.with_file_name(format!(
        "{}.tmp-{}-{}",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "record".to_string()),
        std::process::id(),
        hex_token(4)
    ));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)
            .map_err(|error| {
                HubError::new(
                    "io_error",
                    format!("cannot write {}: {error}", temp.display()),
                )
            })?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|error| {
                HubError::new(
                    "io_error",
                    format!("cannot write {}: {error}", temp.display()),
                )
            })?;
    }
    chmod(&temp, mode).map_err(HubError::from)?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        HubError::new(
            "io_error",
            format!("cannot replace {}: {error}", path.display()),
        )
    })?;
    chmod(path, mode).map_err(HubError::from)
}

pub fn hex_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ ((std::process::id() as u64) << 17) ^ 0x9e37_79b9_7f4a_7c15;
    for slot in buffer.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = (state & 0xff) as u8;
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Lexical path helpers live in `pihub-contracts`; re-exported so core modules
/// keep one import site without a second implementation.
pub use pihub_contracts::{absolute_normalized, normalize_lexical};

/// `fs.realpathSync`, surfaced as a `HubError` with a caller-chosen code.
pub fn real_path(path: &Path, code: &str, label: &str) -> HubResult<PathBuf> {
    fs::canonicalize(path).map_err(|error| {
        HubError::new(code, format!("{label} is not resolvable: {error}"))
    })
}

/// Canonicalise a path that may not exist yet: resolve the deepest existing
/// ancestor, then re-append the missing tail (port of `canonicalizePotentialPath`).
pub fn canonicalize_potential(path: &Path) -> HubResult<PathBuf> {
    let mut existing = absolute_normalized(path);
    let mut missing: Vec<String> = Vec::new();
    loop {
        if path_exists(&existing) {
            break;
        }
        let Some(name) = existing.file_name().map(|name| name.to_string_lossy().into_owned())
        else {
            break;
        };
        missing.insert(0, name);
        match existing.parent() {
            Some(parent) => existing = parent.to_path_buf(),
            None => break,
        }
    }
    let mut resolved = real_path(&existing, "io_error", "path ancestor")?;
    for segment in missing {
        resolved.push(segment);
    }
    Ok(resolved)
}

/// True when `path` is `root` or lives below it (both must be normalised).
pub fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// `String.prototype.localeCompare`-free ordering that still matches the Node
/// reference for ASCII: compare UTF-16 code units.
pub fn utf16_cmp(left: &str, right: &str) -> Ordering {
    let left_units = left.encode_utf16();
    let right_units = right.encode_utf16();
    for (a, b) in left_units.zip(right_units) {
        match a.cmp(&b) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    left.encode_utf16().count().cmp(&right.encode_utf16().count())
}

pub fn sort_strings_utf16(values: &mut [String]) {
    values.sort_by(|left, right| utf16_cmp(left, right));
}

fn is_js_space(unit: u16) -> bool {
    matches!(
        unit,
        0x0009 | 0x000a
            | 0x000b
            | 0x000c
            | 0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

/// JavaScript `String.prototype.trim`: includes U+FEFF, which Rust's `trim`
/// deliberately does not treat as whitespace.
pub fn js_trim(value: &str) -> &str {
    let units: Vec<u16> = value.encode_utf16().collect();
    let mut start = 0;
    let mut end = units.len();
    while start < end && is_js_space(units[start]) {
        start += 1;
    }
    while end > start && is_js_space(units[end - 1]) {
        end -= 1;
    }
    if start == 0 && end == units.len() {
        return value;
    }
    let start_byte = units[..start]
        .iter()
        .map(|unit| utf8_len(*unit))
        .sum::<usize>();
    let take_bytes = units[..end]
        .iter()
        .map(|unit| utf8_len(*unit))
        .sum::<usize>()
        - start_byte;
    &value[start_byte..start_byte + take_bytes]
}

fn utf8_len(unit: u16) -> usize {
    let code = unit as u32;
    if code < 0x80 {
        1
    } else if code < 0x800 {
        2
    } else if (0xd800..0xdc00).contains(&code) {
        // High surrogate: paired with the following low surrogate in the
        // slice-accounting above, one UTF-16 pair is 4 bytes total.
        2
    } else if (0xdc00..0xe000).contains(&code) {
        2
    } else {
        3
    }
}

/// Truncate to at most `max_bytes` UTF-8 bytes on a character boundary.
pub fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

pub fn file_mode(path: &Path) -> HubResult<u32> {
    let metadata = fs::metadata(path).map_err(HubError::from)?;
    Ok(metadata.mode() & 0o777)
}

pub fn mode_string(mode: u32) -> String {
    format!("{mode:03o}")
}

/// Process liveness through `kill(pid, 0)`, matching `isProcessAlive`.
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 only probes for existence and permission.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
