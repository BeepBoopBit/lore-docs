use anyhow::Result;
use lore::{
    embedding::Embedder,
    index::{Index, resolve_link},
    search::{BriefOptions, Mode, estimate_tokens},
};
use std::fs;
use tempfile::TempDir;

struct TestEmbedder {
    calls: usize,
    fail: bool,
}

struct TestReranker {
    calls: usize,
    pairs: usize,
    invalid: bool,
}

#[test]
fn evaluation_rejects_index_changes_between_query_snapshots() {
    struct RefreshingEmbedder {
        model: TestEmbedder,
        root: std::path::PathBuf,
        state: std::path::PathBuf,
        refreshed: bool,
    }
    impl Embedder for RefreshingEmbedder {
        fn identity(&self) -> &str {
            self.model.identity()
        }
        fn dimensions(&self) -> usize {
            self.model.dimensions()
        }
        fn token_count(&self, text: &str) -> Result<usize> {
            self.model.token_count(text)
        }
        fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.model.embed_documents(texts)
        }
        fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
            if !self.refreshed {
                fs::write(
                    self.root.join("added.md"),
                    "# New\n\nA new document during evaluation.",
                )?;
                let mut writer = Index::open(&self.state)?;
                writer.index(&self.root, &mut TestEmbedder::new())?;
                self.refreshed = true;
            }
            self.model.embed_query(query)
        }
    }
    let (corpus, state, index, model) = fixture();
    let mut embedder = RefreshingEmbedder {
        model,
        root: corpus.path().into(),
        state: state.path().into(),
        refreshed: false,
    };
    let cases: Vec<lore::evaluate::EvaluationCase> = serde_json::from_value(serde_json::json!([
        {"id":"first","category":"snapshot","query":"payment","expected":[{"paths":["retry.md"],"contains":"idempotency key"}]},
        {"id":"second","category":"snapshot","query":"retry","expected":[{"paths":["retry.md"],"contains":"idempotency key"}]}
    ])).unwrap();
    let error = lore::evaluate::evaluate_with_options(
        &index,
        &mut embedder,
        &cases,
        &BriefOptions::default(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Index changed during evaluation"),
        "{error:#}"
    );
}

#[test]
fn rerank_shortlist_cli_needs_no_models_and_local_assets_are_checked() {
    let (_corpus, state, _index, _model) = fixture();
    let missing = state.path().join("missing");
    let run = |mode: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_lore"))
            .args([
                "--index-dir",
                state.path().to_str().unwrap(),
                "--model-dir",
                missing.to_str().unwrap(),
                "--reranker-dir",
                missing.to_str().unwrap(),
                "brief",
                "payment",
                "--mode",
                "keyword",
                "--rerank",
                mode,
                "--json",
            ])
            .output()
            .unwrap()
    };
    let control = run("shortlist");
    assert!(
        control.status.success(),
        "{}",
        String::from_utf8_lossy(&control.stderr)
    );
    let local = run("local");
    assert!(!local.status.success());
    assert!(String::from_utf8_lossy(&local.stderr).contains("model install --reranker"));
    assert!(!missing.exists());
    fs::create_dir(&missing).unwrap();
    fs::write(missing.join("model_quint8_avx2.onnx"), "not an ONNX model").unwrap();
    assert!(
        lore::reranking::LocalReranker::load(&missing, 2)
            .err()
            .unwrap()
            .to_string()
            .contains("checksum mismatch")
    );
}

#[test]
fn reranked_evidence_keeps_scope_roles_and_stale_source_errors() {
    use lore::{reranking::RerankMode, search::RoleRule};
    let (corpus, _state, mut index, mut model) = fixture();
    fs::create_dir(corpus.path().join("docs")).unwrap();
    for path in ["docs/current.md", "docs/historical.md", "outside.md"] {
        fs::write(
            corpus.path().join(path),
            format!("# Retry\n\nThe authoritative payment retry rule for {path}.\n"),
        )
        .unwrap();
    }
    index.index(corpus.path(), &mut model).unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        scope: "docs".into(),
        rerank: RerankMode::Local,
        recover_sections: true,
        connections: true,
        roles: vec![RoleRule {
            prefix: "docs/historical.md".into(),
            role: "historical".into(),
            weight: 0.1,
        }],
        ..Default::default()
    };
    let mut scorer = TestReranker {
        calls: 0,
        pairs: 0,
        invalid: false,
    };
    let d = index
        .diagnose_with_reranker("payment retry", None, Some(&mut scorer), &options)
        .unwrap();
    assert!(d.candidates.iter().all(|c| c.path.starts_with("docs/")));
    assert_eq!(d.brief.files[0].path, "docs/current.md");
    let current = d
        .selection_trace
        .iter()
        .find(|t| t.path == "docs/current.md" && t.kind == "block")
        .unwrap()
        .quality;
    let historical = d
        .selection_trace
        .iter()
        .find(|t| t.path == "docs/historical.md" && t.kind == "block")
        .unwrap()
        .quality;
    assert!(historical < current * 0.2);
    fs::write(corpus.path().join("docs/current.md"), "changed").unwrap();
    assert!(
        index
            .brief_with_reranker("payment retry", None, Some(&mut scorer), &options)
            .is_err()
    );
}
impl lore::reranking::Reranker for TestReranker {
    fn identity(&self) -> &str {
        "test-pair-scorer"
    }
    fn score(
        &mut self,
        _query: &str,
        passages: &[String],
    ) -> Result<Vec<lore::reranking::PairScore>> {
        self.calls += 1;
        self.pairs += passages.len();
        Ok(passages
            .iter()
            .map(|p| lore::reranking::PairScore {
                logit: if self.invalid {
                    f32::NAN
                } else if p.contains("authoritative") {
                    8.0
                } else {
                    -4.0
                },
                input_truncated: false,
            })
            .collect())
    }
}

