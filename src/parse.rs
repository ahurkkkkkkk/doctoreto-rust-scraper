// Author: ahura / @mabae
use anyhow::Result;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde_json::{Value, json};
use std::sync::OnceLock;
use url::Url;

use crate::model::{BASE_URL, ClinicLocation, DoctorRecord, PageSection};
use crate::settings::Settings;
use crate::util::{
    clean_text, first_string, labels_from_value, normalize_digits, normalize_slug, unique,
    values_for_keys,
};

static BREAK_TAGS: OnceLock<Regex> = OnceLock::new();
static ADDRESS_HEAD: OnceLock<Regex> = OnceLock::new();
#[cfg(test)]
static NEXT_SECTION: OnceLock<Regex> = OnceLock::new();
static ADDRESS_LABEL: OnceLock<Regex> = OnceLock::new();
static ADDRESS_STOP: OnceLock<Regex> = OnceLock::new();
static PHONE_REVEAL: OnceLock<Regex> = OnceLock::new();
static PHONE_REVEAL_CONTROL: OnceLock<Regex> = OnceLock::new();

pub fn parse_doctor_page(
    html: &str,
    profile_url: &str,
    settings: &Settings,
) -> Result<DoctorRecord> {
    let document = Html::parse_document(html);
    let next_data = parse_next_data(&document);
    let page_props = next_data
        .pointer("/props/pageProps")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let queries = page_props
        .pointer("/dehydratedState/queries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let path_parts = Url::parse(profile_url)
        .ok()
        .map(|url| {
            url.path_segments()
                .into_iter()
                .flatten()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let fallback_id = path_parts.last().cloned().unwrap_or_default();
    let doctor_data = queries
        .iter()
        .find(|query| {
            let key = query.pointer("/queryKey").and_then(Value::as_array);
            key.is_some_and(|key| key.len() == 2 && key[0].as_str() == Some("doctors"))
        })
        .and_then(|query| query.pointer("/state/data"))
        .cloned()
        .or_else(|| find_doctor_like_object(&page_props, &fallback_id, 0).cloned())
        .unwrap_or_else(|| json!({}));
    let page_text = extract_page_text(&document);
    let page_sections = extract_sections(&document);
    let specialities = labels_from_value(
        doctor_data
            .get("specialities")
            .or_else(|| doctor_data.get("specialties")),
    );
    let service_tags = labels_from_value(doctor_data.get("serviceTags"));
    let insurances = labels_from_value(doctor_data.get("insurances"));
    let city_entries = doctor_data
        .get("cities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let full_place = doctor_data
        .get("fullPlaceDetail")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut city_slugs = Vec::new();
    let mut city_names = Vec::new();
    for entry in &city_entries {
        if let Some(text) = entry.as_str() {
            city_names.push(clean_text(text));
        } else {
            city_slugs.extend(
                values_for_keys(entry, &["slug", "citySlug", "city_slug"], 0)
                    .into_iter()
                    .map(|slug| normalize_slug(&slug))
                    .filter(|slug| !slug.is_empty()),
            );
            city_names.extend(values_for_keys(
                entry,
                &["cityName", "name", "title", "label"],
                0,
            ));
        }
    }
    city_slugs.extend(
        values_for_keys(&full_place, &["citySlug", "city_slug", "slug"], 0)
            .into_iter()
            .map(|slug| normalize_slug(&slug))
            .filter(|slug| !slug.is_empty()),
    );
    city_names.extend(values_for_keys(
        &full_place,
        &["cityName", "name", "title"],
        0,
    ));
    city_slugs = unique(city_slugs);
    city_names = unique(
        city_names
            .into_iter()
            .map(|name| clean_text(&name))
            .filter(|name| !name.is_empty()),
    );

    let mut phones = extract_phones_from_html(html);
    phones.extend(extract_phones_after_reveal(&page_text));
    phones = unique(phones);
    let title = title_from_page(&document);
    let avatar_source = first_string(&[
        doctor_data.get("avatarUrl").unwrap_or(&Value::Null),
        doctor_data.get("avatar").unwrap_or(&Value::Null),
    ]);
    let avatar_fallback = first_doctor_image(
        &document,
        doctor_data
            .get("fullName")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    let avatar_url = resolve_avatar_url(
        if !avatar_source.is_empty() {
            &avatar_source
        } else {
            &avatar_fallback
        },
        &document,
    );
    let rate = doctor_data
        .get("rate")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let endorsements = doctor_data
        .get("endorsementCount")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut other_queries = Vec::new();
    for query in &queries {
        let is_doctor_query = query
            .pointer("/queryKey")
            .and_then(Value::as_array)
            .is_some_and(|key| key.len() == 2 && key[0].as_str() == Some("doctors"));
        if !is_doctor_query {
            other_queries.push(json!({
                "queryKey": query.get("queryKey"),
                "data": query.pointer("/state/data").cloned().unwrap_or(Value::Null)
            }));
        }
    }
    let mut other_props = page_props.as_object().cloned().unwrap_or_default();
    other_props.remove("dehydratedState");
    let clinic_locations = extract_clinic_locations(&queries, &doctor_data, &fallback_id);

    let mut record = DoctorRecord {
        id: first_string(&[
            doctor_data.get("id").unwrap_or(&Value::Null),
            &Value::String(fallback_id.clone()),
        ]),
        slug: first_string(&[
            doctor_data.get("slug").unwrap_or(&Value::Null),
            &Value::String(
                path_parts
                    .get(path_parts.len().saturating_sub(2))
                    .cloned()
                    .unwrap_or_default(),
            ),
        ]),
        profile_url: profile_url.to_owned(),
        full_name: first_string(&[
            doctor_data.get("fullName").unwrap_or(&Value::Null),
            doctor_data.get("name").unwrap_or(&Value::Null),
            &Value::String(title.clone()),
        ]),
        title: first_string(&[
            doctor_data.get("title").unwrap_or(&Value::Null),
            doctor_data.get("profession").unwrap_or(&Value::Null),
        ]),
        honor: first_string(&[doctor_data.get("honor").unwrap_or(&Value::Null)]),
        medical_number: first_string(&[
            doctor_data.get("medicalNumber").unwrap_or(&Value::Null),
            doctor_data.get("medical_number").unwrap_or(&Value::Null),
        ]),
        city_slugs,
        city_names,
        specialties: specialities,
        phones,
        addresses: extract_addresses_from_sections(&page_sections),
        clinic_locations,
        avatar_url,
        image_file: String::new(),
        page_html: String::new(),
        about_html: first_string(&[doctor_data.get("about").unwrap_or(&Value::Null)]),
        about_text: html_to_text(&first_string(&[doctor_data
            .get("about")
            .unwrap_or(&Value::Null)])),
        rating_average: first_string(&[
            rate.get("rateAverage").unwrap_or(&Value::Null),
            rate.get("average").unwrap_or(&Value::Null),
        ]),
        rating_count: first_string(&[rate.get("count").unwrap_or(&Value::Null)]),
        recommendation_percent: first_string(&[rate.get("percent").unwrap_or(&Value::Null)]),
        recommendation_count: first_string(&[rate.get("recommend").unwrap_or(&Value::Null)]),
        endorsement_count: first_string(&[endorsements.get("total").unwrap_or(&Value::Null)]),
        completed_orders: first_string(&[doctor_data
            .get("countDoneOrders")
            .unwrap_or(&Value::Null)]),
        is_on_call: first_string(&[doctor_data.get("isOnCall").unwrap_or(&Value::Null)]),
        is_featured: first_string(&[doctor_data.get("isAd").unwrap_or(&Value::Null)]),
        waiting_time: doctor_data
            .get("presenceWaitingTime")
            .cloned()
            .unwrap_or(Value::Null),
        next_free_times: doctor_data
            .get("nextFreeTimes")
            .cloned()
            .unwrap_or(Value::Null),
        service_tags,
        insurances,
        social_links: doctor_data
            .get("socialLinks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        page_sections,
        page_text,
        structured_profile_data: if settings.enabled("structured_data") {
            json!({
                "doctor": doctor_data,
                "otherPageQueries": other_queries,
                "pageProps": Value::Object(other_props)
            })
        } else {
            Value::Null
        },
        listing_city_hints: Vec::new(),
        scrape_error: None,
    };
    if settings.enabled("raw_html") {
        record.page_html = html.to_owned();
    }
    apply_scrape_settings(&mut record, settings);
    Ok(record)
}

pub fn apply_scrape_settings(record: &mut DoctorRecord, settings: &Settings) {
    if !settings.enabled("profile_identity") {
        record.full_name.clear();
        record.title.clear();
        record.honor.clear();
        record.medical_number.clear();
    }
    if !settings.enabled("profile_photo") {
        record.avatar_url.clear();
        record.image_file.clear();
    }
    if !settings.enabled("specialties") {
        record.specialties.clear();
    }
    if !settings.enabled("contact_details") {
        record.phones.clear();
    }
    if !settings.enabled("clinic_addresses") {
        record.addresses.clear();
    }
    if !settings.enabled("clinic_coordinates") {
        record.clinic_locations.clear();
        remove_geo_fields(&mut record.structured_profile_data);
    }
    if !settings.enabled("about") {
        record.about_html.clear();
        record.about_text.clear();
    }
    if !settings.enabled("profile_statistics") {
        record.rating_average.clear();
        record.rating_count.clear();
        record.recommendation_percent.clear();
        record.recommendation_count.clear();
        record.completed_orders.clear();
        record.is_featured.clear();
    }
    if !settings.enabled("doctor_endorsements") {
        record.endorsement_count.clear();
    }
    if !settings.enabled("appointments") {
        record.waiting_time = Value::Null;
        record.next_free_times = Value::Null;
        record.is_on_call.clear();
    }
    if !settings.enabled("services") {
        record.service_tags.clear();
    }
    if !settings.enabled("insurances") {
        record.insurances.clear();
    }
    if !settings.enabled("social_links") {
        record.social_links.clear();
    }

    let visible_sections: Vec<PageSection> = record
        .page_sections
        .iter()
        .filter(|section| {
            let category = if section.category.is_empty() {
                classify_section_category(&section.heading)
            } else {
                &section.category
            };
            settings.enabled(category)
        })
        .cloned()
        .collect();
    if !settings.enabled("full_page_text") {
        record.page_text.clear();
    }
    if settings.enabled("page_sections") {
        record.page_sections = visible_sections;
    } else {
        record.page_sections.clear();
    }
    if !settings.enabled("structured_data") {
        record.structured_profile_data = Value::Null;
    }
    if !settings.enabled("raw_html") {
        record.page_html.clear();
    }
}

fn extract_clinic_locations(
    queries: &[Value],
    doctor_data: &Value,
    doctor_id: &str,
) -> Vec<ClinicLocation> {
    let mut locations = Vec::new();
    if let Some(place) = doctor_data.get("fullPlaceDetail") {
        push_clinic_location(
            place,
            place.get("coordinates").or_else(|| place.get("location")),
            &mut locations,
        );
    }
    for query in queries {
        let Some(key) = query.pointer("/queryKey").and_then(Value::as_array) else {
            continue;
        };
        let is_doctor_consultation = key.first().and_then(Value::as_str) == Some("consultations")
            && key.get(1).and_then(Value::as_str) == Some("doctors")
            && key.get(2).and_then(Value::as_str) == Some(doctor_id);
        let is_doctor_offer = key.first().and_then(Value::as_str) == Some("offers")
            && key.get(1).and_then(Value::as_str) == Some("filtered")
            && query
                .pointer("/queryKey/2/doctorId")
                .and_then(Value::as_str)
                == Some(doctor_id);
        if (is_doctor_consultation || is_doctor_offer)
            && let Some(data) = query.pointer("/state/data")
        {
            collect_place_locations(data, &mut locations);
        }
    }
    deduplicate_clinic_locations(locations)
}

fn collect_place_locations(value: &Value, locations: &mut Vec<ClinicLocation>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_place_locations(item, locations);
            }
        }
        Value::Object(object) => {
            let clinic_place = object.get("place");
            if let Some(place) = clinic_place {
                if let Some(coordinates) =
                    place.get("coordinates").or_else(|| place.get("location"))
                {
                    push_clinic_location(place, Some(coordinates), locations);
                }
                if let Some(consultations) = object.get("consultations").and_then(Value::as_array) {
                    for consultation in consultations {
                        let Some(consultation_place) = consultation.get("place") else {
                            continue;
                        };
                        let coordinates = consultation_place
                            .get("location")
                            .or_else(|| consultation_place.get("coordinates"));
                        push_clinic_location(place, coordinates, locations);
                    }
                }
            }
            for (key, child) in object {
                if !matches!(
                    key.as_str(),
                    "place" | "consultations" | "city" | "province"
                ) {
                    collect_place_locations(child, locations);
                }
            }
        }
        _ => {}
    }
}

fn push_clinic_location(
    place: &Value,
    coordinates: Option<&Value>,
    locations: &mut Vec<ClinicLocation>,
) {
    let Some((latitude, longitude)) = coordinates.and_then(parse_coordinate_pair) else {
        return;
    };
    locations.push(ClinicLocation {
        name: location_text(place, &["name", "fullName", "clinicName", "title"]),
        latitude,
        longitude,
    });
}

fn parse_coordinate_pair(value: &Value) -> Option<(f64, f64)> {
    let latitude = number_from_value(value.get("latitude").or_else(|| value.get("lat"))?)?;
    let longitude = number_from_value(
        value
            .get("longitude")
            .or_else(|| value.get("lng"))
            .or_else(|| value.get("lon"))?,
    )?;
    if !latitude.is_finite()
        || !longitude.is_finite()
        || !(-90.0..=90.0).contains(&latitude)
        || !(-180.0..=180.0).contains(&longitude)
        || (latitude == 0.0 && longitude == 0.0)
    {
        return None;
    }
    Some((latitude, longitude))
}

fn number_from_value(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| {
        value
            .as_str()
            .and_then(|text| normalize_digits(text).trim().parse::<f64>().ok())
    })
}

fn location_text(place: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| place.get(*key).and_then(Value::as_str).map(clean_text))
        .unwrap_or_default()
}

fn deduplicate_clinic_locations(locations: Vec<ClinicLocation>) -> Vec<ClinicLocation> {
    let mut unique_locations: Vec<ClinicLocation> = Vec::new();
    for location in locations {
        if let Some(existing) = unique_locations.iter_mut().find(|existing| {
            existing.latitude == location.latitude
                && existing.longitude == location.longitude
                && (existing.name.is_empty()
                    || location.name.is_empty()
                    || crate::util::normalize_city_name(&existing.name)
                        == crate::util::normalize_city_name(&location.name))
        }) {
            if existing.name.is_empty() {
                existing.name = location.name;
            }
        } else {
            unique_locations.push(location);
        }
    }
    unique_locations
}

fn remove_geo_fields(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                remove_geo_fields(item);
            }
        }
        Value::Object(object) => {
            object.retain(|key, _| {
                !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "latitude" | "longitude" | "lat" | "lng" | "lon" | "coordinates"
                )
            });
            for child in object.values_mut() {
                remove_geo_fields(child);
            }
        }
        _ => {}
    }
}

