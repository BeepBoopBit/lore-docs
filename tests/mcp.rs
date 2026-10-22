use anyhow::Result;
use lore::{embedding::Embedder, index::Index, search::estimate_tokens};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct FixtureEncoder;
impl Embedder for FixtureEncoder {
    fn identity(&self) -> &str {
        "mcp-keyword-fixture"
    }
    fn dimensions(&self) -> usize {
        384
    }
    fn token_count(&self, text: &str) -> Result<usize> {
        Ok(text.len().div_ceil(3))
    }
    fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0; 384]).collect())
    }
    fn embed_query(&mut self, _: &str) -> Result<Vec<f32>> {
        panic!("keyword fixture must not embed queries")
    }
}

fn fixture() -> (TempDir, TempDir, Index) {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut source = String::from(
        "# API\n\nIntroduction for the entire API.\n\n## Deliver\n\n`deliver(text, submit=True)` sends input.\n\n### Pending\n\nUse `submit=False` to leave input pending.\n",
    );
    for i in 0..35 {
        source.push_str(&format!(
            "\n## Topic {i}\n\nUTF-8 日本語 source paragraph {i}. {}\n",
            "Retain delivery identity and retry carefully. ".repeat(5)
        ));
    }
    fs::write(corpus.path().join("api.md"), source).unwrap();
    fs::write(
        corpus.path().join("other.md"),
        "# Other\n\nOther delivery details.\n",
    )
    .unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index.index(corpus.path(), &mut FixtureEncoder).unwrap();
    (corpus, state, index)
}

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<std::result::Result<Value, String>>,
    next_id: u64,
}
impl Client {
    fn spawn(state: &Path, profile: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        command
            .args(["--index-dir", state.to_str().unwrap(), "--model-dir"])
            .arg(state.join("missing-model"))
            .arg("mcp");
        if let Some(path) = profile {
            command.arg("--usage-config").arg(path);
        }
        let mut child = command
            .current_dir(state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let value = line.map_err(|e| e.to_string()).and_then(|line| {
                    serde_json::from_str(&line)
                        .map_err(|e| format!("Non-protocol stdout: {e}: {line}"))
                });
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            replies,
            next_id: 1,
        }
    }
    fn send(&mut self, value: Value) {
        let input = self.input.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &value).unwrap();
        writeln!(input).unwrap();
        input.flush().unwrap();
    }
    fn receive(&self) -> Value {
        let response = self
            .replies
            .recv_timeout(Duration::from_secs(15))
            .expect("MCP response timeout/disconnect")
            .unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        response
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let result = self.receive();
        assert_eq!(result["id"], id);
        result
    }
    fn initialize(&mut self, version: &str) {
        let r = self.rpc("initialize", json!({"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"lore-tests","version":"1"}}));
        assert_eq!(r["result"]["protocolVersion"], version, "{r}");
        assert_eq!(r["result"]["serverInfo"]["name"], "lore");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    }
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({"name":name,"arguments":arguments}))
    }
    fn close(&mut self) {
        self.input.take();
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "Server did not stop after stdin EOF"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn payload(response: Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    let result = &response["result"];
    assert_eq!(result["isError"], false, "{result}");
    let data = result["structuredContent"].clone();
    assert!(data.is_object(), "{result}");
    assert_eq!(
        serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
        data
    );
    data
}
fn tool_error(response: Value, needle: &str) {
    assert!(
        response.get("error").is_none(),
        "Expected tool execution error: {response}"
    );
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains(&needle.to_lowercase()),
        "{response}"
    );
}
fn stable(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            map.remove("elapsed_ms");
            map.remove("estimated_tokens");
            for v in map.values_mut() {
                *v = stable(v.take());
            }
        }
        Value::Array(values) => {
            for v in values {
                *v = stable(v.take());
            }
        }
        _ => {}
    }
    value
}