#[test]
fn reranking_scores_a_fixed_bounded_pool_and_keeps_original_sources() {
    use lore::reranking::RerankMode;
    let (corpus, _state, mut index, mut model) = fixture();
    for i in 0..20 {
        let mut body = format!("# Payment reference {i}\n\n");
        for j in 0..8 {
            body.push_str(&format!(
                "## Retry {j}\n\nPayment retry detail {i} {j}. {}\n\n",
                if i == 0 && j == 0 {
                    "The authoritative rule uses a stable key."
                } else {
                    "Related background detail."
                }
            ));
        }
        fs::write(corpus.path().join(format!("p{i:02}.md")), body).unwrap();
    }
    index.index(corpus.path(), &mut model).unwrap();
    let mut options = BriefOptions {
        mode: Mode::Keyword,
        rerank: RerankMode::Shortlist,
        max_tokens: 700,
        candidates: 100,
        connections: false,
        ..Default::default()
    };
    let control = index.diagnose("payment retry", None, &options).unwrap();
    options.rerank = RerankMode::Local;
    let mut scorer = TestReranker {
        calls: 0,
        pairs: 0,
        invalid: false,
    };
    let result = index
        .diagnose_with_reranker("payment retry", None, Some(&mut scorer), &options)
        .unwrap();
    let report = result.reranking.as_ref().unwrap();
    assert_eq!(report.shortlist.len(), 64);
    assert_eq!(
        report.shortlist,
        control.reranking.as_ref().unwrap().shortlist
    );
    assert_eq!(scorer.calls, 1);
    assert_eq!(scorer.pairs, 64);
    assert_eq!(report.scores.len(), 64);
    assert!(
        result
            .brief
            .files
            .iter()
            .flat_map(|f| &f.passages)
            .any(|p| p.text.contains("authoritative"))
    );
    assert!(estimate_tokens(&serde_json::to_string(&result.brief).unwrap()) <= 700);
    for f in &result.brief.files {
        let bytes = fs::read(corpus.path().join(&f.path)).unwrap();
        for p in &f.passages {
            assert_eq!(p.text.as_bytes(), &bytes[p.bytes[0]..p.bytes[1]]);
        }
    }
    let signature = |d: &lore::search::Diagnostics| {
        d.candidates
            .iter()
            .map(|c| {
                (
                    c.section_id,
                    c.keyword_rank,
                    c.semantic_rank,
                    c.score,
                    c.file_score,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(signature(&control), signature(&result));
    assert!(
        result
            .selection_trace
            .iter()
            .any(|s| s.decision == "rerank_shortlist_limit")
    );
}

#[test]
fn reranking_reuses_scores_for_alternative_packing_and_rejects_bad_engines() {
    use lore::reranking::RerankMode;
    let (_corpus, _state, index, _model) = fixture();
    let options = BriefOptions {
        mode: Mode::Keyword,
        rerank: RerankMode::Local,
        alternatives: 3,
        max_tokens: 500,
        ..Default::default()
    };
    let mut scorer = TestReranker {
        calls: 0,
        pairs: 0,
        invalid: false,
    };
    let d = index
        .diagnose_with_reranker("payment", None, Some(&mut scorer), &options)
        .unwrap();
    assert_eq!(scorer.calls, 1);
    assert_eq!(scorer.pairs, d.reranking.unwrap().scores.len());
    assert!(
        index
            .brief("payment", None, &options)
            .unwrap_err()
            .to_string()
            .contains("requires a provisioned reranker")
    );
    scorer.invalid = true;
    assert!(
        index
            .brief_with_reranker("payment", None, Some(&mut scorer), &options)
            .unwrap_err()
            .to_string()
            .contains("nonfinite")
    );
    assert!(
        index
            .brief(
                "payment",
                None,
                &BriefOptions {
                    selection: lore::search::Selection::Ranked,
                    ..options.clone()
                }
            )
            .is_err()
    );
    assert!(
        index
            .brief(
                "payment",
                None,
                &BriefOptions {
                    passage_scoring: lore::search::PassageScoring::Content,
                    ..options
                }
            )
            .is_err()
    );
}

#[test]
fn exact_lookups_bypass_recovery_and_reranking() {
    use lore::reranking::RerankMode;
    let (_corpus, _state, index, _model) = fixture();
    let control_options = BriefOptions {
        mode: Mode::Keyword,
        ..Default::default()
    };
    let mut control = index
        .brief("retry.md#payment-retries", None, &control_options)
        .unwrap();
    let options = BriefOptions {
        rerank: RerankMode::Local,
        recover_sections: true,
        ..control_options
    };
    let mut scorer = TestReranker {
        calls: 0,
        pairs: 0,
        invalid: true,
    };
    let d = index
        .diagnose_with_reranker(
            "retry.md#payment-retries",
            None,
            Some(&mut scorer),
            &options,
        )
        .unwrap();
    assert_eq!(scorer.calls, 0);
    assert!(d.reranking.unwrap().bypassed_for_exact);
    assert!(
        !d.candidates
            .iter()
            .flat_map(|c| &c.hits)
            .any(|h| h.channel == "file_recovery")
    );
    let mut result = d.brief;
    control.elapsed_ms = 0.0;
    result.elapsed_ms = 0.0;
    control.estimated_tokens = 0;
    result.estimated_tokens = 0;
    assert_eq!(
        serde_json::to_value(control).unwrap(),
        serde_json::to_value(result).unwrap()
    );
}

#[test]
fn section_recovery_is_scoped_bounded_and_preserves_channel_scores() {
    let (corpus, _state, mut index, mut model) = fixture();
    for dir in ["docs", "docs-old"] {
        fs::create_dir(corpus.path().join(dir)).unwrap();
        for i in 0..8 {
            let mut body = format!("# Retry payment {i}\n\n");
            for j in 0..12 {
                body.push_str(&format!(
                    "## Procedure {j}\n\nRetry payment using procedure {i} {j}.\n\n"
                ));
            }
            fs::write(corpus.path().join(format!("{dir}/p{i}.md")), body).unwrap();
        }
    }
    index.index(corpus.path(), &mut model).unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        scope: "docs".into(),
        candidates: 2,
        connections: false,
        ..Default::default()
    };
    let control = index.diagnose("retry payment", None, &options).unwrap();
    let recovered = index
        .diagnose(
            "retry payment",
            None,
            &BriefOptions {
                recover_sections: true,
                ..options
            },
        )
        .unwrap();
    let mut counts = std::collections::BTreeMap::<String, Vec<i64>>::new();
    for c in &recovered.candidates {
        assert!(c.path.starts_with("docs/"));
        for h in c.hits.iter().filter(|h| h.channel == "file_recovery") {
            counts.entry(c.path.clone()).or_default().push(c.section_id);
            assert_eq!(h.rank, 0);
        }
    }
    assert!(!counts.is_empty() && counts.len() <= 5);
    for sections in counts.values() {
        assert!(sections.len() <= 4);
        assert!(
            sections
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                <= 2
        );
        for s in sections {
            assert!(sections.iter().filter(|x| *x == s).count() <= 2);
        }
    }
    for old in &control.candidates {
        let new = recovered
            .candidates
            .iter()
            .find(|c| c.section_id == old.section_id)
            .unwrap();
        assert_eq!(
            (
                old.keyword_rank,
                old.semantic_rank,
                old.score,
                old.file_score
            ),
            (
                new.keyword_rank,
                new.semantic_rank,
                new.score,
                new.file_score
            )
        );
    }
    let orig_sections: std::collections::BTreeSet<_> =
        control.candidates.iter().map(|c| c.section_id).collect();
    assert!(
        recovered
            .candidates
            .iter()
            .any(|c| !orig_sections.contains(&c.section_id))
    );
}

#[test]
fn evaluation_reports_loss_stages_and_checks_source_freshness() {
    use lore::evaluate::{EvaluationCase, Target, evaluate_with_options};
    let (corpus, _state, index, mut model) = fixture();
    let cases = vec![
        EvaluationCase {
            id: "multi-path".into(),
            category: "fixture".into(),
            query: "payment".into(),
            expected: vec![Target {
                paths: vec!["delivery.md".into(), "retry.md".into()],
                contains: "idempotency key".into(),
                contains_any: vec![],
                section: None,
            }],
        },
        EvaluationCase {
            id: "missing".into(),
            category: "fixture".into(),
            query: "payment".into(),
            expected: vec![Target {
                paths: vec!["gardening.md".into()],
                contains: "Water roses".into(),
                contains_any: vec![],
                section: None,
            }],
        },
    ];
    let report = evaluate_with_options(
        &index,
        &mut model,
        &cases,
        &BriefOptions {
            max_tokens: 4000,
            ..Default::default()
        },
    )
    .unwrap();
    let keyword = &report.modes[0];
    assert_eq!(keyword.cases[0].evidence_recall, Some(1.0));
    let returned = &keyword.cases[0].evidence_stages[0];
    assert_eq!(returned.loss_stage, "returned");
    assert!(
        returned.block_discovered && returned.selection_pool_reached && returned.evidence_returned
    );
    assert_eq!(
        keyword.cases[1].evidence_stages[0].loss_stage,
        "file_discovery"
    );
    assert_eq!(
        keyword.summary.evidence_loss_stages.get("file_discovery"),
        Some(&1)
    );
    fs::write(
        corpus.path().join("gardening.md"),
        "# Changed\n\nWater roses",
    )
    .unwrap();
    assert!(evaluate_with_options(&index, &mut model, &cases, &BriefOptions::default()).is_err());
}

#[test]
fn preferences_use_structural_fields_and_preserve_retrieval_candidates() {
    use lore::preferences::{Qualifier, QualifierMode};
    let (corpus, _state, mut index, mut model) = fixture();
    fs::create_dir(corpus.path().join("python")).unwrap();
    fs::write(
        corpus.path().join("python/path.md"),
        "# Retry policy\n\nRetry a payment using a stable identity.",
    )
    .unwrap();
    fs::write(
        corpus.path().join("heading.md"),
        "# Python retries\n\nRetry a payment using a stable identity.",
    )
    .unwrap();
    fs::write(
        corpus.path().join("fence.md"),
        "# Retry example\n\n```python\nretry(payment_key)\n```\n",
    )
    .unwrap();
    fs::write(corpus.path().join("incidental.md"),"# Shared contract\n\nThis contract covers Python and Rust retry callers. Reuse a payment identity.").unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let options = BriefOptions {
        qualifiers: QualifierMode::Auto,
        mode: Mode::Keyword,
        connections: false,
        ..Default::default()
    };
    let control = index
        .diagnose(
            "Python retry payment",
            None,
            &BriefOptions {
                qualifiers: QualifierMode::Off,
                ..options.clone()
            },
        )
        .unwrap();
    let result = index
        .diagnose("Python retry payment", None, &options)
        .unwrap();
    assert_eq!(result.brief.qualifiers, vec![Qualifier::Python]);
    let signature = |d: &lore::search::Diagnostics| {
        let mut rows: Vec<_> = d
            .candidates
            .iter()
            .map(|c| {
                (
                    c.section_id,
                    c.keyword_rank,
                    c.semantic_rank,
                    c.score,
                    c.file_score,
                )
            })
            .collect();
        rows.sort_by_key(|r| r.0);
        rows
    };
    assert_eq!(signature(&control), signature(&result));
    for (path, via) in [
        ("python/path.md", "path"),
        ("heading.md", "heading"),
        ("fence.md", "code_fence"),
    ] {
        let c = result
            .candidates
            .iter()
            .find(|c| c.path == path && !c.qualifier_matches.is_empty())
            .unwrap();
        assert!(c.qualifier_matches[0].via.contains(&via.to_string()));
        assert_eq!(c.qualifier_boost, 1.35);
    }
    assert!(
        result
            .candidates
            .iter()
            .filter(|c| c.path == "incidental.md")
            .all(|c| c.qualifier_boost == 1.0)
    );
}

#[test]
fn automatic_preferences_handle_negation_comparisons_and_explicit_override() {
    use lore::preferences::{Qualifier as Q, QualifierMode};
    let (_corpus, _state, index, _model) = fixture();
    let options = BriefOptions {
        qualifiers: QualifierMode::Auto,
        mode: Mode::Keyword,
        connections: false,
        ..Default::default()
    };
    for (query, expected) in [
        ("Rust, not Python: retry", vec![Q::Rust]),
        ("not using Python for retry", vec![]),
        ("not only Python but Rust retry", vec![Q::Python, Q::Rust]),
        (
            "Python and Rust on Windows retry",
            vec![Q::Python, Q::Rust, Q::Windows],
        ),
        ("Mac OS retry", vec![Q::Macos]),
        ("pythonic retry", vec![]),
        ("retry a payment", vec![]),
    ] {
        assert_eq!(
            index.brief(query, None, &options).unwrap().qualifiers,
            expected,
            "{query}"
        );
    }
    assert_eq!(
        index
            .brief(
                "Python retry",
                None,
                &BriefOptions {
                    prefer: vec![Q::Rust, Q::Rust],
                    ..options.clone()
                }
            )
            .unwrap()
            .qualifiers,
        vec![Q::Rust]
    );
    assert!(
        index
            .brief(
                "Python retry",
                None,
                &BriefOptions {
                    qualifiers: QualifierMode::Off,
                    ..options.clone()
                }
            )
            .unwrap()
            .qualifiers
            .is_empty()
    );
    assert_eq!(
        index
            .brief(
                "retry",
                None,
                &BriefOptions {
                    qualifiers: QualifierMode::Off,
                    prefer: vec![Q::Python],
                    ..options
                }
            )
            .unwrap()
            .qualifiers,
        vec![Q::Python]
    );
}

#[test]
fn preferences_preserve_scope_roles_exact_lookups_and_dimension_cap() {
    use lore::{preferences::Qualifier as Q, search::RoleRule};
    let (corpus, _state, mut index, mut model) = fixture();
    fs::create_dir(corpus.path().join("python")).unwrap();
    fs::write(
        corpus.path().join("python/guide.md"),
        "# Windows Rust retry\n\nRetry with an identity.",
    )
    .unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        connections: false,
        prefer: vec![Q::Python, Q::Rust, Q::Windows],
        ..Default::default()
    };
    let d = index.diagnose("retry", None, &options).unwrap();
    assert!(d.candidates.iter().all(|c| c.qualifier_boost <= 1.35));
    assert_eq!(
        d.candidates
            .iter()
            .find(|c| c.path == "python/guide.md")
            .unwrap()
            .qualifier_boost,
        1.35
    );
    let scoped = index
        .diagnose(
            "retry",
            None,
            &BriefOptions {
                scope: "retry.md".into(),
                ..options.clone()
            },
        )
        .unwrap();
    assert!(scoped.candidates.iter().all(|c| c.path == "retry.md"));
    let roles = vec![RoleRule {
        prefix: "python".into(),
        role: "historical".into(),
        weight: 0.1,
    }];
    let reduced = index
        .diagnose(
            "retry",
            None,
            &BriefOptions {
                roles: roles.clone(),
                ..options.clone()
            },
        )
        .unwrap();
    let score = |d: &lore::search::Diagnostics| {
        d.candidates
            .iter()
            .find(|c| c.path == "python/guide.md")
            .unwrap()
            .file_score
    };
    assert!((score(&reduced) / score(&d) - 0.1).abs() < 1e-8);
    let exact = index
        .diagnose(
            "python/guide.md#windows-rust-retry",
            None,
            &BriefOptions {
                alternatives: 3,
                roles,
                ..options
            },
        )
        .unwrap();
    assert_eq!(exact.brief.files[0].path, "python/guide.md");
    assert!(exact.brief.qualifiers.is_empty() && exact.brief.alternative_sections.is_empty());
}

#[test]
fn alternative_sections_are_bounded_unique_readable_and_scoped() {
    let (corpus, _state, mut index, mut model) = fixture();
    fs::create_dir(corpus.path().join("guides")).unwrap();
    for i in 0..5 {
        fs::write(
            corpus.path().join(format!("guides/retry{i}.md")),
            format!("# Retry {i}\n\nUse retry identity {i} for delivery.\n"),
        )
        .unwrap();
    }
    index.index(corpus.path(), &mut model).unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        scope: "guides".into(),
        max_files: 1,
        alternatives: 3,
        connections: false,
        ..Default::default()
    };
    let d = index.diagnose("retry delivery", None, &options).unwrap();
    assert!(!d.brief.alternative_sections.is_empty());
    assert!(d.brief.alternative_sections.len() <= 3);
    let mut paths = std::collections::BTreeSet::new();
    for a in &d.brief.alternative_sections {
        let (path, anchor) = a.target.split_once('#').unwrap();
        assert!(path.starts_with("guides/") && paths.insert(path));
        assert!(
            !d.brief
                .files
                .iter()
                .flat_map(|f| f.passages.iter().map(move |p| (&f.path, &p.anchor)))
                .any(|(p, h)| p == path && h == anchor)
        );
        let r = index.read(&a.target, &Default::default()).unwrap();
        assert!(r.content.text.contains("retry identity"));
    }
    assert!(estimate_tokens(&serde_json::to_string(&d.brief).unwrap()) <= 2000);
    let navigation_bytes = serde_json::to_vec(&d.brief.alternative_sections)
        .unwrap()
        .len()
        + ",\"alternative_sections\":".len();
    assert!(navigation_bytes <= 2000 * 3 * 15 / 100);
    let no = index
        .brief(
            "retry delivery",
            None,
            &BriefOptions {
                alternatives: 0,
                ..options.clone()
            },
        )
        .unwrap();
    assert!(no.alternative_sections.is_empty());
    assert!(
        index
            .brief(
                "retry",
                None,
                &BriefOptions {
                    alternatives: 9,
                    ..options
                }
            )
            .is_err()
    );
}

#[test]
fn alternatives_fail_on_stale_sources_and_release_unused_reservations() {
    let (corpus, _state, index, _model) = fixture();
    let opts = BriefOptions {
        mode: Mode::Keyword,
        max_files: 1,
        alternatives: 3,
        connections: false,
        ..Default::default()
    };
    let result = index.brief("retry payment delivery", None, &opts).unwrap();
    let path = result
        .alternative_sections
        .first()
        .unwrap()
        .target
        .split('#')
        .next()
        .unwrap()
        .to_string();
    fs::write(corpus.path().join(path), "Changed source").unwrap();
    assert!(
        index
            .brief("retry payment delivery", None, &opts)
            .unwrap_err()
            .to_string()
            .contains("changed since indexing")
    );
    let (_corpus, _state, index, _model) = fixture();
    let only = BriefOptions {
        scope: "retry.md".into(),
        ..opts
    };
    let a = index.brief("idempotency", None, &only).unwrap();
    let b = index
        .brief(
            "idempotency",
            None,
            &BriefOptions {
                alternatives: 0,
                ..only
            },
        )
        .unwrap();
    assert!(a.alternative_sections.is_empty());
    assert_eq!(
        serde_json::to_value(a.files).unwrap(),
        serde_json::to_value(b.files).unwrap()
    );
}
impl TestEmbedder {
    fn new() -> Self {
        Self {
            calls: 0,
            fail: false,
        }
    }
}
impl Embedder for TestEmbedder {
    fn identity(&self) -> &str {
        "test-encoder"
    }
    fn dimensions(&self) -> usize {
        384
    }
    fn token_count(&self, text: &str) -> Result<usize> {
        Ok(text.chars().count().div_ceil(3) + 2)
    }
    fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        anyhow::ensure!(!self.fail, "injected embedding failure");
        self.calls += texts.len();
        Ok(texts
            .iter()
            .map(|text| {
                let mut vector = vec![0.0f32; 384];
                for word in text.split(|c: char| !c.is_alphanumeric()) {
                    let word = word.to_lowercase();
                    let word = match word.as_str() {
                        "duplicates" | "duplicate" | "idempotency" => "retry",
                        other => other,
                    };
                    if word.is_empty() {
                        continue;
                    }
                    let hash = word.bytes().fold(0usize, |acc, b| {
                        acc.wrapping_mul(31).wrapping_add(b as usize)
                    });
                    vector[hash % 384] += 1.0;
                }
                let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt().max(1.0);
                vector.iter_mut().for_each(|v| *v /= norm);
                vector
            })
            .collect())
    }
    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        Ok(self.embed_documents(&[query.into()])?.remove(0))
    }
}
fn fixture() -> (TempDir, TempDir, Index, TestEmbedder) {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(corpus.path().join("retry.md"),"# Payment retries\n\nUse an idempotency key to prevent duplicate payments. `PaymentKey` identifies the original request.\n\nSee [delivery](delivery.md).\n").unwrap();
    fs::write(corpus.path().join("delivery.md"),"# Delivery\n\n`PaymentKey` is carried by the delivery worker. Repeated delivery reuses the original payment.\n").unwrap();
    fs::write(
        corpus.path().join("gardening.md"),
        "# Gardening\n\nWater roses and plant tulips in rich soil.\n",
    )
    .unwrap();
    let mut index = Index::open(state.path()).unwrap();
    let mut model = TestEmbedder::new();
    index.index_with_content(corpus.path(), &mut model).unwrap();
    (corpus, state, index, model)
}

