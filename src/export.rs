// Author: ahura / @mabae
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use chrono::Utc;
use docx_rs::{AlignmentType, Docx, Paragraph, Pic, Run, RunFonts};
use futures::{StreamExt, stream};
use image::{ImageFormat, ImageReader};
use rust_xlsxwriter::{Color, DocProperties, Format, FormatAlign, Image as XlsxImage, Workbook};
use serde_json::json;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use tokio::fs as async_fs;
use tokio::sync::Semaphore;

use crate::browser::render_html_to_pdf;
use crate::http::SiteClient;
use crate::model::{AUTHOR, DIRECTORY_URL, DoctorRecord, EXPORT_BATCH_SIZE};
use crate::state;
use crate::util::{json_text, safe_city_directory_name};

#[derive(Clone, Debug, Default)]
pub struct ExportedCity {
    pub directory: String,
    pub doctor_count: usize,
    pub image_count: usize,
    pub embedded_image_count: usize,
    pub workbook_file: String,
}

pub async fn export_city(
    output_root: &Path,
    slug: &str,
    city_name: &str,
    mut doctors: Vec<DoctorRecord>,
    client: &SiteClient,
    download_images: bool,
    concurrency: usize,
) -> Result<ExportedCity> {
    doctors.sort_by(|left, right| {
        left.full_name
            .cmp(&right.full_name)
            .then_with(|| left.profile_url.cmp(&right.profile_url))
    });
    let directory = output_root.join(safe_city_directory_name(slug));
    let images_directory = directory.join("images");
    let cache_directory = output_root.join(".state").join("images");
    async_fs::create_dir_all(&images_directory).await?;
    async_fs::create_dir_all(&cache_directory).await?;
    prepare_images(
        &mut doctors,
        &directory,
        &images_directory,
        &cache_directory,
        client,
        download_images,
        concurrency,
    )
    .await;

    let export = json!({
        "source": DIRECTORY_URL,
        "generatedAt": Utc::now().to_rfc3339(),
        "city": {"slug": slug, "name": city_name},
        "doctorCount": doctors.len(),
        "doctors": doctors
    });
    state::atomic_write(
        &directory.join("doctors.json"),
        &serde_json::to_vec_pretty(&export)?,
    )?;

    let batches = doctors.chunks(EXPORT_BATCH_SIZE);
    let total_batches = batches.len();
    let mut embedded_image_count = 0usize;
    for (index, batch) in batches.enumerate() {
        let suffix = if total_batches == 1 {
            String::new()
        } else {
            format!("-part-{:04}", index + 1)
        };
        let workbook_path = directory.join(format!("doctors{suffix}.xlsx"));
        let word_path = directory.join(format!("doctors{suffix}.docx"));
        let pdf_path = directory.join(format!("doctors{suffix}.pdf"));
        write_workbook(&workbook_path, city_name, batch, &directory)?;
        write_word_document(&word_path, city_name, batch, &directory)?;
        write_pdf_document(&pdf_path, city_name, batch, &directory)?;
        embedded_image_count += batch
            .iter()
            .filter(|doctor| embed_image_type(&doctor.image_file))
            .count();
    }
    let image_count = doctors
        .iter()
        .filter(|doctor| !doctor.image_file.is_empty())
        .count();
    let workbook_file = if total_batches == 1 {
        "doctors.xlsx".to_owned()
    } else {
        "doctors-part-*.xlsx".to_owned()
    };
    Ok(ExportedCity {
        directory: directory.display().to_string(),
        doctor_count: doctors.len(),
        image_count,
        embedded_image_count,
        workbook_file,
    })
}

