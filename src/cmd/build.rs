use std::path::PathBuf;

use anyhow::Result;
use structopt::StructOpt;

use crate::build::BuildSystem;
use crate::config::{ConfigOpts, ConfigOptsBuild};

/// Build the Rust WASM app and all of its assets.
#[derive(Clone, Debug, StructOpt)]
#[structopt(name = "build")]
pub struct Build {
    #[structopt(flatten)]
    pub build: ConfigOptsBuild,
}

impl Build {
    #[tracing::instrument(level = "trace", skip(self, config))]
    pub async fn run(self, config: Option<PathBuf>) -> Result<()> {
        let cfg = ConfigOpts::rtc_build(self.build, config)?;
        let mut system = BuildSystem::new(cfg, None).await?;
        system.build().await?;
        Ok(())
    }
}
