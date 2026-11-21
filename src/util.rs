// Author: ahura / @mabae
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::OnceLock;
use url::Url;

static PERSIAN_DIGITS: OnceLock<Vec<char>> = OnceLock::new();
static ARABIC_DIGITS: OnceLock<Vec<char>> = OnceLock::new();

pub fn clean_text(value: &str) -> String {
    normalize_line_breaks(value)
        .replace(['\u{200b}', '\u{200c}', '\u{200d}', '\u{feff}'], "‌")
        .trim()
        .to_owned()
}

pub fn normalize_line_breaks(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut previous_space = false;
    let mut newline_count = 0;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' || ch == '\n' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            newline_count += 1;
            if newline_count <= 2 {
                out.push('\n');
            }
            previous_space = false;
        } else if ch == ' ' || ch == '\t' || ch == '\u{000c}' || ch == '\u{000b}' {
            if !previous_space && !out.ends_with('\n') {
                out.push(' ');
            }
            previous_space = true;
            newline_count = 0;
        } else {
            out.push(ch);
            previous_space = false;
            newline_count = 0;
        }
    }
    out.trim().to_owned()
}

pub fn normalize_digits(value: &str) -> String {
    let persian = PERSIAN_DIGITS.get_or_init(|| "۰۱۲۳۴۵۶۷۸۹".chars().collect());
    let arabic = ARABIC_DIGITS.get_or_init(|| "٠١٢٣٤٥٦٧٨٩".chars().collect());
    value
        .chars()
        .map(|ch| {
            if let Some(index) = persian.iter().position(|digit| *digit == ch) {
                char::from(b'0' + index as u8)
            } else if let Some(index) = arabic.iter().position(|digit| *digit == ch) {
                char::from(b'0' + index as u8)
            } else {
                ch
            }
        })
        .collect()
}

pub fn unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

pub fn first_string(values: &[&Value]) -> String {
    values
        .iter()
        .find_map(|value| match value {
            Value::String(text) if !text.trim().is_empty() => Some(clean_text(text)),
            Value::Number(number) => Some(number.to_string()),
            Value::Bool(value) => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn labels_from_value(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        _ => vec![value],
    };
    unique(
        items
            .into_iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(clean_text(text)),
                Value::Number(number) => Some(number.to_string()),
                Value::Object(object) => {
                    ["name", "title", "label", "displayName", "faName", "nameFa"]
                        .iter()
                        .find_map(|key| object.get(*key).and_then(Value::as_str).map(clean_text))
                }
                _ => None,
            })
            .filter(|text| !text.is_empty()),
    )
}

pub fn values_for_keys(value: &Value, keys: &[&str], depth: usize) -> Vec<String> {
    if depth > 5 {
        return Vec::new();
    }
    match value {
        Value::Array(items) => items
            .iter()
            .flat_map(|item| values_for_keys(item, keys, depth + 1))
            .collect(),
        Value::Object(object) => {
            let mut out = Vec::new();
            for key in keys {
                if let Some(value) = object.get(*key) {
                    match value {
                        Value::String(text) if !text.is_empty() => out.push(clean_text(text)),
                        Value::Number(number) => out.push(number.to_string()),
                        _ => {}
                    }
                }
            }
            for child in object.values() {
                out.extend(values_for_keys(child, keys, depth + 1));
            }
            out
        }
        _ => Vec::new(),
    }
}

pub fn safe_site_url(raw: &str, base: &str) -> Option<String> {
    let mut url = Url::parse(base).ok()?.join(raw).ok()?;
    if url.scheme() != "https" || url.host_str()? != "doctoreto.com" {
        return None;
    }
    url.set_fragment(None);
    Some(url.to_string())
}

pub fn is_profile_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let parts: Vec<&str> = url.path_segments().into_iter().flatten().collect();
    parts.len() == 3 && parts[0] == "doctor" && !parts[1].is_empty() && !parts[2].is_empty()
}

pub fn is_listing_url(raw: &str) -> bool {
    Url::parse(raw).is_ok_and(|url| url.path().starts_with("/doctors/") && !is_profile_url(raw))
}

