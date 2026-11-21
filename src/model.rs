// Author: ahura / @mabae
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

pub const AUTHOR: &str = "ahura / @mabae";
pub const BASE_URL: &str = "https://doctoreto.com";
pub const DIRECTORY_URL: &str = "https://doctoreto.com/doctor";
pub const USER_AGENT: &str =
    "DoctoretoPublicDirectoryCollector/2.0 (polite; author: ahura / @mabae)";
pub const MAX_PAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;
pub const EXPORT_BATCH_SIZE: usize = 400;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PageSection {
    pub heading: String,
    pub category: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ClinicLocation {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct DoctorRecord {
    pub id: String,
    pub slug: String,
    pub profile_url: String,
    pub full_name: String,
    pub title: String,
    pub honor: String,
    pub medical_number: String,
    pub city_slugs: Vec<String>,
    pub city_names: Vec<String>,
    pub specialties: Vec<String>,
    pub phones: Vec<String>,
    pub addresses: Vec<String>,
    pub clinic_locations: Vec<ClinicLocation>,
    pub avatar_url: String,
    pub image_file: String,
    pub page_html: String,
    pub about_html: String,
    pub about_text: String,
    pub rating_average: String,
    pub rating_count: String,
    pub recommendation_percent: String,
    pub recommendation_count: String,
    pub endorsement_count: String,
    pub completed_orders: String,
    pub is_on_call: String,
    pub is_featured: String,
    pub waiting_time: Value,
    pub next_free_times: Value,
    pub service_tags: Vec<String>,
    pub insurances: Vec<String>,
    pub social_links: Vec<Value>,
    pub page_sections: Vec<PageSection>,
    pub page_text: String,
    pub structured_profile_data: Value,
    pub listing_city_hints: Vec<String>,
    pub scrape_error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DiscoverySnapshot {
    pub version: u32,
    pub queue: VecDeque<String>,
    pub queued: HashSet<String>,
    pub visited: HashSet<String>,
    pub pages: usize,
    pub profile_urls: HashSet<String>,
    pub city_labels: HashMap<String, String>,
    pub profile_city_hints: HashMap<String, HashSet<String>>,
    pub specialty_slugs: HashSet<String>,
    pub failed_attempts: HashMap<String, u8>,
    pub pagination_checked: HashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DiscoveryResult {
    pub profile_urls: Vec<String>,
    pub city_labels: HashMap<String, String>,
    pub profile_city_hints: HashMap<String, HashSet<String>>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CitySummary {
    pub slug: String,
    pub name: String,
    pub doctor_count: usize,
    pub image_count: usize,
    pub embedded_image_count: usize,
    pub directory: String,
    pub workbook_file: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub source: String,
    pub generated_at: String,
    pub discovered_profile_urls: usize,
    pub checkpointed_profiles: usize,
    pub unique_doctor_ids: usize,
    pub scraped_this_run: usize,
    pub failed_this_run: usize,
    pub city_count: usize,
    pub cities: Vec<CitySummary>,
    pub city_export_errors: Vec<serde_json::Value>,
    pub output_files_per_city: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_json_keeps_legacy_camel_case_field_names() {
        let doctor = DoctorRecord {
            profile_url: "https://doctoreto.com/doctor/example/1".to_owned(),
            full_name: "دکتر نمونه".to_owned(),
            page_sections: vec![PageSection {
                heading: "آدرس مطب دکتر نمونه".to_owned(),
                category: "clinic_addresses".to_owned(),
                text: "آدرس: تهران".to_owned(),
            }],
            ..DoctorRecord::default()
        };
        let serialized = serde_json::to_value(&doctor).expect("serialize doctor");
        assert_eq!(
            serialized["profileUrl"],
            "https://doctoreto.com/doctor/example/1"
        );
        assert_eq!(serialized["fullName"], "دکتر نمونه");
        assert!(serialized.get("profile_url").is_none());

        let restored: DoctorRecord = serde_json::from_value(serde_json::json!({
            "profileUrl": "https://doctoreto.com/doctor/example/1",
            "fullName": "دکتر نمونه",
            "pageSections": [{"heading": "آدرس مطب دکتر نمونه", "text": "آدرس: تهران"}]
        }))
        .expect("read legacy checkpoint record");
        assert_eq!(restored.profile_url, doctor.profile_url);
        assert_eq!(restored.page_sections, doctor.page_sections);
    }
}