async fn prepare_images(
    doctors: &mut [DoctorRecord],
    city_directory: &Path,
    images_directory: &Path,
    cache_directory: &Path,
    client: &SiteClient,
    download_images: bool,
    concurrency: usize,
) {
    if !download_images {
        for doctor in doctors {
            doctor.image_file.clear();
        }
        return;
    }
    let semaphore = std::sync::Arc::new(Semaphore::new(concurrency.clamp(1, 8)));
    let jobs: Vec<(usize, DoctorRecord)> = doctors
        .iter()
        .cloned()
        .enumerate()
        .filter(|(_, doctor)| !doctor.avatar_url.is_empty())
        .collect();
    let results = stream::iter(jobs.into_iter().map(|(index, doctor)| {
        let semaphore = semaphore.clone();
        async move {
            let _permit = semaphore.acquire().await;
            (
                index,
                download_image(&doctor, images_directory, cache_directory, client).await,
            )
        }
    }))
    .buffer_unordered(concurrency.clamp(1, 8))
    .collect::<Vec<_>>()
    .await;
    for (index, result) in results {
        match result {
            Ok(path) => {
                doctors[index].image_file = city_directory
                    .join(path)
                    .strip_prefix(city_directory)
                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
            }
            Err(error) => eprintln!("[image] {}: {error}", doctors[index].full_name),
        }
    }
}

async fn download_image(
    doctor: &DoctorRecord,
    images_directory: &Path,
    cache_directory: &Path,
    client: &SiteClient,
) -> Result<PathBuf> {
    let identifier = safe_file_component(if doctor.id.is_empty() {
        &doctor.slug
    } else {
        &doctor.id
    });
    for extension in [".jpg", ".png"] {
        let cached = cache_directory.join(format!("{identifier}{extension}"));
        if cached.exists() {
            let file_name = format!("{identifier}{extension}");
            async_fs::copy(&cached, images_directory.join(&file_name)).await?;
            return Ok(PathBuf::from("images").join(file_name));
        }
    }
    let bytes = client.fetch_bytes(&doctor.avatar_url).await?;
    let (bytes, extension) = normalize_image(bytes)?;
    let file_name = format!("{identifier}{extension}");
    let cache_path = cache_directory.join(&file_name);
    if !cache_path.exists() {
        write_new_file(&cache_path, &bytes).await?;
    }
    async_fs::copy(&cache_path, images_directory.join(&file_name)).await?;
    Ok(PathBuf::from("images").join(file_name))
}

async fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("img")
    ));
    async_fs::write(&temporary, bytes).await?;
    if path.exists() {
        let _ = async_fs::remove_file(&temporary).await;
        return Ok(());
    }
    async_fs::rename(&temporary, path)
        .await
        .context("could not save image cache entry")
}

fn normalize_image(bytes: Vec<u8>) -> Result<(Vec<u8>, String)> {
    let format = image::guess_format(&bytes).context("could not identify image format")?;
    if format == ImageFormat::Jpeg {
        return Ok((bytes, ".jpg".to_owned()));
    }
    if format == ImageFormat::Png {
        return Ok((bytes, ".png".to_owned()));
    }
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()
        .context("unsupported image format")?;
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png)?;
    Ok((output.into_inner(), ".png".to_owned()))
}

fn safe_file_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if cleaned.is_empty() {
        "doctor".to_owned()
    } else {
        cleaned
    }
}

