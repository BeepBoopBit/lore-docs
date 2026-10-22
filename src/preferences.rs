//! Small, explicit preferences. They never filter the candidate pool.
use crate::{index::Index, retrieval::Candidate};
use anyhow::Result;
use clap::ValueEnum;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum QualifierMode {
    Auto,
    Off,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Qualifier {
    Python,
    Rust,
    Javascript,
    Typescript,
    Windows,
    Linux,
    Macos,
}

impl Qualifier {
    fn dimension(self) -> usize {
        match self {
            Self::Windows | Self::Linux | Self::Macos => 1,
            _ => 0,
        }
    }
    fn token(token: &str) -> Option<Self> {
        match token {
            "python" => Some(Self::Python),
            "rust" => Some(Self::Rust),
            "javascript" => Some(Self::Javascript),
            "typescript" => Some(Self::Typescript),
            "windows" => Some(Self::Windows),
            "linux" => Some(Self::Linux),
            "macos" => Some(Self::Macos),
            _ => None,
        }
    }
    fn fence(token: &str) -> Option<Self> {
        match token {
            "py" => Some(Self::Python),
            "rs" => Some(Self::Rust),
            "js" => Some(Self::Javascript),
            "ts" => Some(Self::Typescript),
            _ => Self::token(token),
        }
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub(crate) fn requested(
    query: &str,
    mode: QualifierMode,
    explicit: &[Qualifier],
) -> Vec<Qualifier> {
    if !explicit.is_empty() {
        return explicit
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
    }
    if mode == QualifierMode::Off {
        return Vec::new();
    }
    let terms = words(query);
    let mut found = BTreeSet::new();
    for (i, term) in terms.iter().enumerate() {
        let qualifier = Qualifier::token(term).or_else(|| {
            (term == "mac" && terms.get(i + 1).is_some_and(|t| t == "os"))
                .then_some(Qualifier::Macos)
        });
        if let Some(q) = qualifier {
            let before = i.checked_sub(1).map(|j| terms[j].as_str()).unwrap_or("");
            let two_before = i.checked_sub(2).map(|j| terms[j].as_str()).unwrap_or("");
            let negated = matches!(before, "not" | "without" | "except" | "excluding" | "non")
                || (two_before == "not" && matches!(before, "in" | "using" | "for"));
            if !negated {
                found.insert(q);
            }
        }
    }
    found.into_iter().collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct QualifierMatch {
    pub qualifier: Qualifier,
    /// Exact structural fields that supplied the label, not an inferred answer.
    pub via: Vec<String>,
}

pub(crate) fn unboosted(value: &f64) -> bool {
    *value == 1.0
}

impl Index {
    pub(crate) fn apply_preferences(
        &self,
        candidates: &mut [Candidate],
        requested: &[Qualifier],
    ) -> Result<()> {
        if requested.is_empty() {
            return Ok(());
        }
        let dimensions: BTreeSet<_> = requested.iter().map(|q| q.dimension()).collect();
        let mut codes = self
            .db
            .prepare("SELECT text FROM passages WHERE section_id=? AND kind='code'")?;
        for c in candidates {
            if c.exact {
                continue;
            }
            let path = words(&c.path);
            let heading = words(&c.heading);
            let mut fence_labels = BTreeSet::new();
            for text in codes.query_map([c.section_id], |r| r.get::<_, String>(0))? {
                let text = text?;
                let first = text.lines().next().unwrap_or("").trim_start();
                if first.starts_with("```") || first.starts_with("~~~") {
                    let label = first
                        .trim_start_matches(['`', '~'])
                        .split([',', ' ', '\t'])
                        .next()
                        .unwrap_or("")
                        .to_lowercase();
                    if let Some(q) = Qualifier::fence(&label) {
                        fence_labels.insert(q);
                    }
                }
            }
            for &q in requested {
                let mut via = Vec::new();
                if path
                    .iter()
                    .filter_map(|t| Qualifier::token(t))
                    .any(|v| v == q)
                {
                    via.push("path".into());
                }
                if heading
                    .iter()
                    .filter_map(|t| Qualifier::token(t))
                    .any(|v| v == q)
                {
                    via.push("heading".into());
                }
                if fence_labels.contains(&q) {
                    via.push("code_fence".into());
                }
                if !via.is_empty() {
                    c.qualifier_matches
                        .push(QualifierMatch { qualifier: q, via });
                }
            }
            let matched: BTreeSet<_> = c
                .qualifier_matches
                .iter()
                .map(|m| m.qualifier.dimension())
                .collect();
            c.qualifier_boost = 1.0 + 0.35 * matched.len() as f64 / dimensions.len() as f64;
        }
        Ok(())
    }
}