pub fn is_sitemap_url(raw: &str) -> bool {
    Url::parse(raw).is_ok_and(|url| {
        url.path().to_ascii_lowercase().contains("sitemap")
            || url.path().to_ascii_lowercase().ends_with(".xml")
    })
}

pub fn city_slug_from_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    let parts: Vec<String> = url.path_segments()?.map(str::to_owned).collect();
    let index = parts.iter().position(|part| part == "city")?;
    parts
        .get(index + 1)
        .filter(|slug| !slug.is_empty())
        .map(|slug| normalize_slug(slug))
}

pub fn specialty_slug_from_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    let parts: Vec<String> = url.path_segments()?.map(str::to_owned).collect();
    let index = parts.iter().position(|part| part == "speciality")?;
    parts
        .get(index + 1)
        .filter(|slug| !slug.is_empty())
        .map(|slug| slug.to_lowercase())
}

pub fn normalize_slug(value: &str) -> String {
    value.trim().trim_matches('/').to_lowercase()
}

pub fn default_city_name(slug: &str) -> String {
    let names = [
        ("tehran", "تهران"),
        ("esfahan", "اصفهان"),
        ("isfahan", "اصفهان"),
        ("mashhad", "مشهد"),
        ("shiraz", "شیراز"),
        ("tabriz", "تبریز"),
        ("karaj", "کرج"),
        ("ahvaz", "اهواز"),
        ("rasht", "رشت"),
        ("kerman", "کرمان"),
        ("bushehr", "بوشهر"),
        ("bandarabbas", "بندرعباس"),
        ("bandar-abbas", "بندرعباس"),
        ("urmia", "ارومیه"),
        ("qom", "قم"),
        ("yazd", "یزد"),
        ("hamadan", "همدان"),
        ("sanandaj", "سنندج"),
        ("zanjan", "زنجان"),
        ("qazvin", "قزوین"),
        ("sari", "ساری"),
        ("gorgan", "گرگان"),
        ("kermanshah", "کرمانشاه"),
        ("arak", "اراک"),
        ("ardabil", "اردبیل"),
        ("khorramabad", "خرم‌آباد"),
        ("birjand", "بیرجند"),
        ("bojnurd", "بجنورد"),
        ("ilam", "ایلام"),
        ("shahrekord", "شهرکرد"),
        ("yasuj", "یاسوج"),
        ("zahedan", "زاهدان"),
        ("semnan", "سمنان"),
    ];
    names
        .iter()
        .find_map(|(key, value)| (*key == slug).then_some((*value).to_owned()))
        .unwrap_or_else(|| slug.replace('-', " "))
}

pub fn city_slug_from_name(name: &str) -> Option<String> {
    let normalized = normalize_city_name(name);
    let names = [
        ("تهران", "tehran"),
        ("اصفهان", "esfahan"),
        ("مشهد", "mashhad"),
        ("شیراز", "shiraz"),
        ("تبریز", "tabriz"),
        ("کرج", "karaj"),
        ("اهواز", "ahvaz"),
        ("رشت", "rasht"),
        ("کرمان", "kerman"),
        ("بوشهر", "bushehr"),
        ("بندرعباس", "bandar-abbas"),
        ("ارومیه", "urmia"),
        ("قم", "qom"),
        ("یزد", "yazd"),
        ("همدان", "hamadan"),
        ("سنندج", "sanandaj"),
        ("زنجان", "zanjan"),
        ("قزوین", "qazvin"),
        ("ساری", "sari"),
        ("گرگان", "gorgan"),
        ("کرمانشاه", "kermanshah"),
        ("اراک", "arak"),
        ("اردبیل", "ardabil"),
        ("خرم آباد", "khorramabad"),
        ("بیرجند", "birjand"),
        ("بجنورد", "bojnurd"),
        ("ایلام", "ilam"),
        ("شهرکرد", "shahrekord"),
        ("یاسوج", "yasuj"),
        ("زاهدان", "zahedan"),
        ("سمنان", "semnan"),
    ];
    names
        .iter()
        .find_map(|(key, value)| (*key == normalized).then_some((*value).to_owned()))
}

