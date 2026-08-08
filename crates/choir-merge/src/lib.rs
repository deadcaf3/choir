//! Merge-strategy pipeline seam (plan.md §E seam 2, D4/D19).
//!
//! Ordered strategies, cheapest first; each maps (base, left, right) to
//! Resolved or Conflict. LLM resolution and Mergiraf are just strategy slots,
//! so widening or dropping them is configuration, not surgery. Mergiraf is
//! GPLv3 (audited 2026-08-08): subprocess only, never linked (D4).

pub enum MergeOutcome {
    Resolved(String),
    /// Strategy ran but could not resolve; carry the conflict downstream
    /// (jj-style first-class conflict is the pipeline's terminal fallback).
    Conflict { annotated: String },
    /// Strategy not applicable in this environment (e.g. binary missing).
    Unavailable(String),
}

pub trait MergeStrategy: Send + Sync {
    fn name(&self) -> &'static str;
    fn merge(&self, base: &str, left: &str, right: &str) -> MergeOutcome;
}

/// Runs strategies in order; first Resolved wins. If none resolves, returns
/// the last Conflict (first-class conflict, never a silent pick).
pub struct Pipeline {
    strategies: Vec<Box<dyn MergeStrategy>>,
}

pub struct PipelineResult {
    pub outcome: MergeOutcome,
    /// Which strategy produced the outcome (provenance for review escalation).
    pub strategy: &'static str,
}

impl Pipeline {
    pub fn new(strategies: Vec<Box<dyn MergeStrategy>>) -> Self {
        Self { strategies }
    }

    /// The v1 default: trivial, then line-based. Structured (Mergiraf) and
    /// LLM slots are appended by the caller when enabled.
    pub fn default_v1() -> Self {
        Self::new(vec![Box::new(TrivialMerge), Box::new(LineMerge)])
    }

    pub fn merge(&self, base: &str, left: &str, right: &str) -> PipelineResult {
        let mut last_conflict: Option<PipelineResult> = None;
        for s in &self.strategies {
            match s.merge(base, left, right) {
                MergeOutcome::Resolved(text) => {
                    return PipelineResult {
                        outcome: MergeOutcome::Resolved(text),
                        strategy: s.name(),
                    }
                }
                c @ MergeOutcome::Conflict { .. } => {
                    last_conflict = Some(PipelineResult {
                        outcome: c,
                        strategy: s.name(),
                    })
                }
                MergeOutcome::Unavailable(_) => continue,
            }
        }
        last_conflict.expect("pipeline contains at least one applicable strategy")
    }
}

/// Cheapest checks: unchanged sides and identical edits.
pub struct TrivialMerge;

impl MergeStrategy for TrivialMerge {
    fn name(&self) -> &'static str {
        "trivial"
    }
    fn merge(&self, base: &str, left: &str, right: &str) -> MergeOutcome {
        if left == right {
            return MergeOutcome::Resolved(left.to_string());
        }
        if left == base {
            return MergeOutcome::Resolved(right.to_string());
        }
        if right == base {
            return MergeOutcome::Resolved(left.to_string());
        }
        MergeOutcome::Conflict {
            annotated: format!("<<<<<<< left\n{left}=======\n{right}>>>>>>> right\n"),
        }
    }
}

/// Line-based 3-way merge (diffy), the histogram/ORT analog in the plan.
pub struct LineMerge;

impl MergeStrategy for LineMerge {
    fn name(&self) -> &'static str {
        "line"
    }
    fn merge(&self, base: &str, left: &str, right: &str) -> MergeOutcome {
        match diffy::merge(base, left, right) {
            Ok(clean) => MergeOutcome::Resolved(clean),
            Err(conflicted) => MergeOutcome::Conflict {
                annotated: conflicted,
            },
        }
    }
}

/// Structured (AST) merge via the mergiraf binary, subprocess only (GPLv3).
/// CLI verified against mergiraf 0.18.0: `mergiraf merge <BASE> <LEFT> <RIGHT>
/// -o <OUT>`; language is detected from the input file extension.
pub struct MergirafMerge {
    binary: std::path::PathBuf,
    /// File extension used for language detection (e.g. "rs", "py").
    pub extension: String,
}

impl MergirafMerge {
    /// Returns None when mergiraf is not installed; the pipeline then simply
    /// skips this slot (D4 fallback: line merge + first-class conflicts).
    pub fn detect(extension: &str) -> Option<Self> {
        let out = std::process::Command::new("which")
            .arg("mergiraf")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
        Some(Self {
            binary: path.into(),
            extension: extension.to_string(),
        })
    }

    fn run(&self, base: &str, left: &str, right: &str) -> std::io::Result<MergeOutcome> {
        let dir = std::env::temp_dir().join(format!(
            "choir-mergiraf-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir)?;
        let ext = &self.extension;
        let b = dir.join(format!("base.{ext}"));
        let l = dir.join(format!("left.{ext}"));
        let r = dir.join(format!("right.{ext}"));
        let o = dir.join(format!("merged.{ext}"));
        std::fs::write(&b, base)?;
        std::fs::write(&l, left)?;
        std::fs::write(&r, right)?;
        let out = std::process::Command::new(&self.binary)
            .arg("merge")
            .arg(&b)
            .arg(&l)
            .arg(&r)
            .arg("-o")
            .arg(&o)
            .output();
        let merged = std::fs::read_to_string(&o).unwrap_or_default();
        std::fs::remove_dir_all(&dir).ok();
        let out = out?;
        if out.status.success() {
            Ok(MergeOutcome::Resolved(merged))
        } else if !merged.is_empty() {
            Ok(MergeOutcome::Conflict { annotated: merged })
        } else {
            Ok(MergeOutcome::Unavailable(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ))
        }
    }
}

impl MergeStrategy for MergirafMerge {
    fn name(&self) -> &'static str {
        "mergiraf"
    }
    fn merge(&self, base: &str, left: &str, right: &str) -> MergeOutcome {
        match self.run(base, left, right) {
            Ok(outcome) => outcome,
            Err(e) => MergeOutcome::Unavailable(e.to_string()),
        }
    }
}
