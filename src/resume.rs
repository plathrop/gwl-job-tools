//! Resume loading (decision 0004): own lenient parsing of the JSON Resume
//! schema, for the fields v0 consumes. The official `json-resume-serde` crate
//! is not used (v0.1.0, unpublished, and too strict to parse Grey's actual
//! resume.json).
//!
//! v0 consumes `skills` (scoring, Increment 3); `basics`/`work` land with
//! the apply-package cheat sheet (Increment 5).

use std::path::Path;

use miette::{Context, IntoDiagnostic, Result};
use serde::Deserialize;
use tracing::warn;

/// The subset of the JSON Resume schema v0 reads. Lenient: unknown fields are
/// ignored, missing sections default to empty.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Resume {
    pub skills: Vec<Skill>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Skill {
    pub name: Option<String>,
    pub level: Option<String>,
    pub keywords: Vec<String>,
}

impl Resume {
    /// Flatten all skills' keywords into one list (the scoring input).
    pub fn keywords(&self) -> Vec<String> {
        self.skills
            .iter()
            .flat_map(|s| s.keywords.iter().cloned())
            .collect()
    }
}

/// Load the resume at `path`. `None` (no `resume_path` configured) logs a
/// WARN and returns `None` — the skills dimension degrades. A configured path
/// that is missing or unparseable fails loudly (decision 0004).
pub fn load(path: Option<&Path>) -> Result<Option<Resume>> {
    match path {
        None => {
            warn!("no resume_path configured; the skills dimension will be unavailable");
            Ok(None)
        }
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .into_diagnostic()
                .wrap_err_with(|| format!("reading resume {}", path.display()))?;
            let resume: Resume = serde_json::from_str(&text)
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!(
                        "parsing resume {} (expected JSON Resume schema)",
                        path.display()
                    )
                })?;
            Ok(Some(resume))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_resume(dir: &tempfile::TempDir, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join("resume.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn no_path_warns_and_returns_none() {
        assert!(load(None).unwrap().is_none());
    }

    #[test]
    fn missing_file_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(load(Some(&path)).is_err());
    }

    #[test]
    fn unparseable_file_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_resume(&dir, "not json");
        assert!(load(Some(&path)).is_err());
    }

    #[test]
    fn parses_skills_leniently() {
        let dir = tempfile::tempdir().unwrap();
        // Missing sections (work, education, …) and unknown fields ($schema,
        // meta) are tolerated; only `skills` is read.
        let path = write_resume(
            &dir,
            r#"{
                "$schema": "https://jsonresume.org/schema/1.0.0",
                "skills": [
                    {"name": "Cloud", "level": "Expert", "keywords": ["Kubernetes", "Terraform"]}
                ],
                "meta": {"version": "v1.0.0"}
            }"#,
        );
        let resume = load(Some(&path)).unwrap().unwrap();
        assert_eq!(resume.keywords(), vec!["Kubernetes", "Terraform"]);
    }

    #[test]
    fn missing_skills_defaults_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_resume(&dir, r#"{"basics": {"name": "Grey"}}"#);
        let resume = load(Some(&path)).unwrap().unwrap();
        assert!(resume.keywords().is_empty());
    }
}