#[test]
fn read_returns_the_whole_procedure_and_children_in_source_order() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    let source = "# Guide\r\n\r\nIntro.\r\n\r\n## Start {#resume}\r\n\r\n1. Save identity.\r\n2. Inspect before retry.\r\n3. Empty results do not prove failure.\r\n\r\n```sh\r\necho '🙂'\r\n```\r\n\r\n### Caveat\r\n\r\nDo not repeat an uncertain mutation.\r\n\r\n| Kind | Meaning |\r\n| --- | --- |\r\n| unknown | investigate |\r\n\r\n[ref]: target.md\r\n\r\n## Next\r\n\r\nUnrelated.\r\n";
    fs::write(corpus.path().join("guide.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let calls = model.calls;
    let result = index
        .read("guide.md#resume", &PageOptions::default())
        .unwrap();
    let start = source.find("## Start").unwrap();
    let end = source.find("## Next").unwrap();
    assert_eq!(result.content.text, source[start..end]);
    assert_eq!(result.section.bytes, [start, end]);
    assert_eq!(result.content.bytes, [start, end]);
    assert_eq!(result.content.lines, [5, 24]);
    assert!(!result.has_more && result.next_cursor.is_none());
    assert!(!result.starts_mid_block && !result.ends_mid_block);
    assert_eq!(calls, model.calls);
    assert_eq!(
        index
            .read("guide.md", &PageOptions::default())
            .unwrap()
            .content
            .text,
        source
    );
}

#[test]
fn read_pagination_preserves_every_byte_of_oversized_unicode_blocks() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    let source = format!(
        "# Escapes\r\n\r\n```text\r\n{}\r\n```\r\n\r\n1. Finish.\r\n2. Check.\r\n",
        "🙂\t\"\\甲".repeat(900)
    );
    fs::write(corpus.path().join("escaped.md"), &source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let mut options = PageOptions {
        max_tokens: 420,
        ..Default::default()
    };
    let mut combined = String::new();
    let mut previous_mid = false;
    let mut fragments = 0;
    for _ in 0..200 {
        let page = index.read("escaped.md", &options).unwrap();
        assert_eq!(page.content.bytes[0], combined.len());
        assert_eq!(page.starts_mid_block, previous_mid);
        previous_mid = page.ends_mid_block;
        fragments += usize::from(previous_mid);
        assert!(!page.content.text.is_empty());
        let [start, end] = page.content.bytes;
        assert_eq!(page.content.text, source[start..end]);
        assert_eq!(
            page.content.lines,
            [
                lore::markdown::line_at(&source, start),
                lore::markdown::line_at(&source, end - 1)
            ]
        );
        let cost = estimate_tokens(&serde_json::to_string(&page).unwrap());
        assert_eq!(page.estimated_tokens, cost);
        assert!(cost <= options.max_tokens);
        assert_eq!(page.has_more, page.next_cursor.is_some());
        combined.push_str(&page.content.text);
        options.cursor = page.next_cursor;
        if options.cursor.is_none() {
            break;
        }
    }
    assert!(fragments > 1);
    assert!(options.cursor.is_none());
    assert_eq!(combined, source);
}

#[test]
fn read_prefers_complete_blocks_and_rejects_insufficient_budget() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    let source = format!(
        "# Blocks\n\n{}\n\n{}\n",
        "First paragraph. ".repeat(35),
        "Second paragraph. ".repeat(35)
    );
    fs::write(corpus.path().join("blocks.md"), &source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let page = index
        .read(
            "blocks.md",
            &PageOptions {
                max_tokens: 500,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(page.has_more);
    assert!(!page.ends_mid_block);
    assert!(page.content.text.contains("First paragraph."));
    assert!(!page.content.text.contains("Second paragraph."));
    assert!(
        index
            .read(
                "blocks.md",
                &PageOptions {
                    max_tokens: 128,
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("max-tokens")
    );
}

#[test]
fn read_cursors_reject_wrong_target_tampering_and_changed_snapshots() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(
        corpus.path().join("long.md"),
        format!("# Long\n\n{}", "A repeated paragraph.\n\n".repeat(200)),
    )
    .unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let first = index
        .read(
            "long.md",
            &PageOptions {
                max_tokens: 400,
                ..Default::default()
            },
        )
        .unwrap();
    let options = PageOptions {
        max_tokens: 400,
        cursor: first.next_cursor,
    };
    assert!(options.cursor.is_some());
    for target in ["retry.md", "long.md#long"] {
        assert!(
            index
                .read(target, &options)
                .unwrap_err()
                .to_string()
                .contains("Continuation")
        );
    }
    assert!(
        index
            .outline("long.md", &options)
            .unwrap_err()
            .to_string()
            .contains("Continuation")
    );
    for cursor in [
        "bad".to_string(),
        format!("{}x", options.cursor.as_ref().unwrap()),
        "v1:999999:deadbeef".into(),
    ] {
        assert!(
            index
                .read(
                    "long.md",
                    &PageOptions {
                        cursor: Some(cursor),
                        ..options.clone()
                    }
                )
                .is_err()
        );
    }
    // Unchanged refresh preserves the revision and permits continuation.
    index.index(corpus.path(), &mut model).unwrap();
    assert!(index.read("long.md", &options).is_ok());
    fs::write(corpus.path().join("long.md"), "# Changed\n\nNew source.").unwrap();
    assert!(
        index
            .read("long.md", &options)
            .unwrap_err()
            .to_string()
            .contains("changed since indexing")
    );
    assert!(
        index
            .outline("long.md", &PageOptions::default())
            .unwrap_err()
            .to_string()
            .contains("changed since indexing")
    );
    index.index(corpus.path(), &mut model).unwrap();
    assert!(
        index
            .read("long.md", &options)
            .unwrap_err()
            .to_string()
            .contains("Continuation")
    );
}

#[test]
fn outline_pages_preserve_hierarchy_and_exact_unique_anchors() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    let mut source = "# Guide\n\n## Repeat\n\n### Child {#custom}\n\n```md\n# Not a heading\n```\n\n## Repeat\n\n".to_string();
    for i in 0..50 {
        source.push_str(&format!("## Section {i}\n\nContent.\n\n"));
    }
    fs::write(corpus.path().join("outline.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let all = index
        .outline(
            "outline.md",
            &PageOptions {
                max_tokens: 10000,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(all.sections.len(), 54);
    assert_eq!(all.sections[1].anchor, "repeat");
    assert_eq!(all.sections[2].anchor, "custom");
    assert_eq!(all.sections[2].depth, 3);
    assert_eq!(all.sections[3].anchor, "repeat-1");
    let mut collected = Vec::new();
    let mut options = PageOptions {
        max_tokens: 400,
        ..Default::default()
    };
    for _ in 0..100 {
        let page = index.outline("outline.md", &options).unwrap();
        assert!(estimate_tokens(&serde_json::to_string(&page).unwrap()) <= options.max_tokens);
        assert!(!page.sections.is_empty());
        assert_eq!(page.has_more, page.next_cursor.is_some());
        collected.extend(page.sections);
        options.cursor = page.next_cursor;
        if options.cursor.is_none() {
            break;
        }
    }
    assert!(options.cursor.is_none());
    assert_eq!(
        serde_json::to_value(collected).unwrap(),
        serde_json::to_value(all.sections).unwrap()
    );
}

#[test]
fn reading_handles_empty_files_missing_anchors_and_unindexed_paths() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(corpus.path().join("empty.md"), "").unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let page = index.read("empty.md", &PageOptions::default()).unwrap();
    assert_eq!(page.content.text, "");
    assert_eq!(page.content.bytes, [0, 0]);
    assert!(!page.has_more);
    assert!(
        index
            .outline("empty.md", &PageOptions::default())
            .unwrap()
            .sections
            .is_empty()
    );
    assert!(
        index
            .read("retry.md#missing", &PageOptions::default())
            .unwrap_err()
            .to_string()
            .contains("anchor")
    );
    fs::write(corpus.path().join("unindexed.md"), "Unindexed source").unwrap();
    assert!(
        index
            .read("unindexed.md", &PageOptions::default())
            .unwrap_err()
            .to_string()
            .contains("not indexed")
    );
    for target in ["../retry.md", "/retry.md", "./retry.md"] {
        assert!(index.read(target, &PageOptions::default()).is_err());
    }
    fs::remove_file(corpus.path().join("retry.md")).unwrap();
    assert!(
        index
            .read("retry.md", &PageOptions::default())
            .unwrap_err()
            .to_string()
            .contains("missing")
    );
}

#[test]
#[cfg(unix)]
fn reading_rejects_sources_replaced_with_external_symlinks() {
    let (corpus, _state, index, _model) = fixture();
    let outside = TempDir::new().unwrap();
    let bytes = fs::read(corpus.path().join("retry.md")).unwrap();
    fs::write(outside.path().join("same.md"), bytes).unwrap();
    fs::remove_file(corpus.path().join("retry.md")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("same.md"),
        corpus.path().join("retry.md"),
    )
    .unwrap();
    assert!(
        index
            .read("retry.md", &Default::default())
            .unwrap_err()
            .to_string()
            .contains("outside the corpus")
    );
}

#[test]
fn standalone_read_outline_and_context_require_no_model_assets() {
    use std::process::Command;
    let (_corpus, state, index, _model) = fixture();
    drop(index);
    for command in [
        vec!["read"],
        vec!["outline"],
        vec!["plan"],
        vec!["read", "--context"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_lore"))
            .args([
                "--index-dir",
                state.path().to_str().unwrap(),
                "--model-dir",
                "/nonexistent/lore-model",
            ])
            .args(command)
            .args(["retry.md", "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["path"], "retry.md");
        assert_eq!(value["has_more"], false);
    }
}

#[test]
fn context_plan_keeps_ancestor_introductions_and_focus_children_but_no_siblings() {
    use lore::{context::ContextRole, reading::PageOptions};
    let (corpus, _state, mut index, mut model) = fixture();
    let source = "Preamble.\n\n# Guide\n\nShared setup.\n\n## Earlier\n\nExclude this sibling.\n\n## Container\n\n### Operation\n\nShared declaration.\n\n#### Repeat\n\nExclude this earlier option.\n\n#### Repeat\n\nChosen option.\n\n##### Caveat\n\nKeep this child.\n\n#### Next\n\nExclude this next option.\n";
    fs::write(corpus.path().join("context.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let target = "context.md#repeat-1";
    let plan = index.plan(target, &PageOptions::default()).unwrap();
    assert_eq!(plan.total_steps, 4);
    assert_eq!(
        plan.steps
            .iter()
            .map(|s| s.target.as_str())
            .collect::<Vec<_>>(),
        [
            "context.md",
            "context.md#guide",
            "context.md#operation",
            target
        ]
    );
    let expected = [
        "Preamble.\n\n",
        "# Guide\n\nShared setup.\n\n",
        "### Operation\n\nShared declaration.\n\n",
        "#### Repeat\n\nChosen option.\n\n##### Caveat\n\nKeep this child.\n\n",
    ];
    let read = index.read_context(target, &PageOptions::default()).unwrap();
    assert!(!plan.has_more && !read.has_more);
    assert_eq!(
        read.focus.parent_target.as_deref(),
        Some("context.md#operation")
    );
    for (i, ((step, part), text)) in plan.steps.iter().zip(&read.parts).zip(expected).enumerate() {
        assert_eq!(step.order, i + 1);
        assert_eq!(part.step, step.order);
        assert_eq!(part.target, step.target);
        assert_eq!(part.content.text, text);
        assert_eq!(&source[step.bytes[0]..step.bytes[1]], text);
        assert_eq!(part.content.bytes, step.bytes);
        assert_eq!(part.content.lines, step.lines);
        assert_eq!(
            step.role,
            if i == 3 {
                ContextRole::Section
            } else {
                ContextRole::Introduction
            }
        );
    }
    assert_eq!(
        estimate_tokens(&serde_json::to_string(&read).unwrap()),
        read.estimated_tokens
    );
    let whole = index
        .read_context("context.md", &PageOptions::default())
        .unwrap();
    assert_eq!(whole.parts.len(), 1);
    assert_eq!(whole.parts[0].content.text, source);
    assert_eq!(whole.parts[0].role, ContextRole::Section);
    assert!(whole.focus.parent_target.is_none());
}

#[test]
fn context_pages_reconstruct_each_separate_slice_and_signal_delayed_focus() {
    use lore::{context::ContextRole, reading::PageOptions};
    let (corpus, _state, mut index, mut model) = fixture();
    let source = format!(
        "# Guide\r\n\r\n```text\r\n{}\r\n```\r\n\r\n## Skip\r\n\r\nUnrelated sibling.\r\n\r\n## Focus {{#chosen}}\r\n\r\n{}\r\n\r\n### Caveat\r\n\r\nFinish.\r\n",
        "🙂\t\"\\甲".repeat(500),
        "A long instruction. ".repeat(150)
    );
    fs::write(corpus.path().join("context.md"), &source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let target = "context.md#chosen";
    let plan = index.plan(target, &PageOptions::default()).unwrap();
    assert_eq!(plan.steps.len(), 2);
    for budget in [450, 700, 2000] {
        let mut options = PageOptions {
            max_tokens: budget,
            ..Default::default()
        };
        let mut combined = vec![String::new(); plan.steps.len()];
        let mut previous: Option<(usize, bool)> = None;
        let mut fragments = 0;
        let mut delayed = false;
        for _ in 0..200 {
            let page = index.read_context(target, &options).unwrap();
            assert!(!page.parts.is_empty());
            assert_eq!(page.has_more, page.next_cursor.is_some());
            assert_eq!(page.has_more, page.next_role.is_some());
            delayed |= page.next_role == Some(ContextRole::Introduction);
            let cost = estimate_tokens(&serde_json::to_string(&page).unwrap());
            assert_eq!(cost, page.estimated_tokens);
            assert!(cost <= budget);
            for part in &page.parts {
                let step = &plan.steps[part.step - 1];
                let [start, end] = part.content.bytes;
                assert_eq!(start, step.bytes[0] + combined[part.step - 1].len());
                assert!(start < end && end <= step.bytes[1]);
                assert_eq!(part.content.text, source[start..end]);
                assert_eq!(
                    part.content.lines,
                    [
                        lore::markdown::line_at(&source, start),
                        lore::markdown::line_at(&source, end - 1)
                    ]
                );
                if let Some((number, mid)) = previous {
                    if number == part.step {
                        assert_eq!(part.starts_mid_block, mid);
                    } else {
                        assert!(!part.starts_mid_block && !mid);
                    }
                }
                previous = Some((part.step, part.ends_mid_block));
                fragments += usize::from(part.ends_mid_block);
                combined[part.step - 1].push_str(&part.content.text);
            }
            options.cursor = page.next_cursor;
            if options.cursor.is_none() {
                break;
            }
        }
        assert!(options.cursor.is_none() && delayed && fragments > 0);
        for (step, text) in plan.steps.iter().zip(&combined) {
            assert_eq!(text, &source[step.bytes[0]..step.bytes[1]]);
        }
    }
}

#[test]
fn plan_pages_and_context_cursors_are_bound_to_mode_target_and_snapshot() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    let mut source = "Preamble.\n\n".to_string();
    for depth in 1..=6 {
        source.push_str(&format!(
            "{} Level {depth}\n\n{}\n\n",
            "#".repeat(depth),
            "Context. ".repeat(100)
        ));
    }
    fs::write(corpus.path().join("deep.md"), &source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let target = "deep.md#level-6";
    let all = index.plan(target, &PageOptions::default()).unwrap();
    assert_eq!(all.steps.len(), 7);
    let mut options = PageOptions {
        max_tokens: 420,
        ..Default::default()
    };
    let mut steps = Vec::new();
    let mut plan_cursor = None;
    for _ in 0..10 {
        let page = index.plan(target, &options).unwrap();
        assert!(!page.steps.is_empty());
        assert_eq!(
            page.estimated_tokens,
            estimate_tokens(&serde_json::to_string(&page).unwrap())
        );
        assert!(page.estimated_tokens <= options.max_tokens);
        assert_eq!(page.has_more, page.next_cursor.is_some());
        plan_cursor = plan_cursor.or(page.next_cursor.clone());
        steps.extend(page.steps);
        options.cursor = page.next_cursor;
        if options.cursor.is_none() {
            break;
        }
    }
    assert!(options.cursor.is_none() && plan_cursor.is_some());
    assert_eq!(
        serde_json::to_value(steps).unwrap(),
        serde_json::to_value(&all.steps).unwrap()
    );
    options.cursor = plan_cursor;
    assert!(index.read_context(target, &options).is_err());
    assert!(index.plan("deep.md#level-5", &options).is_err());
    let first = index
        .read_context(
            target,
            &PageOptions {
                max_tokens: 500,
                ..Default::default()
            },
        )
        .unwrap();
    let context_options = PageOptions {
        max_tokens: 700,
        cursor: first.next_cursor,
    };
    assert!(context_options.cursor.is_some());
    assert!(index.read(target, &context_options).is_err());
    assert!(index.plan(target, &context_options).is_err());
    assert!(
        index
            .read_context("deep.md#level-5", &context_options)
            .is_err()
    );
    let normal = index
        .read(
            target,
            &PageOptions {
                max_tokens: 400,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(normal.next_cursor.is_some());
    assert!(
        index
            .read_context(
                target,
                &PageOptions {
                    cursor: normal.next_cursor,
                    ..Default::default()
                }
            )
            .is_err()
    );
    for cursor in [
        "bad".to_string(),
        format!("{}x", context_options.cursor.as_ref().unwrap()),
        "v1:999999:deadbeef".into(),
    ] {
        assert!(
            index
                .read_context(
                    target,
                    &PageOptions {
                        cursor: Some(cursor),
                        ..Default::default()
                    }
                )
                .is_err()
        );
    }
    index.index(corpus.path(), &mut model).unwrap();
    assert!(index.plan(target, &options).is_ok());
    assert!(index.read_context(target, &context_options).is_ok());
    fs::write(
        corpus.path().join("deep.md"),
        format!("{source}\nChanged.\n"),
    )
    .unwrap();
    assert!(
        index
            .plan(target, &options)
            .unwrap_err()
            .to_string()
            .contains("changed since indexing")
    );
    assert!(
        index
            .read_context(target, &context_options)
            .unwrap_err()
            .to_string()
            .contains("changed since indexing")
    );
    index.index(corpus.path(), &mut model).unwrap();
    assert!(
        index
            .plan(target, &options)
            .unwrap_err()
            .to_string()
            .contains("Continuation")
    );
    assert!(
        index
            .read_context(target, &context_options)
            .unwrap_err()
            .to_string()
            .contains("Continuation")
    );
}

#[test]
fn context_handles_empty_sources_and_rejects_missing_targets_and_small_budgets() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(corpus.path().join("empty.md"), "").unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let page = index
        .read_context("empty.md", &PageOptions::default())
        .unwrap();
    assert!(page.parts.is_empty() && !page.has_more && page.next_cursor.is_none());
    let plan = index.plan("empty.md", &PageOptions::default()).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].bytes, [0, 0]);
    for target in [
        "retry.md#missing",
        "unindexed.md",
        "../retry.md",
        "/retry.md",
        "./retry.md",
    ] {
        assert!(index.plan(target, &PageOptions::default()).is_err());
        assert!(index.read_context(target, &PageOptions::default()).is_err());
    }
    let tiny = PageOptions {
        max_tokens: 128,
        ..Default::default()
    };
    assert!(
        index
            .plan("retry.md#payment-retries", &tiny)
            .unwrap_err()
            .to_string()
            .contains("max-tokens")
    );
    assert!(
        index
            .read_context("retry.md#payment-retries", &tiny)
            .unwrap_err()
            .to_string()
            .contains("max-tokens")
    );
    fs::remove_file(corpus.path().join("retry.md")).unwrap();
    assert!(
        index
            .plan("retry.md", &PageOptions::default())
            .unwrap_err()
            .to_string()
            .contains("missing")
    );
    assert!(
        index
            .read_context("retry.md", &PageOptions::default())
            .unwrap_err()
            .to_string()
            .contains("missing")
    );
}

#[test]
fn hybrid_returns_original_passages_and_sources() {
    let (corpus, _state, index, mut model) = fixture();
    let options = BriefOptions {
        connections: false,
        min_similarity: 0.0,
        ..Default::default()
    };
    let result = index
        .brief("duplicate payments", Some(&mut model), &options)
        .unwrap();
    assert_eq!(result.files[0].path, "retry.md");
    for file in &result.files {
        let source = fs::read_to_string(corpus.path().join(&file.path)).unwrap();
        for p in &file.passages {
            assert_eq!(&source[p.bytes[0]..p.bytes[1]], p.text);
        }
    }
    assert!(estimate_tokens(&serde_json::to_string(&result).unwrap()) <= options.max_tokens);
}
#[test]
fn edits_deletions_and_ignore_changes_update_all_indexes() {
    let (corpus, _state, mut index, mut model) = fixture();
    let first = index.status().unwrap().revision;
    let report = index.index(corpus.path(), &mut model).unwrap();
    assert_eq!(report.embeddings_computed, 0);
    assert_eq!(report.embeddings_reused, report.passages);
    assert_eq!(report.revision, first);
    fs::write(
        corpus.path().join("retry.md"),
        "# Payments\n\nUse transaction fencing.\n",
    )
    .unwrap();
    fs::remove_file(corpus.path().join("delivery.md")).unwrap();
    fs::write(corpus.path().join(".loreignore"), "gardening.md\n").unwrap();
    let report = index.index(corpus.path(), &mut model).unwrap();
    assert_eq!(report.documents, 1);
    assert_eq!(report.removed, 2);
    assert!(report.revision > first);
    let options = BriefOptions {
        mode: Mode::Keyword,
        ..Default::default()
    };
    assert!(
        index
            .brief("PaymentKey", None, &options)
            .unwrap()
            .files
            .is_empty()
    );
    assert_eq!(
        index.brief("fencing", None, &options).unwrap().files.len(),
        1
    );
    assert!(
        index
            .related("retry.md", 10)
            .unwrap()
            .connections
            .is_empty()
    );
}
#[test]
fn stale_source_is_reported_instead_of_returning_wrong_citations() {
    let (corpus, _state, index, _model) = fixture();
    fs::write(corpus.path().join("retry.md"), "changed").unwrap();
    let error = index
        .brief(
            "idempotency",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("changed since indexing"));
}
#[test]
fn failed_embedding_keeps_previous_snapshot() {
    let (corpus, _state, mut index, mut model) = fixture();
    let before = index.status().unwrap();
    fs::write(corpus.path().join("new.md"), "# Unindexed failure\n").unwrap();
    model.fail = true;
    assert!(index.index(corpus.path(), &mut model).is_err());
    let after = index.status().unwrap();
    assert_eq!(before.revision, after.revision);
    assert_eq!(before.passages, after.passages);
}
#[test]
fn explicit_links_and_identifiers_have_evidence() {
    let (corpus, _state, index, _model) = fixture();
    let related = index.related("retry.md", 10).unwrap();
    assert!(
        related
            .connections
            .iter()
            .any(|c| c.kind == "references" && c.path == "delivery.md")
    );
    assert!(
        related
            .connections
            .iter()
            .any(|c| c.kind == "shared_identifier"
                && c.identifier.as_deref() == Some("PaymentKey")
                && c.evidence.len() == 2)
    );
    assert!(
        index
            .related("delivery.md", 10)
            .unwrap()
            .connections
            .iter()
            .any(|c| c.direction == "incoming")
    );
    for connection in related.connections {
        for evidence in connection.evidence {
            let source = fs::read_to_string(corpus.path().join(&evidence.path)).unwrap();
            assert_eq!(&source[evidence.bytes[0]..evidence.bytes[1]], evidence.text);
            if let Some(identifier) = &connection.identifier {
                assert!(evidence.text.contains(identifier));
            }
        }
    }
}

#[test]
fn excerpt_packing_preserves_unicode_offsets_and_multiple_files() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    for i in 0..5 {
        let source = format!(
            "# Guide {i}\n\n{}\n\nRetry retries idempotency details for area {i}.\n",
            "Unicode café 🙂 background information.\n".repeat(12)
        );
        fs::write(corpus.path().join(format!("guide{i}.md")), source).unwrap();
    }
    let mut index = Index::open(state.path()).unwrap();
    let mut model = TestEmbedder::new();
    index.index(corpus.path(), &mut model).unwrap();
    let result = index
        .brief(
            "retry",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(result.files.len(), 5);
    assert!(estimate_tokens(&serde_json::to_string(&result).unwrap()) <= 2000);
    for file in result.files {
        let source = fs::read_to_string(corpus.path().join(file.path)).unwrap();
        for passage in file.passages {
            assert_eq!(&source[passage.bytes[0]..passage.bytes[1]], passage.text);
            assert_eq!(
                lore::markdown::line_at(&source, passage.bytes[0]),
                passage.lines[0]
            );
            assert_eq!(
                lore::markdown::line_at(&source, passage.bytes[1] - 1),
                passage.lines[1]
            );
        }
    }
}

#[test]
fn index_cannot_be_reused_for_a_different_repository() {
    let (_corpus, _state, mut index, mut model) = fixture();
    let other = TempDir::new().unwrap();
    assert!(
        index
            .index(other.path(), &mut model)
            .unwrap_err()
            .to_string()
            .contains("belongs to")
    );
}

#[test]
fn oversized_query_metadata_does_not_silently_exceed_budget() {
    let (_corpus, _state, index, _model) = fixture();
    assert!(
        index
            .brief(
                &"query ".repeat(500),
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    max_tokens: 128,
                    ..Default::default()
                }
            )
            .is_err()
    );
}

#[test]
fn readers_can_open_the_previous_snapshot_during_indexing() {
    use std::sync::mpsc;
    struct PausedEmbedder {
        inner: TestEmbedder,
        entered: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    }
    impl Embedder for PausedEmbedder {
        fn identity(&self) -> &str {
            self.inner.identity()
        }
        fn dimensions(&self) -> usize {
            384
        }
        fn token_count(&self, text: &str) -> Result<usize> {
            self.inner.token_count(text)
        }
        fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.entered.send(()).unwrap();
            self.resume
                .recv_timeout(std::time::Duration::from_secs(15))
                .unwrap();
            self.inner.embed_documents(texts)
        }
        fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
            self.inner.embed_query(text)
        }
    }
    let (corpus, state, index, _model) = fixture();
    drop(index);
    fs::write(corpus.path().join("new.md"), "# Newly added document\n").unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let directory = state.path().to_owned();
    let root = corpus.path().to_owned();
    let worker = std::thread::spawn(move || {
        let mut index = Index::open(&directory).unwrap();
        index
            .index(
                &root,
                &mut PausedEmbedder {
                    inner: TestEmbedder::new(),
                    entered: entered_tx,
                    resume: resume_rx,
                },
            )
            .unwrap();
    });
    let reader = Index::open(state.path()).unwrap();
    for _ in 0..2 {
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(reader.status().unwrap().documents, 3);
        assert!(
            !reader
                .brief(
                    "idempotency",
                    None,
                    &BriefOptions {
                        mode: Mode::Keyword,
                        ..Default::default()
                    }
                )
                .unwrap()
                .files
                .is_empty()
        );
        resume_tx.send(()).unwrap();
    }
    worker.join().unwrap();
    assert_eq!(reader.status().unwrap().documents, 4);
}
#[test]
fn invalid_queries_and_fts_syntax_are_safe() {
    let (_corpus, _state, index, _model) = fixture();
    let options = BriefOptions {
        mode: Mode::Keyword,
        ..Default::default()
    };
    assert!(index.brief(" ", None, &options).is_err());
    assert!(index.brief("\" OR * NEAR( --", None, &options).is_ok());
    assert!(
        index
            .brief("the and how", None, &options)
            .unwrap()
            .files
            .is_empty()
    );
    assert!(
        index
            .brief(
                "retry",
                None,
                &BriefOptions {
                    max_files: 0,
                    ..options
                }
            )
            .is_err()
    );
}
#[test]
fn tiny_budget_does_not_return_oversized_passages() {
    let (_corpus, _state, index, _model) = fixture();
    let result = index
        .brief(
            "retry",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                max_tokens: 180,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(estimate_tokens(&serde_json::to_string(&result).unwrap()) <= 180);
    assert!(result.files.is_empty());
    assert_eq!(result.status, "budget_too_small");
}
#[test]
fn gitignore_is_respected_without_a_git_directory() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::create_dir(corpus.path().join(".docs")).unwrap();
    fs::write(corpus.path().join(".gitignore"), "/.docs/\nsecret.md\n").unwrap();
    for path in ["visible.md", "secret.md", ".docs/history.md"] {
        fs::write(corpus.path().join(path), "# Example").unwrap();
    }
    let report = Index::open(state.path())
        .unwrap()
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    assert_eq!(report.documents, 1);
}
#[test]
fn links_cannot_escape_corpus_and_decode_spaces() {
    assert_eq!(
        resolve_link("docs/start.md", "../a%20b.md#heading"),
        Some("a b.md".into())
    );
    assert_eq!(resolve_link("a.md", "../../secret.md"), None);
    assert_eq!(resolve_link("a.md", "https://example.com"), None);
    assert_eq!(resolve_link("a.md", "%2Fetc/passwd"), None);
}
#[cfg(unix)]
#[test]
fn symlinked_files_and_directories_are_not_indexed() {
    let corpus = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(outside.path().join("outside.md"), "# Outside").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("outside.md"),
        corpus.path().join("escape.md"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), corpus.path().join("linked")).unwrap();
    let report = Index::open(state.path())
        .unwrap()
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    assert_eq!(report.documents, 0);
}

#[test]
fn scope_is_applied_before_candidate_limits_and_connections() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::create_dir(corpus.path().join("docs")).unwrap();
    fs::create_dir(corpus.path().join("docs-old")).unwrap();
    for (path, text) in [
        (
            "docs/retry.md",
            "# Retry\n\nIdempotency prevents duplicate payments. [external](../docs-old/retry.md).",
        ),
        (
            "docs-old/retry.md",
            "# Retry retry\n\nDuplicate payments idempotency retry.",
        ),
        ("outside.md", "# Retry\n\nDuplicate payments."),
    ] {
        fs::write(corpus.path().join(path), text).unwrap();
    }
    let mut index = Index::open(state.path()).unwrap();
    let mut model = TestEmbedder::new();
    index.index(corpus.path(), &mut model).unwrap();
    for mode in [Mode::Keyword, Mode::Semantic, Mode::Hybrid] {
        let options = BriefOptions {
            mode,
            scope: "docs".into(),
            candidates: 1,
            min_similarity: 0.0,
            graph: true,
            ..Default::default()
        };
        let d = index
            .diagnose("duplicate payments", Some(&mut model), &options)
            .unwrap();
        assert!(!d.brief.files.is_empty());
        assert!(d.candidates.iter().all(|c| c.path.starts_with("docs/")));
        assert!(d.brief.files.iter().all(|f| f.path.starts_with("docs/")
            && f.connections.iter().all(|c| c.path.starts_with("docs/"))));
    }
    assert!(
        index
            .brief(
                "retry",
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    scope: "../".into(),
                    ..Default::default()
                }
            )
            .is_err()
    );
}
#[test]
fn duplicate_blocks_do_not_starve_distinct_sections() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(
        corpus.path().join("large.md"),
        format!("# Retry\n\n{}", "Retry duplicate payment.\n\n".repeat(80)),
    )
    .unwrap();
    fs::write(
        corpus.path().join("useful.md"),
        "# Delivery guarantees\n\nRetry safely after a timeout by retaining the payment key.",
    )
    .unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let d = index
        .diagnose(
            "retry",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                candidates: 2,
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(d.candidates.iter().any(|c| c.path == "useful.md"));
    assert!(d.candidates.iter().all(|c| c.hits.len() <= 6));
}
#[test]
fn literal_heading_anchor_survives_small_candidate_budget() {
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(corpus.path().join("odd.md"),"# Identity\n\nIntroduction.\n\n## zxqv8394 {#literal-anchor}\n\nThe exact recovery rule lives here.\n").unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let d = index
        .diagnose(
            "odd.md#literal-anchor",
            Some(&mut model),
            &BriefOptions {
                candidates: 1,
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(d.brief.files[0].path, "odd.md");
    assert_eq!(d.brief.files[0].passages[0].anchor, "literal-anchor");
    assert!(
        d.brief.files[0].passages[0]
            .text
            .contains("exact recovery rule")
    );
    assert!(
        d.candidates
            .iter()
            .any(|c| c.exact && !c.selected_passages.is_empty())
    );
}
#[test]
fn table_evidence_is_a_complete_row_with_a_separate_exact_header() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = "# States\r\n\r\n| State | Meaning |\r\n| --- | --- |\r\n| running | owns a process |\r\n| failed | must inspect evidence |\r\n";
    fs::write(corpus.path().join("states.md"), source).unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let d = index
        .diagnose(
            "failed",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                connections: false,
                max_files: 1,
                ..Default::default()
            },
        )
        .unwrap();
    let p = &d.brief.files[0].passages[0];
    assert_eq!(p.kind, "table_row");
    assert!(p.text.starts_with('|') && p.text.ends_with('|'));
    let ctx = p.context.as_ref().unwrap();
    assert_eq!(&source[ctx.bytes[0]..ctx.bytes[1]], ctx.text);
    assert_eq!(&source[p.bytes[0]..p.bytes[1]], p.text);
    assert!(ctx.text.contains("State | Meaning"));
}
#[test]
fn graph_follows_heading_anchor_and_retains_evidence() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(
        corpus.path().join("start.md"),
        "# Duplicate payments\n\nDuplicate payments require [recovery](target.md#recover).\n",
    )
    .unwrap();
    fs::write(corpus.path().join("target.md"),"# Target\n\nUnrelated gardening.\n\n## Recover\n\nDuplicate transactions require a retained token.\n\n## Elsewhere\n\nWater roses.\n").unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        candidates: 1,
        connections: false,
        graph: true,
        max_tokens: 3000,
        ..Default::default()
    };
    let d = index
        .diagnose("duplicate payments", None, &options)
        .unwrap();
    let expanded = d
        .candidates
        .iter()
        .find(|c| c.path == "target.md" && !c.graph_support.is_empty())
        .unwrap();
    assert_eq!(expanded.anchor, "recover");
    assert_eq!(expanded.graph_support[0].source_path, "start.md");
    let links = index.related("start.md", 10).unwrap();
    assert!(
        links
            .connections
            .iter()
            .any(|c| c.anchor.as_deref() == Some("recover"))
    );
    let scoped = index.related("target.md#recover", 10).unwrap();
    assert!(scoped.connections.iter().any(|c| c.path == "start.md"));
}
#[test]
fn diagnostics_and_brief_have_identical_selected_evidence() {
    let (_corpus, _state, index, mut model) = fixture();
    let options = BriefOptions {
        min_similarity: 0.0,
        ..Default::default()
    };
    let d = index
        .diagnose("duplicates", Some(&mut model), &options)
        .unwrap();
    let b = index
        .brief("duplicates", Some(&mut model), &options)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&d.brief.files).unwrap(),
        serde_json::to_value(&b.files).unwrap()
    );
    for f in &b.files {
        for p in &f.passages {
            let c = d
                .candidates
                .iter()
                .find(|c| c.section_id == p.section_id)
                .unwrap();
            assert!(c.selected_passages.contains(&p.id));
            assert!(c.hits.iter().any(|h| h.passage_id == p.id));
            assert_eq!(c.decision, "selected");
        }
    }
}
#[test]
fn repository_roles_are_explicit_and_exact_paths_take_precedence() {
    use lore::search::RoleRule;
    let (corpus, _state, mut index, mut model) = fixture();
    fs::create_dir(corpus.path().join("archive")).unwrap();
    fs::write(
        corpus.path().join("archive/old.md"),
        "# Payment retries\n\nDuplicate payments retry using idempotency.",
    )
    .unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        connections: false,
        roles: vec![RoleRule {
            prefix: "archive".into(),
            role: "historical".into(),
            weight: 0.1,
        }],
        ..Default::default()
    };
    let b = index.brief("archive/old.md", None, &options).unwrap();
    assert_eq!(b.files[0].path, "archive/old.md");
    assert_eq!(b.files[0].role.as_deref(), Some("historical"));
}
#[test]
fn standalone_cli_batch_continues_after_invalid_and_oversized_input() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let (_corpus, state, index, _model) = fixture();
    drop(index);
    let mut child = Command::new(env!("CARGO_BIN_EXE_lore"))
        .args([
            "--index-dir",
            state.path().to_str().unwrap(),
            "batch",
            "--mode",
            "keyword",
            "--no-connections",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        writeln!(stdin, "\"retry\"\n{{}}").unwrap();
        writeln!(stdin, "{}", "x".repeat(70000)).unwrap();
        writeln!(stdin, "\"gardening\"").unwrap();
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(!out.status.success());
    let rows = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 4);
    assert!(rows[0]["files"].is_array());
    assert!(rows[1]["error"].is_string());
    assert!(rows[2]["error"].is_string());
    assert_eq!(rows[3]["files"][0]["path"], "gardening.md");
}

