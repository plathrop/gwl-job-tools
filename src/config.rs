use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use miette::{Context, IntoDiagnostic, Result};
use serde::Deserialize;
use tracing::instrument;

use crate::APP_NAME;

/// TOML config (spec: comp floor + ceiling, remote-only flag, blacklist,
/// alias table, scoring weights, generic-letter path, target-companies).
/// Gates and scoring consume these from Increment 2 onward; the full key
/// set parses now.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub compensation_floor: Option<u64>,
    pub compensation_ceiling: Option<u64>,
    pub remote_only: bool,
    /// When true, a posting that lists a physical location but says nothing
    /// about work arrangement is treated as non-remote by the remote-only
    /// gate. Default false (start permissive: pass to review and revisit if
    /// it produces too many false leads).
    pub reject_location_only: bool,
    pub blacklist: Vec<String>,
    pub aliases: HashMap<String, String>,
    pub scoring_weights: ScoringWeights,
    pub cover_letter_path: Option<PathBuf>,
    pub target_companies: Vec<String>,
    /// Ideological red lines (spec §2): the MECHANISM ships in v0 as this
    /// filter list over posting text; the CONTENT is deferred to the later
    /// LLM scorer (ship empty).
    pub ideological_red_lines: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoringWeights {
    pub level: f64,
    pub skills: f64,
    pub compensation: f64,
    /// Fourth dimension (confirmed 2026-08-26): confident remote = 100,
    /// unknown = 50. Lands with Increment 3.
    pub remote: f64,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        // Spec: default equal weights.
        Self {
            level: 1.0,
            skills: 1.0,
            compensation: 1.0,
            remote: 1.0,
        }
    }
}

impl Config {
    pub const FILE_NAME: &'static str = "config.toml";

    /// Load `<config_dir>/config.toml`; a missing file means defaults.
    #[instrument]
    pub fn load(paths: &AppPaths) -> Result<Self> {
        let path = paths.config_dir().join(Self::FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .into_diagnostic()
                .wrap_err_with(|| format!("parsing {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err)
                .into_diagnostic()
                .wrap_err_with(|| format!("reading {}", path.display())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl AppPaths {
    pub fn new(config_dir: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            config_dir,
            data_dir,
        }
    }

    #[instrument]
    pub fn discover() -> Result<Self> {
        let project_dirs = ProjectDirs::from("st.ember", "gwl", APP_NAME).ok_or_else(|| {
            miette::miette!("could not determine platform config/data directories")
        })?;

        Ok(Self {
            config_dir: project_dirs.config_dir().to_path_buf(),
            data_dir: project_dirs.data_dir().to_path_buf(),
        })
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_returns_configured_path() {
        let paths = AppPaths::new(
            PathBuf::from("/home/user/.config/gwl-jobs"),
            PathBuf::from("/home/user/.local/share/gwl-jobs"),
        );

        assert_eq!(paths.config_dir(), Path::new("/home/user/.config/gwl-jobs"));
    }

    #[test]
    fn data_dir_returns_configured_path() {
        let paths = AppPaths::new(
            PathBuf::from("/home/user/.config/gwl-jobs"),
            PathBuf::from("/home/user/.local/share/gwl-jobs"),
        );

        assert_eq!(
            paths.data_dir(),
            Path::new("/home/user/.local/share/gwl-jobs")
        );
    }

    // ── Config (TOML) ────────────────────────────────────────────

    #[test]
    fn missing_config_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(dir.path().join("config"), dir.path().join("data"));
        let config = Config::load(&paths).unwrap();
        assert_eq!(config.compensation_floor, None);
        assert!(!config.remote_only);
        assert!(config.blacklist.is_empty());
        assert_eq!(config.scoring_weights.level, 1.0);
        assert_eq!(config.scoring_weights.skills, 1.0);
        assert_eq!(config.scoring_weights.compensation, 1.0);
        assert_eq!(config.scoring_weights.remote, 1.0);
    }

    #[test]
    fn parses_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join(Config::FILE_NAME),
            r#"
compensation_floor = 180000
compensation_ceiling = 300000
remote_only = true
blacklist = ["salesforce"]
cover_letter_path = "~/letters/generic.pdf"
target_companies = []

[aliases]
K8s = "Kubernetes"

[scoring_weights]
level = 0.3
skills = 0.3
compensation = 0.4
"#,
        )
        .unwrap();
        let paths = AppPaths::new(config_dir, dir.path().join("data"));
        let config = Config::load(&paths).unwrap();
        assert_eq!(config.compensation_floor, Some(180_000));
        assert_eq!(config.compensation_ceiling, Some(300_000));
        assert!(config.remote_only);
        assert_eq!(config.blacklist, vec!["salesforce"]);
        assert_eq!(
            config.aliases.get("K8s").map(String::as_str),
            Some("Kubernetes")
        );
        assert_eq!(config.scoring_weights.compensation, 0.4);
        assert_eq!(
            config.cover_letter_path.as_deref(),
            Some(Path::new("~/letters/generic.pdf"))
        );
        assert!(config.target_companies.is_empty());
    }

    #[test]
    fn unknown_config_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join(Config::FILE_NAME), "bogus_key = 1\n").unwrap();
        let paths = AppPaths::new(config_dir, dir.path().join("data"));
        assert!(Config::load(&paths).is_err());
    }

    #[test]
    fn unknown_scoring_weight_key_is_rejected() {
        // A typo inside [scoring_weights] currently falls back to the
        // default weight silently (deny_unknown_fields only applies at the
        // top level). A misspelled weight would silently change ranking.
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join(Config::FILE_NAME),
            "[scoring_weights]\ncompensaton = 0.4\n",
        )
        .unwrap();
        let paths = AppPaths::new(config_dir, dir.path().join("data"));
        assert!(Config::load(&paths).is_err());
    }
}
