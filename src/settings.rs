// Author: ahura / @mabae
use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;

const SCRAPE_PARTS: &[&str] = &[
    "PROFILE_IDENTITY",
    "PROFILE_PHOTO",
    "SPECIALTIES",
    "CONTACT_DETAILS",
    "REVEAL_HIDDEN_PHONES",
    "CLINIC_ADDRESSES",
    "CLINIC_COORDINATES",
    "ABOUT",
    "PROFILE_STATISTICS",
    "APPOINTMENTS",
    "CONSULTATIONS",
    "RECOMMENDATION_CARDS",
    "SERVICE_PRICE_CARDS",
    "SERVICES",
    "INSURANCES",
    "DOCTOR_ENDORSEMENTS",
    "PATIENT_REVIEWS",
    "FAQS",
    "EDITORIAL_GUIDE",
    "RELATED_DOCTORS",
    "SOCIAL_LINKS",
    "OTHER_PAGE_SECTIONS",
    "PAGE_SECTIONS",
    "FULL_PAGE_TEXT",
    "STRUCTURED_DATA",
    "RAW_HTML",
];

#[derive(Clone, Debug)]
pub struct Settings {
    scrape_parts: HashMap<String, bool>,
    all_cities_enabled: bool,
    enabled_cities: HashSet<String>,
    disabled_cities: HashSet<String>,
    pub expand_review_list: bool,
    pub expand_doctor_endorsements: bool,
    pub review_expansion_limit: usize,
    pub config_path: Option<PathBuf>,
}

impl Settings {
    pub fn load() -> Result<Self> {
        let current_path = env::current_dir()?.join(".env");
        let project_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
        let config_path = if current_path.is_file() {
            current_path
        } else {
            project_path
        };
        let mut values = match fs::read_to_string(&config_path) {
            Ok(contents) => parse_env_file(&contents)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error).context("could not read .env"),
        };
        for (key, value) in env::vars() {
            values.insert(key, value);
        }
        let mut settings = Self::from_values(values)?;
        settings.config_path = config_path.is_file().then_some(config_path);
        Ok(settings)
    }

    fn from_values(values: HashMap<String, String>) -> Result<Self> {
        let scrape_all = bool_value(&values, "SCRAPE_ALL", true)?;
        let mut scrape_parts = HashMap::new();
        for part in SCRAPE_PARTS {
            let key = format!("SCRAPE_{part}");
            let default = if *part == "RAW_HTML" {
                false
            } else {
                scrape_all
            };
            scrape_parts.insert(
                part.to_ascii_lowercase(),
                scrape_part_value(&values, &key, default)?,
            );
        }
        let enabled_values = list_value(&values, "CITIES_ENABLED", "*");
        let all_cities_enabled =
            enabled_values.is_empty() || enabled_values.iter().any(|city| city == "*");
        let enabled_cities = normalize_city_list(enabled_values);
        let disabled_cities = normalize_city_list(list_value(&values, "CITIES_DISABLED", ""));
        let expand_review_list = bool_value(&values, "EXPAND_REVIEW_LIST", false)?;
        let expand_doctor_endorsements = bool_value(&values, "EXPAND_DOCTOR_ENDORSEMENTS", false)?;
        let review_expansion_limit = integer_value(&values, "REVIEW_EXPANSION_LIMIT", 10)?;
        if !(1..=100).contains(&review_expansion_limit) {
            return Err(anyhow!("REVIEW_EXPANSION_LIMIT must be between 1 and 100"));
        }
        Ok(Self {
            scrape_parts,
            all_cities_enabled,
            enabled_cities,
            disabled_cities,
            expand_review_list,
            expand_doctor_endorsements,
            review_expansion_limit,
            config_path: None,
        })
    }

    pub fn enabled(&self, name: &str) -> bool {
        self.scrape_parts.get(name).copied().unwrap_or(true)
    }

    pub fn city_enabled(&self, city: &str) -> bool {
        let slug = normalize_city(city);
        if slug.is_empty() {
            return self.all_cities_enabled && self.disabled_cities.is_empty();
        }
        !self.disabled_cities.contains(&slug)
            && (self.all_cities_enabled || self.enabled_cities.contains(&slug))
    }

    pub fn has_city_allowlist(&self) -> bool {
        !self.all_cities_enabled || !self.disabled_cities.is_empty()
    }
}