fn parse_next_data(document: &Html) -> Value {
    let selector = Selector::parse("script#__NEXT_DATA__").ok();
    selector
        .and_then(|selector| document.select(&selector).next())
        .and_then(|element| serde_json::from_str(element.inner_html().trim()).ok())
        .unwrap_or(Value::Null)
}

fn find_doctor_like_object<'a>(value: &'a Value, id: &str, depth: usize) -> Option<&'a Value> {
    if depth > 7 {
        return None;
    }
    match value {
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_doctor_like_object(item, id, depth + 1)),
        Value::Object(object) => {
            let matches = object
                .get("id")
                .or_else(|| object.get("doctorId"))
                .and_then(|value| value.as_str())
                == Some(id)
                && (object.contains_key("fullName")
                    || object.contains_key("name")
                    || object.contains_key("avatar"));
            if matches {
                return Some(value);
            }
            object
                .values()
                .find_map(|child| find_doctor_like_object(child, id, depth + 1))
        }
        _ => None,
    }
}

fn page_root(document: &Html) -> Option<ElementRef<'_>> {
    let main = Selector::parse("main").ok()?;
    if let Some(element) = document.select(&main).next() {
        return Some(element);
    }
    Selector::parse("#__next")
        .ok()
        .and_then(|selector| document.select(&selector).next())
}

