// Author: ahura / @mabae
use anyhow::{Context, Result};
use headless_chrome::{Browser, LaunchOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static PDF_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub fn new_browser() -> Result<Browser> {
    let mut builder = LaunchOptions::default_builder();
    builder.headless(true).sandbox(true);
    if let Some(path) = configured_browser() {
        builder.path(Some(path));
    }
    let options = builder.build().context("invalid browser launch options")?;
    Browser::new(options).context("could not launch Chrome or Edge")
}

fn configured_browser() -> Option<PathBuf> {
    std::env::var_os("PLAYWRIGHT_EXECUTABLE_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            std::env::var_os("CHROME")
                .map(PathBuf::from)
                .filter(|path| path.is_file())
        })
        .or_else(|| {
            let roots = [
                std::env::var_os("PROGRAMFILES").map(PathBuf::from),
                std::env::var_os("PROGRAMFILES(X86)").map(PathBuf::from),
                std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            ];
            let candidates = [
                Path::new("Google/Chrome/Application/chrome.exe"),
                Path::new("Google/Chrome Beta/Application/chrome.exe"),
                Path::new("Microsoft/Edge/Application/msedge.exe"),
                Path::new("Chromium/Application/chrome.exe"),
            ];
            roots
                .into_iter()
                .flatten()
                .flat_map(|root| candidates.map(|relative| root.join(relative)))
                .find(|candidate| candidate.is_file())
        })
}

pub fn render_html_to_pdf(html: &str) -> Result<Vec<u8>> {
    let id = PDF_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("doctoreto-report-{}-{id}.html", std::process::id()));
    std::fs::write(&path, html).context("could not write temporary report HTML")?;
    let url = url::Url::from_file_path(&path)
        .map_err(|_| anyhow::anyhow!("invalid temporary HTML path"))?
        .to_string();
    let browser = new_browser()?;
    let pdf_result: Result<Vec<u8>> = (|| {
        let tab = browser.new_tab().context("could not open a browser tab")?;
        tab.navigate_to(&url)?
            .wait_until_navigated()?
            .print_to_pdf(None)
            .context("Chrome could not render the PDF")
    })();
    let _ = std::fs::remove_file(path);
    let pdf = pdf_result?;
    Ok(pdf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_html_renders_to_a_pdf_when_chrome_is_installed() {
        if configured_browser().is_none() {
            return;
        }
        let pdf = render_html_to_pdf(
            "<!doctype html><html lang=\"fa\" dir=\"rtl\"><meta charset=\"utf-8\"><body>آزمایش PDF</body></html>",
        )
        .expect("render a local synthetic PDF");
        assert!(pdf.starts_with(b"%PDF"));
    }
}
