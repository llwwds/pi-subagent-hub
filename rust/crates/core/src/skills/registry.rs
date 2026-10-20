//! Content-addressed Skill store (`src/skills/registry.js`).
//!
//! Import never mutates an existing object: a package is copied into a private
//! staging directory, hashed, then renamed to `objects/<sha256>` and frozen.
//! Authorizations capture the digest of the frozen object, so a later import can
//! never widen an existing Agent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pihub_contracts::{
    is_safe_token, now_iso, sha256_hex, skill_authorization_digest, HubError, HubResult,
    SkillAuthorization,
    SkillGrant, SkillImportInput, SkillImportResult, SkillPolicy, SkillPolicyMode, SkillRegistry,
    SkillSummary, SkillVersion,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::fsutil;

const CATALOG_VERSION: u32 = 1;
const MAX_FILES: u64 = 1000;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const OBJECT_DIR_MODE: u32 = 0o500;
const OBJECT_FILE_MODE: u32 = 0o400;
const OBJECT_EXEC_MODE: u32 = 0o500;

const SEMANTIC_HIDDEN_DIRECTORIES: [&str; 2] = [".signature", ".claude-plugin"];
const BLOCKED_DIRECTORIES: [&str; 5] = [".ssh", ".aws", ".config", "secrets", "credentials"];
const OMIT_DIRECTORIES: [&str; 20] = [
    ".git",
    ".svn",
    ".hg",
    ".github",
    ".cache",
    ".pytest_cache",
    ".mypy_cache",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "cache",
    "caches",
    "sessions",
    "session",
    "state",
    "logs",
    "tmp",
    "temp",
    "tests",
];
/// `OMIT_DIRECTORIES` in the Node source also omits `test`; kept separate so the
/// list above stays readable.
const OMIT_DIRECTORIES_EXTRA: [&str; 1] = ["test"];

fn reject(code: &'static str, message: impl Into<String>) -> HubError {
    HubError::new(code, message)
}

