// Author: ahura / @mabae
use anyhow::{Context, Result, anyhow};
use futures::future::join_all;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use crate::http::{SiteClient, robots_sitemaps};
use crate::model::{BASE_URL, DiscoveryResult, DiscoverySnapshot};
use crate::parse::{
    extract_city_slugs_from_source, extract_links, extract_sitemap_locations,
    is_pagination_audit_candidate,
};
use crate::settings::Settings;
use crate::state;
use crate::util::{
    city_slug_from_url, default_city_name, is_listing_url, is_profile_url, is_sitemap_url,
    safe_site_url,
};

pub async fn discover_profiles(
    client: &SiteClient,
    concurrency: usize,
    checkpoint_path: &Path,
    settings: &Settings,
) -> Result<DiscoveryResult> {
    let home_url = format!("{BASE_URL}/doctor");
    let mut profile_urls = HashSet::new();
    let mut city_labels = HashMap::new();
    let mut profile_city_hints: HashMap<String, HashSet<String>> = HashMap::new();
    let mut listing_urls = HashSet::new();
    let mut specialty_slugs = HashSet::new();
    let mut sitemap_queue = HashSet::from([
        format!("{BASE_URL}/sitemap.xml"),
        format!("{BASE_URL}/sitemap_index.xml"),
        format!("{BASE_URL}/sitemap-index.xml"),
        format!("{BASE_URL}/sitemapindex.xml"),
    ]);

    if let Ok(robots) = client.fetch_text(&format!("{BASE_URL}/robots.txt")).await {
        sitemap_queue.extend(robots_sitemaps(&robots.text));
    }

    let home = client
        .fetch_text(&home_url)
        .await
        .context("could not read the doctor directory")?;
    if home.status >= 400 {
        return Err(anyhow!(
            "directory returned HTTP {} at {home_url}",
            home.status
        ));
    }
    let home_links = extract_links(&home.text, &home.final_url, &mut city_labels);
    if !settings.has_city_allowlist() {
        profile_urls.extend(home_links.profile_urls);
    }
    listing_urls.extend(
        home_links
            .listing_urls
            .into_iter()
            .filter(|url| listing_city_enabled(url, settings)),
    );
    city_labels.retain(|slug, _| settings.city_enabled(slug));
    specialty_slugs.extend(home_links.specialty_slugs);
    for slug in extract_city_slugs_from_source(&home.text) {
        if settings.city_enabled(&slug) {
            city_labels
                .entry(slug.clone())
                .or_insert_with(|| default_city_name(&slug));
        }
    }

    let mut visited_sitemaps = HashSet::new();
    let mut sitemap_count = 0usize;
    while !sitemap_queue.is_empty() && sitemap_count < 2500 {
        let batch: Vec<String> = sitemap_queue
            .iter()
            .filter(|url| !visited_sitemaps.contains(url.as_str()))
            .take(concurrency.max(1))
            .cloned()
            .collect();
        if batch.is_empty() {
            break;
        }
        for url in &batch {
            sitemap_queue.remove(url);
            visited_sitemaps.insert(url.clone());
        }
        let responses = join_all(
            batch
                .iter()
                .map(|url| async move { (url, client.fetch_text(url).await) }),
        )
        .await;
        for (source_url, response) in responses {
            sitemap_count += 1;
            let Ok(response) = response else { continue };
            if response.status >= 400 || !response.text.contains('<') {
                continue;
            }
            let locations = extract_sitemap_locations(&response.text, source_url);
            for location in locations {
                let Some(url) = safe_site_url(&location, source_url) else {
                    continue;
                };
                if is_profile_url(&url) {
                    if !settings.has_city_allowlist() {
                        profile_urls.insert(url);
                    }
                    continue;
                }
                if let Some(slug) = city_slug_from_url(&url)
                    && settings.city_enabled(&slug)
                {
                    city_labels
                        .entry(slug.clone())
                        .or_insert_with(|| default_city_name(&slug));
                }
                if is_listing_url(&url) && listing_city_enabled(&url, settings) {
                    listing_urls.insert(url.clone());
                }
                if is_sitemap_url(&url) && !visited_sitemaps.contains(&url) {
                    sitemap_queue.insert(url);
                }
            }
        }
    }
    if sitemap_count >= 2500 {
        eprintln!("[sitemap] reached the 2,500-file safety cap.");
    }

    for slug in city_labels.keys() {
        listing_urls.insert(format!(
            "{BASE_URL}/doctors/city/{}",
            urlencoding::encode(slug)
        ));
    }
    if profile_urls.len() < 500 {
        for city in city_labels.keys() {
            for specialty in &specialty_slugs {
                listing_urls.insert(format!(
                    "{BASE_URL}/doctors/speciality/{}/city/{}",
                    urlencoding::encode(specialty),
                    urlencoding::encode(city)
                ));
            }
        }
    }
    if !listing_urls.is_empty() {
        eprintln!(
            "[discover] inspecting {} city/specialty directory pages.",
            listing_urls.len()
        );
        crawl_listings(
            client,
            listing_urls,
            &mut profile_urls,
            &mut city_labels,
            &mut profile_city_hints,
            &mut specialty_slugs,
            concurrency,
            checkpoint_path,
            settings,
        )
        .await?;
    }
    let mut profile_urls: Vec<String> = profile_urls.into_iter().collect();
    profile_urls.sort();
    eprintln!(
        "[discover] found {} unique doctor profile URLs and {} city slugs.",
        profile_urls.len(),
        city_labels.len()
    );
    Ok(DiscoveryResult {
        profile_urls,
        city_labels,
        profile_city_hints,
    })
}