fn extract_page_text(document: &Html) -> String {
    let Some(root) = page_root(document) else {
        return String::new();
    };
    let html = root.inner_html();
    html_to_text(&html)
}

fn extract_sections(document: &Html) -> Vec<PageSection> {
    let Some(root) = page_root(document) else {
        return Vec::new();
    };
    let Ok(selector) = Selector::parse("h1, h2, h3, h4") else {
        return Vec::new();
    };
    let mut sections = Vec::new();
    for heading in root.select(&selector) {
        let title = clean_text(&heading.text().collect::<Vec<_>>().join(" "));
        if title.is_empty() {
            continue;
        }
        let parent = heading
            .parent()
            .and_then(ElementRef::wrap)
            .unwrap_or(heading);
        let mut text = html_to_text(&parent.inner_html());
        if text.starts_with(&title) {
            text = text[title.len()..].trim().to_owned()
        }
        if !text.is_empty()
            && !sections
                .iter()
                .any(|section: &PageSection| section.heading == title && section.text == text)
        {
            let category = classify_section_category(&title).to_owned();
            sections.push(PageSection {
                heading: title,
                category,
                text,
            });
        }
    }
    sections
}

fn classify_section_category(heading: &str) -> &'static str {
    let heading = crate::util::normalize_city_name(heading);
    if heading.contains("سوالات متداول") || heading.contains("سؤالات متداول")
    {
        "faqs"
    } else if heading.contains("راهنمای نوبت")
        || heading.contains("بیوگرافی و معرفی")
        || heading.contains("زمینه تخصص")
        || heading.contains("برای چه بیماری")
        || heading.contains("کدام مرکز درمانی")
        || heading.contains("شهرهای فعالیت")
    {
        "editorial_guide"
    } else if heading.contains("نظرات پزشکان درباره")
        || heading.contains("تاییدیه")
        || heading.contains("توصیه پزشکان")
    {
        "doctor_endorsements"
    } else if heading.contains("نظرات درباره") || heading.contains("نظرات کاربران")
    {
        "patient_reviews"
    } else if heading.contains("تخصص") {
        "specialties"
    } else if heading.contains("درباره دکتر") {
        "about"
    } else if heading.contains("آدرس")
        || heading.starts_with("مطب ")
        || heading == "مطب"
        || heading.contains("کلینیک")
        || heading.starts_with("مرکز درمانی")
        || heading.starts_with("مرکز جامع")
    {
        "clinic_addresses"
    } else if heading.contains("نوبت")
        || heading.contains("اولین نوبت")
        || heading.contains("ساعت کاری")
    {
        "appointments"
    } else if heading.contains("مشاوره") {
        "consultations"
    } else if heading.contains("پیشنهاد") {
        "recommendation_cards"
    } else if heading.contains("خدمات دکترتو کلینیک")
        || heading.contains("هزینه خدمات")
        || heading.contains("هزینه ویزیت")
    {
        "service_price_cards"
    } else if heading.contains("بیمه") {
        "insurances"
    } else if heading.contains("خدمات") || heading.contains("بیماری") || heading.contains("علائم")
    {
        "services"
    } else if heading.contains("پزشکان دیگر") || heading.contains("برخی از پزشکان")
    {
        "related_doctors"
    } else if heading.contains("اطلاعات پزشک") {
        "profile_identity"
    } else {
        "other_page_sections"
    }
}