#[test]
fn mcp_negotiates_lists_schemas_and_matches_cli_without_models() {
    let (_corpus, state, _) = fixture();
    let mut client = Client::spawn(state.path(), None);
    client.initialize("2025-11-25");
    let list = client.rpc("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "lore_brief",
            "lore_read",
            "lore_outline",
            "lore_plan",
            "lore_diagnose",
            "lore_status",
            "lore_init"
        ])
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        let initializes = tool["name"] == "lore_init";
        assert_eq!(tool["annotations"]["readOnlyHint"], !initializes);
        assert_eq!(tool["annotations"]["openWorldHint"], initializes);
    }
    let status = payload(client.call("lore_status", json!({})));
    assert_eq!(status["needs_reindex"], false);
    assert_eq!(status["source_check"]["checked_documents"], 2);
    assert_eq!(status["source_profiles"], json!(["default"]));
    let query =
        json!({"query":"pending input","mode":"keyword","scope":"api.md","prefer":["python"]});
    let brief = payload(client.call("lore_brief", query.clone()));
    assert!(brief["estimated_tokens"].as_u64().unwrap() <= 2000);
    assert_eq!(brief["guidance"]["answer_completeness"], "not_assessed");
    let cli = Command::new(env!("CARGO_BIN_EXE_lore"))
        .args([
            "--index-dir",
            state.path().to_str().unwrap(),
            "brief",
            "pending input",
            "--mode",
            "keyword",
            "--scope",
            "api.md",
            "--prefer",
            "python",
            "--with-guidance",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(cli.status.success());
    assert_eq!(
        stable(brief),
        stable(serde_json::from_slice(&cli.stdout).unwrap())
    );
    let diagnostic = payload(client.call("lore_diagnose", query));
    assert_eq!(diagnostic["answer_completeness"], "not_assessed");
    assert!(estimate_tokens(&diagnostic.to_string()) <= 2000);
    client.close();
}

#[test]
fn mcp_paginates_and_reconstructs_verified_source_and_context() {
    let (corpus, state, index) = fixture();
    let mut client = Client::spawn(state.path(), None);
    client.initialize("2025-06-18");
    let mut cursor = Value::Null;
    let mut text = String::new();
    let mut pages = 0;
    loop {
        let read = payload(client.call(
            "lore_read",
            json!({"target":"api.md","max_tokens":600,"cursor":cursor}),
        ));
        assert!(estimate_tokens(&read.to_string()) <= 600);
        let range = read["content"]["bytes"].as_array().unwrap();
        assert_eq!(range[0].as_u64().unwrap() as usize, text.len());
        text.push_str(read["content"]["text"].as_str().unwrap());
        assert_eq!(range[1].as_u64().unwrap() as usize, text.len());
        pages += 1;
        assert!(pages < 100);
        if !read["has_more"].as_bool().unwrap() {
            break;
        }
        cursor = read["next_cursor"].clone();
    }
    assert!(pages > 1);
    assert_eq!(
        text,
        fs::read_to_string(corpus.path().join("api.md")).unwrap()
    );
    let plan = payload(client.call("lore_plan", json!({"target":"api.md#pending"})));
    assert_eq!(plan["total_steps"], 3);
    let context = payload(client.call(
        "lore_read",
        json!({"target":"api.md#pending","context":true}),
    ));
    assert_eq!(context["parts"].as_array().unwrap().len(), 3);
    assert_eq!(
        stable(context),
        stable(
            serde_json::to_value(
                index
                    .read_context("api.md#pending", &Default::default())
                    .unwrap()
            )
            .unwrap()
        )
    );
    let outline = payload(client.call("lore_outline", json!({"path":"api.md","max_tokens":600})));
    assert_eq!(outline["has_more"], true);
    let next = payload(client.call(
        "lore_outline",
        json!({"path":"api.md","max_tokens":600,"cursor":outline["next_cursor"]}),
    ));
    assert_ne!(
        outline["sections"][0]["anchor"],
        next["sections"][0]["anchor"]
    );
    tool_error(
        client.call(
            "lore_plan",
            json!({"target":"api.md","cursor":outline["next_cursor"]}),
        ),
        "cursor",
    );
    client.close();
}

#[test]
fn mcp_errors_are_recoverable_and_preserve_source_boundaries() {
    let (corpus, state, mut index) = fixture();
    let mut client = Client::spawn(state.path(), None);
    client.initialize("2025-11-25");
    assert!(client.call("missing_tool", json!({}))["error"].is_object());
    for (name, args) in [
        ("lore_read", json!({"target":42})),
        ("lore_status", json!({"index_dir":"elsewhere"})),
        ("lore_brief", json!({"query":"input","mode":"invalid"})),
    ] {
        tool_error(client.call(name, args), "deserialize");
    }
    tool_error(
        client.call("lore_read", json!({"target":"../outside.md"})),
        "relative",
    );
    tool_error(
        client.call("lore_read", json!({"target":corpus.path().join("api.md")})),
        "relative",
    );
    tool_error(
        client.call("lore_read", json!({"target":"api.md#absent"})),
        "anchor",
    );
    tool_error(
        client.call("lore_brief", json!({"query":"input"})),
        "model asset",
    );
    tool_error(
        client.call(
            "lore_brief",
            json!({"query":"input","mode":"keyword","source_profile":"usage"}),
        ),
        "unavailable",
    );
    tool_error(
        client.call("lore_read", json!({"target":"api.md","max_tokens":16001})),
        "max_tokens",
    );
    tool_error(
        client.call("lore_brief", json!({"query":" ","mode":"keyword"})),
        "nonempty",
    );
    let first = payload(client.call("lore_read", json!({"target":"api.md","max_tokens":600})));
    fs::write(corpus.path().join("api.md"), "# Changed\n\nNew source.\n").unwrap();
    tool_error(
        client.call("lore_read", json!({"target":"api.md"})),
        "changed",
    );
    let stale = payload(client.call("lore_status", json!({})));
    assert_eq!(stale["needs_reindex"], true);
    index.index(corpus.path(), &mut FixtureEncoder).unwrap();
    tool_error(
        client.call(
            "lore_read",
            json!({"target":"api.md","cursor":first["next_cursor"]}),
        ),
        "cursor",
    );
    let fresh = payload(client.call("lore_read", json!({"target":"api.md"})));
    assert_eq!(fresh["content"]["text"], "# Changed\n\nNew source.\n");
    assert_eq!(
        payload(client.call("lore_status", json!({})))["needs_reindex"],
        false
    );
    client.close();
}

#[test]
fn mcp_source_profiles_are_per_call_and_concurrent_ids_are_preserved() {
    let (_corpus, state, _) = fixture();
    let config = state.path().join("usage.json");
    fs::write(
        &config,
        r#"{"roles":[{"prefix":"api.md","role":"consumer","weight":1.1}]}"#,
    )
    .unwrap();
    let mut client = Client::spawn(state.path(), Some(&config));
    client.initialize("2025-03-26");
    assert_eq!(
        payload(client.call("lore_status", json!({})))["source_profiles"],
        json!(["default", "usage"])
    );
    for profile in ["default", "usage", "default"] {
        let brief = payload(client.call(
            "lore_brief",
            json!({"query":"api.md#pending","mode":"keyword","source_profile":profile}),
        ));
        assert_eq!(brief["brief"]["files"][0]["path"], "api.md");
        assert_eq!(
            brief["brief"]["files"][0]["role"],
            if profile == "usage" {
                json!("consumer")
            } else {
                Value::Null
            }
        );
    }
    for id in 100..104 {
        client.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"lore_read","arguments":{"target":"api.md#pending"}}}));
    }
    client.send(json!({"jsonrpc":"2.0","id":"ping-id","method":"ping"}));
    let mut ids = BTreeSet::new();
    for _ in 0..5 {
        let r = client.receive();
        ids.insert(r["id"].to_string());
        if r["id"] != "ping-id" {
            assert!(
                payload(r)["content"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("submit=False")
            );
        }
    }
    assert_eq!(
        ids,
        BTreeSet::from(["100", "101", "102", "103", "\"ping-id\""])
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    client.close();
}

#[test]
fn mcp_supports_discovery_and_inline_protocol_metadata() {
    let (_corpus, state, _) = fixture();
    let mut client = Client::spawn(state.path(), None);
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28", "io.modelcontextprotocol/clientInfo":{"name":"lore-tests","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}});
    let discover = client.rpc("server/discover", json!({"_meta":meta}));
    assert!(
        discover["result"]["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28"))
    );
    let list = client.rpc("tools/list", json!({"_meta":meta}));
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 7);
    let r = client.rpc(
        "tools/call",
        json!({"_meta":meta,"name":"lore_status","arguments":{}}),
    );
    assert_eq!(payload(r)["documents"], 2);
    client.close();
}

