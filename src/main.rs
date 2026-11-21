// Author: ahura / @mabae
mod browser;
mod cli;
mod discovery;
mod export;
mod http;
mod model;
mod parse;
mod pipeline;
mod settings;
mod state;
mod util;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let options = cli::Options::parse();
    let settings = settings::Settings::load()?;
    pipeline::run(options, settings).await
}