fn html_to_text(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    let break_tags = BREAK_TAGS.get_or_init(|| {
        Regex::new(r"(?is)<br\s*/?>|</(?:p|div|li|h[1-6]|section|article|tr)>\s*").unwrap()
    });
    let broken = break_tags.replace_all(html, |captures: &regex::Captures<'_>| {
        if captures
            .get(0)
            .is_some_and(|matched| matched.as_str().to_ascii_lowercase().starts_with("<br"))
        {
            "\n".to_owned()
        } else {
            format!(
                "{}\n",
                captures
                    .get(0)
                    .map(|item| item.as_str())
                    .unwrap_or_default()
            )
        }
    });
    let fragment = Html::parse_fragment(&broken);
    let Ok(ignored) = Selector::parse("script, style, noscript, svg, form, nav, header, footer")
    else {
        return String::new();
    };
    let mut output = String::new();
    for node in fragment.tree.root().descendants() {
        if let Some(element) = ElementRef::wrap(node)
            && ignored.matches(&element)
        {
            continue;
        }
        if let Some(text) = node.value().as_text() {
            output.push_str(text);
        }
    }
    clean_text(&output)
}

fn title_from_page(document: &Html) -> String {
    Selector::parse("h1")
        .ok()
        .and_then(|selector| document.select(&selector).next())
        .map(|element| clean_text(&element.text().collect::<Vec<_>>().join(" ")))
        .filter(|title| !title.is_empty())
        .or_else(|| {
            let selector = Selector::parse("meta[property=\"og:title\"]").ok()?;
            document
                .select(&selector)
                .next()?
                .value()
                .attr("content")
                .map(clean_text)
        })
        .unwrap_or_default()
}