fn assert_id(value: &str) -> HubResult<&str> {
    let reserved = matches!(value, "." | ".." | "__proto__" | "prototype" | "constructor");
    if !is_safe_token(value, 80) || reserved {
        return Err(reject(
            "invalid_skill_id",
            "skill ID must be 1-80 safe letters, digits, dots, underscores, or hyphens",
        ));
    }
    Ok(value)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn assert_digest(value: &str) -> HubResult<&str> {
    if !is_sha256(value) {
        return Err(reject(
            "invalid_skill_catalog",
            "skill catalog contains an invalid SHA-256 digest",
        ));
    }
    Ok(value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VersionRecord {
    sha256: String,
    source: String,
    #[serde(rename = "importedAt")]
    imported_at: String,
    #[serde(rename = "fileCount")]
    file_count: u64,
    #[serde(rename = "byteCount")]
    byte_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillRecord {
    id: String,
    #[serde(rename = "currentSha256")]
    current_sha256: String,
    versions: Vec<VersionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Catalog {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    skills: BTreeMap<String, SkillRecord>,
}

impl Catalog {
    fn empty() -> Self {
        Self {
            schema_version: CATALOG_VERSION,
            skills: BTreeMap::new(),
        }
    }
}

/// Lock guard that always removes the lock file, even on error paths.
struct CatalogLock {
    path: PathBuf,
}

impl CatalogLock {
    fn acquire(path: &Path) -> HubResult<Self> {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(fsutil::PRIVATE_FILE_MODE)
            .open(path)
        {
            Ok(_) => Ok(Self {
                path: path.to_path_buf(),
            }),
            Err(error) => Err(reject(
                "skill_registry_busy",
                format!("skill registry import is locked: {error}"),
            )),
        }
    }
}

impl Drop for CatalogLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct FsSkillRegistry {
    home: PathBuf,
    root: PathBuf,
    objects_root: PathBuf,
    catalog_path: PathBuf,
    lock_path: PathBuf,
    import_lock: Mutex<()>,
}

impl std::fmt::Debug for FsSkillRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsSkillRegistry")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl FsSkillRegistry {
    pub fn new(home: impl AsRef<Path>) -> HubResult<Self> {
        let home = fsutil::absolute_normalized(home.as_ref());
        if !home.is_absolute() {
            return Err(reject(
                "unsafe_skill_path",
                "home must be an absolute path",
            ));
        }
        let root = home.join("skills");
        Ok(Self {
            objects_root: root.join("objects"),
            catalog_path: root.join("catalog.json"),
            lock_path: root.join(".catalog.lock"),
            root,
            home,
            import_lock: Mutex::new(()),
        })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Create the private storage layout, refusing symlinked locations.
    fn ensure_storage(&self) -> HubResult<()> {
        assert_private_directory(&self.home, true)?;
        assert_private_directory(&self.root, true)?;
        assert_private_directory(&self.objects_root, true)?;
        if let Some(metadata) = fsutil::stat_no_follow(&self.catalog_path)? {
            if metadata.file_type().is_symlink() {
                return Err(reject(
                    "unsafe_skill_path",
                    "skill catalog must not be a symlink",
                ));
            }
        }
        Ok(())
    }

    fn read_catalog(&self) -> HubResult<Catalog> {
        if !assert_private_directory(&self.home, false)? {
            return Ok(Catalog::empty());
        }
        if !assert_private_directory(&self.root, false)? {
            return Ok(Catalog::empty());
        }
        if !assert_private_directory(&self.objects_root, false)? {
            return Ok(Catalog::empty());
        }
        let Some(metadata) = fsutil::stat_no_follow(&self.catalog_path)? else {
            return Ok(Catalog::empty());
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(reject(
                "unsafe_skill_path",
                "skill catalog must be a real file",
            ));
        }
        let text = fs::read_to_string(&self.catalog_path).map_err(HubError::from)?;
        let catalog: Catalog = serde_json::from_str(&text).map_err(|error| {
            reject(
                "invalid_skill_catalog",
                format!("skill catalog is not valid JSON: {error}"),
            )
        })?;
        if catalog.schema_version != CATALOG_VERSION {
            return Err(reject(
                "invalid_skill_catalog",
                "unsupported skill catalog format",
            ));
        }
        for (id, record) in &catalog.skills {
            assert_id(id)?;
            assert_digest(&record.current_sha256)?;
            if !record
                .versions
                .iter()
                .any(|version| version.sha256 == record.current_sha256)
            {
                return Err(reject(
                    "invalid_skill_catalog",
                    format!("invalid versions for skill: {id}"),
                ));
            }
            for version in &record.versions {
                assert_digest(&version.sha256)?;
            }
        }
        Ok(catalog)
    }

    /// Import a package under `id`; returns the frozen version record.
    pub fn import_skill_sync(
        &self,
        id: &str,
        source_path: &Path,
        source_label: Option<&str>,
    ) -> HubResult<SkillImportResult> {
        assert_id(id)?;
        let source = assert_source_path(source_path)?;
        if let Some(label) = source_label {
            if fsutil::js_trim(label).is_empty() {
                return Err(reject(
                    "unsafe_skill_source",
                    "sourceLabel must be a non-empty string",
                ));
            }
        }

        let intended = fsutil::canonicalize_potential(&self.root)?;
        if fsutil::is_within(&source, &intended) || fsutil::is_within(&intended, &source) {
            return Err(reject(
                "unsafe_skill_source",
                "skill source must not overlap platform skill storage",
            ));
        }
        self.ensure_storage()?;
        let storage = fsutil::real_path(&self.root, "unsafe_skill_path", "skill storage")?;
        if fsutil::is_within(&source, &storage) || fsutil::is_within(&storage, &source) {
            return Err(reject(
                "unsafe_skill_source",
                "skill source must not overlap platform skill storage",
            ));
        }

        let _serialize = self.import_lock.lock().map_err(|_| {
            reject(
                "skill_registry_busy",
                "skill registry import lock is poisoned",
            )
        })?;
        let _lock = CatalogLock::acquire(&self.lock_path)?;
        let staging = self
            .objects_root
            .join(format!(".staging-{}", pihub_contracts::new_id()));
        fs::create_dir(&staging).map_err(|error| {
            reject(
                "io_error",
                format!("cannot create skill staging directory: {error}"),
            )
        })?;
        fsutil::chmod(&staging, fsutil::PRIVATE_DIR_MODE).map_err(HubError::from)?;

        let outcome = (|| -> HubResult<SkillImportResult> {
            let size = copy_package(&source, &staging)?;
            let sha256 = hash_package(&staging)?;
            let destination = self.objects_root.join(&sha256);
            if fsutil::path_exists(&destination) {
                if hash_package(&destination)? != sha256 {
                    return Err(reject(
                        "skill_snapshot_changed",
                        format!("existing skill snapshot is corrupt: {sha256}"),
                    ));
                }
                let _ = fs::remove_dir_all(&staging);
            } else {
                fs::rename(&staging, &destination).map_err(|error| {
                    reject(
                        "io_error",
                        format!("cannot publish skill snapshot: {error}"),
                    )
                })?;
                freeze_package(&destination)?;
            }

            let mut catalog = self.read_catalog()?;
            let source_label = source_label
                .map(|label| label.to_string())
                .unwrap_or_else(|| source.to_string_lossy().into_owned());
            let mut record = catalog
                .skills
                .remove(id)
                .unwrap_or_else(|| SkillRecord {
                    id: id.to_string(),
                    current_sha256: String::new(),
                    versions: Vec::new(),
                });
            let mut version = record
                .versions
                .iter()
                .find(|version| version.sha256 == sha256)
                .cloned();
            if version.is_none() {
                let created = VersionRecord {
                    sha256: sha256.clone(),
                    source: source_label,
                    imported_at: now_iso(),
                    file_count: size.files,
                    byte_count: size.bytes,
                };
                record.versions.push(created.clone());
                version = Some(created);
            }
            record.current_sha256 = sha256.clone();
            let version = version.expect("version inserted above");
            catalog.skills.insert(id.to_string(), record);
            fsutil::atomic_write_json(
                &self.catalog_path,
                &catalog,
                fsutil::PRIVATE_FILE_MODE,
            )?;
            Ok(SkillImportResult {
                id: id.to_string(),
                sha256,
                source: version.source,
                imported_at: version.imported_at,
                file_count: version.file_count,
                byte_count: version.byte_count,
                path: destination,
            })
        })();

        if fsutil::path_exists(&staging) {
            let _ = fs::remove_dir_all(&staging);
        }
        outcome
    }

    pub fn list_sync(&self) -> HubResult<Vec<SkillSummary>> {
        let catalog = self.read_catalog()?;
        let mut summaries: Vec<SkillSummary> = catalog
            .skills
            .values()
            .filter_map(|record| {
                let current = record
                    .versions
                    .iter()
                    .find(|version| version.sha256 == record.current_sha256)?;
                Some(SkillSummary {
                    id: record.id.clone(),
                    name: record.id.clone(),
                    version: record.current_sha256.chars().take(12).collect(),
                    digest: format!("sha256:{}", record.current_sha256),
                    current_sha256: record.current_sha256.clone(),
                    source: current.source.clone(),
                    imported_at: current.imported_at.clone(),
                    versions: record
                        .versions
                        .iter()
                        .map(|version| SkillVersion {
                            sha256: version.sha256.clone(),
                            source: version.source.clone(),
                            imported_at: version.imported_at.clone(),
                            file_count: version.file_count,
                            byte_count: version.byte_count,
                        })
                        .collect(),
                })
            })
            .collect();
        summaries.sort_by(|left, right| fsutil::utf16_cmp(&left.id, &right.id));
        Ok(summaries)
    }

    /// Normalise a creation-time policy, rejecting duplicates and bad ids.
    pub fn normalize_policy(policy: Option<&SkillPolicy>) -> HubResult<(SkillPolicyMode, Vec<String>)> {
        match policy {
            None => Ok((SkillPolicyMode::All, Vec::new())),
            Some(SkillPolicy::All) => Ok((SkillPolicyMode::All, Vec::new())),
            Some(SkillPolicy::Only { ids }) => {
                // 2.1 keeps this wrapper separate from `assert_id`: the code is
                // `invalid_skill_policy`, and duplicates are rejected before sorting.
                let mut seen: Vec<&str> = Vec::new();
                for id in ids {
                    assert_id(id)?;
                    if seen.contains(&id.as_str()) {
                        return Err(reject(
                            "invalid_skill_policy",
                            "duplicate skill IDs are not allowed",
                        ));
                    }
                    seen.push(id.as_str());
                }
                let mut sorted: Vec<String> = ids.clone();
                fsutil::sort_strings_utf16(&mut sorted);
                Ok((SkillPolicyMode::Only, sorted))
            }
        }
    }

    pub fn resolve_authorization_sync(
        &self,
        policy: Option<&SkillPolicy>,
    ) -> HubResult<SkillAuthorization> {
        let (mode, only_ids) = Self::normalize_policy(policy)?;
        let catalog = self.read_catalog()?;
        let ids: Vec<String> = match mode {
            SkillPolicyMode::All => catalog.skills.keys().cloned().collect(),
            SkillPolicyMode::Only => only_ids,
        };
        let mut grants: Vec<SkillGrant> = Vec::new();
        for id in ids {
            let record = catalog.skills.get(&id).ok_or_else(|| {
                reject("skill_not_found", format!("unknown skill ID: {id}"))
            })?;
            let sha256 = assert_digest(&record.current_sha256)?.to_string();
            let path = self.objects_root.join(&sha256);
            if !fsutil::path_exists(&path) || hash_package(&path)? != sha256 {
                return Err(reject(
                    "skill_snapshot_changed",
                    format!("skill snapshot is missing or changed: {id}@{sha256}"),
                ));
            }
            grants.push(SkillGrant { id, sha256, path });
        }
        let digest = skill_authorization_digest(mode, &grants);
        Ok(SkillAuthorization {
            mode,
            grants,
            digest,
        })
    }

    pub fn verify_sync(&self, authorization: &SkillAuthorization) -> HubResult<()> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for grant in &authorization.grants {
            assert_id(&grant.id)?;
            if !is_sha256(&grant.sha256) {
                return Err(reject(
                    "invalid_skill_policy",
                    "invalid frozen skill authorization",
                ));
            }
            if !seen.insert(grant.id.as_str()) {
                return Err(reject(
                    "invalid_skill_policy",
                    format!("duplicate grant: {}", grant.id),
                ));
            }
            let expected = self.objects_root.join(&grant.sha256);
            if grant.path != expected
                || !fsutil::path_exists(&expected)
                || hash_package(&expected)? != grant.sha256
            {
                return Err(reject(
                    "skill_snapshot_changed",
                    format!(
                        "skill snapshot is missing or changed: {}@{}",
                        grant.id, grant.sha256
                    ),
                ));
            }
        }
        let expected_digest = skill_authorization_digest(authorization.mode, &authorization.grants);
        if authorization.digest != expected_digest {
            return Err(reject(
                "invalid_skill_policy",
                "frozen skill authorization digest does not match grants",
            ));
        }
        Ok(())
    }
}

fn assert_private_directory(path: &Path, create: bool) -> HubResult<bool> {
    if create && fsutil::stat_no_follow(path)?.is_none() {
        fsutil::ensure_private_dir(path)?;
    }
    let Some(metadata) = fsutil::stat_no_follow(path)? else {
        return Ok(false);
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(reject(
            "unsafe_skill_path",
            format!("skill storage must be a real directory: {}", path.display()),
        ));
    }
    if create {
        fsutil::chmod(path, fsutil::PRIVATE_DIR_MODE).map_err(HubError::from)?;
    }
    Ok(true)
}

/// Absolute, traversal-free, real directory (never a symlink).
fn assert_source_path(source_path: &Path) -> HubResult<PathBuf> {
    if !source_path.is_absolute()
        || source_path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(reject(
            "unsafe_skill_source",
            "sourcePath must be an absolute directory path without traversal",
        ));
    }
    let resolved = fsutil::absolute_normalized(source_path);
    let Some(metadata) = fsutil::stat_no_follow(&resolved)? else {
        return Err(reject(
            "unsafe_skill_source",
            format!(
                "skill source must be a real directory: {}",
                resolved.display()
            ),
        ));
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(reject(
            "unsafe_skill_source",
            format!(
                "skill source must be a real directory: {}",
                resolved.display()
            ),
        ));
    }
    fsutil::real_path(&resolved, "unsafe_skill_source", "skill source")
}

struct PackageSize {
    files: u64,
    bytes: u64,
}

fn omitted_directory(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    OMIT_DIRECTORIES.contains(&lower.as_str()) || OMIT_DIRECTORIES_EXTRA.contains(&lower.as_str())
}

fn omitted_file(name: &str) -> bool {
    if name == ".DS_Store" {
        return true;
    }
    if name == ".gitignore" || name == ".gitkeep" {
        return true;
    }
    if let Some(rest) = name.strip_prefix('.') {
        let allowed = rest
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'));
        if allowed {
            let lower = rest.to_ascii_lowercase();
            let keywords = [
                "token",
                "password",
                "secret",
                "credential",
                "apikey",
                "api-key",
                "api_key",
            ];
            if keywords.iter().any(|keyword| lower.contains(keyword)) {
                return true;
            }
        }
    }
    name.to_ascii_lowercase().ends_with(".pyc")
}

fn blocked_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == ".ds_store" || lower == ".npmrc" {
        return true;
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }
    if let Some(extension) = lower.rsplit_once('.').map(|(_, extension)| extension) {
        if matches!(
            extension,
            "pem" | "p12" | "pfx" | "key" | "sqlite" | "sqlite3" | "db" | "log" | "pid" | "sock"
        ) {
            return true;
        }
        let stem = lower.rsplit_once('.').map(|(stem, _)| stem).unwrap_or("");
        let base = stem
            .split(|character| matches!(character, '.' | '_' | '-'))
            .next()
            .unwrap_or("");
        let sensitive_stem = matches!(
            base,
            "auth" | "credential" | "credentials" | "secret" | "secrets" | "token" | "tokens"
                | "model" | "models"
        );
        if sensitive_stem && matches!(extension, "json" | "yaml" | "yml" | "toml" | "txt" | "env") {
            return true;
        }
    }
    matches!(lower.as_str(), "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519")
        || matches!(
            lower.as_str(),
            "id_rsa.pub" | "id_dsa.pub" | "id_ecdsa.pub" | "id_ed25519.pub"
        )
}

fn assert_safe_entry(name: &str, relative: &str, directory: bool) -> HubResult<()> {
    if name == "." || name == ".." || name.contains('\0') || name.contains('/') {
        return Err(reject(
            "unsafe_skill_source",
            format!("invalid skill package entry: {relative}"),
        ));
    }
    if directory {
        let hidden = name.starts_with('.') && !SEMANTIC_HIDDEN_DIRECTORIES.contains(&name);
        let blocked = BLOCKED_DIRECTORIES.contains(&name.to_ascii_lowercase().as_str());
        if hidden || blocked {
            return Err(reject(
                "sensitive_skill_content",
                format!("runtime or credential directory is not importable: {relative}"),
            ));
        }
        return Ok(());
    }
    if blocked_file(name) {
        return Err(reject(
            "sensitive_skill_content",
            format!("runtime or credential file is not importable: {relative}"),
        ));
    }
    Ok(())
}

fn contains_secret_material(text: &str) -> bool {
    if text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----") {
        return true;
    }
    let bytes = text.as_bytes();
    for (index, window) in bytes.windows(4).enumerate() {
        let prefix = &window[..3];
        if !matches!(prefix, b"ghp" | b"gho" | b"ghu" | b"ghs" | b"ghr") {
            continue;
        }
        if bytes.get(index + 3) != Some(&b'_') {
            continue;
        }
        let run = bytes[index + 4..]
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric())
            .count();
        if run >= 30 {
            return true;
        }
    }
    let mut cursor = 0usize;
    while let Some(found) = text[cursor..].find("sk-") {
        let start = cursor + found + 3;
        let run = text[start..]
            .chars()
            .take_while(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
            .count();
        if run >= 30 {
            return true;
        }
        cursor = start;
    }
    false
}

fn copy_package(source: &Path, target: &Path) -> HubResult<PackageSize> {
    let mut size = PackageSize {
        files: 0,
        bytes: 0,
    };
    walk_copy(source, target, "", &mut size)?;
    let manifest = target.join("SKILL.md");
    let valid = fsutil::stat_no_follow(&manifest)?
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false);
    if !valid {
        return Err(reject(
            "invalid_skill_package",
            "skill package must contain a root SKILL.md file",
        ));
    }
    Ok(size)
}