#[test]
fn document_ranking_control_preserves_discovery_pool() {
    use lore::search::Ranking;
    let (_corpus, _state, index, mut model) = fixture();
    let opts = BriefOptions {
        min_similarity: 0.0,
        connections: false,
        ranking: Ranking::Document,
        ..Default::default()
    };
    let a = index
        .diagnose("duplicate payments", Some(&mut model), &opts)
        .unwrap();
    let b = index
        .diagnose(
            "duplicate payments",
            Some(&mut model),
            &BriefOptions {
                ranking: Ranking::Section,
                ..opts
            },
        )
        .unwrap();
    let pool = |d: &lore::search::Diagnostics| {
        d.candidates
            .iter()
            .map(|c| (c.section_id, (c.keyword_rank, c.semantic_rank, c.score)))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    assert_eq!(pool(&a), pool(&b));
}
#[test]
fn schema_upgrade_requires_atomic_reindex_before_queries() {
    let (corpus, state, index, mut model) = fixture();
    drop(index);
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF; DROP INDEX passages_section; ALTER TABLE passages DROP COLUMN section_id; ALTER TABLE passages DROP COLUMN block_id; ALTER TABLE passages DROP COLUMN kind; ALTER TABLE passages DROP COLUMN view; ALTER TABLE passages DROP COLUMN partial; ALTER TABLE passages DROP COLUMN context; DROP TABLE sections; PRAGMA user_version=1; UPDATE metadata SET value='markdown-v1' WHERE key='pipeline';").unwrap();
    drop(db);
    let mut index = Index::open(state.path()).unwrap();
    assert!(index.status().unwrap().needs_reindex);
    assert!(
        index
            .brief(
                "retry",
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("structure changed")
    );
    index.index(corpus.path(), &mut model).unwrap();
    assert!(!index.status().unwrap().needs_reindex);
    assert!(
        !index
            .brief(
                "retry",
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    ..Default::default()
                }
            )
            .unwrap()
            .files
            .is_empty()
    );
}
#[test]
fn same_file_links_and_long_link_lines_keep_exact_source_evidence() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = format!(
        "# Guide\n\n{} [next](#next).\n\n## Next\n\nRecovery details.\n",
        "A long line with context ".repeat(100)
    );
    fs::write(corpus.path().join("guide.md"), &source).unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let related = index.related("guide.md#guide", 10).unwrap();
    let link = related
        .connections
        .iter()
        .find(|c| c.kind == "references")
        .unwrap();
    assert_eq!(link.path, "guide.md");
    assert_eq!(link.anchor.as_deref(), Some("next"));
    assert!(link.evidence[0].text.contains("[next](#next)"));
    let e = &link.evidence[0];
    assert_eq!(e.text, source[e.bytes[0]..e.bytes[1]]);
}

