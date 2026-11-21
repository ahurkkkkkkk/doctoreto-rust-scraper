// Author: ahura / @mabae
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::model::{DiscoverySnapshot, DoctorRecord};
use crate::parse::extract_addresses_from_sections;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailureRecord {
    pub profile_url: String,
    pub error: String,
    pub at: String,
}

pub fn load_discovery(path: &Path) -> Result<DiscoverySnapshot> {
    let data = fs::read(path)
        .with_context(|| format!("could not read discovery checkpoint {}", path.display()))?;
    serde_json::from_slice(&data).context("invalid discovery checkpoint")
}

pub fn save_discovery(path: &Path, snapshot: &DiscoverySnapshot) -> Result<()> {
    let bytes = serde_json::to_vec(snapshot)?;
    atomic_write(path, &bytes)
}

pub fn load_checkpoint_urls(path: &Path) -> Result<HashSet<String>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut urls = HashSet::new();
    for line in reader.lines() {
        let line = line?;
        if let Ok(record) = serde_json::from_str::<DoctorRecord>(&line) {
            if !record.profile_url.is_empty() {
                urls.insert(record.profile_url);
            }
        } else {
            eprintln!("[resume] ignored one incomplete checkpoint line.");
        }
    }
    Ok(urls)
}

pub fn load_checkpoint_records(path: &Path) -> Result<HashMap<String, DoctorRecord>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut records = HashMap::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<DoctorRecord>(&line) {
            Ok(mut record) if !record.profile_url.is_empty() => {
                record.addresses = extract_addresses_from_sections(&record.page_sections);
                records.insert(record.profile_url.clone(), record);
            }
            _ => eprintln!("[resume] ignored one incomplete checkpoint record."),
        }
    }
    Ok(records)
}

pub fn append_record(path: &Path, record: &DoctorRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

pub fn append_failure(path: &Path, record: &FailureRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

pub fn reset_checkpoint(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, b"")?;
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(path);
    fs::write(&temporary, bytes)?;
    for attempt in 0..7 {
        if path.exists() {
            let backup = path.with_extension("replace-backup");
            match fs::rename(&temporary, path) {
                Ok(()) => return Ok(()),
                Err(_) => {
                    if backup.exists() {
                        let _ = fs::remove_file(&backup);
                    }
                    fs::rename(path, &backup).with_context(|| {
                        format!("could not stage existing file {}", path.display())
                    })?;
                    match fs::rename(&temporary, path) {
                        Ok(()) => {
                            let _ = fs::remove_file(backup);
                            return Ok(());
                        }
                        Err(error) => {
                            let _ = fs::rename(&backup, path);
                            if attempt == 6 {
                                return Err(error.into());
                            }
                            std::thread::sleep(std::time::Duration::from_millis(
                                100 * (1 << attempt),
                            ));
                        }
                    }
                }
            }
        } else {
            match fs::rename(&temporary, path) {
                Ok(()) => return Ok(()),
                Err(error) if attempt == 6 => return Err(error.into()),
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)))
                }
            }
        }
    }
    let _ = fs::remove_file(temporary);
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("tmp");
    let name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    path.with_file_name(format!("{name}.{}.{}.tmp", std::process::id(), extension))
}