fn walk_copy(
    from: &Path,
    to: &Path,
    prefix: &str,
    size: &mut PackageSize,
) -> HubResult<()> {
    let mut entries: Vec<(String, PathBuf)> = fs::read_dir(from)
        .map_err(HubError::from)?
        .map(|entry| {
            entry
                .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path()))
                .map_err(HubError::from)
        })
        .collect::<HubResult<Vec<_>>>()?;
    entries.sort_by(|left, right| fsutil::utf16_cmp(&left.0, &right.0));

    for (name, input) in entries {
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if omitted_directory(&name) || omitted_file(&name) {
            continue;
        }
        let output = to.join(&name);
        let metadata = fs::symlink_metadata(&input).map_err(HubError::from)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(reject(
                "unsafe_skill_source",
                format!("symlinks are not importable: {relative}"),
            ));
        }
        if name.to_ascii_lowercase() == "skill.md" && relative != "SKILL.md" {
            return Err(reject(
                "invalid_skill_package",
                format!("only the root SKILL.md is allowed in a skill package: {relative}"),
            ));
        }
        assert_safe_entry(&name, &relative, file_type.is_dir())?;
        if file_type.is_dir() {
            fs::create_dir(&output).map_err(HubError::from)?;
            fsutil::chmod(&output, fsutil::PRIVATE_DIR_MODE).map_err(HubError::from)?;
            walk_copy(&input, &output, &relative, size)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(reject(
                "unsafe_skill_source",
                format!("special files are not importable: {relative}"),
            ));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&input)
            .map_err(|error| {
                reject(
                    "unsafe_skill_source",
                    format!("cannot read {relative}: {error}"),
                )
            })?;
        let current = file.metadata().map_err(HubError::from)?;
        if !current.is_file() || current.len() > MAX_FILE_BYTES {
            return Err(reject(
                "unsafe_skill_source",
                format!("skill file is not regular or is too large: {relative}"),
            ));
        }
        let mut data = Vec::with_capacity(current.len() as usize);
        file.read_to_end(&mut data).map_err(HubError::from)?;
        size.files += 1;
        size.bytes += data.len() as u64;
        if size.files > MAX_FILES || size.bytes > MAX_TOTAL_BYTES {
            return Err(reject(
                "unsafe_skill_source",
                "skill package exceeds import size limits",
            ));
        }
        if contains_secret_material(&String::from_utf8_lossy(&data)) {
            return Err(reject(
                "sensitive_skill_content",
                format!("possible credential material found in: {relative}"),
            ));
        }
        let mode = if current.mode() & 0o111 != 0 {
            OBJECT_EXEC_MODE
        } else {
            OBJECT_FILE_MODE
        };
        let mut output_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&output)
            .map_err(|error| {
                reject("io_error", format!("cannot write {relative}: {error}"))
            })?;
        output_file.write_all(&data).map_err(HubError::from)?;
        fsutil::chmod(&output, mode).map_err(HubError::from)?;
    }
    Ok(())
}

