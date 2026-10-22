use anyhow::Result;
use lore::{embedding::Embedder, index::Index};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct StubModel;
impl Embedder for StubModel {
    fn identity(&self) -> &str {
        "onboarding-test"
    }
    fn dimensions(&self) -> usize {
        384
    }
    fn token_count(&self, text: &str) -> Result<usize> {
        Ok(text.len() / 4 + 1)
    }
    fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![
            (0..384)
                .map(|i| if i == 0 { 1.0 } else { 0.0 })
                .collect();
            texts.len()
        ])
    }
    fn embed_query(&mut self, _: &str) -> Result<Vec<f32>> {
        Ok((0..384).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect())
    }
}
fn command(root: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    command
        .current_dir(root)
        .env("HOME", home)
        .env("CODEX_HOME", home.join("codex"));
    command
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project with spaces");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("README.md"),
        "# Authentication\nUse request tokens to authenticate.\n",
    )
    .unwrap();
    (temp, root, home)
}

#[test]
fn uninitialized_status_and_search_do_not_create_an_index() {
    let (_temp, root, home) = fixture();
    let status = command(&root, &home)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let value: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(value["initialized"], false);
    assert_eq!(value["state"], "not_initialized");
    let search = command(&root, &home)
        .args(["brief", "authentication", "--mode", "keyword"])
        .output()
        .unwrap();
    assert!(!search.status.success());
    assert!(String::from_utf8_lossy(&search.stderr).contains("not_initialized"));
    assert!(!root.join(".lore").exists());
    let failed = command(&root, &home)
        .args(["init", "--offline", "--json"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("Missing model asset"));
    assert!(!root.join(".lore/index.sqlite").exists());
    assert!(!root.join(".lore/project.json").exists());
}

#[test]
fn legacy_indexes_are_adopted_and_saved_custom_paths_work_from_subdirectories() {
    let (temp, root, home) = fixture();
    let state = temp.path().join("custom index");
    let mut index = Index::open(&state).unwrap();
    index.index(&root, &mut StubModel).unwrap();
    let search = command(&root, &home)
        .arg("--index-dir")
        .arg(&state)
        .args(["brief", "authentication", "--mode", "keyword", "--json"])
        .output()
        .unwrap();
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let result: Value = serde_json::from_slice(&search.stdout).unwrap();
    assert!(!result["files"].as_array().unwrap().is_empty());
    fs::create_dir(root.join(".lore")).unwrap();
    fs::write(
        root.join(".lore/project.json"),
        serde_json::to_string_pretty(&json!({
            "version":1,"root":root,"index_dir":state,"model_dir":home.join("saved models"),
            "threads":3,"content":false
        }))
        .unwrap(),
    )
    .unwrap();
    let child = root.join("nested");
    fs::create_dir(&child).unwrap();
    let status = command(&child, &home)
        .args(["status", "--check", "--json"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let result: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(result["initialized"], true);
    assert_eq!(result["root"], root.to_str().unwrap());
    let config = lore::onboarding::configuration(Some(&root), None, None, None, None).unwrap();
    assert_eq!(config.index_dir, state);
    assert_eq!(config.model_dir, home.join("saved models"));
    assert_eq!(config.threads, 3);
}

#[test]
fn conflicting_client_registration_is_rejected_before_model_download_or_file_updates() {
    let (_temp, root, home) = fixture();
    let original = r#"{"mcpServers":{"lore":{"command":"another-tool"}}}"#;
    fs::write(root.join(".mcp.json"), original).unwrap();
    fs::write(root.join("CLAUDE.md"), "Existing instructions.\n").unwrap();
    let result = command(&root, &home)
        .args(["setup", "claude", "--offline", "--json"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--replace"));
    assert!(!root.join(".lore").exists());
    assert!(!home.exists());
    assert_eq!(
        fs::read_to_string(root.join(".mcp.json")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
        "Existing instructions.\n"
    );
    let result = command(&root, &home)
        .args(["setup", "claude", "--offline", "--replace", "--json"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Missing model asset"));
    assert_eq!(
        fs::read_to_string(root.join(".mcp.json")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
        "Existing instructions.\n"
    );
}

#[test]
fn moved_project_settings_cannot_redirect_initialization_to_the_original_project() {
    let (temp, root, home) = fixture();
    let original = temp.path().join("original");
    fs::create_dir(&original).unwrap();
    fs::create_dir(root.join(".lore")).unwrap();
    fs::write(
        root.join(".lore/project.json"),
        serde_json::to_string(&json!({
            "version":1,"root":original,"index_dir":original.join(".lore"),
            "model_dir":home.join("model"),"threads":2,"content":false
        }))
        .unwrap(),
    )
    .unwrap();
    let result = command(&root, &home)
        .args(["init", "--offline"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("moved or copied"));
    assert!(fs::read_dir(original).unwrap().next().is_none());
}
