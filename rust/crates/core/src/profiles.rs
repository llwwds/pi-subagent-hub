//! Hub profile: resource hashing, normalisation and digest.
//!
//! The digest must match `src/profile.js` byte for byte: resource trees are
//! hashed with the Node header lines, and the normalised document goes through
//! canonical JSON (recursively sorted keys).

use std::fs;
use std::path::{Path, PathBuf};

use pihub_contracts::{
    canonical_json, sha256_hex, sha256_prefixed, HubError, HubResult, ProfileView, ResourceRef,
    DEFAULT_TOOLS,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::fsutil;

/// Resolved profile shared by every Agent created under one Hub home.
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub schema_version: u32,
    pub name: String,
    pub tools: Vec<String>,
    pub skills: Vec<ResourceRef>,
    pub extensions: Vec<ResourceRef>,
    pub digest: String,
}

impl ResolvedProfile {
    pub fn view(&self) -> ProfileView {
        ProfileView {
            name: self.name.clone(),
            digest: self.digest.clone(),
            tools: self.tools.clone(),
        }
    }

    /// Document shape hashed for the digest (`resolveProfile` normalisation).
    pub fn normalized(&self) -> Value {
        let mut map = Map::new();
        map.insert("schemaVersion".into(), json!(1));
        map.insert("name".into(), json!(self.name));
        map.insert("tools".into(), json!(self.tools));
        map.insert("skills".into(), resource_values(&self.skills));
        map.insert("extensions".into(), resource_values(&self.extensions));
        Value::Object(map)
    }
}

fn resource_values(resources: &[ResourceRef]) -> Value {
    Value::Array(
        resources
            .iter()
            .map(|resource| {
                json!({
                    "path": resource.path.to_string_lossy(),
                    "sha256": resource.sha256,
                })
            })
            .collect(),
    )
}

/// Hash one resource tree exactly like `src/profile.js:hashPath`.
pub fn hash_path(path: &Path) -> HubResult<ResourceRef> {
    let root = fsutil::absolute_normalized(path);
    if !fsutil::path_exists(&root) {
        return Err(HubError::new(
            "profile_resource_missing",
            format!("profile resource does not exist: {}", root.display()),
        ));
    }
    let mut hasher = Sha256::new();
    walk(&root, &root, &mut hasher)?;
    Ok(ResourceRef {
        path: root,
        sha256: sha256_hex(hasher.finalize()),
    })
}

fn relative_name(root: &Path, current: &Path) -> String {
    let relative = current.strip_prefix(root).unwrap_or(Path::new(""));
    let text = relative.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        root.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned())
    } else {
        text
    }
}

fn walk(root: &Path, current: &Path, hasher: &mut Sha256) -> HubResult<()> {
    let metadata = fs::symlink_metadata(current).map_err(HubError::from)?;
    let name = relative_name(root, current);
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(current).map_err(HubError::from)?;
        hasher.update(format!("L\0{name}\0{}\0", target.to_string_lossy()).as_bytes());
        return Ok(());
    }
    if file_type.is_dir() {
        hasher.update(format!("D\0{name}\0").as_bytes());
        let mut entries: Vec<(String, PathBuf)> = fs::read_dir(current)
            .map_err(HubError::from)?
            .map(|entry| {
                entry
                    .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path()))
                    .map_err(HubError::from)
            })
            .collect::<HubResult<Vec<_>>>()?;
        entries.sort_by(|left, right| fsutil::utf16_cmp(&left.0, &right.0));
        for (_, path) in entries {
            walk(root, &path, hasher)?;
        }
        return Ok(());
    }
    if file_type.is_file() {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode() & 0o777;
        hasher.update(format!("F\0{name}\0{mode}\0").as_bytes());
        let bytes = fs::read(current).map_err(HubError::from)?;
        hasher.update(&bytes);
        hasher.update(b"\0");
    }
    Ok(())
}

/// `resolveProfile`: validate, normalise, hash resources, digest.
pub fn resolve_profile(document: &Value) -> HubResult<ResolvedProfile> {
    let object = document.as_object().ok_or_else(|| {
        HubError::new("invalid_profile", "profile must be a JSON object")
    })?;
    let schema_version = object
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if schema_version != 1 {
        return Err(HubError::new(
            "unsupported_profile_schema",
            "unsupported profile schemaVersion",
        ));
    }

    let mut tools: Vec<String> = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(js_string).collect())
        .unwrap_or_default();
    tools.sort_by(|left, right| fsutil::utf16_cmp(left, right));
    tools.dedup();
    if tools.is_empty() {
        return Err(HubError::new(
            "invalid_profile",
            "profile must enable at least one tool",
        ));
    }

    let name = object
        .get("name")
        .map(js_string)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string());

    let mut skills = hash_resources(object.get("skills"))?;
    skills.sort_by(|left, right| left.path.cmp(&right.path));
    let mut extensions = hash_resources(object.get("extensions"))?;
    extensions.sort_by(|left, right| left.path.cmp(&right.path));

    let mut profile = ResolvedProfile {
        schema_version: 1,
        name,
        tools,
        skills,
        extensions,
        digest: String::new(),
    };
    let digest = sha256_prefixed(canonical_json(&profile.normalized()));
    profile.digest = digest;
    Ok(profile)
}

fn hash_resources(value: Option<&Value>) -> HubResult<Vec<ResourceRef>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let entries = value.as_array().ok_or_else(|| {
        HubError::new("invalid_profile", "profile resources must be an array of paths")
    })?;
    entries
        .iter()
        .map(|entry| {
            let path = entry.as_str().ok_or_else(|| {
                HubError::new("invalid_profile", "profile resource entries must be paths")
            })?;
            hash_path(Path::new(path))
        })
        .collect()
}

/// JavaScript `String(value)` for the scalar shapes a profile may contain.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// Create the default profile when absent, otherwise return the stored document.
pub fn ensure_default_profile(profile_path: &Path) -> HubResult<Value> {
    if let Some(existing) = fsutil::read_json_value(profile_path)? {
        return Ok(existing);
    }
    let profile = json!({
        "schemaVersion": 1,
        "name": "default",
        "tools": DEFAULT_TOOLS,
        "skills": [],
        "extensions": [],
    });
    fsutil::atomic_write_json(profile_path, &profile, fsutil::PRIVATE_FILE_MODE)?;
    Ok(profile)
}

/// Read the profile file (creating the default) and resolve it.
pub fn load_profile(profile_path: &Path) -> HubResult<ResolvedProfile> {
    let document = ensure_default_profile(profile_path)?;
    resolve_profile(&document)
}
