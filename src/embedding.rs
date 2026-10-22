use anyhow::{Context, Result, ensure};
use fastembed::{
    InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use std::{fs, io::Read, path::Path, time::Duration};

pub const DIMENSIONS: usize = 384;
pub const REVISION: &str = "aa8f8b060edb00e03bfdd08813a2949946c8ba55";
pub const MODEL_ID: &str = "bge-small-en-v1.5-Q/aa8f8b0/cls-normalized/query-instruction/v1";
const FILES: &[&str] = &[
    "model_optimized.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];
const MODEL_SHA: &str = "51f1bd0addd6e859e42c2c8021a5e5461385bb676a649f4b269aa445449f2431";

const ASSET_HASHES: &[&str] = &[
    MODEL_SHA,
    "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    "13582bcf2effc85b7bf3d3f5532e686bc1c9ce86bb009d10f0ec33cbe92299dd",
    "5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a",
    "0b29c7bfc889e53b36d9dd3e686dd4300f6525110eaa98c76a5dafceb2029f53",
];

pub trait Embedder {
    fn identity(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn token_count(&self, text: &str) -> Result<usize>;
    fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>>;
}

pub struct LocalEmbedder {
    model: TextEmbedding,
    identity: String,
}

impl LocalEmbedder {
    pub fn load(directory: &Path, threads: usize) -> Result<Self> {
        ensure!(threads > 0, "threads must be positive");
        let read = |name: &str| -> Result<Vec<u8>> {
            let bytes = fs::read(directory.join(name)).with_context(|| {
                format!(
                    "Missing model asset {}. Run `lore model install` or supply --model-dir.",
                    directory.join(name).display()
                )
            })?;
            let position = FILES.iter().position(|f| *f == name).expect("known asset");
            ensure!(
                crate::digest(&bytes) == ASSET_HASHES[position],
                "Checksum mismatch for {name}; run `lore model install` to repair assets"
            );
            Ok(bytes)
        };
        let onnx = read(FILES[0])?;
        ensure!(
            crate::digest(&onnx) == MODEL_SHA,
            "Model checksum mismatch; expected the pinned BGE-small model"
        );
        let tokenizer_file = read(FILES[1])?;
        let config_file = read(FILES[2])?;
        let special_tokens_map_file = read(FILES[3])?;
        let tokenizer_config_file = read(FILES[4])?;
        let mut hashes = String::from(MODEL_ID);
        for bytes in [
            &tokenizer_file,
            &config_file,
            &special_tokens_map_file,
            &tokenizer_config_file,
        ] {
            hashes.push_str(&crate::digest(bytes));
        }
        let assets = TokenizerFiles {
            tokenizer_file,
            config_file,
            special_tokens_map_file,
            tokenizer_config_file,
        };
        let model = TextEmbedding::try_new_from_user_defined(
            UserDefinedEmbeddingModel::new(onnx, assets)
                .with_pooling(Pooling::Cls)
                .with_quantization(QuantizationMode::Static),
            InitOptionsUserDefined::new()
                .with_max_length(512)
                .with_intra_threads(threads),
        )?;
        Ok(Self {
            model,
            identity: crate::digest(hashes.as_bytes()),
        })
    }
}

impl Embedder for LocalEmbedder {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn dimensions(&self) -> usize {
        DIMENSIONS
    }
    fn token_count(&self, text: &str) -> Result<usize> {
        let mut tokenizer = self.model.tokenizer.clone();
        tokenizer
            .with_truncation(None)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        tokenizer.with_padding(None);
        Ok(tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .len())
    }
    fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        for text in texts {
            ensure!(
                self.token_count(text)? <= 512,
                "Passage exceeds embedding input limit"
            );
        }
        Ok(self.model.embed(texts, Some(16))?)
    }
    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        let text = format!("Represent this sentence for searching relevant passages: {query}");
        ensure!(
            self.token_count(&text)? <= 512,
            "Query exceeds the model's 512-token limit"
        );
        Ok(self.model.embed([text], Some(1))?.remove(0))
    }
}

/// The only network operation in Lore. Called solely by `model install`.
pub fn install(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .into();
    for (name, expected_hash) in FILES.iter().zip(ASSET_HASHES) {
        let destination = directory.join(name);
        if destination.is_file() {
            let bytes = fs::read(&destination)?;
            if crate::digest(&bytes) == *expected_hash {
                continue;
            }
        }
        eprintln!("Downloading {name} at pinned revision {REVISION}...");
        let url = format!(
            "https://huggingface.co/Qdrant/bge-small-en-v1.5-onnx-Q/resolve/{REVISION}/{name}"
        );
        let mut bytes = Vec::new();
        agent
            .get(&url)
            .call()?
            .body_mut()
            .as_reader()
            .take(80_000_001)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 80_000_000, "Model asset exceeds size limit");
        ensure!(
            crate::digest(&bytes) == *expected_hash,
            "Downloaded {name} checksum mismatch"
        );
        let temporary = destination.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, &bytes)?;
        fs::rename(temporary, destination)?;
    }
    eprintln!(
        "Model ready at {}. Indexing and querying now run offline.",
        directory.display()
    );
    Ok(())
}
