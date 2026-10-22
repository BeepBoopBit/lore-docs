//! Optional local query/passage scoring. No text generation or runtime downloads.
use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use fastembed::{
    RerankInitOptionsUserDefined, TextRerank, TokenizerFiles, UserDefinedRerankingModel,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

pub const REVISION: &str = "233902d25c440f23af6f7d6e94d2946bac0bee0a";
pub const MODEL_ID: &str = "ms-marco-MiniLM-L6-v2/quint8-avx2/233902d/pair512/v1";
pub(crate) const LIMIT: usize = 64;
const ASSETS: &[(&str, &str, &str)] = &[
    (
        "model_quint8_avx2.onnx",
        "onnx/model_quint8_avx2.onnx",
        "c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9",
    ),
    (
        "tokenizer.json",
        "tokenizer.json",
        "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    ),
    (
        "config.json",
        "config.json",
        "380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc",
    ),
    (
        "special_tokens_map.json",
        "special_tokens_map.json",
        "3c3507f36dff57bce437223db3b3081d1e2b52ec3e56ee55438193ecb2c94dd6",
    ),
    (
        "tokenizer_config.json",
        "tokenizer_config.json",
        "a5c2e5a7b1a29a0702cd28c08a399b5ecc110c263009d17f7e3b415f25905fd8",
    ),
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum RerankMode {
    #[default]
    Off,
    /// Same bounded pool as local scoring, retaining original heuristic quality.
    Shortlist,
    Local,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairScore {
    pub logit: f32,
    pub input_truncated: bool,
}

pub trait Reranker {
    fn identity(&self) -> &str;
    /// One finite score per passage, in input order.
    fn score(&mut self, query: &str, passages: &[String]) -> Result<Vec<PairScore>>;
}

pub struct LocalReranker {
    model: TextRerank,
}
impl LocalReranker {
    pub fn load(directory: &Path, threads: usize) -> Result<Self> {
        ensure!(threads > 0, "threads must be positive");
        let read = |i: usize| -> Result<Vec<u8>> {
            let (name, _, hash) = ASSETS[i];
            let bytes = fs::read(directory.join(name)).with_context(|| format!(
                "Missing reranker asset {name}; run `lore model install --reranker` or supply --reranker-dir"))?;
            ensure!(
                crate::digest(&bytes) == hash,
                "Reranker checksum mismatch for {name}; run `lore model install --reranker` to repair assets"
            );
            Ok(bytes)
        };
        let model = TextRerank::try_new_from_user_defined(
            UserDefinedRerankingModel::new(
                read(0)?,
                TokenizerFiles {
                    tokenizer_file: read(1)?,
                    config_file: read(2)?,
                    special_tokens_map_file: read(3)?,
                    tokenizer_config_file: read(4)?,
                },
            ),
            RerankInitOptionsUserDefined::new()
                .with_max_length(512)
                .with_intra_threads(threads),
        )?;
        Ok(Self { model })
    }
}
impl Reranker for LocalReranker {
    fn identity(&self) -> &str {
        MODEL_ID
    }
    fn score(&mut self, query: &str, passages: &[String]) -> Result<Vec<PairScore>> {
        ensure!(
            passages.len() <= LIMIT,
            "Reranker batch exceeds 64 passages"
        );
        let mut tokenizer = self.model.tokenizer.clone();
        tokenizer
            .with_truncation(None)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        tokenizer.with_padding(None);
        let encode_error =
            |e: Box<dyn std::error::Error + Send + Sync>| anyhow::anyhow!(e.to_string());
        ensure!(
            tokenizer.encode(query, true).map_err(encode_error)?.len() <= 128,
            "Reranker query exceeds 128 tokens; shorten the query or use --rerank off"
        );
        if passages.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = Vec::with_capacity(passages.len());
        for p in passages {
            // With the query <=128 tokens, longest-first pair truncation retains the query.
            let size = tokenizer
                .encode((query, p.as_str()), true)
                .map_err(encode_error)?
                .len();
            result.push(PairScore {
                logit: f32::NAN,
                input_truncated: size > 512,
            });
        }
        let docs: Vec<_> = passages.iter().map(String::as_str).collect();
        for row in self.model.rerank(query, docs, false, Some(8))? {
            ensure!(row.index < result.len(), "Invalid reranker result index");
            ensure!(
                result[row.index].logit.is_nan(),
                "Duplicate reranker result index"
            );
            result[row.index].logit = row.score;
        }
        ensure!(
            result.iter().all(|s| s.logit.is_finite()),
            "Missing or nonfinite reranker result"
        );
        Ok(result)
    }
}

#[derive(Debug, Serialize)]
pub struct ScoredPassage {
    pub passage_id: i64,
    pub logit: f32,
    pub input_truncated: bool,
}
#[derive(Debug, Serialize)]
pub struct RerankReport {
    pub mode: RerankMode,
    pub model: Option<String>,
    pub shortlist_limit: usize,
    pub shortlist: Vec<i64>,
    pub scores: Vec<ScoredPassage>,
    pub scoring_ms: f64,
    pub bypassed_for_exact: bool,
}

pub(crate) struct RerankSession<'a> {
    pub engine: Option<&'a mut dyn Reranker>,
    pub cache: BTreeMap<i64, PairScore>,
    pub shortlist: Vec<i64>,
    pub scoring_ms: f64,
    pub bypassed: bool,
}
impl<'a> RerankSession<'a> {
    pub fn new(engine: Option<&'a mut dyn Reranker>) -> Self {
        Self {
            engine,
            cache: BTreeMap::new(),
            shortlist: Vec::new(),
            scoring_ms: 0.0,
            bypassed: false,
        }
    }
    pub fn score(&mut self, query: &str, inputs: &[(i64, String)]) -> Result<()> {
        let missing: Vec<_> = inputs
            .iter()
            .filter(|(id, _)| !self.cache.contains_key(id))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        ensure!(
            self.cache.len() + missing.len() <= LIMIT,
            "Reranker query work limit exceeded"
        );
        let model = self.engine.as_mut().context(
            "Local reranking requires a provisioned reranker; use the reranker-aware library API",
        )?;
        let texts: Vec<_> = missing.iter().map(|(_, text)| text.clone()).collect();
        let started = Instant::now();
        let scores = model.score(query, &texts)?;
        self.scoring_ms += started.elapsed().as_secs_f64() * 1000.0;
        ensure!(
            scores.len() == missing.len() && scores.iter().all(|s| s.logit.is_finite()),
            "Reranker returned missing or nonfinite scores"
        );
        for ((id, _), score) in missing.into_iter().zip(scores) {
            self.cache.insert(*id, score);
        }
        Ok(())
    }
    pub fn report(&self, mode: RerankMode) -> Option<RerankReport> {
        (mode != RerankMode::Off).then(|| RerankReport {
            mode,
            model: self.engine.as_ref().map(|e| e.identity().into()),
            shortlist_limit: LIMIT,
            shortlist: self.shortlist.clone(),
            scoring_ms: self.scoring_ms,
            bypassed_for_exact: self.bypassed,
            scores: self
                .cache
                .iter()
                .map(|(&id, score)| ScoredPassage {
                    passage_id: id,
                    logit: score.logit,
                    input_truncated: score.input_truncated,
                })
                .collect(),
        })
    }
}

/// Explicit provisioning only; load/score never use the network.
pub fn install(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .into();
    for (name, remote, hash) in ASSETS {
        let dest = directory.join(name);
        if dest.is_file() && crate::digest(&fs::read(&dest)?) == *hash {
            continue;
        }
        eprintln!("Downloading reranker {name} at pinned revision {REVISION}...");
        let url = format!(
            "https://huggingface.co/cross-encoder/ms-marco-MiniLM-L6-v2/resolve/{REVISION}/{remote}"
        );
        let mut bytes = Vec::new();
        agent
            .get(&url)
            .call()?
            .body_mut()
            .as_reader()
            .take(30_000_001)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 30_000_000,
            "Reranker asset exceeds size limit"
        );
        ensure!(
            crate::digest(&bytes) == *hash,
            "Downloaded reranker {name} checksum mismatch"
        );
        let temp = dest.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temp, bytes)?;
        fs::rename(temp, dest)?;
    }
    eprintln!(
        "Reranker ready at {}. Scoring runs offline.",
        directory.display()
    );
    Ok(())
}