#[test]
fn weak_table_row_cannot_tie_stronger_prose_within_a_section() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = "# Recovery\n\n| Name | Result |\n| --- | --- |\n| payment | pending |\n\nSafe retry of duplicate payments requires the retained transaction token.\n";
    fs::write(corpus.path().join("recovery.md"), source).unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let d = index
        .diagnose(
            "safe retry duplicate payments",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                connections: false,
                max_files: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        d.brief.files[0].passages[0]
            .text
            .contains("retained transaction token")
    );
    let section = d
        .candidates
        .iter()
        .find(|c| !c.selected_passages.is_empty())
        .unwrap();
    let strongest = section
        .hits
        .iter()
        .max_by(|a, b| {
            a.lexical_score
                .unwrap()
                .total_cmp(&b.lexical_score.unwrap())
        })
        .unwrap();
    assert_eq!(strongest.passage_id, d.brief.files[0].passages[0].id);
}

#[test]
fn evidence_selector_reclaims_unused_connection_space() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(corpus.path().join("retry.md"), format!(
        "# Retry\n\nRetry with the saved identity after a lost reply. {}\n\nRetry status is separate from the original effect.\n",
        "Retain the original request and inspect its recorded outcome. ".repeat(4)
    )).unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        connections: false,
        ..Default::default()
    };
    let full = index
        .brief("retry saved identity lost reply", None, &options)
        .unwrap();
    let budget = estimate_tokens(&serde_json::to_string(&full).unwrap()) + 40;
    let with_connections = index
        .brief(
            "retry saved identity lost reply",
            None,
            &BriefOptions {
                max_tokens: budget,
                connections: true,
                ..options
            },
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&full.files).unwrap(),
        serde_json::to_value(&with_connections.files).unwrap()
    );
    assert!(estimate_tokens(&serde_json::to_string(&with_connections).unwrap()) <= budget);
}

#[test]
fn evidence_bundles_keep_adjacent_code_and_source_coordinates() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = "# Recovery\n\nReconcile the uncertain operation using the saved identity:\n\n```sh\nuc inspect abc123\n```\n\n## Unrelated\n\n```sh\nrm elsewhere\n```\n";
    fs::write(corpus.path().join("recovery.md"), source).unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        connections: false,
        ..Default::default()
    };
    let d = index
        .diagnose("reconcile uncertain operation", None, &options)
        .unwrap();
    let passages = &d.brief.files[0].passages;
    assert!(passages.iter().any(|p| p.text.contains("uc inspect abc123")
        && p.matched_by.iter().any(|s| s == "adjacent_context")));
    assert!(!passages.iter().any(|p| p.text.contains("rm elsewhere")));
    for p in passages {
        assert_eq!(p.text, source[p.bytes[0]..p.bytes[1]]);
        assert_eq!(p.lines[0], lore::markdown::line_at(source, p.bytes[0]));
    }
    assert!(
        d.selection_trace
            .iter()
            .any(|s| s.kind == "bundle" && s.decision == "selected")
    );
    let repeat = index
        .diagnose("reconcile uncertain operation", None, &options)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&d.selection_trace).unwrap(),
        serde_json::to_value(&repeat.selection_trace).unwrap()
    );
}

#[test]
fn evidence_selection_can_return_more_than_three_blocks_from_one_file() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(corpus.path().join("retry.md"),"# Retry\n\nRetry first preserves the identity.\n\nRetry second reads the ledger.\n\n```sh\nretry --status\n```\n\n## Other retry cases\n\nRetry third checks the deadline.\n\nRetry fourth reports an unresolved effect.\n").unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let d = index
        .diagnose(
            "retry",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                connections: false,
                max_files: 1,
                max_tokens: 4000,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(d.brief.files.len(), 1);
    assert!(d.brief.files[0].passages.len() > 3);
    let ids: std::collections::BTreeSet<_> =
        d.brief.files[0].passages.iter().map(|p| p.id).collect();
    assert_eq!(ids.len(), d.brief.files[0].passages.len());
}

