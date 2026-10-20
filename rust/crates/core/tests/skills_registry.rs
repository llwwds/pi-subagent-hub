//! Skill registry: content-addressed import, provenance, frozen grants,
//! tamper detection and the import guards.

mod common;

use std::os::unix::fs::symlink;

use pihub_contracts::{SkillPolicy, SkillPolicyMode, SkillRegistry};
use pihub_core::FsSkillRegistry;

const GOLDEN_PACKAGE_SHA256: &str =
    "07dceaae2922da8513387f927db25ad6a8d096f448105127157d0a9902bdea49";
const GOLDEN_ONLY_DIGEST: &str =
    "sha256:9f33302f683b83e3a84b754befd8e20b9f64cdb61ae0d1d453f745ccfa012c16";

#[tokio::test]
async fn import_is_content_addressed_and_pins_grants() {
    let home = common::temp_home("skills-import");
    let source = home.path().join("source");
    common::mkdir(&source);
    // Identical bytes and mode to the Node reference fixture, so the package
    // hash must match the value produced by `src/skills/registry.js`.
    common::write_file_mode(
        &source.join("SKILL.md"),
        "---\nname: sample\n---\nbody\n",
        0o644,
    );
    let registry = FsSkillRegistry::new(home.path().join("home")).expect("registry");

    let imported = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source.clone(),
            source_label: Some("parity source".to_string()),
        })
        .await
        .expect("import");
    assert_eq!(imported.sha256, GOLDEN_PACKAGE_SHA256);
    assert_eq!(imported.source, "parity source");
    assert_eq!(imported.file_count, 1);
    assert_eq!(
        imported.path,
        home.path()
            .join("home/skills/objects")
            .join(GOLDEN_PACKAGE_SHA256)
    );

    let only = registry
        .resolve_authorization(Some(&SkillPolicy::Only {
            ids: vec!["sample".to_string()],
        }))
        .await
        .expect("authorization");
    assert_eq!(only.digest, GOLDEN_ONLY_DIGEST);
    assert_eq!(only.mode, SkillPolicyMode::Only);
    assert_eq!(only.grants[0].sha256, GOLDEN_PACKAGE_SHA256);
    registry.verify(&only).await.expect("verify frozen authorization");

    // A changed source produces a new immutable object; the old grant stays valid.
    common::set_mode(&imported.path, 0o700);
    common::set_mode(&imported.path.join("SKILL.md"), 0o600);
    common::write_file(&source.join("SKILL.md"), "---\nname: sample\n---\nupdated\n");
    let second = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source.clone(),
            source_label: None,
        })
        .await
        .expect("re-import");
    assert_ne!(second.sha256, GOLDEN_PACKAGE_SHA256);
    let listed = registry.list().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].current_sha256, second.sha256);
    assert_eq!(listed[0].versions.len(), 2);
    registry.verify(&only).await.expect("old grant still valid");
    common::remove_tree(&home.path().join("home/skills/objects"));
}

#[tokio::test]
async fn import_rejects_traversal_symlinks_secrets_and_bad_manifests() {
    let home = common::temp_home("skills-guards");
    let source = common::write_skill_package(home.path(), "source", "body");
    let registry = FsSkillRegistry::new(home.path().join("home")).expect("registry");

    let error = registry
        .import(pihub_contracts::SkillImportInput {
            id: "../escape".to_string(),
            source_path: source.clone(),
            source_label: None,
        })
        .await
        .expect_err("invalid id");
    assert_eq!(error.code, "invalid_skill_id");

    let error = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source.join("..").join("source"),
            source_label: None,
        })
        .await
        .expect_err("path traversal");
    assert_eq!(error.code, "unsafe_skill_source");

    // Symlinks (including one pointing outside the package) are refused.
    let outside = home.path().join("outside.txt");
    common::write_file(&outside, "outside");
    let link = source.join("escape");
    symlink(&outside, &link).expect("symlink");
    let error = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source.clone(),
            source_label: None,
        })
        .await
        .expect_err("symlink");
    assert_eq!(error.code, "unsafe_skill_source");
    std::fs::remove_file(&link).expect("remove link");

    // Credential material is refused by name and by content.
    common::write_file(&source.join("auth.json"), "{}\n");
    let error = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source.clone(),
            source_label: None,
        })
        .await
        .expect_err("credential file");
    assert_eq!(error.code, "sensitive_skill_content");
    std::fs::remove_file(source.join("auth.json")).expect("remove");

    common::write_file(
        &source.join("secret.txt"),
        "-----BEGIN RSA PRIVATE KEY-----\n",
    );
    let error = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source.clone(),
            source_label: None,
        })
        .await
        .expect_err("credential content");
    assert_eq!(error.code, "sensitive_skill_content");
    std::fs::remove_file(source.join("secret.txt")).expect("remove");

    // Only the root SKILL.md spelling is a manifest.
    let nested = source.join("nested");
    common::mkdir(&nested);
    common::write_file(&nested.join("SKILL.md"), "---\nname: hidden\n---\n");
    let error = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source.clone(),
            source_label: None,
        })
        .await
        .expect_err("nested manifest");
    assert_eq!(error.code, "invalid_skill_package");
    std::fs::remove_file(nested.join("SKILL.md")).expect("remove");
    common::write_file(&nested.join("sKiLl.Md"), "---\nname: hidden\n---\n");
    let error = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source.clone(),
            source_label: None,
        })
        .await
        .expect_err("nested manifest case");
    assert_eq!(error.code, "invalid_skill_package");

    assert!(registry.list().await.expect("list").is_empty());
}