#[test]
fn mcp_bootstraps_missing_indexes_and_rejects_invalid_profiles_on_stderr() {
    let empty = TempDir::new().unwrap();
    let mut client = Client::spawn(empty.path(), None);
    client.initialize("2025-11-25");
    let status = payload(client.call("lore_status", json!({})));
    assert_eq!(status["initialized"], false);
    assert_eq!(status["state"], "not_initialized");
    let search = client.call("lore_brief", json!({"query":"pending", "mode":"keyword"}));
    assert_eq!(search["result"]["isError"], true);
    let initialized = client.call("lore_init", json!({"offline":true}));
    assert_eq!(initialized["result"]["isError"], true);
    assert!(!empty.path().join("index.sqlite").exists());
    let escaped = client.call("lore_init", json!({"root":"/elsewhere", "offline":true}));
    assert_eq!(escaped["result"]["isError"], true);
    client.close();
    let (_corpus, state, _) = fixture();
    let config = state.path().join("invalid.json");
    fs::write(
        &config,
        r#"{"roles":[{"prefix":"api.md","role":"consumer","weight":0}]}"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lore"))
        .args([
            "--index-dir",
            state.path().to_str().unwrap(),
            "mcp",
            "--usage-config",
            config.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success() && out.stdout.is_empty());
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("weight between")
    );
}