#[test]
fn selection_control_preserves_discovery_and_budget_limits() {
    use lore::search::Selection;
    let (_corpus, _state, index, mut model) = fixture();
    let mut pools = Vec::new();
    for selection in [Selection::Ranked, Selection::Evidence] {
        let options = BriefOptions {
            selection,
            connections: false,
            min_similarity: 0.0,
            max_tokens: 450,
            max_files: 1,
            ..Default::default()
        };
        let d = index
            .diagnose("duplicate payments", Some(&mut model), &options)
            .unwrap();
        assert!(d.brief.files.len() <= 1);
        assert!(estimate_tokens(&serde_json::to_string(&d.brief).unwrap()) <= 450);
        pools.push(
            d.candidates
                .iter()
                .map(|c| (c.section_id, c.keyword_rank, c.semantic_rank, c.score))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(pools[0], pools[1]);
}

#[test]
fn evidence_connections_compete_without_exceeding_budget_or_scope() {
    let (_corpus, _state, index, mut model) = fixture();
    let d = index
        .diagnose(
            "duplicate payments PaymentKey",
            Some(&mut model),
            &BriefOptions {
                min_similarity: 0.0,
                max_files: 1,
                max_tokens: 2000,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(d.brief.files.len(), 1);
    assert!(
        d.brief.files[0]
            .connections
            .iter()
            .any(|c| c.path == "delivery.md")
    );
    assert!(
        d.selection_trace
            .iter()
            .any(|s| s.kind == "connection" && s.decision == "selected")
    );
    assert!(estimate_tokens(&serde_json::to_string(&d.brief).unwrap()) <= 2000);
    let limited = index
        .diagnose(
            "duplicate payments PaymentKey",
            Some(&mut model),
            &BriefOptions {
                min_similarity: 0.0,
                max_files: 1,
                max_tokens: 450,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(estimate_tokens(&serde_json::to_string(&limited.brief).unwrap()) <= 450);
}

#[test]
fn large_evidence_requests_report_bounded_selection() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut source = String::new();
    for section in 0..12 {
        source.push_str(&format!("# Topic {section}\n\n"));
        for block in 0..3 {
            source.push_str(&format!("Retry operation {section} step {block} records an independent outcome.\n\n\n\n\n```sh\nretry --topic {section} --step {block}\n```\n\n\n\n\n"));
        }
    }
    fs::write(corpus.path().join("retry.md"), source).unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let d = index
        .diagnose(
            "retry",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                connections: false,
                max_tokens: 100_000,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(d.brief.truncated);
    assert!(
        d.selection_trace
            .iter()
            .any(|s| s.decision == "selection_step_limit")
    );
    assert!(d.brief.estimated_tokens < 100_000);
}

#[test]
fn content_vectors_ignore_headings_and_keep_table_headers() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    for (path, heading) in [("a.md", "Quasar"), ("b.md", "Garden")] {
        fs::write(corpus.path().join(path),format!("# {heading}\n\nRetain the original request.\n\n| Status | Meaning |\n| --- | --- |\n| waiting | pending input |\n")).unwrap();
    }
    let mut index = Index::open(state.path()).unwrap();
    let mut model = TestEmbedder::new();
    index.index_with_content(corpus.path(), &mut model).unwrap();
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    let rows=db.prepare("SELECT p.embedding_key,c.embedding_key FROM passages p JOIN passage_content c ON c.passage_id=p.id WHERE p.text='Retain the original request.' ORDER BY p.path").unwrap().query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].0, rows[1].0);
    assert_eq!(rows[0].1, rows[1].1);
    let row:String=db.query_row("SELECT f.text FROM content_fts f JOIN passages p ON p.id=f.rowid WHERE p.path='a.md' AND p.kind='table_row'",[],|r|r.get(0)).unwrap();
    assert!(row.contains("| Status | Meaning |"));
    assert!(row.contains("| waiting | pending input |"));
    assert!(!row.contains("Quasar"));
    let refresh = index.index_with_content(corpus.path(), &mut model).unwrap();
    assert_eq!(refresh.content_embeddings_computed, 0);
    assert_eq!(refresh.content_embeddings_reused, refresh.passages);
    assert!(!index.status().unwrap().needs_content_index);
}

#[test]
fn content_scoring_preserves_discovery_and_the_selection_choice_pool() {
    use lore::search::PassageScoring;
    let (_corpus, _state, index, mut model) = fixture();
    let mut pools = Vec::new();
    let mut discovery = Vec::new();
    for scoring in [
        PassageScoring::Contextual,
        PassageScoring::Content,
        PassageScoring::Blended,
    ] {
        let d = index
            .diagnose(
                "duplicate payments",
                Some(&mut model),
                &BriefOptions {
                    passage_scoring: scoring,
                    connections: false,
                    min_similarity: 0.0,
                    ..Default::default()
                },
            )
            .unwrap();
        discovery.push(
            d.candidates
                .iter()
                .map(|c| {
                    (
                        c.section_id,
                        c.keyword_rank,
                        c.semantic_rank,
                        c.score,
                        c.file_score,
                    )
                })
                .collect::<Vec<_>>(),
        );
        pools.push(
            d.selection_trace
                .iter()
                .map(|s| (s.kind.clone(), s.path.clone(), s.passage_ids.clone()))
                .collect::<Vec<_>>(),
        );
        assert!(estimate_tokens(&serde_json::to_string(&d.brief).unwrap()) <= 2000);
        if scoring != PassageScoring::Contextual {
            assert!(
                d.selection_trace
                    .iter()
                    .any(|s| !s.passage_scores.is_empty())
            );
            assert!(
                d.selection_trace
                    .iter()
                    .flat_map(|s| &s.passage_scores)
                    .all(|s| s.content_similarity.is_some())
            );
        }
    }
    assert_eq!(discovery[0], discovery[1]);
    assert_eq!(discovery[1], discovery[2]);
    assert_eq!(pools[0], pools[1]);
    assert_eq!(pools[1], pools[2]);
}

#[test]
fn content_lexical_scores_do_not_inherit_heading_matches() {
    use lore::search::PassageScoring;
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(corpus.path().join("guide.md"),"# Quasar navigation\n\nBack to the documentation index.\n\nQuasar decoding uses the retained frame checksum.\n").unwrap();
    let mut index = Index::open(state.path()).unwrap();
    index
        .index_with_content(corpus.path(), &mut TestEmbedder::new())
        .unwrap();
    let d = index
        .diagnose(
            "quasar",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                passage_scoring: PassageScoring::Content,
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(d.brief.files[0].passages[0].text.contains("frame checksum"));
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    let footer: i64 = db
        .query_row(
            "SELECT id FROM passages WHERE text='Back to the documentation index.'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let signal = d
        .selection_trace
        .iter()
        .flat_map(|s| &s.passage_scores)
        .find(|s| s.passage_id == footer)
        .unwrap();
    assert_eq!(signal.content_bm25, 0.0);
    assert_eq!(signal.content_similarity, None);
}

#[test]
fn schema_two_backfill_is_atomic_and_preserves_discovery_ids() {
    use lore::search::PassageScoring;
    let (corpus, state, index, mut model) = fixture();
    let old_revision = index.status().unwrap().revision;
    drop(index);
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    let snapshot = |db: &rusqlite::Connection| {
        db.prepare("SELECT id,embedding_key FROM passages ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = snapshot(&db);
    db.execute_batch("DROP TRIGGER content_delete; DROP TABLE passage_content; DROP TABLE content_fts; DELETE FROM embeddings WHERE key NOT IN (SELECT embedding_key FROM passages); DELETE FROM metadata WHERE key='content_pipeline'; PRAGMA user_version=2;").unwrap();
    drop(db);
    let mut index = Index::open(state.path()).unwrap();
    assert!(index.status().unwrap().needs_content_index);
    assert!(!index.status().unwrap().needs_reindex);
    let options = BriefOptions {
        mode: Mode::Keyword,
        connections: false,
        ..Default::default()
    };
    assert!(
        !index
            .brief("retry", None, &options)
            .unwrap()
            .files
            .is_empty()
    );
    assert!(
        index
            .brief(
                "retry",
                None,
                &BriefOptions {
                    passage_scoring: PassageScoring::Content,
                    ..options
                }
            )
            .unwrap_err()
            .to_string()
            .contains("Content representation")
    );
    model.fail = true;
    assert!(index.index_with_content(corpus.path(), &mut model).is_err());
    assert_eq!(index.status().unwrap().revision, old_revision);
    assert!(index.status().unwrap().needs_content_index);
    model.fail = false;
    let r = index.index_with_content(corpus.path(), &mut model).unwrap();
    assert_eq!(r.embeddings_computed, 0);
    assert_eq!(r.changed, 0);
    assert!(r.content_embeddings_computed > 0);
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    assert_eq!(snapshot(&db), before);
    assert!(!index.status().unwrap().needs_content_index);
}

#[test]
fn content_cache_survives_heading_edits_and_removes_deleted_sources() {
    let (corpus, state, mut index, mut model) = fixture();
    let old = fs::read_to_string(corpus.path().join("retry.md")).unwrap();
    fs::write(
        corpus.path().join("retry.md"),
        old.replace("# Payment retries", "# Request recovery"),
    )
    .unwrap();
    let report = index.index(corpus.path(), &mut model).unwrap();
    assert_eq!(report.content_embeddings_computed, 1); // Only the changed heading has new content.
    fs::remove_file(corpus.path().join("delivery.md")).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    let (p,c,f):(i64,i64,i64)=db.query_row("SELECT (SELECT count(*) FROM passages),(SELECT count(*) FROM passage_content),(SELECT count(*) FROM content_fts)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(p, c);
    assert_eq!(c, f);
    let unused:i64=db.query_row("SELECT count(*) FROM embeddings WHERE key NOT IN (SELECT embedding_key FROM passages UNION SELECT embedding_key FROM passage_content)",[],|r|r.get(0)).unwrap();
    assert_eq!(unused, 0);
    assert!(
        db.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

#[test]
fn content_scoring_keeps_exact_lookup_scope_and_role_controls() {
    use lore::search::{PassageScoring, RoleRule, Selection};
    let (_corpus, _state, index, mut model) = fixture();
    for scoring in [PassageScoring::Content, PassageScoring::Blended] {
        let options = BriefOptions {
            passage_scoring: scoring,
            scope: "retry.md".into(),
            roles: vec![RoleRule {
                prefix: "retry.md".into(),
                role: "historical".into(),
                weight: 0.1,
            }],
            ..Default::default()
        };
        let d = index
            .diagnose("retry.md#payment-retries", Some(&mut model), &options)
            .unwrap();
        assert_eq!(d.brief.files[0].path, "retry.md");
        assert_eq!(d.brief.files[0].passages[0].anchor, "payment-retries");
        assert!(d.brief.files.iter().all(|f|f.path=="retry.md" && f.connections.iter().all(|c|c.path=="retry.md")));
        assert!(
            index
                .brief(
                    "retry",
                    Some(&mut model),
                    &BriefOptions {
                        selection: Selection::Ranked,
                        ..options
                    }
                )
                .unwrap_err()
                .to_string()
                .contains("requires --selection evidence")
        );
    }
}

#[test]
fn failure_during_content_work_rolls_back_discovery_updates() {
    struct ContentFailure {
        inner: TestEmbedder,
        mutate: Option<std::path::PathBuf>,
    }
    impl Embedder for ContentFailure {
        fn identity(&self) -> &str {
            self.inner.identity()
        }
        fn dimensions(&self) -> usize {
            384
        }
        fn token_count(&self, text: &str) -> Result<usize> {
            self.inner.token_count(text)
        }
        fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
            self.inner.embed_query(text)
        }
        fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            if texts.iter().any(|t| t == "A newly indexed body.") {
                if let Some(path) = self.mutate.take() {
                    fs::write(path, "Changed while content embeddings were running.")?;
                } else {
                    anyhow::bail!("injected content failure");
                }
            }
            self.inner.embed_documents(texts)
        }
    }
    for mutate in [false, true] {
        let (corpus, _state, mut index, _model) = fixture();
        let before = index.status().unwrap();
        let path = corpus.path().join("new.md");
        fs::write(&path, "# New source\n\nA newly indexed body.\n").unwrap();
        let mut model = ContentFailure {
            inner: TestEmbedder::new(),
            mutate: mutate.then_some(path),
        };
        let error = index
            .index(corpus.path(), &mut model)
            .unwrap_err()
            .to_string();
        assert!(error.contains(if mutate {
            "changed during content indexing"
        } else {
            "injected content failure"
        }));
        let after = index.status().unwrap();
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.documents, before.documents);
        assert_eq!(after.content_passages, before.content_passages);
        assert!(!after.needs_content_index);
    }
}

#[test]
fn content_provisioning_is_opt_in_and_ordinary_refresh_maintains_it() {
    use lore::search::PassageScoring;
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(
        corpus.path().join("guide.md"),
        "# Recovery\n\nRetry the retained request.",
    )
    .unwrap();
    let mut index = Index::open(state.path()).unwrap();
    let mut model = TestEmbedder::new();
    let first = index.index(corpus.path(), &mut model).unwrap();
    assert_eq!(first.content_embeddings_computed, 0);
    assert_eq!(index.status().unwrap().content_passages, 0);
    assert!(index.status().unwrap().needs_content_index);
    let options = BriefOptions {
        mode: Mode::Keyword,
        passage_scoring: PassageScoring::Content,
        ..Default::default()
    };
    assert!(
        index
            .brief("retry", None, &options)
            .unwrap_err()
            .to_string()
            .contains("--content")
    );
    let provision = index.index_with_content(corpus.path(), &mut model).unwrap();
    assert_eq!(provision.embeddings_computed, 0);
    assert!(provision.content_embeddings_computed > 0);
    fs::write(
        corpus.path().join("guide.md"),
        "# Recovery\n\nRetry only after reconciliation.",
    )
    .unwrap();
    let updated = index.index(corpus.path(), &mut model).unwrap();
    assert!(updated.content_embeddings_computed > 0);
    assert!(!index.status().unwrap().needs_content_index);
    assert!(
        index.brief("reconciliation", None, &options).unwrap().files[0]
            .passages
            .iter()
            .any(|p| p.text.contains("reconciliation"))
    );
}

#[test]
fn api_bundles_are_atomic_across_budgets() {
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::create_dir(corpus.path().join("docs")).unwrap();
    fs::write(corpus.path().join("docs/api.md"), "# Input\n\n## Deliver input\n\n`deliver(text, submit=True)` writes text followed by Enter when requested.\n\n### Leave input pending: `submit=False`\n\nLeaves input pending without Enter.\n\n### Send a key\n\n`press(key)` sends one key.\n\n| Key form | Value |\n| --- | --- |\n| Simple | `enter` |\n").unwrap();
    fs::write(
        corpus.path().join("outside.md"),
        "# Other\n\n`outside(value)` returns another value.\n",
    )
    .unwrap();
    let mut index = Index::open(state.path()).unwrap();
    let mut model = TestEmbedder::new();
    index.index(corpus.path(), &mut model).unwrap();
    let mut saw_option = false;
    let mut saw_table = false;
    for budget in [400, 650, 1000, 2000] {
        let options = BriefOptions {
            scope: "docs".into(),
            max_tokens: budget,
            connections: false,
            ..Default::default()
        };
        let result = index
            .brief("input pending enter key", Some(&mut model), &options)
            .unwrap();
        assert!(estimate_tokens(&serde_json::to_string(&result).unwrap()) <= budget);
        let texts: Vec<_> = result
            .files
            .iter()
            .flat_map(|f| {
                assert_eq!(f.path, "docs/api.md");
                f.passages.iter().map(|p| p.text.as_str())
            })
            .collect();
        if texts.iter().any(|t| t.contains("Leaves input pending")) {
            saw_option = true;
            assert!(
                texts
                    .iter()
                    .any(|t| t.contains("`deliver(text, submit=True)`"))
            );
        }
        if texts.iter().any(|t| t.contains("| Simple |")) {
            saw_table = true;
            assert!(texts.iter().any(|t| t.contains("`press(key)`")));
        }
    }
    assert!(saw_option && saw_table);
}

#[test]
fn source_health_detects_edits_additions_deletions_and_ignore_changes_without_writes() {
    let (corpus, _state, mut index, mut model) = fixture();
    let before = index.status().unwrap();
    assert!(before.root_available && before.source_check.is_none());
    let clean = index.check_status().unwrap();
    assert!(!clean.needs_reindex);
    assert_eq!(clean.source_check.unwrap().checked_documents, 3);
    fs::write(corpus.path().join("retry.md"), "# New retry guidance\n").unwrap();
    fs::write(corpus.path().join("added.markdown"), "# Added\n").unwrap();
    fs::remove_file(corpus.path().join("delivery.md")).unwrap();
    fs::write(
        corpus.path().join(".loreignore"),
        "gardening.md\nignored.md\n",
    )
    .unwrap();
    fs::write(corpus.path().join("ignored.md"), "# Ignored\n").unwrap();
    fs::create_dir(corpus.path().join("target")).unwrap();
    fs::write(corpus.path().join("target/generated.md"), "# Generated\n").unwrap();
    let large = fs::File::create(corpus.path().join("large.md")).unwrap();
    large.set_len(2 * 1024 * 1024 + 1).unwrap();
    let status = index.check_status().unwrap();
    assert!(status.needs_reindex);
    assert_eq!(status.revision, before.revision);
    assert_eq!(status.documents, before.documents);
    let check = status.source_check.unwrap();
    assert_eq!(check.checked_documents, 2);
    assert_eq!(check.added, ["added.markdown"]);
    assert_eq!(check.changed, ["retry.md"]);
    assert_eq!(check.removed, ["delivery.md", "gardening.md"]);
    assert!(check.errors.is_empty());
    index.index(corpus.path(), &mut model).unwrap();
    assert!(!index.check_status().unwrap().needs_reindex);
    fs::write(corpus.path().join("invalid.md"), [0xff]).unwrap();
    let broken = index.check_status().unwrap();
    assert!(broken.needs_reindex);
    assert!(broken.source_check.unwrap().errors[0].contains("invalid.md"));
}

#[test]
fn relocated_index_reuses_vectors_and_invalidates_old_read_cursors() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(
        corpus.path().join("long.md"),
        format!("# Long\n\n{}", "Keep reading.\n\n".repeat(200)),
    )
    .unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let first = index
        .read(
            "long.md",
            &PageOptions {
                max_tokens: 400,
                ..Default::default()
            },
        )
        .unwrap();
    let before = index.status().unwrap();
    let destination = TempDir::new().unwrap();
    let moved = destination.path().join("moved");
    fs::rename(corpus.path(), &moved).unwrap();
    let missing = index.status().unwrap();
    assert!(!missing.root_available && missing.needs_reindex);
    assert!(index.check_status().unwrap().source_check.unwrap().errors[0].contains("--relocate"));
    assert!(
        index
            .index(&moved, &mut model)
            .unwrap_err()
            .to_string()
            .contains("--relocate")
    );
    let report = index.index_relocated(&moved, &mut model, false).unwrap();
    assert_eq!(report.relocated_from, before.root);
    assert_eq!(report.revision, before.revision + 1);
    assert_eq!(report.changed, 0);
    assert_eq!(report.embeddings_computed, 0);
    assert_eq!(report.content_embeddings_computed, 0);
    assert_eq!(report.embeddings_reused, before.passages);
    assert!(!index.check_status().unwrap().needs_reindex);
    assert!(index.read("retry.md", &PageOptions::default()).is_ok());
    let cursor = first.next_cursor.expect("fixture is paginated");
    assert!(
        index
            .read(
                "long.md",
                &PageOptions {
                    max_tokens: 400,
                    cursor: Some(cursor)
                }
            )
            .unwrap_err()
            .to_string()
            .contains("Continuation")
    );
    let no_op = index.index_relocated(&moved, &mut model, false).unwrap();
    assert!(no_op.relocated_from.is_none());
    assert_eq!(no_op.revision, report.revision);
}

#[test]
fn failed_relocation_preserves_old_root_and_success_refreshes_changed_sources() {
    let (corpus, _state, mut index, mut model) = fixture();
    let before = index.status().unwrap();
    let destination = TempDir::new().unwrap();
    fs::copy(
        corpus.path().join("retry.md"),
        destination.path().join("retry.md"),
    )
    .unwrap();
    fs::write(
        destination.path().join("added.md"),
        "# New source\n\nNovel information.\n",
    )
    .unwrap();
    model.fail = true;
    assert!(
        index
            .index_relocated(destination.path(), &mut model, true)
            .is_err()
    );
    let after = index.status().unwrap();
    assert_eq!(after.root, before.root);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.documents, before.documents);
    assert!(!index.check_status().unwrap().needs_reindex);
    model.fail = false;
    let report = index
        .index_relocated(destination.path(), &mut model, false)
        .unwrap();
    assert_eq!(report.removed, 2);
    assert_eq!(report.changed, 1);
    assert!(report.embeddings_reused > 0);
    assert!(report.content_embeddings_computed > 0);
    assert!(!index.check_status().unwrap().needs_reindex);
    assert!(index.read("added.md", &Default::default()).is_ok());
    assert!(index.read("delivery.md", &Default::default()).is_err());
}

#[test]
fn section_reads_expose_verified_parent_targets_across_pagination() {
    use lore::reading::PageOptions;
    let (corpus, _state, mut index, mut model) = fixture();
    let source = format!(
        "# Guide {{#guide-id}}\n\n## Procedure {{#procedure-id}}\n\nRequired prefix.\n\n### Step\n\n{}\n### Step\n\nFinish.\n",
        "An extended step.\n\n".repeat(120)
    );
    fs::write(corpus.path().join("guide.md"), &source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let mut options = PageOptions {
        max_tokens: 450,
        ..Default::default()
    };
    let mut combined = String::new();
    loop {
        let page = index.read("guide.md#step", &options).unwrap();
        assert_eq!(
            page.section.parent_target.as_deref(),
            Some("guide.md#procedure-id")
        );
        assert!(estimate_tokens(&serde_json::to_string(&page).unwrap()) <= 450);
        combined.push_str(&page.content.text);
        options.cursor = page.next_cursor;
        if options.cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        combined,
        source[source.find("### Step").unwrap()..source.rfind("### Step").unwrap()]
    );
    let child = index
        .read("guide.md#step-1", &PageOptions::default())
        .unwrap();
    let parent = index
        .read(
            child.section.parent_target.as_deref().unwrap(),
            &PageOptions::default(),
        )
        .unwrap();
    assert!(parent.content.text.contains("Required prefix."));
    assert_eq!(
        parent.section.parent_target.as_deref(),
        Some("guide.md#guide-id")
    );
    let top = index
        .read("guide.md#guide-id", &PageOptions::default())
        .unwrap();
    assert_eq!(top.section.parent_target.as_deref(), Some("guide.md"));
    assert!(
        index
            .read("guide.md", &PageOptions::default())
            .unwrap()
            .section
            .parent_target
            .is_none()
    );
}

#[test]
fn diagnostics_explain_api_activation_without_changing_the_brief() {
    use lore::preferences::Qualifier;
    let (_corpus, _state, index, mut model) = fixture();
    let options = BriefOptions::default();
    let default = index
        .diagnose("payment", Some(&mut model), &options)
        .unwrap();
    assert!(default.api_context.requested && default.api_context.enabled);
    assert!(default.api_context.disabled_by.is_empty());
    let preferred = index
        .diagnose(
            "payment",
            Some(&mut model),
            &BriefOptions {
                prefer: vec![Qualifier::Python],
                alternatives: 2,
                ..options.clone()
            },
        )
        .unwrap();
    assert!(preferred.api_context.requested && !preferred.api_context.enabled);
    assert_eq!(preferred.api_context.disabled_by, ["alternatives"]);
    assert!(
        !preferred
            .selection_trace
            .iter()
            .any(|t| t.kind == "api_bundle")
    );
    let exact = index
        .diagnose("retry.md", Some(&mut model), &options)
        .unwrap();
    assert_eq!(exact.api_context.disabled_by, ["exact_lookup"]);
    let off = index
        .diagnose(
            "payment",
            Some(&mut model),
            &BriefOptions {
                api_context: false,
                ..options
            },
        )
        .unwrap();
    assert!(!off.api_context.requested && !off.api_context.enabled);
    assert_eq!(off.api_context.disabled_by, ["api_context"]);
}

#[test]
fn procedure_groups_follow_real_markdown_lists_and_adjacent_restrictions() {
    let (corpus, state, mut index, mut model) = fixture();
    let source = "# Workflow\r\n\r\nKeep the saved identity:\r\n\r\n1. Save the request.\r\n\r\n   - Preserve nested details.\r\n   - Keep both flags.\r\n\r\n1. Inspect the result.\r\n\r\n   ```text\r\n   9. This is code, not a new step.\r\n   ```\r\n\r\nThis does **not** authorize repeating a mutation.\r\n\r\n## Other\r\n\r\n1) Another first item.\r\n2) Another second item.\r\n\r\nAn unrelated explanation.\r\n\r\n- An unordered item.\r\n- Another unordered item.\r\n\r\n> 1. A quoted list.\r\n> 2. A quoted second step.\r\n";
    fs::write(corpus.path().join("workflow.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    let ids = db
        .prepare("SELECT id FROM passages WHERE path='workflow.md' ORDER BY start_byte")
        .unwrap()
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let groups = index.procedure_groups(&ids).unwrap();
    assert_eq!(groups.len(), 2);
    let text = |id| {
        db.query_row("SELECT text FROM passages WHERE id=?", [id], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
    };
    assert_eq!(groups[0].items.len(), 2);
    assert_eq!(
        text(groups[0].introduction.unwrap()),
        "Keep the saved identity:"
    );
    assert!(text(groups[0].restriction.unwrap()).starts_with("This does **not**"));
    assert!(text(groups[0].items[0]).contains("Preserve nested details"));
    assert!(text(groups[0].items[1]).contains("9. This is code"));
    assert!(groups[1].introduction.is_none() && groups[1].restriction.is_none());
    // A single middle seed brings its whole list, never a different section's list.
    let one = index.procedure_groups(&[groups[0].items[1]]).unwrap();
    assert_eq!(
        serde_json::to_value(&one[0]).unwrap(),
        serde_json::to_value(&groups[0]).unwrap()
    );
    assert_eq!(one.len(), 1);
    fs::write(
        corpus.path().join("workflow.md"),
        format!("{source}\nChanged."),
    )
    .unwrap();
    assert!(
        index
            .procedure_groups(&ids)
            .unwrap_err()
            .to_string()
            .contains("changed since indexing")
    );
}

#[test]
fn procedure_groups_skip_fragments_large_lists_and_nonadjacent_context() {
    let (corpus, state, mut index, mut model) = fixture();
    let mut source = format!(
        "# Fragmented\n\n1. {}\n2. Short second step.\n\n# Too many\n\n",
        "Long instruction. ".repeat(300)
    );
    for i in 1..=13 {
        source.push_str(&format!("{i}. One step.\n"));
    }
    source.push_str("\n# Separated\n\nBefore:\n\n<!-- boundary -->\n\n1. First.\n2. Second.\n\n<!-- boundary -->\n\nNever combine across a boundary.\n\n# Sibling\n\nNever attach this sibling warning.\n\n# Dense\n\n1. First.\n2. Second.\n\n");
    for _ in 0..128 {
        source.push_str("Extra paragraph.\n\n");
    }
    fs::write(corpus.path().join("workflow.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    let ids = db
        .prepare("SELECT id FROM passages WHERE path='workflow.md' ORDER BY start_byte")
        .unwrap()
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let groups = index.procedure_groups(&ids).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].items.len(), 2);
    assert!(groups[0].introduction.is_none() && groups[0].restriction.is_none());
}

#[test]
fn diagnostics_identify_missing_steps_and_context_without_altering_evidence() {
    let (corpus, state, mut index, mut model) = fixture();
    let source = "# Workflow {#exact-flow}\n\nKeep the original identity:\n\n1. Save the request before sending.\n2. Send the uniquemiddle request.\n3. Inspect the result before continuing.\n\nNever repeat an uncertain mutation.\n";
    fs::write(corpus.path().join("workflow.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let options = BriefOptions {
        mode: Mode::Keyword,
        scope: "workflow.md".into(),
        connections: false,
        ..Default::default()
    };
    let diagnostic = index.diagnose("uniquemiddle", None, &options).unwrap();
    let brief = index.brief("uniquemiddle", None, &options).unwrap();
    assert_eq!(
        serde_json::to_value(&brief.files).unwrap(),
        serde_json::to_value(&diagnostic.brief.files).unwrap()
    );
    assert!(estimate_tokens(&serde_json::to_string(&brief).unwrap()) <= options.max_tokens);
    let groups = &diagnostic.procedure_coverage.groups;
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(group.read_target, "workflow.md#exact-flow");
    assert_eq!(group.source_hash, brief.files[0].source_hash);
    assert_eq!(
        (group.total_steps, group.visible_steps, group.group_complete),
        (3, 1, false)
    );
    assert_eq!(
        group.missing.iter().map(|p| p.role).collect::<Vec<_>>(),
        ["introduction", "step", "step", "following_restriction"]
    );
    let read = index.read(&group.read_target, &Default::default()).unwrap();
    assert_eq!(read.content.text, source);
    for part in &group.missing {
        assert!(read.content.bytes[0] <= part.bytes[0] && part.bytes[1] <= read.content.bytes[1]);
        assert_eq!(
            part.lines,
            [
                lore::markdown::line_at(source, part.bytes[0]),
                lore::markdown::line_at(source, part.bytes[1] - 1)
            ]
        );
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lore"))
        .args([
            "--index-dir",
            state.path().to_str().unwrap(),
            "--model-dir",
            "/nonexistent/lore-model",
            "diagnose",
            "uniquemiddle",
            "--mode",
            "keyword",
            "--scope",
            "workflow.md",
            "--no-connections",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        cli["procedure_coverage"],
        serde_json::to_value(diagnostic.procedure_coverage).unwrap()
    );
}

#[test]
fn diagnostics_distinguish_complete_groups_from_no_recognized_groups() {
    let (corpus, _state, mut index, mut model) = fixture();
    for (path, header) in [("root-list.md", ""), ("empty-anchor.md", "# !!!\n\n")] {
        fs::write(corpus.path().join(path), format!("{header}1. Execute uniquestep alpha.\n2. Execute uniquestep beta.\n3. Execute uniquestep gamma.\n")).unwrap();
    }
    index.index(corpus.path(), &mut model).unwrap();
    for (path, target) in [
        ("root-list.md", "root-list.md"),
        ("empty-anchor.md", "empty-anchor.md#"),
    ] {
        let diagnostic = index
            .diagnose(
                "uniquestep",
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    connections: false,
                    scope: path.into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let group = &diagnostic.procedure_coverage.groups[0];
        assert_eq!(group.read_target, target);
        assert_eq!(group.total_steps, 3);
        assert_eq!(group.visible_steps, 3);
        assert!(group.group_complete && group.missing.is_empty());
        assert!(index.read(target, &Default::default()).is_ok());
    }
    let unrelated = index
        .diagnose(
            "payment",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                connections: false,
                scope: "retry.md".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(unrelated.procedure_coverage.groups.is_empty());
    assert!(
        unrelated
            .procedure_coverage
            .limitations
            .iter()
            .any(|s| s.contains("empty report does not establish completeness"))
    );
}

#[test]
fn compact_guidance_finds_body_and_ancestor_gaps_without_reading_siblings() {
    let (corpus, _state, mut index, mut model) = fixture();
    let source = "# Guide\n\nGlobal setup.\n\n## Keys\n\nLeader is Alt-U.\n\n### Copy\n\nuniquefocus uses prefix then [.\n\nSelect with v.\n\nCopy with y.\n\n### Other\n\nSibling content must not count.\n";
    fs::write(corpus.path().join("guide.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let diagnostic = index
        .diagnose(
            "uniquefocus",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "guide.md".into(),
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    let before = serde_json::to_value(&diagnostic).unwrap();
    let compact = index.compact_diagnostics(&diagnostic, 2000).unwrap();
    assert_eq!(serde_json::to_value(&diagnostic).unwrap(), before);
    assert_eq!(compact.answer_completeness, "not_assessed");
    assert_eq!(compact.next_reads.len(), 1);
    let next = &compact.next_reads[0];
    assert_eq!(next.read_target, "guide.md#copy");
    assert!(next.context);
    assert_eq!(next.gaps.section_blocks, 2);
    assert_eq!(next.gaps.ancestor_blocks, 2);
    assert_eq!(next.gaps.numbered_steps, 0);
    assert!(!compact.probe.limited);
    let read = index
        .read_context(&next.read_target, &Default::default())
        .unwrap();
    assert_eq!(next.source_hash, read.source_hash);
    let text: String = read.parts.iter().map(|p| p.content.text.as_str()).collect();
    assert!(text.contains("Leader is Alt-U.") && text.contains("Copy with y."));
    assert!(!text.contains("Sibling content"));
    for e in &next.examples {
        assert!(
            read.parts
                .iter()
                .any(|p| p.content.bytes[0] <= e.bytes[0] && p.content.bytes[1] >= e.bytes[1])
        );
        assert_eq!(
            e.lines,
            [
                lore::markdown::line_at(source, e.bytes[0]),
                lore::markdown::line_at(source, e.bytes[1] - 1)
            ]
        );
    }
}

#[test]
fn compact_guidance_preserves_numbered_gaps_and_exact_empty_anchors() {
    let (corpus, _state, mut index, mut model) = fixture();
    let source = "# !!!\n\nKeep the identity:\n\n1. Save the request.\n2. Send uniquemiddle.\n3. Inspect the result.\n\nNever blindly repeat.\n";
    fs::write(corpus.path().join("flow.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let diagnostic = index
        .diagnose(
            "uniquemiddle",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "flow.md".into(),
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    let compact = index.compact_diagnostics(&diagnostic, 2000).unwrap();
    let next = &compact.next_reads[0];
    assert_eq!(next.read_target, "flow.md#");
    assert!(!next.context);
    assert_eq!(
        (next.gaps.numbered_steps, next.gaps.numbered_context_blocks),
        (2, 2)
    );
    assert_eq!(next.gaps.section_blocks, 4);
    assert_eq!(
        index
            .read(&next.read_target, &Default::default())
            .unwrap()
            .content
            .text,
        source
    );
}

#[test]
fn compact_guidance_reports_unknown_completeness_for_empty_hints_and_no_matches() {
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(
        corpus.path().join("tiny.md"),
        "# Tiny\n\nuniquewhole body.\n",
    )
    .unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    for (query, status) in [
        ("uniquewhole", "matches"),
        ("nonexistentunicorn", "no_matches"),
    ] {
        let diagnostic = index
            .diagnose(
                query,
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    scope: "tiny.md".into(),
                    connections: false,
                    ..Default::default()
                },
            )
            .unwrap();
        let compact = index.compact_diagnostics(&diagnostic, 2000).unwrap();
        assert_eq!(compact.retrieval_status, status);
        assert_eq!(compact.answer_completeness, "not_assessed");
        assert!(compact.next_reads.is_empty());
        assert_eq!(compact.omitted_suggestions, 0);
        assert!(!compact.probe.limited);
        assert!(
            compact
                .limitations
                .iter()
                .any(|s| s.contains("Empty hints do not establish completeness"))
        );
    }
}

#[test]
fn compact_guidance_bounds_response_and_exposes_probe_and_output_limits() {
    let (corpus, state, mut index, mut model) = fixture();
    let mut source = String::from("# Many\n\nShared setup.\n");
    for n in 0..70 {
        source.push_str(&format!(
            "\n## Part {n}\n\nuniquefocus entry {n}.\n\nOmitted restriction {n}.\n"
        ));
    }
    fs::write(corpus.path().join("many.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let mut diagnostic = index
        .diagnose(
            "uniquefocus",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "many.md".into(),
                connections: false,
                candidates: 1000,
                max_tokens: 100_000,
                ..Default::default()
            },
        )
        .unwrap();
    // Compose valid indexed evidence beyond the default selector's action cap
    // to exercise the independent library probe bound.
    let db = rusqlite::Connection::open(state.path().join("index.sqlite")).unwrap();
    diagnostic.brief.files[0].passages = db.prepare("SELECT p.id,p.section_id,s.anchor,s.heading,p.start_byte,p.end_byte,p.start_line,p.end_line,p.text FROM passages p JOIN sections s ON s.id=p.section_id WHERE p.path='many.md' AND p.text LIKE 'uniquefocus%' ORDER BY p.start_byte").unwrap()
        .query_map([], |r| Ok(lore::search::Passage {
            id:r.get(0)?,section_id:r.get(1)?,anchor:r.get(2)?,heading:r.get(3)?,
            bytes:[r.get::<_,u32>(4)? as usize,r.get::<_,u32>(5)? as usize],
            lines:[r.get::<_,u32>(6)? as usize,r.get::<_,u32>(7)? as usize],text:r.get(8)?,
            kind:"prose".into(),matched_by:vec![],semantic_similarity:None,excerpted:false,context:None,
        })).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    for budget in [500, 800, 1200, 2000] {
        let compact = index.compact_diagnostics(&diagnostic, budget).unwrap();
        let measured = estimate_tokens(&serde_json::to_string(&compact).unwrap());
        assert!(measured <= budget);
        assert_eq!(compact.estimated_tokens, measured);
        assert!(compact.next_reads.len() <= 6);
        assert!(compact.probe.selected_sections > 64);
        assert_eq!(compact.probe.examined_sections, 64);
        assert!(compact.probe.limited);
        assert_eq!(compact.next_reads.len() + compact.omitted_suggestions, 64);
    }
    assert!(
        index
            .compact_diagnostics(&diagnostic, 128)
            .unwrap_err()
            .to_string()
            .contains("increase --max-tokens")
    );

    let mut large = String::from("# Large\n\nuniquelarge block.\n");
    for n in 0..150 {
        large.push_str(&format!("\nUnrelated prose {n}.\n"));
    }
    fs::write(corpus.path().join("large.md"), large).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let diagnostic = index
        .diagnose(
            "uniquelarge",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "large.md".into(),
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    let compact = index.compact_diagnostics(&diagnostic, 2000).unwrap();
    assert!(compact.probe.limited);
    assert_eq!(compact.next_reads[0].gaps.section_blocks, 127);
}

#[test]
fn compact_guidance_rejects_stale_sources_and_diagnostic_snapshots() {
    let (corpus, _state, mut index, mut model) = fixture();
    let diagnostic = index
        .diagnose(
            "payment",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "retry.md".into(),
                ..Default::default()
            },
        )
        .unwrap();
    fs::write(
        corpus.path().join("retry.md"),
        "# Changed\n\nChanged payment rules.",
    )
    .unwrap();
    assert!(
        index
            .compact_diagnostics(&diagnostic, 2000)
            .unwrap_err()
            .to_string()
            .contains("changed since indexing")
    );
    index.index(corpus.path(), &mut model).unwrap();
    assert!(
        index
            .compact_diagnostics(&diagnostic, 2000)
            .unwrap_err()
            .to_string()
            .contains("snapshot changed")
    );
    let (_other_corpus, _other_state, other_index, _other_model) = fixture();
    assert!(
        other_index
            .compact_diagnostics(&diagnostic, 2000)
            .unwrap_err()
            .to_string()
            .contains("snapshot changed")
    );
}

#[test]
fn compact_diagnostic_cli_and_batch_work_without_models() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let (_corpus, state, _index, _model) = fixture();
    let base = [
        "--index-dir",
        state.path().to_str().unwrap(),
        "--model-dir",
        "/nonexistent/lore-model",
    ];
    let one = Command::new(env!("CARGO_BIN_EXE_lore"))
        .args(base)
        .args([
            "diagnose",
            "payment",
            "--compact",
            "--mode",
            "keyword",
            "--max-tokens",
            "1000",
        ])
        .output()
        .unwrap();
    assert!(
        one.status.success(),
        "{}",
        String::from_utf8_lossy(&one.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&one.stdout).unwrap();
    assert!(result.get("candidates").is_none() && result.get("brief").is_none());
    assert_eq!(result["answer_completeness"], "not_assessed");
    assert!(estimate_tokens(std::str::from_utf8(&one.stdout).unwrap().trim_end()) <= 1000);
    let mut batch = Command::new(env!("CARGO_BIN_EXE_lore"))
        .args(base)
        .args([
            "batch",
            "--diagnostics",
            "--compact",
            "--mode",
            "keyword",
            "--max-tokens",
            "1000",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write!(
        batch.stdin.take().unwrap(),
        "\"payment\"\n{{}}\n\"gardening\"\n"
    )
    .unwrap();
    let out = batch.wait_with_output().unwrap();
    assert!(!out.status.success());
    let rows: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], result);
    assert!(rows[1]["error"].is_string());
    assert_eq!(rows[2]["answer_completeness"], "not_assessed");
    let invalid = Command::new(env!("CARGO_BIN_EXE_lore"))
        .args(base)
        .args(["batch", "--compact", "--mode", "keyword"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("--diagnostics"));
}

#[test]
fn compact_consolidation_covers_known_gaps_and_keeps_uncovered_siblings() {
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(corpus.path().join("route.md"), "# Guide\n\nShared setup.\n\n## Input\n\nuniqueroute declaration.\n\n### First\n\nuniqueroute detail.\n\n### Second\n\nuniqueroute option.\n\n## Separate\n\nuniqueroute separate.\n\nAn omitted separate restriction.\n").unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let mut diagnostic = index
        .diagnose(
            "uniqueroute",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "route.md".into(),
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    diagnostic.brief.files[0]
        .passages
        .sort_by_key(|p| p.bytes[0]);
    let compact = index.compact_diagnostics(&diagnostic, 2000).unwrap();
    assert_eq!(compact.consolidated_suggestions, 2);
    let first = &compact.next_reads[0];
    assert_eq!(first.read_target, "route.md#input");
    assert_eq!(
        first.covers_gaps_from,
        ["route.md#first", "route.md#second"]
    );
    assert!(
        compact
            .next_reads
            .iter()
            .any(|r| r.read_target == "route.md#separate")
    );
    assert_eq!(compact.omitted_suggestions, 0);
    let text: String = index
        .read_context(&first.read_target, &Default::default())
        .unwrap()
        .parts
        .iter()
        .map(|p| p.content.text.as_str())
        .collect();
    assert!(text.contains("Shared setup.") && text.contains("uniqueroute option."));
    assert!(!text.contains("omitted separate restriction"));
}

#[test]
fn compact_parent_alternative_uses_query_siblings_and_exact_read_size() {
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(corpus.path().join("parent.md"), "# Guide\n\nGlobal setup.\n\n## Output\n\nShared output setup.\n\n### First\n\nuniqueparent first.\n\n### Second\n\nuniqueparent second 検証🐦.\n\n### Third\n\nuniqueparent third.\n\n## Unrelated\n\nSibling beyond the proposed parent.\n").unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let mut diagnostic = index
        .diagnose(
            "uniqueparent",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "parent.md".into(),
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    diagnostic.brief.files[0]
        .passages
        .retain(|p| p.anchor == "first");
    let compact = index.compact_diagnostics(&diagnostic, 2000).unwrap();
    let next = &compact.next_reads[0];
    assert_eq!(next.read_target, "parent.md#first");
    let broader = next.broader_read.as_ref().unwrap();
    assert_eq!(broader.read_target, "parent.md#output");
    assert!(broader.context && broader.matching_sections.len() >= 2);
    let read = index
        .read_context(&broader.read_target, &Default::default())
        .unwrap();
    let focused = index
        .read_context(&next.read_target, &Default::default())
        .unwrap();
    let bytes: usize = read.parts.iter().map(|p| p.content.text.len()).sum();
    assert_eq!(broader.source_bytes, bytes);
    assert_eq!(
        broader.additional_source_bytes,
        bytes
            - focused
                .parts
                .iter()
                .map(|p| p.content.text.len())
                .sum::<usize>()
    );
    assert!(
        read.parts
            .iter()
            .all(|p| !p.content.text.contains("Sibling beyond"))
    );
    for c in &mut diagnostic.candidates {
        for h in &mut c.hits {
            h.view = "identity".into();
        }
    }
    let heading_only = index.compact_diagnostics(&diagnostic, 2000).unwrap();
    assert!(heading_only.next_reads[0].broader_read.is_none());
}

#[test]
fn compact_parent_alternatives_respect_size_and_top_level_bounds() {
    let (corpus, _state, mut index, mut model) = fixture();
    for (first_bytes, second_bytes, wrapper) in [
        (0, 5000, "## Output\n\n"),
        (4500, 2000, "## Output\n\n"),
        (0, 0, ""),
    ] {
        fs::write(corpus.path().join("bounded.md"), format!("# Guide\n\nGlobal setup.\n\n{wrapper}### First\n\nuniquebounded first.\n\n{}\n\n### Second\n\nuniquebounded second.\n\n{}\n", "a".repeat(first_bytes), "b".repeat(second_bytes))).unwrap();
        index.index(corpus.path(), &mut model).unwrap();
        let mut diagnostic = index
            .diagnose(
                "uniquebounded",
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    scope: "bounded.md".into(),
                    connections: false,
                    max_tokens: 10000,
                    ..Default::default()
                },
            )
            .unwrap();
        diagnostic.brief.files[0]
            .passages
            .retain(|p| p.anchor == "first");
        let compact = index.compact_diagnostics(&diagnostic, 2000).unwrap();
        assert!(!compact.next_reads.is_empty());
        assert!(compact.next_reads.iter().all(|r| r.broader_read.is_none()));
    }
}

#[test]
fn compact_consolidation_does_not_drop_a_read_covered_only_by_an_unpacked_target() {
    let (corpus, _state, mut index, mut model) = fixture();
    fs::write(corpus.path().join("fallback.md"), format!("# Guide\n\nShared setup.\n\n## {}\n\nuniquefallback parent.\n\n### Child\n\nuniquefallback child.\n", "x".repeat(4000))).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    let mut diagnostic = index
        .diagnose(
            "uniquefallback",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "fallback.md".into(),
                connections: false,
                max_tokens: 10000,
                ..Default::default()
            },
        )
        .unwrap();
    diagnostic.brief.files[0]
        .passages
        .sort_by_key(|p| p.bytes[0]);
    let compact = index.compact_diagnostics(&diagnostic, 1000).unwrap();
    assert_eq!(compact.next_reads.len(), 1);
    assert_eq!(compact.next_reads[0].read_target, "fallback.md#child");
    assert_eq!(compact.consolidated_suggestions, 0);
    assert_eq!(compact.omitted_suggestions, 1);
    assert!(estimate_tokens(&serde_json::to_string(&compact).unwrap()) <= 1000);
}

#[test]
fn guided_brief_uses_one_query_and_keeps_citations_and_shared_budget() {
    let (corpus, _state, mut index, mut model) = fixture();
    let source = "# Guide\n\nGlobal setup 検証🐦.\n\n## Copy\n\nuniquefocus uses prefix then [.\n\nSelect with v.\n\nCopy with y.\n";
    fs::write(corpus.path().join("guide.md"), source).unwrap();
    index.index(corpus.path(), &mut model).unwrap();
    for budget in [800, 1000, 2000, 10000] {
        let before = model.calls;
        let result = index
            .guided_brief(
                "uniquefocus",
                Some(&mut model),
                &BriefOptions {
                    max_tokens: budget,
                    scope: "guide.md".into(),
                    connections: false,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(model.calls, before + 1, "guidance must reuse the retrieval");
        assert!(result.estimated_tokens <= budget);
        assert_eq!(
            result.estimated_tokens,
            estimate_tokens(&serde_json::to_string(&result).unwrap())
        );
        assert!(result.guidance.next_reads.len() <= 3);
        assert_eq!(result.guidance.answer_completeness, "not_assessed");
        for file in &result.brief.files {
            assert_eq!(file.source_hash, lore::digest(source.as_bytes()));
            for p in &file.passages {
                assert_eq!(p.text, source[p.bytes[0]..p.bytes[1]]);
            }
        }
    }
    // A lexical query selects just the matching body. Guidance restores both
    // omitted instructions and the introduction without another model call.
    let result = index
        .guided_brief(
            "uniquefocus",
            None,
            &BriefOptions {
                mode: Mode::Keyword,
                scope: "guide.md".into(),
                connections: false,
                ..Default::default()
            },
        )
        .unwrap();
    let hint = &result.guidance.next_reads[0];
    assert_eq!(hint.read_target, "guide.md#copy");
    assert!(hint.context);
    assert_eq!(hint.gaps.section_blocks, 2);
    assert_eq!(hint.gaps.ancestor_blocks, 1);
    let read = index
        .read_context(&hint.read_target, &Default::default())
        .unwrap();
    assert!(
        read.parts
            .iter()
            .any(|p| p.content.text.contains("Copy with y."))
    );
    fs::write(corpus.path().join("guide.md"), "changed").unwrap();
    assert!(
        index
            .guided_brief(
                "uniquefocus",
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
}

#[test]
fn guided_brief_handles_empty_results_and_small_or_metadata_heavy_budgets() {
    let (_corpus, _state, index, _model) = fixture();
    let options = BriefOptions {
        mode: Mode::Keyword,
        ..Default::default()
    };
    let empty = index
        .guided_brief("absentuniqueterm", None, &options)
        .unwrap();
    assert_eq!(empty.brief.status, "no_matches");
    assert!(empty.guidance.next_reads.is_empty());
    assert_eq!(empty.guidance.answer_completeness, "not_assessed");
    assert!(
        index
            .guided_brief(
                "payment",
                None,
                &BriefOptions {
                    max_tokens: 128,
                    ..options.clone()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("too small")
    );
    assert!(
        index
            .guided_brief(&"payment ".repeat(700), None, &options)
            .unwrap_err()
            .to_string()
            .contains("metadata")
    );
    // A large literal path/scope must not silently escape the shared budget.
    let scoped = index
        .guided_brief(
            "payment",
            None,
            &BriefOptions {
                scope: "x".repeat(2000),
                ..options
            },
        )
        .unwrap_err();
    assert!(scoped.to_string().contains("metadata"));
}

#[test]
fn explicit_preferences_keep_api_declarations_atomic_in_plain_and_guided_briefs() {
    use lore::preferences::Qualifier;
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::create_dir(corpus.path().join("python")).unwrap();
    fs::write(corpus.path().join("python/api.md"), "# Input\n\n## Deliver input\n\n`deliver(text, submit=True)` writes text followed by Enter when requested.\n\n### Leave input pending: `submit=False`\n\nLeaves input pending without Enter.\n\n### Send a key\n\n`press(key)` sends one key.\n\n| Key form | Value |\n| --- | --- |\n| Simple | `enter` |\n").unwrap();
    let mut index = Index::open(state.path()).unwrap();
    let mut model = TestEmbedder::new();
    index.index(corpus.path(), &mut model).unwrap();
    let mut saw_option = false;
    let mut saw_table = false;
    for preference in [
        Qualifier::Python,
        Qualifier::Rust,
        Qualifier::Javascript,
        Qualifier::Typescript,
        Qualifier::Windows,
        Qualifier::Linux,
        Qualifier::Macos,
    ] {
        for budget in [800, 1200, 2000] {
            let options = BriefOptions {
                prefer: vec![preference],
                max_tokens: budget,
                connections: false,
                ..Default::default()
            };
            let plain = index
                .diagnose("input pending enter key", Some(&mut model), &options)
                .unwrap();
            let guided = index
                .guided_brief("input pending enter key", Some(&mut model), &options)
                .unwrap();
            assert!(plain.api_context.enabled && guided.guidance.api_context.enabled);
            assert!(guided.estimated_tokens <= budget);
            for brief in [&plain.brief, &guided.brief] {
                let texts: Vec<_> = brief
                    .files
                    .iter()
                    .flat_map(|f| f.passages.iter().map(|p| p.text.as_str()))
                    .collect();
                if texts.iter().any(|t| t.contains("Leaves input pending")) {
                    saw_option = true;
                    assert!(
                        texts
                            .iter()
                            .any(|t| t.contains("`deliver(text, submit=True)`"))
                    );
                }
                if texts.iter().any(|t| t.contains("| Simple |")) {
                    saw_table = true;
                    assert!(texts.iter().any(|t| t.contains("`press(key)`")));
                }
            }
        }
    }
    assert!(saw_option && saw_table);
}

#[test]
fn explicit_source_roles_keep_api_declarations_atomic_in_plain_and_guided_briefs() {
    use lore::{preferences::Qualifier, search::RoleRule};
    let corpus = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::create_dir(corpus.path().join("python")).unwrap();
    fs::write(corpus.path().join("python/api.md"), "# Input\n\n## Deliver input\n\n`deliver(text, submit=True)` writes text followed by Enter when requested.\n\n### Leave input pending: `submit=False`\n\nLeaves input pending without Enter.\n\n### Send a key\n\n`press(key)` sends one key.\n\n| Key form | Value |\n| --- | --- |\n| Simple | `enter` |\n").unwrap();
    fs::write(
        corpus.path().join("review.md"),
        "# Review\n\nInput delivery and pending Enter key behavior were reviewed.\n",
    )
    .unwrap();
    let mut index = Index::open(state.path()).unwrap();
    let mut model = TestEmbedder::new();
    index.index(corpus.path(), &mut model).unwrap();
    for weight in [0.1, 0.5, 1.0, 2.0] {
        let mut saw_option = false;
        let mut saw_table = false;
        for budget in [800, 1200, 2000] {
            let options = BriefOptions {
                roles: vec![
                    RoleRule {
                        prefix: "python".into(),
                        role: "consumer".into(),
                        weight,
                    },
                    RoleRule {
                        prefix: "review.md".into(),
                        role: "review".into(),
                        weight: 0.1,
                    },
                ],
                prefer: vec![Qualifier::Python],
                max_tokens: budget,
                connections: false,
                ..Default::default()
            };
            let plain = index
                .diagnose("input pending enter key", Some(&mut model), &options)
                .unwrap();
            let guided = index
                .guided_brief("input pending enter key", Some(&mut model), &options)
                .unwrap();
            assert!(plain.api_context.enabled && guided.guidance.api_context.enabled);
            assert!(plain.api_context.disabled_by.is_empty());
            assert!(estimate_tokens(&serde_json::to_string(&guided).unwrap()) <= budget);
            for brief in [&plain.brief, &guided.brief] {
                assert!(estimate_tokens(&serde_json::to_string(brief).unwrap()) <= budget);
                let texts: Vec<_> = brief
                    .files
                    .iter()
                    .filter(|f| f.path == "python/api.md")
                    .flat_map(|f| {
                        assert_eq!(f.role.as_deref(), Some("consumer"));
                        f.passages.iter().map(|p| p.text.as_str())
                    })
                    .collect();
                if texts.iter().any(|t| t.contains("Leaves input pending")) {
                    saw_option = true;
                    assert!(
                        texts
                            .iter()
                            .any(|t| t.contains("`deliver(text, submit=True)`"))
                    );
                }
                if texts.iter().any(|t| t.contains("| Simple |")) {
                    saw_table = true;
                    assert!(texts.iter().any(|t| t.contains("`press(key)`")));
                }
            }
            let disabled = index
                .diagnose(
                    "input pending enter key",
                    Some(&mut model),
                    &BriefOptions {
                        api_context: false,
                        ..options.clone()
                    },
                )
                .unwrap();
            assert_eq!(disabled.api_context.disabled_by, ["api_context"]);
            let exact = index
                .diagnose(
                    "python/api.md#leave-input-pending-submitfalse",
                    Some(&mut model),
                    &options,
                )
                .unwrap();
            assert_eq!(exact.brief.files[0].path, "python/api.md");
            assert_eq!(exact.api_context.disabled_by, ["exact_lookup"]);
        }
        assert!(
            saw_option && saw_table,
            "No API details returned at weight {weight}"
        );
    }
}

#[test]
fn guided_cli_and_batch_preserve_errors_and_reject_conflicting_modes() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let (_corpus, state, _index, _model) = fixture();
    let base = [
        "--index-dir",
        state.path().to_str().unwrap(),
        "--model-dir",
        "/nonexistent/lore-model",
    ];
    let single = Command::new(env!("CARGO_BIN_EXE_lore"))
        .args(base)
        .args([
            "brief",
            "payment",
            "--with-guidance",
            "--mode",
            "keyword",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        single.status.success(),
        "{}",
        String::from_utf8_lossy(&single.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&single.stdout).unwrap();
    assert!(value["brief"]["files"].is_array() && value["guidance"]["next_reads"].is_array());
    let mut batch = Command::new(env!("CARGO_BIN_EXE_lore"))
        .args(base)
        .args(["batch", "--with-guidance", "--mode", "keyword"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write!(
        batch.stdin.take().unwrap(),
        "\"payment\"\n{{}}\n\"absentuniqueterm\"\n"
    )
    .unwrap();
    let out = batch.wait_with_output().unwrap();
    assert!(!out.status.success());
    let rows: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["brief"]["files"], value["brief"]["files"]);
    assert_eq!(rows[0]["guidance"], value["guidance"]);
    assert!(rows[1]["error"].is_string());
    assert_eq!(rows[2]["brief"]["status"], "no_matches");
    for flags in [
        vec!["--diagnostics"],
        vec!["--diagnostics", "--compact"],
        vec!["--compact"],
    ] {
        let invalid = Command::new(env!("CARGO_BIN_EXE_lore"))
            .args(base)
            .args(["batch", "--with-guidance"])
            .args(flags)
            .output()
            .unwrap();
        assert!(!invalid.status.success());
    }
}