/// Content hash of a frozen package (`hashPackage`).
pub fn hash_package(root: &Path) -> HubResult<String> {
    let metadata = fsutil::stat_no_follow(root)?.ok_or_else(|| {
        reject(
            "skill_snapshot_changed",
            "skill snapshot root is missing",
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(reject(
            "skill_snapshot_changed",
            "skill snapshot root is not a real directory",
        ));
    }
    let mut hasher = HashingBuffer::default();
    walk_hash(root, "", &mut hasher)?;
    Ok(hasher.finish())
}

#[derive(Default)]
struct HashingBuffer {
    bytes: Vec<u8>,
}

impl HashingBuffer {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn finish(self) -> String {
        sha256_hex(self.bytes)
    }
}

fn walk_hash(directory: &Path, prefix: &str, hasher: &mut HashingBuffer) -> HubResult<()> {
    let mut entries: Vec<(String, PathBuf)> = fs::read_dir(directory)
        .map_err(HubError::from)?
        .map(|entry| {
            entry
                .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path()))
                .map_err(HubError::from)
        })
        .collect::<HubResult<Vec<_>>>()?;
    entries.sort_by(|left, right| fsutil::utf16_cmp(&left.0, &right.0));
    for (name, path) in entries {
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::symlink_metadata(&path).map_err(HubError::from)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(reject(
                "skill_snapshot_changed",
                format!("symlink in skill snapshot: {relative}"),
            ));
        }
        if file_type.is_dir() {
            hasher.push(
                serde_json::to_string(&json!(["D", relative]))
                    .map_err(|error| reject("invalid_skill_catalog", error.to_string()))?
                    .as_bytes(),
            );
            walk_hash(&path, &relative, hasher)?;
        } else if file_type.is_file() {
            let exec = metadata.mode() & 0o111 != 0;
            let bytes = fs::read(&path).map_err(HubError::from)?;
            hasher.push(
                serde_json::to_string(&json!(["F", relative, exec, bytes.len()]))
                    .map_err(|error| reject("invalid_skill_catalog", error.to_string()))?
                    .as_bytes(),
            );
            hasher.push(&bytes);
        } else {
            return Err(reject(
                "skill_snapshot_changed",
                format!("special file in skill snapshot: {relative}"),
            ));
        }
    }
    Ok(())
}