fn write_workbook(
    path: &Path,
    city_name: &str,
    doctors: &[DoctorRecord],
    city_directory: &Path,
) -> Result<()> {
    let mut workbook = Workbook::new();
    workbook.set_properties(
        &DocProperties::new()
            .set_author(AUTHOR)
            .set_title(format!("پزشکان شهر {city_name}"))
            .set_subject("فهرست پزشکان دکترتو")
            .set_company(AUTHOR),
    );
    let header_format = Format::new()
        .set_bold()
        .set_font_color(Color::White)
        .set_background_color(Color::RGB(0x155E75))
        .set_align(FormatAlign::Center)
        .set_text_wrap()
        .set_reading_direction(2);
    let body_format = Format::new()
        .set_font_name("Tahoma")
        .set_font_size(10)
        .set_text_wrap()
        .set_reading_direction(2);
    let sheets: [(&str, &[&str], &[f64]); 3] = [
        (
            "پزشکان",
            &["تصویر", "نام پزشک", "عنوان", "تخصص‌ها", "شهرها"],
            &[11.0, 25.0, 23.0, 28.0, 22.0],
        ),
        (
            "تماس",
            &["نام پزشک", "شماره تماس", "آدرس مطب", "پیوند پروفایل"],
            &[24.0, 20.0, 48.0, 24.0],
        ),
        (
            "داده کامل",
            &["نام پزشک", "فیلد", "مقدار"],
            &[25.0, 22.0, 78.0],
        ),
    ];
    for (sheet_index, (sheet_name, headers, widths)) in sheets.iter().enumerate() {
        let sheet = workbook.add_worksheet();
        sheet.set_name(*sheet_name)?;
        sheet.set_right_to_left(true);
        sheet.set_freeze_panes(1, 0)?;
        for (column, width) in widths.iter().enumerate() {
            sheet.set_column_width(column as u16, *width)?;
        }
        for (column, title) in headers.iter().enumerate() {
            sheet.write_string_with_format(0, column as u16, *title, &header_format)?;
        }
        match sheet_index {
            0 => {
                for (index, doctor) in doctors.iter().enumerate() {
                    let row = index as u32 + 1;
                    sheet.set_row_height(row, 58.0)?;
                    sheet.write_string_with_format(row, 1, &doctor.full_name, &body_format)?;
                    sheet.write_string_with_format(row, 2, &doctor.title, &body_format)?;
                    sheet.write_string_with_format(
                        row,
                        3,
                        doctor.specialties.join("، "),
                        &body_format,
                    )?;
                    sheet.write_string_with_format(
                        row,
                        4,
                        doctor.city_names.join("، "),
                        &body_format,
                    )?;
                    if !doctor.image_file.is_empty() {
                        let image_path = city_directory.join(&doctor.image_file);
                        if let Ok(bytes) = fs::read(image_path)
                            && let Ok(image) = XlsxImage::new_from_buffer(&bytes)
                        {
                            let image = image
                                .set_width(54)
                                .set_height(54)
                                .set_alt_text(doctor.full_name.clone());
                            sheet.insert_image(row, 0, &image)?;
                        }
                    }
                }
            }
            1 => {
                for (index, doctor) in doctors.iter().enumerate() {
                    let row = index as u32 + 1;
                    sheet.set_row_height(row, 42.0)?;
                    sheet.write_string_with_format(row, 0, &doctor.full_name, &body_format)?;
                    sheet.write_string_with_format(
                        row,
                        1,
                        doctor.phones.join("، "),
                        &body_format,
                    )?;
                    sheet.write_string_with_format(
                        row,
                        2,
                        doctor.addresses.join(" | "),
                        &body_format,
                    )?;
                    sheet.write_string_with_format(row, 3, &doctor.profile_url, &body_format)?;
                }
            }
            _ => write_full_data_sheet(sheet, doctors, &body_format)?,
        }
        let last_row = doctors.len().max(1) as u32;
        sheet.autofilter(0, 0, last_row, headers.len() as u16 - 1)?;
    }
    let temporary = path.with_extension("xlsx.tmp");
    workbook.save(&temporary)?;
    replace_file(&temporary, path)?;
    Ok(())
}

fn write_full_data_sheet(
    sheet: &mut rust_xlsxwriter::Worksheet,
    doctors: &[DoctorRecord],
    body_format: &Format,
) -> Result<()> {
    let mut row = 1u32;
    for doctor in doctors {
        for (field, value) in doctor_fields(doctor) {
            let chunks = utf16_chunks(&value, 30_000);
            for (part, chunk) in chunks.iter().enumerate() {
                sheet.set_row_height(row, 32.0)?;
                sheet.write_string_with_format(row, 0, &doctor.full_name, body_format)?;
                let label = if chunks.len() > 1 {
                    format!("{field} ({}/{})", part + 1, chunks.len())
                } else {
                    field.clone()
                };
                sheet.write_string_with_format(row, 1, &label, body_format)?;
                sheet.write_string_with_format(row, 2, chunk, body_format)?;
                row += 1;
            }
        }
    }
    let last_row = row.saturating_sub(1).max(1);
    sheet.autofilter(0, 0, last_row, 2)?;
    Ok(())
}