fn first_doctor_image(document: &Html, full_name: &str) -> Option<String> {
    let selector = Selector::parse("img").ok()?;
    document.select(&selector).find_map(|image| {
        let alt = clean_text(image.value().attr("alt").unwrap_or_default());
        let source = image
            .value()
            .attr("src")
            .or_else(|| image.value().attr("data-src"))?;
        let expected_name = full_name.is_empty() || alt.contains(full_name);
        (expected_name && Regex::new("(?i)avatar|doctor|drto").ok()?.is_match(source))
            .then(|| source.to_owned())
    })
}

fn resolve_avatar_url(value: &str, document: &Html) -> String {
    let candidate = if value.is_empty() {
        Selector::parse("meta[property=\"og:image\"]")
            .ok()
            .and_then(|selector| document.select(&selector).next())
            .and_then(|element| element.value().attr("content"))
            .unwrap_or_default()
            .to_owned()
    } else {
        value.to_owned()
    };
    if candidate.starts_with("https://") {
        return candidate;
    }
    if let Some(rest) = candidate.strip_prefix("http://") {
        return format!("https://{rest}");
    }
    if candidate.starts_with("s3://") {
        return format!("https://statics.doctoreto.com/rt:fit/s:600/plain/{candidate}");
    }
    if candidate.starts_with('/') {
        return Url::parse(BASE_URL)
            .and_then(|base| base.join(&candidate))
            .map(|url| url.to_string())
            .unwrap_or_default();
    }
    String::new()
}

pub fn extract_phones_from_html(html: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    extract_phones_from_document(&document)
}

fn extract_phones_from_document(document: &Html) -> Vec<String> {
    let Ok(selector) = Selector::parse("a[href^=\"tel:\"]") else {
        return Vec::new();
    };
    unique(
        document
            .select(&selector)
            .filter_map(|anchor| anchor.value().attr("href"))
            .map(|href| {
                clean_phone(
                    href.strip_prefix("tel:")
                        .or_else(|| href.strip_prefix("TEL:"))
                        .unwrap_or(href),
                )
            })
            .filter(|phone| !phone.is_empty()),
    )
}

pub fn extract_phones_after_reveal(text: &str) -> Vec<String> {
    let normalized = normalize_digits(text);
    let pattern = PHONE_REVEAL.get_or_init(|| Regex::new(r"(?i)(?:شماره(?:‌|\s*)تماس|تلفن|تماس با مطب)[^0-9]{0,40}((?:\+?98|0098|0)[0-9\s\-()]{8,18})").unwrap());
    unique(
        pattern
            .captures_iter(&normalized)
            .filter_map(|capture| capture.get(1))
            .map(|capture| clean_phone(capture.as_str()))
            .filter(|phone| !phone.is_empty()),
    )
}

pub fn has_phone_reveal_control(html: &str) -> bool {
    let document = Html::parse_document(html);
    let Ok(selector) = Selector::parse("button,[role='button'],a") else {
        return false;
    };
    let pattern = PHONE_REVEAL_CONTROL.get_or_init(|| {
        Regex::new(r"(?is)(?:شماره|تلفن|تماس).{0,40}(?:نمایش|مشاهده|دریافت)|(?:نمایش|مشاهده|دریافت).{0,40}(?:شماره|تلفن|تماس)").unwrap()
    });
    document.select(&selector).any(|control| {
        let label = [
            control.text().collect::<Vec<_>>().join(" "),
            control
                .value()
                .attr("aria-label")
                .unwrap_or_default()
                .to_owned(),
            control.value().attr("title").unwrap_or_default().to_owned(),
        ]
        .join(" ");
        pattern.is_match(&label)
    })
}

