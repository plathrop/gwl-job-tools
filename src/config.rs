use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use miette::Result;

use crate::cli::APP_NAME;

#[derive(Clone, Debug)]
pub struct AppPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl AppPaths {
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