fn doctor_fields(doctor: &DoctorRecord) -> Vec<(String, String)> {
    vec![
        ("شناسه".to_owned(), doctor.id.clone()),
        ("شناسه پیوند".to_owned(), doctor.slug.clone()),
        ("نام پزشک".to_owned(), doctor.full_name.clone()),
        ("عنوان حرفه‌ای".to_owned(), doctor.title.clone()),
        ("افتخار و عنوان".to_owned(), doctor.honor.clone()),
        ("شماره نظام پزشکی".to_owned(), doctor.medical_number.clone()),
        ("شهرها".to_owned(), doctor.city_names.join("، ")),
        ("شناسه شهرها".to_owned(), doctor.city_slugs.join("، ")),
        ("تخصص‌ها".to_owned(), doctor.specialties.join("، ")),
        ("شماره تماس".to_owned(), doctor.phones.join("، ")),
        ("نشانی مطب".to_owned(), doctor.addresses.join(" | ")),
        (
            "مختصات جغرافیایی مطب‌ها".to_owned(),
            doctor
                .clinic_locations
                .iter()
                .map(|location| {
                    let name = if location.name.is_empty() {
                        "مطب"
                    } else {
                        &location.name
                    };
                    format!(
                        "{name} | عرض جغرافیایی: {} | طول جغرافیایی: {}",
                        location.latitude, location.longitude
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ("پیوند پروفایل".to_owned(), doctor.profile_url.clone()),
        ("پیوند تصویر".to_owned(), doctor.avatar_url.clone()),
        ("HTML کامل صفحه".to_owned(), doctor.page_html.clone()),
        (
            "تصویر پروفایل".to_owned(),
            if doctor.image_file.is_empty() {
                String::new()
            } else {
                "در فایل جاسازی شده".to_owned()
            },
        ),
        (
            "درباره پزشک، متن اصلی".to_owned(),
            doctor.about_html.clone(),
        ),
        ("درباره پزشک".to_owned(), doctor.about_text.clone()),
        ("میانگین امتیاز".to_owned(), doctor.rating_average.clone()),
        ("تعداد امتیازها".to_owned(), doctor.rating_count.clone()),
        (
            "درصد پیشنهاد".to_owned(),
            doctor.recommendation_percent.clone(),
        ),
        (
            "تعداد پیشنهادها".to_owned(),
            doctor.recommendation_count.clone(),
        ),
        ("تأییدیه‌ها".to_owned(), doctor.endorsement_count.clone()),
        (
            "سفارش‌های انجام‌شده".to_owned(),
            doctor.completed_orders.clone(),
        ),
        ("مشاوره شبانه‌روزی".to_owned(), doctor.is_on_call.clone()),
        ("ویژه".to_owned(), doctor.is_featured.clone()),
        ("زمان انتظار".to_owned(), json_text(&doctor.waiting_time)),
        (
            "زمان‌های آزاد بعدی".to_owned(),
            json_text(&doctor.next_free_times),
        ),
        ("خدمات".to_owned(), doctor.service_tags.join("، ")),
        ("بیمه‌ها".to_owned(), doctor.insurances.join("، ")),
        (
            "شبکه‌های اجتماعی".to_owned(),
            serde_json::to_string(&doctor.social_links).unwrap_or_default(),
        ),
        (
            "بخش‌های صفحه".to_owned(),
            serde_json::to_string(&doctor.page_sections).unwrap_or_default(),
        ),
        ("متن کامل صفحه".to_owned(), doctor.page_text.clone()),
        (
            "داده ساختاریافته".to_owned(),
            serde_json::to_string(&doctor.structured_profile_data).unwrap_or_default(),
        ),
        (
            "شهرهای صفحه فهرست".to_owned(),
            doctor.listing_city_hints.join("، "),
        ),
        (
            "خطای استخراج".to_owned(),
            doctor.scrape_error.clone().unwrap_or_default(),
        ),
    ]
}

fn utf16_chunks(value: &str, maximum: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0usize;
    for ch in value.chars() {
        let size = ch.len_utf16();
        if units + size > maximum && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += size;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn write_word_document(
    path: &Path,
    city_name: &str,
    doctors: &[DoctorRecord],
    city_directory: &Path,
) -> Result<()> {
    let fonts = RunFonts::new()
        .ascii("Tahoma")
        .east_asia("Tahoma")
        .cs("Tahoma");
    let mut docx = Docx::new()
        .default_fonts(fonts)
        .default_size(20)
        .default_spacing(120)
        .custom_property("Author", AUTHOR)
        .custom_property("Source", DIRECTORY_URL)
        .add_paragraph(rtl_paragraph(
            &format!("فهرست پزشکان شهر {city_name}"),
            true,
            34,
        ))
        .add_paragraph(rtl_paragraph(
            &format!("تعداد پزشکان: {}", doctors.len()),
            false,
            20,
        ))
        .add_paragraph(rtl_paragraph(&format!("منبع: {DIRECTORY_URL}"), false, 18))
        .add_paragraph(rtl_paragraph(&format!("نویسنده: {AUTHOR}"), false, 18))
        .add_paragraph(rtl_paragraph(
            &format!("زمان گردآوری: {}", Utc::now().to_rfc3339()),
            false,
            18,
        ));
    for (index, doctor) in doctors.iter().enumerate() {
        let title = if doctor.full_name.is_empty() {
            "پزشک بدون نام"
        } else {
            &doctor.full_name
        };
        let mut heading = rtl_paragraph(title, true, 30);
        heading = heading.page_break_before(index > 0);
        docx = docx.add_paragraph(heading);
        if !doctor.image_file.is_empty() {
            let image_path = city_directory.join(&doctor.image_file);
            if let Ok(bytes) = fs::read(image_path) {
                let picture = Pic::new(&bytes).size(112 * 9525, 112 * 9525);
                docx = docx
                    .add_paragraph(rtl(Paragraph::new().add_run(Run::new().add_image(picture))));
            }
        }
        for (field, value) in doctor_fields(doctor) {
            if value.is_empty() {
                continue;
            }
            docx = docx.add_paragraph(rtl_paragraph(&format!("{field}: {value}"), false, 18));
        }
    }
    let temporary = path.with_extension("docx.tmp");
    let file = fs::File::create(&temporary)?;
    docx.build()
        .pack(file)
        .context("could not write Word document")?;
    replace_file(&temporary, path)?;
    Ok(())
}

fn rtl_paragraph(text: &str, bold: bool, size: usize) -> Paragraph {
    let mut run = Run::new().add_text(text).size(size);
    if bold {
        run = run.bold();
    }
    rtl(Paragraph::new().add_run(run))
}

fn rtl(mut paragraph: Paragraph) -> Paragraph {
    paragraph.property = paragraph.property.bidi(true).align(AlignmentType::Right);
    paragraph
}

fn write_pdf_document(
    path: &Path,
    city_name: &str,
    doctors: &[DoctorRecord],
    city_directory: &Path,
) -> Result<()> {
    let html = pdf_html(city_name, doctors, city_directory)?;
    let bytes = render_html_to_pdf(&html)?;
    let temporary = path.with_extension("pdf.tmp");
    fs::write(&temporary, bytes)?;
    replace_file(&temporary, path)?;
    Ok(())
}

fn pdf_html(city_name: &str, doctors: &[DoctorRecord], city_directory: &Path) -> Result<String> {
    let mut html = format!(
        "<!doctype html><html lang=\"fa\" dir=\"rtl\"><head><meta charset=\"utf-8\"><style>@page{{size:A4;margin:16mm}}body{{font-family:Tahoma,Arial,sans-serif;color:#17202a;line-height:1.65}}h1{{font-size:22pt}}h2{{font-size:18pt;border-bottom:1px solid #d7dee5;padding-bottom:4mm}}h3{{font-size:12pt;color:#155e75;margin:5mm 0 1mm}}p{{margin:1mm 0;white-space:pre-wrap;overflow-wrap:anywhere}}.meta{{color:#52616f;font-size:9pt}}.doctor{{page-break-before:always}}.photo{{width:100px;height:100px;object-fit:cover;border-radius:8px}}.author{{direction:ltr;text-align:left}}</style></head><body><h1>فهرست پزشکان شهر {}</h1><p>تعداد پزشکان: {}</p><p class=\"author\">Author: {}</p><p class=\"meta\">{}</p>",
        escape_html(city_name),
        doctors.len(),
        escape_html(AUTHOR),
        escape_html(DIRECTORY_URL)
    );
    for (index, doctor) in doctors.iter().enumerate() {
        html.push_str(&format!(
            "<section class=\"doctor\" style=\"page-break-before:{}\"><h2>{}</h2>",
            if index == 0 { "auto" } else { "always" },
            escape_html(&doctor.full_name)
        ));
        if !doctor.image_file.is_empty() {
            let image_path = city_directory.join(&doctor.image_file);
            if let Ok(bytes) = fs::read(image_path) {
                let mime = if doctor.image_file.to_ascii_lowercase().ends_with(".png") {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                html.push_str(&format!(
                    "<img class=\"photo\" alt=\"{}\" src=\"data:{mime};base64,{}\">",
                    escape_html(&doctor.full_name),
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                ));
            }
        }
        for (field, value) in doctor_fields(doctor) {
            if value.is_empty() {
                continue;
            }
            html.push_str(&format!(
                "<h3>{}</h3><p>{}</p>",
                escape_html(&field),
                escape_html(&value)
            ));
        }
        html.push_str("</section>");
    }
    html.push_str("</body></html>");
    Ok(html)
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn embed_image_type(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".png")
}

fn replace_file(temporary: &Path, destination: &Path) -> Result<()> {
    if !destination.exists() {
        fs::rename(temporary, destination)?;
        return Ok(());
    }
    let backup = destination.with_extension("replace-backup");
    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    fs::rename(destination, &backup)
        .with_context(|| format!("could not stage {}", destination.display()))?;
    match fs::rename(temporary, destination) {
        Ok(()) => {
            fs::remove_file(backup)?;
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, destination);
            Err(anyhow!(error).context(format!("could not replace {}", destination.display())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn spreadsheet_text_splits_without_loss_at_utf16_limit() {
        let value = "ن".repeat(65_600);
        let chunks = utf16_chunks(&value, 30_000);
        assert!(
            chunks
                .iter()
                .all(|part| part.encode_utf16().count() <= 30_000)
        );
        assert_eq!(chunks.concat(), value);
    }

    #[test]
    fn pdf_text_is_escaped_and_images_are_embedded_as_data() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let bytes = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jKJcAAAAASUVORK5CYII=").expect("synthetic image");
        fs::write(directory.path().join("image.png"), bytes).expect("write synthetic image");
        let doctor = DoctorRecord {
            full_name: "دکتر <نمونه>".to_owned(),
            image_file: "image.png".to_owned(),
            ..DoctorRecord::default()
        };
        let html = pdf_html("تهران", &[doctor], directory.path()).expect("PDF HTML");
        assert!(html.contains("دکتر &lt;نمونه&gt;"));
        assert!(html.contains("data:image/png;base64,"));
    }

    #[test]
    fn workbook_and_word_report_embed_the_profile_photo_and_use_right_to_left_text() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let photo_path = directory.path().join("doctor.png");
        let mut photo = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut photo, ImageFormat::Png)
            .expect("encode synthetic photo");
        fs::write(&photo_path, photo.into_inner()).expect("write synthetic photo");

        let doctor = DoctorRecord {
            id: "doctor-1".to_owned(),
            full_name: "دکتر نمونه".to_owned(),
            profile_url: "https://doctoreto.com/doctor/doctor-1".to_owned(),
            image_file: "doctor.png".to_owned(),
            ..DoctorRecord::default()
        };

        let workbook_path = directory.path().join("doctors.xlsx");
        write_workbook(
            &workbook_path,
            "تهران",
            std::slice::from_ref(&doctor),
            directory.path(),
        )
        .expect("write workbook");
        let mut workbook =
            zip::ZipArchive::new(fs::File::open(&workbook_path).expect("open workbook"))
                .expect("read workbook archive");
        assert!(
            workbook
                .file_names()
                .any(|name| name.starts_with("xl/media/"))
        );
        let mut sheet = String::new();
        workbook
            .by_name("xl/worksheets/sheet1.xml")
            .expect("read workbook sheet")
            .read_to_string(&mut sheet)
            .expect("read sheet XML");
        assert!(sheet.contains("rightToLeft=\"1\""));

        let word_path = directory.path().join("doctors.docx");
        write_word_document(
            &word_path,
            "تهران",
            std::slice::from_ref(&doctor),
            directory.path(),
        )
        .expect("write Word report");
        let mut word = zip::ZipArchive::new(fs::File::open(&word_path).expect("open Word report"))
            .expect("read Word archive");
        assert!(
            word.file_names()
                .any(|name| name.starts_with("word/media/"))
        );
        let mut document = String::new();
        word.by_name("word/document.xml")
            .expect("read Word document")
            .read_to_string(&mut document)
            .expect("read document XML");
        assert!(document.contains("w:bidi"));
    }
}