pub fn clean_phone(input: &str) -> String {
    let value = normalize_digits(input)
        .chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '+')
        .collect::<String>();
    if let Some(rest) = value.strip_prefix("0098") {
        return format!("+98{rest}");
    }
    if value.starts_with("98") && value.len() >= 11 {
        return format!("+{value}");
    }
    if value.starts_with('9') && value.len() == 10 {
        return format!("0{value}");
    }
    value
}

#[cfg(test)]
pub fn extract_addresses_from_page_text(page_text: &str) -> Vec<String> {
    let normalized = crate::util::normalize_line_breaks(page_text);
    let lines: Vec<&str> = normalized.lines().collect();
    let heading = ADDRESS_HEAD.get_or_init(|| {
        Regex::new(r"(?mi)^\s*(?:آدرس\s+مطب(?:\s+دکتر[^\n]*)?|مطب(?:\s+[^:\n]{2,})?|کلینیک(?:\s+[^:\n]{2,})?|مرکز(?:\s+درمانی)?(?:\s+[^:\n]{2,})?)\s*$").unwrap()
    });
    let next = NEXT_SECTION.get_or_init(|| Regex::new(r"(?mi)(?:^|\n)\s*(?:نظرات(?:\s|$)|سوالات\s+متداول|سؤالات\s+متداول|درباره\s+دکتر|پیشنهاد(?:های)?\s+دکتر|تخصص\s+های\s+دکتر)").unwrap());
    let mut inside_clinic_section = false;
    let mut clinic_lines = Vec::new();
    for line in &lines {
        if heading.is_match(line) {
            inside_clinic_section = true;
            continue;
        }
        if inside_clinic_section && next.is_match(line) {
            break;
        }
        if inside_clinic_section {
            clinic_lines.push(clean_text(line));
        }
    }
    addresses_from_clinic_lines(&clinic_lines)
}

fn addresses_from_clinic_lines(lines: &[String]) -> Vec<String> {
    let address_label =
        ADDRESS_LABEL.get_or_init(|| Regex::new(r"(?i)^آدرس\s*[:：]?\s*(.*)$").unwrap());
    let stop = ADDRESS_STOP.get_or_init(|| Regex::new(r"(?i)^(?:آدرس|تلفن|شماره(?:\s*تماس)?|مسیریابی|نقشه|اولین\s*نوبت|نوبت\s*بگیرید|در\s*حال\s*بارگذاری)\s*[:：]?\s*").unwrap());
    let mut addresses = Vec::new();
    for index in 0..lines.len() {
        let Some(label) = address_label.captures(&lines[index]) else {
            continue;
        };
        let mut parts = Vec::new();
        if let Some(text) = label
            .get(1)
            .map(|value| value.as_str())
            .filter(|text| !text.trim().is_empty())
        {
            parts.push(text.to_owned());
        }
        for line in lines.iter().skip(index + 1) {
            if stop.is_match(line) {
                break;
            }
            parts.push(line.clone());
        }
        let address = clean_text(&parts.join(" "));
        if address.chars().count() >= 8 {
            addresses.push(address);
        }
    }
    unique(addresses)
}

pub fn extract_addresses_from_sections(sections: &[PageSection]) -> Vec<String> {
    let heading = ADDRESS_HEAD.get_or_init(|| {
        Regex::new(r"(?mi)^\s*(?:آدرس\s+مطب(?:\s+دکتر[^\n]*)?|مطب(?:\s+[^:\n]{2,})?|کلینیک(?:\s+[^:\n]{2,})?|مرکز(?:\s+درمانی)?(?:\s+[^:\n]{2,})?)\s*$").unwrap()
    });
    let mut addresses = Vec::new();
    for section in sections {
        if section.category != "clinic_addresses" && !heading.is_match(&section.heading) {
            continue;
        }
        let lines: Vec<String> = section
            .text
            .lines()
            .map(clean_text)
            .filter(|line| !line.is_empty())
            .collect();
        addresses.extend(addresses_from_clinic_lines(&lines));
    }
    unique(addresses)
}

pub fn extract_links(
    html: &str,
    base_url: &str,
    city_labels: &mut std::collections::HashMap<String, String>,
) -> ParsedLinks {
    let document = Html::parse_document(html);
    let Ok(selector) = Selector::parse("a[href]") else {
        return ParsedLinks::default();
    };
    let mut links = ParsedLinks::default();
    for anchor in document.select(&selector) {
        let href = anchor.value().attr("href").unwrap_or_default();
        let Some(url) = crate::util::safe_site_url(href, base_url) else {
            continue;
        };
        if crate::util::is_profile_url(&url) {
            links.profile_urls.insert(url);
            continue;
        }
        if !crate::util::is_listing_url(&url) {
            continue;
        }
        links.listing_urls.insert(url.clone());
        if let Some(city) = crate::util::city_slug_from_url(&url) {
            let label =
                city_label_from_anchor(&clean_text(&anchor.text().collect::<Vec<_>>().join(" ")));
            city_labels.entry(city.clone()).or_insert_with(|| {
                if !label.is_empty() && label.chars().count() < 40 {
                    label
                } else {
                    crate::util::default_city_name(&city)
                }
            });
        }
        if let Some(specialty) = crate::util::specialty_slug_from_url(&url) {
            links.specialty_slugs.insert(specialty);
        }
    }
    let urls = embedded_urls(html, base_url);
    for url in urls {
        if crate::util::is_profile_url(&url) {
            links.profile_urls.insert(url);
        } else if crate::util::is_listing_url(&url) {
            if let Some(city) = crate::util::city_slug_from_url(&url) {
                city_labels
                    .entry(city.clone())
                    .or_insert_with(|| crate::util::default_city_name(&city));
            }
            if let Some(specialty) = crate::util::specialty_slug_from_url(&url) {
                links.specialty_slugs.insert(specialty);
            }
            links.listing_urls.insert(url);
        }
    }
    links
        .listing_urls
        .extend(extract_pagination_urls(html, base_url));
    links
}