fn freeze_package(root: &Path) -> HubResult<()> {
    for entry in fs::read_dir(root).map_err(HubError::from)? {
        let entry = entry.map_err(HubError::from)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(HubError::from)?;
        if metadata.is_dir() {
            freeze_package(&entry.path())?;
        }
    }
    fsutil::chmod(root, OBJECT_DIR_MODE).map_err(HubError::from)
}

#[async_trait::async_trait]
impl SkillRegistry for FsSkillRegistry {
    async fn list(&self) -> HubResult<Vec<SkillSummary>> {
        self.list_sync()
    }

    async fn import(&self, input: SkillImportInput) -> HubResult<SkillImportResult> {
        self.import_skill_sync(&input.id, &input.source_path, input.source_label.as_deref())
    }

    async fn resolve_authorization(
        &self,
        policy: Option<&SkillPolicy>,
    ) -> HubResult<SkillAuthorization> {
        self.resolve_authorization_sync(policy)
    }

    async fn verify(&self, authorization: &SkillAuthorization) -> HubResult<()> {
        self.verify_sync(authorization)
    }
}

/// Unfreeze a frozen tree so tests and callers can delete it.
pub fn thaw_tree(root: &Path) {
    let _ = fsutil::chmod(root, fsutil::PRIVATE_DIR_MODE);
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.is_dir() {
                thaw_tree(&path);
            } else {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
        }
    }
}
