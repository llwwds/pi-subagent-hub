//! Skill freezing for CLI carriers and the private `pihub` route.
//!
//! CLI carriers do not share a process-wide skill directory. Each Agent owns a
//! private mount root whose entries are symlinks to the *frozen* skill
//! snapshots; the loop refuses to proceed when the directory already holds
//! anything else, so an inherited mount can never silently widen an Agent's
//! authorized skill set.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use pihub_contracts::HubResult;
use pihub_contracts::{HubError, SkillRef};

use crate::fsx::ensure_private_dir;

/// Mount entry names follow the shared token rule, capped at 101 bytes
/// (`^[A-Za-z0-9][A-Za-z0-9._-]{0,100}$`), so a skill id cannot escape or
/// collide inside the private mount root.
const SKILL_NAME_MAX: usize = 101;

pub fn is_valid_skill_name(name: &str) -> bool {
    pihub_contracts::is_safe_token(name, SKILL_NAME_MAX)
}

fn skill_path(skill: &SkillRef) -> &Path {
    match skill {
        SkillRef::Grant { path, .. } => path,
        SkillRef::Profile { path, .. } => path,
    }
}

fn skill_label(skill: &SkillRef) -> String {
    match skill {
        SkillRef::Grant { id, .. } => id.clone(),
        SkillRef::Profile { path, .. } => path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Directory that contains `SKILL.md` for one frozen skill.
pub fn skill_root(skill: &SkillRef) -> HubResult<PathBuf> {
    let path = skill_path(skill);
    if path.as_os_str().is_empty() {
        return Err(HubError::new("invalid_skill", "skill must have a path"));
    }
    let root = if path.file_name().map(|name| name == "SKILL.md").unwrap_or(false) {
        path.parent().unwrap_or(Path::new("/")).to_path_buf()
    } else {
        path.to_path_buf()
    };
    if !root.join("SKILL.md").exists() {
        return Err(HubError::new(
            "invalid_skill",
            format!("skill has no SKILL.md: {}", root.display()),
        ));
    }
    Ok(root)
}

/// Materialize `skills` under `destination` as a fixed symlink snapshot.
pub fn mount_skills(skills: &[SkillRef], destination: &Path) -> HubResult<PathBuf> {
    ensure_private_dir(destination).map_err(|error| {
        HubError::new(
            "invalid_skill",
            format!("cannot create skill mount {}: {error}", destination.display()),
        )
    })?;

    let mut expected: BTreeMap<String, PathBuf> = BTreeMap::new();
    for skill in skills {
        let root = skill_root(skill)?;
        let name = skill_label(skill);
        if !is_valid_skill_name(&name) {
            return Err(HubError::new(
                "invalid_skill",
                format!("invalid skill id for CLI carrier: {name}"),
            ));
        }
        if expected.insert(name.clone(), root).is_some() {
            return Err(HubError::new(
                "invalid_skill",
                format!("duplicate skill id for CLI carrier: {name}"),
            ));
        }
    }

    let entries = fs::read_dir(destination).map_err(|error| {
        HubError::new(
            "invalid_skill",
            format!("cannot read skill mount {}: {error}", destination.display()),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| HubError::new("invalid_skill", error.to_string()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !expected.contains_key(&name) {
            return Err(HubError::new(
                "invalid_skill",
                format!("unexpected skill already mounted: {name}"),
            ));
        }
    }

    for (name, root) in expected {
        let target = destination.join(&name);
        match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                let matches = metadata.file_type().is_symlink()
                    && fs::read_link(&target).map(|link| link == root).unwrap_or(false);
                if !matches {
                    return Err(HubError::new(
                        "invalid_skill",
                        format!("skill mount differs from fixed snapshot: {name}"),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                symlink_dir(&root, &target).map_err(|error| {
                    HubError::new(
                        "invalid_skill",
                        format!("cannot mount skill {name}: {error}"),
                    )
                })?;
            }
            Err(error) => {
                return Err(HubError::new("invalid_skill", error.to_string()));
            }
        }
    }
    Ok(destination.to_path_buf())
}

#[cfg(unix)]
fn symlink_dir(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(not(unix))]
fn symlink_dir(_source: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private skill mounts require symlink support",
    ))
}

/// Route a bundled Skill's `pihub` command back to *this* platform instance.
pub fn prepare_private_hub_cli(
    private_home: &Path,
    hub_home: Option<&Path>,
    hub_cli_path: Option<&Path>,
) -> HubResult<Option<PathBuf>> {
    let (Some(hub_home), Some(hub_cli_path)) = (hub_home, hub_cli_path) else {
        return Ok(None);
    };
    if !hub_home.is_absolute() || !hub_cli_path.is_absolute() {
        return Err(HubError::new(
            "runtime_invalid_route",
            "platform pihub route requires absolute home and CLI paths",
        ));
    }
    match fs::metadata(hub_cli_path) {
        Ok(metadata) if metadata.is_file() => {}
        _ => {
            return Err(HubError::new(
                "runtime_invalid_route",
                "platform pihub CLI is unavailable",
            ))
        }
    }
    let bin_dir = private_home.join(".bin");
    ensure_private_dir(&bin_dir).map_err(|error| {
        HubError::new(
            "runtime_invalid_route",
            format!("cannot create {}: {error}", bin_dir.display()),
        )
    })?;
    let command = bin_dir.join("pihub");
    match fs::symlink_metadata(&command) {
        Ok(metadata) => {
            let matches = metadata.file_type().is_symlink()
                && fs::read_link(&command)
                    .map(|link| link == hub_cli_path)
                    .unwrap_or(false);
            if !matches {
                return Err(HubError::new(
                    "runtime_invalid_route",
                    "platform pihub command differs from the private route",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            symlink_file(hub_cli_path, &command).map_err(|error| {
                HubError::new(
                    "runtime_invalid_route",
                    format!("cannot link private pihub command: {error}"),
                )
            })?;
        }
        Err(error) => {
            return Err(HubError::new("runtime_invalid_route", error.to_string()));
        }
    }
    Ok(Some(bin_dir))
}

#[cfg(unix)]
fn symlink_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(not(unix))]
fn symlink_file(_source: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private command routes require symlink support",
    ))
}

#[cfg(test)]
mod tests {
    use super::{is_valid_skill_name, mount_skills, prepare_private_hub_cli, skill_root};
    use pihub_contracts::SkillRef;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "pihub-mount-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_nanos())
                    .unwrap_or_default()
            ));
            fs::create_dir_all(&path).expect("temp root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn skill_dir(root: &Path, name: &str) -> PathBuf {
        let directory = root.join(name);
        fs::create_dir_all(&directory).expect("skill dir");
        fs::write(directory.join("SKILL.md"), "---\nname: x\n---\n").expect("skill file");
        directory
    }

    fn grant(id: &str, path: &Path) -> SkillRef {
        SkillRef::Grant {
            id: id.to_string(),
            path: path.to_path_buf(),
            sha256: "0".repeat(64),
        }
    }

    #[test]
    fn skill_names_follow_the_cli_carrier_rule() {
        assert!(is_valid_skill_name("test-skill"));
        assert!(is_valid_skill_name("a"));
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name("-leading"));
        assert!(!is_valid_skill_name("has space"));
        assert!(!is_valid_skill_name("has/slash"));
        assert!(!is_valid_skill_name(&"x".repeat(102)));
    }

    #[test]
    fn mounts_frozen_symlinks_once_and_rejects_extra_content() {
        let root = TempRoot::new("skills");
        let first = skill_dir(root.path(), "gallery-skill");
        let second = skill_dir(root.path(), "other-skill");
        let mount = root.path().join("private/.agents/skills");
        let skills = vec![grant("test-skill", &first), grant("second-skill", &second)];

        mount_skills(&skills, &mount).expect("first mount");
        assert_eq!(
            fs::read_link(mount.join("test-skill")).expect("symlink"),
            first
        );
        // Idempotent when the snapshot is unchanged.
        mount_skills(&skills, &mount).expect("re-mount");

        // Anything already sitting in the mount that is not part of the frozen
        // snapshot means an inherited/widened skill set: refuse it.
        let unexpected = mount.join("unexpected");
        fs::create_dir_all(&unexpected).expect("unexpected entry");
        let error = mount_skills(&skills, &mount).expect_err("extra directory rejected");
        assert!(error.message.contains("unexpected skill already mounted"));
        fs::remove_dir_all(&unexpected).expect("cleanup unexpected");

        let replaced = mount.join("test-skill");
        fs::remove_file(&replaced).expect("remove link");
        fs::create_dir_all(&replaced).expect("replace with dir");
        let error = mount_skills(&skills, &mount).expect_err("changed mount rejected");
        assert!(error.message.contains("differs from fixed snapshot"));
    }

    #[test]
    fn skill_root_accepts_a_direct_skill_md_path_and_requires_the_file() {
        let root = TempRoot::new("skill-root");
        let directory = skill_dir(root.path(), "one");
        let from_dir = skill_root(&grant("one", &directory)).expect("dir root");
        assert_eq!(from_dir, directory);
        let from_file = skill_root(&grant("one", &directory.join("SKILL.md"))).expect("file root");
        assert_eq!(from_file, directory);

        let empty = root.path().join("empty-skill");
        fs::create_dir_all(&empty).expect("empty");
        assert!(skill_root(&grant("empty", &empty)).is_err());
    }

    #[test]
    fn duplicate_skill_ids_are_rejected_before_any_link_is_written() {
        let root = TempRoot::new("dupes");
        let first = skill_dir(root.path(), "a");
        let second = skill_dir(root.path(), "b");
        let mount = root.path().join("mount");
        let error = mount_skills(
            &[grant("same", &first), grant("same", &second)],
            &mount,
        )
        .expect_err("duplicate ids");
        assert!(error.message.contains("duplicate skill id"));
        assert!(!mount.join("same").exists());
    }

    #[test]
    fn private_hub_route_is_created_once_and_cannot_be_retargeted() {
        let root = TempRoot::new("hubcli");
        let app_root = root.path().join("app");
        fs::create_dir_all(app_root.join("bin")).expect("bin");
        let hub_cli = app_root.join("bin/pihub.js");
        fs::write(&hub_cli, "#!/bin/sh\n").expect("cli");
        let private_home = root.path().join("agent/pi-home");

        assert_eq!(
            prepare_private_hub_cli(&private_home, None, None).expect("no route"),
            None
        );
        let bin = prepare_private_hub_cli(
            &private_home,
            Some(&root.path().join("home")),
            Some(&hub_cli),
        )
        .expect("route")
        .expect("bin dir");
        assert_eq!(bin, private_home.join(".bin"));
        assert_eq!(fs::read_link(bin.join("pihub")).expect("link"), hub_cli);
        prepare_private_hub_cli(&private_home, Some(&root.path().join("home")), Some(&hub_cli))
            .expect("idempotent");

        let missing = root.path().join("app/bin/missing.js");
        let error = prepare_private_hub_cli(
            &private_home,
            Some(&root.path().join("home")),
            Some(&missing),
        )
        .expect_err("missing cli");
        assert!(error.message.contains("unavailable"));

        let other = root.path().join("other.js");
        fs::write(&other, "#!/bin/sh\n").expect("other cli");
        let error = prepare_private_hub_cli(
            &private_home,
            Some(&root.path().join("home")),
            Some(&other),
        )
        .expect_err("retarget");
        assert!(error.message.contains("differs from the private route"));
    }
}
