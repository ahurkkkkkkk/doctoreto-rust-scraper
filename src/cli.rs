// Author: ahura / @mabae
use clap::Parser;
use std::path::PathBuf;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "doctoreto-scraper",
    version,
    about = "Collect public doctor profiles into city folders"
)]
pub struct Options {
    #[arg(long, default_value = "output/doctoreto")]
    pub output: PathBuf,
    #[arg(long, default_value_t = 256)]
    pub concurrency: usize,
    #[arg(long, default_value_t = 24)]
    pub requests_per_second: u32,
    #[arg(long, default_value_t = 0)]
    pub delay_ms: u64,
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub discover_only: bool,
    #[arg(long)]
    pub city: Option<String>,
    #[arg(long)]
    pub no_images: bool,
    #[arg(long)]
    pub browser_fetch: bool,
    #[arg(long)]
    pub fresh: bool,
}