#[tokio::test]
async fn import_skips_runtime_state_and_keeps_semantic_hidden_files() {
    let home = common::temp_home("skills-omit");
    let source = common::write_skill_package(home.path(), "source", "body");
    common::mkdir(&source.join("node_modules"));
    common::write_file(&source.join("node_modules/dependency.js"), "export default 1;\n");
    common::mkdir(&source.join("tests"));
    common::write_file(
        &source.join("tests/fake-key.txt"),
        "-----BEGIN PRIVATE KEY-----\n",
    );
    common::mkdir(&source.join(".signature"));
    common::write_file(&source.join(".signature/manifest.json"), "{}\n");
    common::write_file(&source.join(".todoist-token"), "placeholder\n");
    common::write_file(&source.join(".DS_Store"), "metadata\n");
    let registry = FsSkillRegistry::new(home.path().join("home")).expect("registry");
    let imported = registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source,
            source_label: None,
        })
        .await
        .expect("import");
    assert!(!imported.path.join("node_modules").exists());
    assert!(!imported.path.join("tests").exists());
    assert!(!imported.path.join(".todoist-token").exists());
    assert!(!imported.path.join(".DS_Store").exists());
    assert!(imported.path.join(".signature/manifest.json").exists());
    assert_eq!(imported.file_count, 2);
    common::remove_tree(&home.path().join("home/skills/objects"));
}

#[tokio::test]
async fn frozen_snapshot_tampering_is_detected_before_launch() {
    let home = common::temp_home("skills-tamper");
    let source = common::write_skill_package(home.path(), "source", "body");
    let registry = FsSkillRegistry::new(home.path().join("home")).expect("registry");
    registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source,
            source_label: None,
        })
        .await
        .expect("import");
    let authorization = registry
        .resolve_authorization(Some(&SkillPolicy::Only {
            ids: vec!["sample".to_string()],
        }))
        .await
        .expect("authorization");

    let object = authorization.grants[0].path.clone();
    common::set_mode(&object, 0o700);
    common::set_mode(&object.join("SKILL.md"), 0o600);
    common::write_file(&object.join("SKILL.md"), "tampered\n");
    let error = registry
        .verify(&authorization)
        .await
        .expect_err("tampered snapshot");
    assert_eq!(error.code, "skill_snapshot_changed");
    common::remove_tree(&home.path().join("home/skills/objects"));
}

#[tokio::test]
async fn policy_requires_known_unique_ids_and_rejects_duplicates() {
    let home = common::temp_home("skills-policy");
    let source = common::write_skill_package(home.path(), "source", "body");
    let registry = FsSkillRegistry::new(home.path().join("home")).expect("registry");

    let empty = registry.resolve_authorization(None).await.expect("all");
    assert!(empty.grants.is_empty());
    assert_eq!(empty.mode, SkillPolicyMode::All);

    registry
        .import(pihub_contracts::SkillImportInput {
            id: "sample".to_string(),
            source_path: source,
            source_label: None,
        })
        .await
        .expect("import");

    let error = registry
        .resolve_authorization(Some(&SkillPolicy::Only {
            ids: vec!["missing".to_string()],
        }))
        .await
        .expect_err("unknown id");
    assert_eq!(error.code, "skill_not_found");

    let error = registry
        .resolve_authorization(Some(&SkillPolicy::Only {
            ids: vec!["sample".to_string(), "sample".to_string()],
        }))
        .await
        .expect_err("duplicate id");
    assert_eq!(error.code, "invalid_skill_policy");

    let all = registry.resolve_authorization(None).await.expect("all");
    assert_eq!(all.grants.len(), 1);
    common::remove_tree(&home.path().join("home/skills/objects"));
}