pub fn normalize_city_name(value: &str) -> String {
    clean_text(value)
        .replace('ي', "ی")
        .replace('ك', "ک")
        .replace(['\u{200b}', '\u{200c}', '\u{200d}', '\u{feff}'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn safe_city_directory_name(slug: &str) -> String {
    let safe =
        Regex::new(r#"[<>:"/\\|?*\u0000-\u001f]"#).unwrap_or_else(|_| Regex::new("[<>]").unwrap());
    let base = if slug.is_empty() {
        "unknown-city"
    } else {
        slug
    };
    let name = safe
        .replace_all(base, "_")
        .trim_end_matches(['.', ' '])
        .chars()
        .take(100)
        .collect::<String>();
    let numbered = province_folder_number(slug).map(|number| format!("{number}-{name}"));
    numbered.unwrap_or_else(|| {
        if name.is_empty() {
            "unknown-city".to_owned()
        } else {
            name
        }
    })
}

pub fn city_directory_order_key(slug: &str) -> (u8, u8, String) {
    let name = safe_city_directory_name(slug);
    match name
        .split_once('-')
        .and_then(|(number, _)| number.parse::<u8>().ok())
    {
        Some(number) => (0, number, name),
        None => (1, u8::MAX, name),
    }
}

fn province_folder_number(slug: &str) -> Option<u8> {
    const CAPITALS: [&str; 31] = [
        "tehran",
        "mashhad",
        "esfahan",
        "shiraz",
        "tabriz",
        "karaj",
        "qom",
        "ahvaz",
        "kermanshah",
        "urmia",
        "rasht",
        "zahedan",
        "kerman",
        "yazd",
        "ardabil",
        "bandar-abbas",
        "arak",
        "zanjan",
        "sanandaj",
        "hamadan",
        "gorgan",
        "sari",
        "bushehr",
        "birjand",
        "shahrekord",
        "yasuj",
        "semnan",
        "ilam",
        "bojnurd",
        "qazvin",
        "khorramabad",
    ];
    CAPITALS
        .iter()
        .position(|capital| *capital == slug)
        .map(|index| index as u8 + 1)
}

pub fn json_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(json_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("، "),
        Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
        _ => value.to_string(),
    }
}

pub fn decode_entities(value: &str) -> String {
    let patterns = [
        ("&amp;", "&"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&nbsp;", " "),
    ];
    let mut output = value.to_owned();
    for (from, to) in patterns {
        output = output.replace(from, to);
    }
    output
}

pub fn unescape_json_url(value: &str) -> String {
    value.replace("\\/", "/").replace("\\u0026", "&")
}

pub fn is_site_host(host: &str) -> bool {
    host == "doctoreto.com" || host == "statics.doctoreto.com"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BASE_URL;

    #[test]
    fn city_folder_numbers_are_natural_sort_keys() {
        assert_eq!(safe_city_directory_name("tehran"), "1-tehran");
        assert_eq!(safe_city_directory_name("mashhad"), "2-mashhad");
        assert_eq!(safe_city_directory_name("qazvin"), "30-qazvin");
        let mut cities = ["qazvin", "mashhad", "tehran", "unknown"].map(str::to_owned);
        cities.sort_by_key(|slug| city_directory_order_key(slug));
        assert_eq!(
            cities,
            ["tehran", "mashhad", "qazvin", "unknown"].map(str::to_owned)
        );
    }

    #[test]
    fn city_names_round_trip_for_known_capitals() {
        assert_eq!(city_slug_from_name("تهران").as_deref(), Some("tehran"));
        assert_eq!(default_city_name("bandar-abbas"), "بندرعباس");
    }

    #[test]
    fn safe_urls_reject_other_hosts() {
        assert!(safe_site_url("https://example.com/doctor/a/b", BASE_URL).is_none());
        assert!(safe_site_url("/doctor/a/b", BASE_URL).is_some());
    }
}
