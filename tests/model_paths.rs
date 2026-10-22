#![cfg(unix)]

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn lore(home: &Path, project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    command.env("HOME", home).current_dir(project);
    command
}

fn failure(output: Output) -> String {
    assert!(!output.status.success());
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn search_models_are_shared_across_working_directories_and_indexes() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home with spaces");
    let shared = home.join(".lore/models/bge-small");
    for name in ["first", "second"] {
        let project = temp.path().join(name);
        fs::create_dir(&project).unwrap();
        // A legacy project-local asset must not override the shared default.
        let legacy = project.join(".lore/models/bge-small");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("model_optimized.onnx"), "legacy asset").unwrap();
        for index in [".lore", "custom index"] {
            let error = failure(
                lore(&home, &project)
                    .args(["--index-dir", index, "index", "."])
                    .output()
                    .unwrap(),
            );
            assert!(
                error.contains(&shared.join("model_optimized.onnx").display().to_string()),
                "{error}"
            );
        }
    }
    assert!(!shared.exists(), "loading must not install model assets");
}

#[test]
fn explicit_search_model_directory_overrides_the_shared_default() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    for directory in [
        temp.path().join("absolute models"),
        "relative models".into(),
    ] {
        let error = failure(
            lore(&home, temp.path())
                .arg("--model-dir")
                .arg(&directory)
                .args(["index", "."])
                .output()
                .unwrap(),
        );
        assert!(
            error.contains(&directory.join("model_optimized.onnx").display().to_string()),
            "{error}"
        );
        assert!(!home.exists());
    }
}

#[test]
fn rerankers_are_shared_and_allow_an_explicit_directory() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let shared = home.join(".lore/models/minilm-reranker");
    fs::create_dir_all(&shared).unwrap();
    // Reaching checksum validation proves the shared asset was found.
    fs::write(shared.join("model_quint8_avx2.onnx"), "invalid model").unwrap();
    for name in ["first", "second"] {
        let project = temp.path().join(name);
        fs::create_dir(&project).unwrap();
        let state = project.join("custom index");
        lore::index::Index::open(&state).unwrap();
        rusqlite::Connection::open(state.join("index.sqlite"))
            .unwrap()
            .execute(
                "INSERT INTO metadata(key,value) VALUES('root', ?1)",
                [project.canonicalize().unwrap().to_str().unwrap()],
            )
            .unwrap();
        let args = [
            "--index-dir",
            "custom index",
            "brief",
            "question",
            "--mode",
            "keyword",
            "--rerank",
            "local",
        ];
        let error = failure(lore(&home, &project).args(args).output().unwrap());
        assert!(error.contains("Reranker checksum mismatch"), "{error}");
        let error = failure(
            lore(&home, &project)
                .args(["--reranker-dir", "custom reranker"])
                .args(args)
                .output()
                .unwrap(),
        );
        assert!(error.contains("Missing reranker asset"), "{error}");
        assert!(!project.join("custom reranker").exists());
    }
}