#[derive(Clone, Debug, Default)]
pub struct ParsedLinks {
    pub profile_urls: std::collections::HashSet<String>,
    pub listing_urls: std::collections::HashSet<String>,
    pub specialty_slugs: std::collections::HashSet<String>,
}

fn city_label_from_anchor(value: &str) -> String {
    let text = Regex::new(r"(?i)^\s*(?:بهترین\s*)?(?:دکتر(?:ان)?|پزشک(?:ان)?)\s*")
        .ok()
        .map(|regex| regex.replace(value, "").to_string())
        .unwrap_or_else(|| value.to_owned());
    clean_text(
        &Regex::new(r"(?i)\s*[0-9۰-۹٠-٩+]+\s*\+?\s*(?:دکتر|پزشک).*|\s*(?:دکتر|پزشک)\s*$")
            .unwrap()
            .replace(&text, ""),
    )
}

fn embedded_urls(html: &str, base_url: &str) -> Vec<String> {
    let pattern = Regex::new(r#"(?i)(?:https?:)?\\?/\\?/doctoreto\.com[^\"'\s<>\\]*|/doctors?/(?:city|speciality|service)/[^\"'\s<>\\]+|/doctor/[^\"'\s<>\\]+"#).unwrap();
    unique(pattern.find_iter(html).filter_map(|matched| {
        let unescaped = crate::util::unescape_json_url(matched.as_str());
        let raw = unescaped.trim_start_matches("//");
        crate::util::safe_site_url(raw, base_url)
    }))
}

fn extract_pagination_urls(html: &str, base_url: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let data = parse_next_data(&document);
    let queries = data
        .pointer("/props/pageProps/dehydratedState/queries")
        .and_then(Value::as_array);
    let Some(query) = queries.and_then(|queries| {
        queries.iter().find(|query| {
            let key = query.pointer("/queryKey").and_then(Value::as_array);
            key.is_some_and(|key| {
                key.len() >= 3
                    && key[0].as_str() == Some("doctors")
                    && key[1].as_str() == Some("list")
                    && key[2].as_str() == Some("plp")
            })
        })
    }) else {
        return Vec::new();
    };
    let page = query
        .pointer("/state/data/pagination")
        .unwrap_or(&Value::Null);
    let Some(last_page) = page
        .get("lastPage")
        .and_then(value_as_u64)
        .filter(|n| (2..=10_000).contains(n))
    else {
        return Vec::new();
    };
    let Ok(source) = Url::parse(base_url) else {
        return Vec::new();
    };
    let current = page
        .get("currentPage")
        .and_then(value_as_u64)
        .or_else(|| {
            source
                .query_pairs()
                .find(|(key, _)| key == "page")
                .and_then(|(_, value)| value.parse().ok())
        })
        .unwrap_or(1);
    (1..=last_page)
        .filter(|number| *number != current)
        .filter_map(|number| {
            let mut page_url = source.clone();
            let remaining: Vec<(String, String)> = page_url
                .query_pairs()
                .filter(|(key, _)| key != "page")
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            page_url.set_query(None);
            if !remaining.is_empty() {
                page_url.query_pairs_mut().extend_pairs(remaining);
            }
            if number > 1 {
                page_url
                    .query_pairs_mut()
                    .append_pair("page", &number.to_string());
            }
            crate::util::safe_site_url(page_url.as_str(), BASE_URL)
        })
        .collect()
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

pub fn extract_sitemap_locations(xml: &str, base_url: &str) -> Vec<String> {
    let pattern = Regex::new(r"(?is)<loc[^>]*>\s*(.*?)\s*</loc>").unwrap();
    unique(
        pattern
            .captures_iter(xml)
            .filter_map(|capture| capture.get(1))
            .filter_map(|value| {
                crate::util::safe_site_url(&crate::util::decode_entities(value.as_str()), base_url)
            }),
    )
}

pub fn extract_city_slugs_from_source(html: &str) -> Vec<String> {
    let pattern = Regex::new(r#"(?i)/doctors/(?:speciality/[^/"'?]+/)?city/([^/"'?]+)"#).unwrap();
    unique(
        pattern
            .captures_iter(&crate::util::unescape_json_url(html))
            .filter_map(|capture| capture.get(1))
            .map(|value| normalize_slug(value.as_str()))
            .filter(|slug| !slug.is_empty()),
    )
}

pub fn is_pagination_audit_candidate(raw_url: &str) -> bool {
    let Ok(url) = Url::parse(raw_url) else {
        return false;
    };
    let path = url.path();
    let valid = path.starts_with("/doctors/city/")
        || path.starts_with("/doctors/speciality/")
        || path.starts_with("/doctors/service/")
        || path.starts_with("/doctors/telehealth");
    valid
        && !path.ends_with(".js")
        && !path.contains("/consultation/")
        && !url.query_pairs().any(|(key, _)| key == "page")
}

pub fn robots_path_matches(path: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let anchored = pattern.ends_with('$');
    let body = if anchored {
        &pattern[..pattern.len() - 1]
    } else {
        pattern
    };
    let escaped = regex::escape(body).replace(r"\*", ".*");
    Regex::new(&format!(
        "^{}{end}",
        escaped,
        end = if anchored { "$" } else { "" }
    ))
    .is_ok_and(|regex| regex.is_match(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biography_addresses_are_not_exported() {
        let text = "\nآدرس مطب دکتر مینا\nمطب وکیل آباد\nآدرس: مشهد، بلوار وکیل آباد، ساختمان پزشکان\nتلفن: 05112345678\nدرباره دکتر\nآدرس: این متن زندگی‌نامه است و نباید نشانی شود\n";
        assert_eq!(
            extract_addresses_from_page_text(text),
            vec!["مشهد، بلوار وکیل آباد، ساختمان پزشکان"]
        );
    }

    #[test]
    fn address_parser_returns_nothing_without_the_clinic_section() {
        let text = "\nدرباره دکتر\nآدرس: نشانی داخل زندگی‌نامه که ممنوع است\n";
        assert!(extract_addresses_from_page_text(text).is_empty());
    }

    #[test]
    fn section_address_extraction_ignores_biography_sections() {
        let sections = vec![
            PageSection {
                heading: "درباره دکتر نمونه".to_owned(),
                category: "about".to_owned(),
                text: "آدرس مطب: این متن زندگی‌نامه است و نشانی مطب نیست".to_owned(),
            },
            PageSection {
                heading: "آدرس مطب دکتر نمونه".to_owned(),
                category: "clinic_addresses".to_owned(),
                text: "مطب مرکزی\nآدرس: تهران، خیابان نمونه\nتلفن: 02112345678".to_owned(),
            },
        ];
        assert_eq!(
            extract_addresses_from_sections(&sections),
            vec!["تهران، خیابان نمونه"]
        );
    }

    #[test]
    fn phone_numbers_are_normalized_from_persian_digits() {
        assert_eq!(clean_phone("۰۹۱۲ ۳۴۵ ۶۷۸۹"), "09123456789");
        assert_eq!(clean_phone("00985138656600"), "+985138656600");
    }

    #[test]
    fn hidden_phone_buttons_are_detected_before_browser_fallback() {
        assert!(has_phone_reveal_control(
            r#"<button aria-label="مشاهده شماره تلفن">مشاهده</button>"#
        ));
        assert!(!has_phone_reveal_control("<button>نوبت بگیرید</button>"));
    }

    #[test]
    fn profile_fields_come_from_structured_data_and_visible_text() {
        let html = r#"<html><head><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"dehydratedState":{"queries":[{"queryKey":["doctors","abc"],"state":{"data":{"id":"abc","fullName":"دکتر نمونه","specialities":[{"name":"قلب"}],"avatar":"s3://drto/avatar/doctor/example.jpg","cities":[{"slug":"tehran","name":"تهران"}],"rate":{"average":4.8,"count":12}}}}]}}}}</script></head><body><main><h1>دکتر نمونه</h1><section><h2>آدرس مطب دکتر نمونه</h2><div>آدرس: تهران، خیابان نمونه</div></section></main></body></html>"#;
        let record = parse_doctor_page(
            html,
            "https://doctoreto.com/doctor/sample/abc",
            &Settings::default(),
        )
        .expect("synthetic page parses");
        assert_eq!(record.full_name, "دکتر نمونه");
        assert_eq!(record.specialties, vec!["قلب"]);
        assert_eq!(record.addresses, vec!["تهران، خیابان نمونه"]);
        assert_eq!(record.city_slugs, vec!["tehran"]);
    }

    #[test]
    fn listing_pagination_keeps_existing_filters_and_discovers_each_page() {
        let html = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"dehydratedState":{"queries":[{"queryKey":["doctors","list","plp"],"state":{"data":{"pagination":{"currentPage":1,"lastPage":3}}}}]}}}}</script>"#;
        let urls = extract_pagination_urls(
            html,
            "https://doctoreto.com/doctors/city/tehran?sort=top&page=1",
        );
        assert_eq!(
            urls,
            vec![
                "https://doctoreto.com/doctors/city/tehran?sort=top&page=2",
                "https://doctoreto.com/doctors/city/tehran?sort=top&page=3",
            ]
        );
    }
}