#[allow(clippy::too_many_arguments)]
async fn crawl_listings(
    client: &SiteClient,
    initial_urls: HashSet<String>,
    profile_urls: &mut HashSet<String>,
    city_labels: &mut HashMap<String, String>,
    profile_city_hints: &mut HashMap<String, HashSet<String>>,
    specialty_slugs: &mut HashSet<String>,
    concurrency: usize,
    checkpoint_path: &Path,
    settings: &Settings,
) -> Result<()> {
    let initial_urls: HashSet<String> = initial_urls
        .into_iter()
        .filter(|url| listing_city_enabled(url, settings))
        .collect();
    let mut queue: VecDeque<String> = initial_urls.iter().cloned().collect();
    let mut queued = initial_urls.clone();
    let mut visited = HashSet::new();
    let mut pages = 0usize;
    let mut failed_attempts = HashMap::new();
    let mut pagination_checked = HashSet::new();

    if checkpoint_path.exists()
        && let Ok(snapshot) = state::load_discovery(checkpoint_path)
        && snapshot.version == 1
    {
        queue = snapshot.queue;
        queued = snapshot.queued;
        visited = snapshot.visited;
        pages = snapshot.pages;
        failed_attempts = snapshot.failed_attempts;
        pagination_checked = snapshot.pagination_checked;
        let snapshot_profiles = snapshot.profile_urls;
        for (slug, label) in snapshot.city_labels {
            city_labels.entry(slug).or_insert(label);
        }
        for (url, hints) in snapshot.profile_city_hints {
            let enabled_hints: HashSet<String> = hints
                .into_iter()
                .filter(|city| settings.city_enabled(city))
                .collect();
            if !settings.has_city_allowlist() || !enabled_hints.is_empty() {
                if snapshot_profiles.contains(&url) {
                    profile_urls.insert(url.clone());
                }
                if !enabled_hints.is_empty() {
                    profile_city_hints
                        .entry(url)
                        .or_default()
                        .extend(enabled_hints);
                }
            }
        }
        if !settings.has_city_allowlist() {
            profile_urls.extend(snapshot_profiles);
        }
        specialty_slugs.extend(snapshot.specialty_slugs);
        let queued_now: HashSet<String> = queue.iter().cloned().collect();
        let mut recheck = Vec::new();
        for url in &visited {
            let failed = failed_attempts.contains_key(url);
            if !is_pagination_audit_candidate(url)
                || (!failed && pagination_checked.contains(url))
                || queued_now.contains(url)
            {
                continue;
            }
            recheck.push((url.clone(), failed));
        }
        for (url, failed) in recheck {
            queue.push_back(url.clone());
            queued.insert(url.clone());
            visited.remove(&url);
            pagination_checked.remove(&url);
            if failed {
                failed_attempts.remove(&url);
            }
        }
        eprintln!(
            "[discover] resumed listing crawl at {pages} pages with {} queued pages.",
            queue.len()
        );
    }

    queue.retain(|url| listing_city_enabled(url, settings));
    queued.retain(|url| listing_city_enabled(url, settings));
    city_labels.retain(|slug, _| settings.city_enabled(slug));

    for url in initial_urls.iter() {
        if !queued.contains(url) && !visited.contains(url) {
            queued.insert(url.clone());
            queue.push_back(url.clone());
        }
    }

    while !queue.is_empty() && pages < 1_000_000 {
        let mut batch = Vec::with_capacity(concurrency.max(1));
        while batch.len() < concurrency.max(1) && !queue.is_empty() {
            if let Some(url) = queue.pop_front()
                && !visited.contains(&url)
            {
                batch.push(url);
            }
        }
        if batch.is_empty() {
            continue;
        }
        let responses = join_all(
            batch
                .iter()
                .map(|url| async move { (url, client.fetch_text(url).await) }),
        )
        .await;
        for (url, response) in responses {
            pages += 1;
            match response {
                Ok(response) if response.status < 400 => {
                    visited.insert(url.clone());
                    failed_attempts.remove(url);
                    let links = extract_links(&response.text, &response.final_url, city_labels);
                    city_labels.retain(|slug, _| settings.city_enabled(slug));
                    if let Some(city) = city_slug_from_url(url)
                        && settings.city_enabled(&city)
                    {
                        for profile_url in &links.profile_urls {
                            profile_city_hints
                                .entry(profile_url.clone())
                                .or_default()
                                .insert(city.clone());
                        }
                    }
                    profile_urls.extend(links.profile_urls);
                    for slug in extract_city_slugs_from_source(&response.text) {
                        if settings.city_enabled(&slug) {
                            city_labels
                                .entry(slug.clone())
                                .or_insert_with(|| default_city_name(&slug));
                        }
                    }
                    for listing_url in links.listing_urls {
                        if !listing_city_enabled(&listing_url, settings) {
                            continue;
                        }
                        if let Some(slug) = city_slug_from_url(&listing_url) {
                            city_labels
                                .entry(slug.clone())
                                .or_insert_with(|| default_city_name(&slug));
                        }
                        if queued.insert(listing_url.clone()) && !visited.contains(&listing_url) {
                            queue.push_back(listing_url);
                        }
                    }
                    specialty_slugs.extend(links.specialty_slugs);
                    if is_pagination_audit_candidate(url) {
                        pagination_checked.insert(url.clone());
                    }
                }
                Ok(response) => retry_listing(
                    url,
                    format!("HTTP {}", response.status),
                    &mut queue,
                    &mut visited,
                    &mut failed_attempts,
                    &mut pagination_checked,
                ),
                Err(error) => retry_listing(
                    url,
                    error.to_string(),
                    &mut queue,
                    &mut visited,
                    &mut failed_attempts,
                    &mut pagination_checked,
                ),
            }
            if pages.is_multiple_of(100) {
                eprintln!(
                    "[listing] inspected {pages} pages; found {} profiles.",
                    profile_urls.len()
                );
                save_snapshot(
                    checkpoint_path,
                    &queue,
                    &queued,
                    &visited,
                    pages,
                    profile_urls,
                    city_labels,
                    profile_city_hints,
                    specialty_slugs,
                    &failed_attempts,
                    &pagination_checked,
                )?;
            }
        }
    }
    if pages >= 1_000_000 {
        eprintln!("[listing] reached the 1,000,000-page safety cap.");
    }
    save_snapshot(
        checkpoint_path,
        &queue,
        &queued,
        &visited,
        pages,
        profile_urls,
        city_labels,
        profile_city_hints,
        specialty_slugs,
        &failed_attempts,
        &pagination_checked,
    )?;
    Ok(())
}

