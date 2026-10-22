//! Local Markdown retrieval. Indexes and queries never use a network service.
pub mod api_context;
mod assembly;
pub mod briefing;
mod content;
pub mod context;
pub mod embedding;
pub mod evaluate;
pub mod guidance;
pub mod index;
mod leads;
pub mod markdown;
pub mod mcp;
pub mod onboarding;
pub mod preferences;
pub mod procedure;
mod project;
pub mod reading;
pub mod recommendation;
mod recovery;
pub mod reranking;
pub mod retrieval;
pub mod search;
pub mod selection;

use sha2::{Digest, Sha256};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A portable little-endian SQLite vector blob.
pub fn vector_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn vector_from_blob(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| x * y)
        .sum::<f32>()
        .clamp(-1.0, 1.0)
}
