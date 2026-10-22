//! File updates and client registration shared by CLI onboarding.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Debug, Clone, Copy, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    Codex,
    Claude,
}

pub fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

pub fn read_text(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                !meta.file_type().is_symlink() && meta.is_file(),
                "Refusing to replace a symlink or non-file: {}",
                path.display()
            );
            Ok(Some(fs::read_to_string(path).with_context(|| {
                format!("Cannot read {}", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let previous = read_text(path)?;
    if previous.as_deref() == Some(text) {
        return Ok(());
    }
    let parent = path.parent().context("File needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if path.exists() {
        temporary
            .as_file()
            .set_permissions(fs::metadata(path)?.permissions())?;
    }
    temporary.write_all(text.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .with_context(|| format!("Cannot save {}", path.display()))?;
    Ok(())
}

pub fn ignore_state(root: &Path, directory: &str) -> Result<()> {
    ignore_path(root, &root.join(directory), true)?;
    ignore_path(root, &root.join(directory).join("project.json"), false)
}

pub fn ignore_database(root: &Path, database: &Path) -> Result<()> {
    ignore_path(root, database, false)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = database.as_os_str().to_owned();
        sidecar.push(suffix);
        ignore_path(root, Path::new(&sidecar), false)?;
    }
    Ok(())
}

fn ignore_path(root: &Path, target: &Path, directory: bool) -> Result<()> {
    let Ok(relative) = target.strip_prefix(root) else {
        return Ok(());
    };
    if relative.as_os_str().is_empty() {
        return Ok(());
    }
    let path = root.join(".gitignore");
    let mut text = read_text(&path)?.unwrap_or_default();
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    for line in text.lines() {
        let _ = builder.add_line(Some(path.clone()), line);
    }
    let rules = builder.build()?;
    if !rules
        .matched_path_or_any_parents(target, directory)
        .is_ignore()
    {
        let literal = relative
            .components()
            .map(|part| {
                part.as_os_str()
                    .to_string_lossy()
                    .chars()
                    .flat_map(|ch| {
                        if "*?[]\\!# ".contains(ch) {
                            vec!['\\', ch]
                        } else {
                            vec![ch]
                        }
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("/");
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("/{literal}{}\n", if directory { "/" } else { "" }));
        write_atomic(&path, &text)?;
    }
    Ok(())
}

pub fn validate_state_directory(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "State directory must be a directory, not a symlink: {}",
            path.display()
        );
    }
    Ok(())
}

pub fn home() -> Result<PathBuf> {
    std::env::home_dir()
        .filter(|path| path.is_absolute())
        .context("Cannot determine home directory")
}

pub fn discover(start: &Path, directory: &str) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|root| root.join(directory).join("project.json").is_file())
        .map(Path::to_owned)
}

pub fn managed_instructions(path: &Path, product: &str, instructions: &str) -> Result<String> {
    let original = read_text(path)?.unwrap_or_default();
    let begin = format!("<!-- {product}:begin -->");
    let end = format!("<!-- {product}:end -->");
    let block = format!("{begin}\n{instructions}\n{end}");
    match (original.find(&begin), original.find(&end)) {
        (Some(start), Some(finish)) if finish >= start => {
            ensure!(
                original.matches(&begin).count() == 1 && original.matches(&end).count() == 1,
                "Duplicate managed blocks in {}; fix them before setup",
                path.display()
            );
            Ok(format!(
                "{}{}{}",
                &original[..start],
                block,
                &original[finish + end.len()..]
            ))
        }
        (None, None) => {
            let separator = if original.is_empty() || original.ends_with("\n\n") {
                ""
            } else if original.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            Ok(format!("{original}{separator}{block}\n"))
        }
        _ => bail!(
            "Incomplete managed block in {}; fix it before setup",
            path.display()
        ),
    }
}

pub struct Registration {
    pub name: String,
    pub configuration: PathBuf,
    pub instructions: PathBuf,
    writes: Vec<(PathBuf, String)>,
}

impl Registration {
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        product: &str,
        root: &Path,
        client: Client,
        global: bool,
        name: Option<&str>,
        replace: bool,
        args: &[String],
        instructions: &str,
    ) -> Result<Self> {
        let name = name.map(str::to_owned).unwrap_or_else(|| {
            if global {
                let hash = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
                let basename = root
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .chars()
                    .map(|ch| {
                        if ch.is_ascii_alphanumeric() || ch == '-' {
                            ch
                        } else {
                            '-'
                        }
                    })
                    .collect::<String>();
                format!("{product}-{basename}-{}", &hash[..8])
            } else {
                product.to_owned()
            }
        });
        ensure!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'),
            "Server name must contain only letters, digits, underscores, or hyphens"
        );
        let executable = std::env::current_exe()?.canonicalize()?;
        let instruction_path = root.join(match client {
            Client::Codex => "AGENTS.md",
            Client::Claude => "CLAUDE.md",
        });
        let instruction_text = managed_instructions(&instruction_path, product, instructions)?;
        let (configuration, text) = match client {
            Client::Codex => {
                let path = if global {
                    std::env::var_os("CODEX_HOME")
                        .map(PathBuf::from)
                        .unwrap_or(home()?.join(".codex"))
                        .join("config.toml")
                } else {
                    root.join(".codex/config.toml")
                };
                let mut doc = read_text(&path)?
                    .unwrap_or_default()
                    .parse::<DocumentMut>()
                    .with_context(|| format!("Invalid TOML in {}", path.display()))?;
                if doc.get("mcp_servers").is_none() {
                    doc["mcp_servers"] = Item::Table(Table::new());
                }
                let servers = doc["mcp_servers"]
                    .as_table_like_mut()
                    .context("mcp_servers must be a TOML table")?;
                let desired_args = args
                    .iter()
                    .map(|arg| toml_edit::Value::from(arg.as_str()))
                    .collect::<toml_edit::Array>();
                let same = servers.get(&name).is_some_and(|entry| {
                    entry.get("command").and_then(Item::as_str) == executable.to_str()
                        && entry
                            .get("args")
                            .and_then(Item::as_array)
                            .is_some_and(|items| {
                                items
                                    .iter()
                                    .filter_map(toml_edit::Value::as_str)
                                    .eq(args.iter().map(String::as_str))
                            })
                        && entry.get("cwd").and_then(Item::as_str) == root.to_str()
                });
                ensure!(
                    servers.get(&name).is_none() || same || replace,
                    "MCP server '{name}' already has different settings in {}; use --replace or --name",
                    path.display()
                );
                if !same {
                    let mut server = Table::new();
                    server["command"] = value(executable.to_string_lossy().as_ref());
                    server["args"] = value(desired_args);
                    server["cwd"] = value(root.to_string_lossy().as_ref());
                    server["startup_timeout_sec"] = value(60);
                    server["tool_timeout_sec"] = value(300);
                    servers.insert(&name, Item::Table(server));
                }
                (absolute(&path)?, doc.to_string())
            }
            Client::Claude => {
                let path = if global {
                    if let Some(directory) = std::env::var_os("CLAUDE_CONFIG_DIR") {
                        PathBuf::from(directory).join(".claude.json")
                    } else {
                        home()?.join(".claude.json")
                    }
                } else {
                    root.join(".mcp.json")
                };
                let mut doc: Value =
                    serde_json::from_str(&read_text(&path)?.unwrap_or_else(|| "{}".into()))
                        .with_context(|| format!("Invalid JSON in {}", path.display()))?;
                let object = doc
                    .as_object_mut()
                    .context("Claude configuration must be a JSON object")?;
                let servers = object
                    .entry("mcpServers")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .context("mcpServers must be a JSON object")?;
                let desired = json!({"type":"stdio", "command":executable, "args":args});
                ensure!(
                    servers.get(&name).is_none() || servers.get(&name) == Some(&desired) || replace,
                    "MCP server '{name}' already has different settings in {}; use --replace or --name",
                    path.display()
                );
                servers.insert(name.clone(), desired);
                (
                    absolute(&path)?,
                    format!("{}\n", serde_json::to_string_pretty(&doc)?),
                )
            }
        };
        Ok(Self {
            name,
            configuration: configuration.clone(),
            instructions: instruction_path.clone(),
            writes: vec![(configuration, text), (instruction_path, instruction_text)],
        })
    }

    pub fn complete<T>(&self, prepare: impl FnOnce() -> Result<T>) -> Result<T> {
        let originals = self
            .writes
            .iter()
            .map(|(path, _)| Ok((path, read_text(path)?)))
            .collect::<Result<Vec<_>>>()?;
        let result = (|| {
            // Include instructions in the first index, keeping setup's index fresh.
            write_atomic(&self.writes[1].0, &self.writes[1].1)?;
            let report = prepare()?;
            ensure!(
                read_text(&self.writes[0].0)? == originals[0].1,
                "Client configuration changed during setup; rerun setup to preserve the new settings"
            );
            ensure!(
                read_text(&self.writes[1].0)?.as_deref() == Some(&self.writes[1].1),
                "Agent instructions changed during setup; rerun setup to preserve the new instructions"
            );
            write_atomic(&self.writes[0].0, &self.writes[0].1)?;
            Ok(report)
        })();
        if result.is_err() {
            for ((path, original), (_, written)) in originals.iter().zip(&self.writes) {
                if read_text(path)?.as_deref() == Some(written) {
                    if let Some(original) = original {
                        write_atomic(path, original)?;
                    } else {
                        fs::remove_file(path)?;
                    }
                }
            }
        }
        result
    }
}

pub fn probe(args: &[String], status_tool: &str, root: &Path) -> Result<Value> {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(
        Command::new(std::env::current_exe()?)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let mut input = child.0.stdin.take().context("MCP stdin unavailable")?;
    let output = child.0.stdout.take().context("MCP stdout unavailable")?;
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let result = (|| -> Result<Value> {
        {
            let mut request = |id: u64, method: &str, params: Value| -> Result<Value> {
                writeln!(
                    input,
                    "{}",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )?;
                input.flush()?;
                let deadline = std::time::Instant::now() + Duration::from_secs(60);
                loop {
                    let line = receiver
                        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                        .context("MCP verification timed out or the server closed stdout")??;
                    let value: Value = serde_json::from_str(&line)
                        .context("MCP stdout contained non-protocol output")?;
                    if value["id"] == id {
                        ensure!(value.get("error").is_none(), "MCP request failed: {value}");
                        return Ok(value);
                    }
                }
            };
            request(
                1,
                "initialize",
                json!({"protocolVersion":"2025-11-25","clientInfo":{"name":"setup-verification","version":"1"},"capabilities":{}}),
            )?;
        }
        // Initialization is complete before requests to the tool router are sent.
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )?;
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":status_tool,"arguments":{}}})
        )?;
        input.flush()?;
        loop {
            let line = receiver
                .recv_timeout(Duration::from_secs(60))
                .context("MCP status verification timed out")??;
            let value: Value = serde_json::from_str(&line)?;
            if value["id"] != 2 {
                continue;
            }
            ensure!(
                value.get("error").is_none() && value["result"]["isError"] != true,
                "MCP status failed: {value}"
            );
            let status = value["result"]["structuredContent"].clone();
            ensure!(
                status["root"].as_str() == root.to_str(),
                "MCP returned the wrong project root: {status}"
            );
            ensure!(
                status["initialized"] == true,
                "MCP project is not initialized: {status}"
            );
            ensure!(
                status["needs_reindex"] != true && status["index"]["syntax_freshness"] != "stale",
                "Project sources changed during setup; rerun setup to refresh them"
            );
            return Ok(status);
        }
    })();
    drop(input);
    drop(child);
    let _ = reader.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_rules_escape_literal_paths_and_preserve_existing_ignore_rules() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join(".gitignore"), "# Keep\nbuild/\n").unwrap();
        let database = root.join("custom [index].sqlite");
        ignore_database(&root, &database).unwrap();
        let first = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(first.starts_with("# Keep\nbuild/\n"));
        ignore_database(&root, &database).unwrap();
        assert_eq!(fs::read_to_string(root.join(".gitignore")).unwrap(), first);
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
        builder.add(root.join(".gitignore"));
        let rules = builder.build().unwrap();
        assert!(
            rules
                .matched_path_or_any_parents(&database, false)
                .is_ignore()
        );
        assert!(
            !rules
                .matched_path_or_any_parents(root.join("custom i.sqlite"), false)
                .is_ignore()
        );
        ignore_database(&root, &temp.path().parent().unwrap().join("outside.sqlite")).unwrap();
        assert_eq!(fs::read_to_string(root.join(".gitignore")).unwrap(), first);
    }

    #[test]
    fn concurrent_client_edits_are_preserved_when_setup_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let plan = Registration::prepare(
            "tool",
            &root,
            Client::Claude,
            false,
            None,
            false,
            &["mcp".into()],
            "Use the tool.",
        )
        .unwrap();
        let result = plan.complete(|| {
            fs::write(root.join(".mcp.json"), r#"{"newSetting":true}"#)?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(root.join(".mcp.json")).unwrap(),
            r#"{"newSetting":true}"#
        );
        assert!(!root.join("CLAUDE.md").exists());
    }

    #[test]
    fn managed_blocks_are_repeatable_and_preserve_surrounding_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("AGENTS.md");
        fs::write(&path, "Keep these instructions.\n").unwrap();
        let first = managed_instructions(&path, "tool", "Use the tool.").unwrap();
        write_atomic(&path, &first).unwrap();
        assert_eq!(
            managed_instructions(&path, "tool", "Use the tool.").unwrap(),
            first
        );
        let updated = managed_instructions(&path, "tool", "New guidance.").unwrap();
        assert!(updated.starts_with("Keep these instructions.\n"));
        assert_eq!(updated.matches("<!-- tool:begin -->").count(), 1);
        assert!(!updated.contains("Use the tool."));
        fs::write(&path, "<!-- tool:begin -->\nunfinished").unwrap();
        assert!(managed_instructions(&path, "tool", "new").is_err());
    }

    #[test]
    fn codex_setup_preserves_toml_and_rolls_back_instructions_on_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir(root.join(".codex")).unwrap();
        let path = root.join(".codex/config.toml");
        let original = "# Personal settings\nmodel = 'keep-model'\n\n[mcp_servers.other]\ncommand = 'other-tool' # Keep this comment\n";
        fs::write(&path, original).unwrap();
        fs::write(root.join("AGENTS.md"), "Original instructions.\n").unwrap();
        let plan = Registration::prepare(
            "tool",
            &root,
            Client::Codex,
            false,
            None,
            false,
            &["mcp".into(), "--root".into(), root.to_string_lossy().into()],
            "Use the tool.",
        )
        .unwrap();
        assert!(
            plan.complete(|| -> Result<()> { bail!("preparation failed") })
                .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            fs::read_to_string(root.join("AGENTS.md")).unwrap(),
            "Original instructions.\n"
        );
        plan.complete(|| Ok(())).unwrap();
        let configured = fs::read_to_string(&path).unwrap();
        assert!(configured.contains("model = 'keep-model'"));
        assert!(configured.contains("# Keep this comment"));
        let document = configured.parse::<DocumentMut>().unwrap();
        assert_eq!(
            document["mcp_servers"]["other"]["command"].as_str(),
            Some("other-tool")
        );
        let again = Registration::prepare(
            "tool",
            &root,
            Client::Codex,
            false,
            None,
            false,
            &["mcp".into(), "--root".into(), root.to_string_lossy().into()],
            "Use the tool.",
        )
        .unwrap();
        again.complete(|| Ok(())).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), configured);
        assert!(
            Registration::prepare(
                "tool",
                &root,
                Client::Codex,
                false,
                None,
                false,
                &["different-command".into()],
                "Use the tool."
            )
            .is_err()
        );
    }

    #[test]
    fn claude_setup_preserves_other_servers_and_requires_explicit_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join(".mcp.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"keep"}},"custom":42}"#,
        )
        .unwrap();
        let plan = Registration::prepare(
            "tool",
            &root,
            Client::Claude,
            false,
            None,
            false,
            &["mcp".into()],
            "Use the tool.",
        )
        .unwrap();
        plan.complete(|| Ok(())).unwrap();
        let configured: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(configured["custom"], 42);
        assert_eq!(configured["mcpServers"]["other"]["command"], "keep");
        assert!(
            Registration::prepare(
                "tool",
                &root,
                Client::Claude,
                false,
                None,
                false,
                &["changed".into()],
                "Use the tool."
            )
            .is_err()
        );
        Registration::prepare(
            "tool",
            &root,
            Client::Claude,
            false,
            None,
            true,
            &["changed".into()],
            "Use the tool.",
        )
        .unwrap()
        .complete(|| Ok(()))
        .unwrap();
        let changed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(changed["mcpServers"]["tool"]["args"][0], "changed");
        fs::write(&path, "{ invalid json").unwrap();
        assert!(
            Registration::prepare(
                "tool",
                &root,
                Client::Claude,
                false,
                None,
                true,
                &["mcp".into()],
                "Use the tool."
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn initialization_never_replaces_a_symlinked_gitignore() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("outside");
        fs::write(&target, "private\n").unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join(".gitignore")).unwrap();
        assert!(ignore_state(temp.path(), ".tool").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "private\n");
    }
}