fn retry_listing(
    url: &str,
    error: String,
    queue: &mut VecDeque<String>,
    visited: &mut HashSet<String>,
    failed_attempts: &mut HashMap<String, u8>,
    pagination_checked: &mut HashSet<String>,
) {
    let attempts = failed_attempts.entry(url.to_owned()).or_default();
    *attempts += 1;
    if *attempts < 3 {
        queue.push_back(url.to_owned());
    } else {
        visited.insert(url.to_owned());
        if is_pagination_audit_candidate(url) {
            pagination_checked.insert(url.to_owned());
        }
        eprintln!("[listing] giving up after {attempts} failed attempts ({url}): {error}");
    }
}

fn listing_city_enabled(url: &str, settings: &Settings) -> bool {
    city_slug_from_url(url).is_none_or(|slug| settings.city_enabled(&slug))
}

#[allow(clippy::too_many_arguments)]
fn save_snapshot(
    path: &Path,
    queue: &VecDeque<String>,
    queued: &HashSet<String>,
    visited: &HashSet<String>,
    pages: usize,
    profile_urls: &HashSet<String>,
    city_labels: &HashMap<String, String>,
    profile_city_hints: &HashMap<String, HashSet<String>>,
    specialty_slugs: &HashSet<String>,
    failed_attempts: &HashMap<String, u8>,
    pagination_checked: &HashSet<String>,
) -> Result<()> {
    state::save_discovery(
        path,
        &DiscoverySnapshot {
            version: 1,
            queue: queue.clone(),
            queued: queued.clone(),
            visited: visited.clone(),
            pages,
            profile_urls: profile_urls.clone(),
            city_labels: city_labels.clone(),
            profile_city_hints: profile_city_hints.clone(),
            specialty_slugs: specialty_slugs.clone(),
            failed_attempts: failed_attempts.clone(),
            pagination_checked: pagination_checked.clone(),
        },
    )
}
