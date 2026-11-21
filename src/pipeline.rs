// Author: ahura / @mabae
use anyhow::{Result, anyhow};
use chrono::Utc;
use futures::future::join_all;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use tokio::fs;

use crate::cli::Options;
use crate::discovery::discover_profiles;
use crate::export::{ExportedCity, export_city};
use crate::http::SiteClient;
use crate::model::{CitySummary, DIRECTORY_URL, DoctorRecord, ExportSummary};
use crate::parse::parse_doctor_page;
use crate::settings::Settings;
use crate::state::{self, FailureRecord};
use crate::util::{
    city_directory_order_key, city_slug_from_name, normalize_city_name, normalize_slug,
};

pub async fn run(options: Options, settings: Settings) -> Result<()> {
    if !(1..=256).contains(&options.concurrency) {
        return Err(anyhow!("--concurrency must be between 1 and 256"));
    }
    if !(1..=24).contains(&options.requests_per_second) {
        return Err(anyhow!("--requests-per-second must be between 1 and 24"));
    }
    if options.limit == Some(0) {
        return Err(anyhow!("--limit must be at least 1"));
    }
    if let Some(city) = options.city.as_deref()
        && !settings.city_enabled(city)
    {
        return Err(anyhow!("city '{city}' is disabled by .env"));
    }
    let output = if options.output.is_absolute() {
        options.output.clone()
    } else {
        std::env::current_dir()?.join(&options.output)
    };
    let state_dir = output.join(".state");
    fs::create_dir_all(&state_dir).await?;
    let checkpoint_path = state_dir.join("doctors.jsonl");
    let discovery_path = state_dir.join("discovery.json");
    let failures_path = state_dir.join("failed.jsonl");
    if options.fresh {
        state::reset_checkpoint(&checkpoint_path)?;
        if discovery_path.exists() {
            fs::remove_file(&discovery_path).await?;
        }
    }

    let client = SiteClient::new(
        options.concurrency,
        options.requests_per_second,
        options.delay_ms,
        options.browser_fetch,
    )?;
    if let Some(path) = &settings.config_path {
        eprintln!("[settings] loaded {}", path.display());
    } else {
        eprintln!("[settings] using built-in defaults; no .env file found.");
    }
    let discovery =
        discover_profiles(&client, options.concurrency, &discovery_path, &settings).await?;
    let discovered_profile_url_count = discovery.profile_urls.len();
    if options.discover_only {
        eprintln!(
            "[discover-only] found {} unique doctor profile URLs across {} city slugs.",
            discovery.profile_urls.len(),
            discovery.city_labels.len()
        );
        client.close().await;
        return Ok(());
    }
    if discovery.profile_urls.is_empty() {
        client.close().await;
        return Err(anyhow!(
            "no doctor profiles were found; check the network connection and try again"
        ));
    }

    let checkpointed_urls = state::load_checkpoint_urls(&checkpoint_path)?;
    let pending: Vec<String> = discovery
        .profile_urls
        .iter()
        .filter(|url| !checkpointed_urls.contains(*url))
        .filter(|url| {
            let Some(hints) = discovery.profile_city_hints.get(*url) else {
                return true;
            };
            hints.is_empty()
                || hints.iter().any(|city| {
                    settings.city_enabled(city)
                        && options
                            .city
                            .as_deref()
                            .is_none_or(|selected| normalize_slug(selected) == normalize_slug(city))
                })
        })
        .cloned()
        .collect();
    let selected: Vec<String> = options
        .limit
        .map(|limit| pending.iter().take(limit).cloned().collect())
        .unwrap_or(pending);
    eprintln!(
        "[scrape] {} profiles already checkpointed; {} new profiles queued.",
        checkpointed_urls.len(),
        selected.len()
    );
    if let Some(limit) = options.limit {
        eprintln!("[scrape] this run is capped at {limit} new profile pages.");
    }

    let mut completed = 0usize;
    let mut failed = 0usize;
    let settings_ref = &settings;
    for batch in selected.chunks(options.concurrency.max(1)) {
        let client_ref = &client;
        let results = join_all(batch.iter().map(|url| {
            let hints = discovery
                .profile_city_hints
                .get(url)
                .cloned()
                .unwrap_or_default();
            async move {
                (
                    url,
                    scrape_doctor(client_ref, url, hints, settings_ref).await,
                )
            }
        }))
        .await;
        for (url, result) in results {
            match result {
                Ok(mut record) => {
                    add_city_slugs_from_names(&mut record, &discovery.city_labels);
                    state::append_record(&checkpoint_path, &record)?;
                    completed += 1;
                    if completed.is_multiple_of(25) || completed == selected.len() {
                        eprintln!(
                            "[scrape] {completed}/{} new profile pages saved.",
                            selected.len()
                        );
                    }
                }
                Err(error) => {
                    failed += 1;
                    let entry = FailureRecord {
                        profile_url: url.clone(),
                        error: short_error(&error),
                        at: Utc::now().to_rfc3339(),
                    };
                    state::append_failure(&failures_path, &entry)?;
                    eprintln!("[profile] {} ({url})", entry.error);
                }
            }
        }
    }
    client.close().await;

    let mut records = state::load_checkpoint_records(&checkpoint_path)?;
    let profile_city_hints = discovery.profile_city_hints.clone();
    let mut city_labels = discovery.city_labels.clone();
    city_labels.remove("all");
    for record in records.values_mut() {
        if let Some(hints) = profile_city_hints.get(&record.profile_url) {
            let clean_hints: Vec<String> = hints
                .iter()
                .map(|slug| normalize_slug(slug))
                .filter(|slug| !slug.is_empty() && slug != "all")
                .collect();
            record
                .listing_city_hints
                .extend(clean_hints.iter().cloned());
            record.listing_city_hints.sort();
            record.listing_city_hints.dedup();
            if record.city_slugs.is_empty() {
                record.city_slugs = clean_hints;
            }
        }
        for (index, slug) in record.city_slugs.iter().enumerate() {
            if let Some(name) = record.city_names.get(index).filter(|name| !name.is_empty()) {
                city_labels
                    .entry(slug.clone())
                    .or_insert_with(|| name.clone());
            }
        }
        record.city_slugs = normalize_slug_list(&record.city_slugs);
        record.listing_city_hints = normalize_slug_list(&record.listing_city_hints);
        add_city_slugs_from_names(record, &city_labels);
        crate::parse::apply_scrape_settings(record, &settings);
        record.city_slugs.retain(|slug| settings.city_enabled(slug));
        record
            .listing_city_hints
            .retain(|slug| settings.city_enabled(slug));
        record.city_names.retain(|name| {
            city_slug_from_name(name).is_none_or(|slug| settings.city_enabled(&slug))
        });
    }

    let selected_city = options.city.as_deref().map(normalize_slug);
    let mut city_records: HashMap<String, Vec<DoctorRecord>> = HashMap::new();
    let mut seen_by_city: HashMap<String, HashSet<String>> = HashMap::new();
    let mut sorted_records: Vec<DoctorRecord> = records.into_values().collect();
    sorted_records.sort_by(|left, right| left.profile_url.cmp(&right.profile_url));
    for record in sorted_records {
        let mut slugs: Vec<String> = record
            .city_slugs
            .iter()
            .chain(record.listing_city_hints.iter())
            .filter(|slug| *slug != "all")
            .cloned()
            .collect();
        slugs.sort();
        slugs.dedup();
        if slugs.is_empty() {
            if settings.has_city_allowlist() {
                continue;
            }
            slugs.push("unknown-city".to_owned());
        }
        let doctor_id = if record.id.trim().is_empty() {
            record
                .profile_url
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned()
        } else {
            record.id.trim().to_owned()
        };
        for slug in slugs {
            if !settings.city_enabled(&slug) {
                continue;
            }
            if selected_city
                .as_ref()
                .is_some_and(|selected| selected != &slug)
            {
                continue;
            }
            let seen = seen_by_city.entry(slug.clone()).or_default();
            if !doctor_id.is_empty() && !seen.insert(doctor_id.clone()) {
                continue;
            }
            city_records.entry(slug).or_default().push(record.clone());
        }
    }
    if city_records.is_empty() {
        return Err(anyhow!(
            "no city groups were available to export; try again without --city or inspect the checkpoint"
        ));
    }

    let mut city_order: Vec<String> = city_records.keys().cloned().collect();
    city_order.sort_by_key(|slug| city_directory_order_key(slug));
    eprintln!(
        "[export] writing {} city folders under {}",
        city_order.len(),
        output.display()
    );
    let mut cities = Vec::new();
    let mut export_errors = Vec::new();
    for slug in city_order {
        let city_name = city_labels
            .get(&slug)
            .cloned()
            .unwrap_or_else(|| crate::util::default_city_name(&slug));
        let doctors = city_records.remove(&slug).unwrap_or_default();
        match export_city(
            &output,
            &slug,
            &city_name,
            doctors,
            &client,
            !options.no_images && settings.enabled("profile_photo"),
            options.concurrency,
        )
        .await
        {
            Ok(exported) => {
                eprintln!(
                    "[export] {}: {} doctors; {} image files; {} embedded photos.",
                    city_name,
                    exported.doctor_count,
                    exported.image_count,
                    exported.embedded_image_count
                );
                cities.push(city_summary(&slug, &city_name, exported));
            }
            Err(error) => {
                let message = short_error(&error);
                eprintln!("[export] {city_name} failed: {message}");
                export_errors.push(json!({"slug": slug, "name": city_name, "error": message}));
            }
        }
    }

    let checkpointed = state::load_checkpoint_records(&checkpoint_path)?;
    let mut ids: HashSet<String> = checkpointed
        .values()
        .map(|record| record.id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .collect();
    let checkpointed_profiles = checkpointed.len();
    if ids.is_empty() {
        ids.extend(checkpointed.keys().cloned());
    }
    let summary = ExportSummary {
        source: DIRECTORY_URL.to_owned(),
        generated_at: Utc::now().to_rfc3339(),
        discovered_profile_urls: discovered_profile_url_count,
        checkpointed_profiles,
        unique_doctor_ids: ids.len(),
        scraped_this_run: completed,
        failed_this_run: failed,
        city_count: cities.len(),
        cities,
        city_export_errors: export_errors,
        output_files_per_city: vec![
            "doctors.xlsx or doctors-part-NNNN.xlsx".to_owned(),
            "doctors.docx/doctors.pdf or doctors-part-NNNN.docx/.pdf".to_owned(),
            "doctors.json".to_owned(),
            "images/".to_owned(),
        ],
    };
    state::atomic_write(
        &output.join("summary.json"),
        &serde_json::to_vec_pretty(&summary)?,
    )?;
    eprintln!(
        "[done] {} distinct doctor IDs are checkpointed across {} profile URLs. Summary: {}",
        summary.unique_doctor_ids,
        checkpointed_profiles,
        output.join("summary.json").display()
    );
    if failed > 0 {
        eprintln!(
            "[done] {failed} profile pages failed this run; see {} and rerun to retry.",
            failures_path.display()
        );
    }
    if !summary.city_export_errors.is_empty() {
        eprintln!(
            "[done] {} city exports failed; see summary.json and rerun to retry.",
            summary.city_export_errors.len()
        );
    }
    if !summary.city_export_errors.is_empty() {
        return Err(anyhow!("one or more city exports failed"));
    }
    Ok(())
}

async fn scrape_doctor(
    client: &SiteClient,
    profile_url: &str,
    city_hints: HashSet<String>,
    settings: &Settings,
) -> Result<DoctorRecord> {
    let response = client.fetch_text(profile_url).await?;
    if response.status >= 400 {
        return Err(anyhow!("profile HTTP {}", response.status));
    }
    let mut record = parse_doctor_page(
        &response.text,
        if response.final_url.is_empty() {
            profile_url
        } else {
            &response.final_url
        },
        settings,
    )?;
    record.listing_city_hints = city_hints.iter().cloned().collect();
    record.listing_city_hints.sort();
    if record.city_slugs.is_empty() && !city_hints.is_empty() {
        record.city_slugs = city_hints.into_iter().collect();
        record.city_slugs.sort();
    }
    let reveal_phone = settings.enabled("contact_details")
        && settings.enabled("reveal_hidden_phones")
        && (record.phones.is_empty() || crate::parse::has_phone_reveal_control(&response.text));
    let expand_reviews = settings.expand_review_list && settings.enabled("patient_reviews");
    let expand_endorsements =
        settings.expand_doctor_endorsements && settings.enabled("doctor_endorsements");
    if reveal_phone || expand_reviews || expand_endorsements {
        match client
            .interact_with_profile_page(
                profile_url,
                reveal_phone,
                expand_reviews,
                expand_endorsements,
                settings.review_expansion_limit,
            )
            .await
        {
            Ok(revealed) => {
                if reveal_phone {
                    record.phones.extend(revealed.phones);
                }
                record.phones.sort();
                record.phones.dedup();
                if !revealed.html.is_empty() {
                    let mut revealed_record =
                        parse_doctor_page(&revealed.html, &record.profile_url, settings)?;
                    for section in revealed_record.page_sections {
                        if !record.page_sections.iter().any(|existing| {
                            existing.heading == section.heading
                                && existing.category == section.category
                                && existing.text == section.text
                        }) {
                            record.page_sections.push(section);
                        }
                    }
                    if !revealed_record.page_text.is_empty() {
                        record.page_text = revealed_record.page_text;
                    }
                    if settings.enabled("raw_html") {
                        record.page_html = std::mem::take(&mut revealed_record.page_html);
                    }
                    crate::parse::apply_scrape_settings(&mut record, settings);
                    if settings.enabled("clinic_addresses") {
                        record.addresses =
                            crate::parse::extract_addresses_from_sections(&record.page_sections);
                    }
                }
            }
            Err(error) => {
                record.scrape_error = Some(format!(
                    "Profile interaction failed: {}",
                    short_error(&error)
                ))
            }
        }
    }
    Ok(record)
}

fn add_city_slugs_from_names(record: &mut DoctorRecord, labels: &HashMap<String, String>) {
    let known: HashMap<String, String> = labels
        .iter()
        .map(|(slug, name)| (normalize_city_name(name), slug.clone()))
        .collect();
    let added: Vec<String> = record
        .city_names
        .iter()
        .filter_map(|name| {
            known
                .get(&normalize_city_name(name))
                .cloned()
                .or_else(|| city_slug_from_name(name))
        })
        .collect();
    record.city_slugs.extend(added);
    record.city_slugs = normalize_slug_list(&record.city_slugs);
}

fn normalize_slug_list(values: &[String]) -> Vec<String> {
    let mut slugs: Vec<String> = values
        .iter()
        .map(|value| normalize_slug(value))
        .filter(|slug| !slug.is_empty() && slug != "all")
        .collect();
    slugs.sort();
    slugs.dedup();
    slugs
}

fn city_summary(slug: &str, name: &str, city: ExportedCity) -> CitySummary {
    CitySummary {
        slug: slug.to_owned(),
        name: name.to_owned(),
        doctor_count: city.doctor_count,
        image_count: city.image_count,
        embedded_image_count: city.embedded_image_count,
        directory: city.directory,
        workbook_file: city.workbook_file,
    }
}

fn short_error(error: &anyhow::Error) -> String {
    error.to_string()
}