impl Default for Settings {
    fn default() -> Self {
        let scrape_parts = SCRAPE_PARTS
            .iter()
            .map(|part| {
                let enabled = *part != "RAW_HTML";
                (part.to_ascii_lowercase(), enabled)
            })
            .collect();
        Self {
            scrape_parts,
            all_cities_enabled: true,
            enabled_cities: HashSet::new(),
            disabled_cities: HashSet::new(),
            expand_review_list: false,
            expand_doctor_endorsements: false,
            review_expansion_limit: 10,
            config_path: None,
        }
    }
}

fn parse_env_file(contents: &str) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    for (line_index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid .env line {}; expected KEY=VALUE", line_index + 1))?;
        let key = key.trim();
        if key.is_empty()
            || !key.chars().all(|character| {
                character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
            })
        {
            return Err(anyhow!("invalid .env key on line {}", line_index + 1));
        }
        values.insert(key.to_owned(), unquote(value.trim()));
    }
    Ok(values)
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

fn bool_value(values: &HashMap<String, String>, key: &str, default: bool) -> Result<bool> {
    let Some(value) = values.get(key) else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("{key} must be true or false")),
    }
}

fn scrape_part_value(values: &HashMap<String, String>, key: &str, default: bool) -> Result<bool> {
    let Some(value) = values.get(key) else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "default" | "auto" | "" => Ok(default),
        _ => bool_value(values, key, default),
    }
}

fn integer_value(values: &HashMap<String, String>, key: &str, default: usize) -> Result<usize> {
    values
        .get(key)
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("{key} must be a nonnegative integer"))
        })
        .unwrap_or(Ok(default))
}

fn list_value(values: &HashMap<String, String>, key: &str, default: &str) -> Vec<String> {
    values
        .get(key)
        .map(String::as_str)
        .unwrap_or(default)
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn normalize_city_list(values: Vec<String>) -> HashSet<String> {
    values
        .into_iter()
        .map(|city| normalize_city(&city))
        .filter(|city| !city.is_empty())
        .collect()
}

fn normalize_city(city: &str) -> String {
    crate::util::city_slug_from_name(city).unwrap_or_else(|| crate::util::normalize_slug(city))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parses_author_line_booleans_lists_and_quotes() {
        let values = parse_env_file(
            "# Author: ahura / @mabae\nSCRAPE_FAQS=off\nCITIES_ENABLED='tehran,مشهد'\n",
        )
        .expect("parse settings");
        assert_eq!(values.get("SCRAPE_FAQS").map(String::as_str), Some("off"));
        assert_eq!(
            values.get("CITIES_ENABLED").map(String::as_str),
            Some("tehran,مشهد")
        );
    }

    #[test]
    fn city_allowlist_and_blocklist_use_slugs_or_persian_city_names() {
        let values = HashMap::from([
            ("CITIES_ENABLED".to_owned(), "تهران,mashhad,qom".to_owned()),
            ("CITIES_DISABLED".to_owned(), "مشهد".to_owned()),
        ]);
        let settings = Settings::from_values(values).expect("build settings");
        assert!(settings.city_enabled("tehran"));
        assert!(!settings.city_enabled("mashhad"));
        assert!(!settings.city_enabled("shiraz"));
    }

    #[test]
    fn scrape_all_can_be_overridden_for_individual_sections() {
        let values = HashMap::from([
            ("SCRAPE_ALL".to_owned(), "false".to_owned()),
            ("SCRAPE_PATIENT_REVIEWS".to_owned(), "true".to_owned()),
        ]);
        let settings = Settings::from_values(values).expect("build settings");
        assert!(settings.enabled("patient_reviews"));
        assert!(!settings.enabled("faqs"));
    }
}
