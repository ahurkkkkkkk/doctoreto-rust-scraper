// Author: ahura / @mabae
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use headless_chrome::Browser;
use reqwest::{Client, StatusCode, header};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell, Semaphore};
use tokio::time::Instant;

use crate::browser::new_browser;
use crate::model::{BASE_URL, MAX_IMAGE_BYTES, MAX_PAGE_BYTES, USER_AGENT};
use crate::parse::{
    clean_phone, extract_phones_after_reveal, extract_phones_from_html, robots_path_matches,
};
use crate::util::{is_site_host, safe_site_url, unique};

#[derive(Clone, Debug, Default)]
pub struct HttpResult {
    pub status: u16,
    pub text: String,
    pub final_url: String,
}

#[derive(Clone, Debug, Default)]
pub struct RevealResult {
    pub phones: Vec<String>,
    pub html: String,
}

#[derive(Debug)]
struct RateState {
    base_interval: Duration,
    interval: Duration,
    next: Instant,
    blocked_until: Instant,
    successes: usize,
}

pub struct SiteClient {
    client: Client,
    requests: Arc<Semaphore>,
    phone_reveals: Arc<Semaphore>,
    rate: Mutex<RateState>,
    delay: Duration,
    browser_only: bool,
    browser: OnceCell<Arc<Browser>>,
    robots: OnceCell<Vec<String>>,
}

impl SiteClient {
    pub fn new(
        concurrency: usize,
        requests_per_second: u32,
        delay_ms: u64,
        browser_only: bool,
    ) -> Result<Self> {
        let paced_rps = if delay_ms > 0 {
            requests_per_second.min((1000 / delay_ms.max(1)).max(1) as u32)
        } else {
            requests_per_second
        };
        let base_interval = Duration::from_secs_f64(1.0 / paced_rps.max(1) as f64);
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(35))
            .pool_max_idle_per_host(concurrency)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                if attempt.url().scheme() != "https"
                    || !attempt.url().host_str().is_some_and(is_site_host)
                {
                    return attempt.error("redirect outside the approved HTTPS host list");
                }
                attempt.follow()
            }))
            .build()
            .context("could not configure the HTTP client")?;
        Ok(Self {
            client,
            requests: Arc::new(Semaphore::new(concurrency.max(1))),
            phone_reveals: Arc::new(Semaphore::new(16)),
            rate: Mutex::new(RateState {
                base_interval,
                interval: base_interval,
                next: Instant::now(),
                blocked_until: Instant::now(),
                successes: 0,
            }),
            delay: Duration::from_millis(delay_ms),
            browser_only,
            browser: OnceCell::new(),
            robots: OnceCell::new(),
        })
    }

    pub async fn fetch_text(&self, url: &str) -> Result<HttpResult> {
        self.assert_site_url(url)?;
        if !url.ends_with("/robots.txt") {
            let rules = self.robots_rules().await;
            let path = reqwest::Url::parse(url)?.path().to_owned();
            if let Some(blocked) = rules
                .iter()
                .find(|pattern| robots_path_matches(&path, pattern))
            {
                return Err(anyhow!("robots.txt disallows {path} (rule: {blocked})"));
            }
        }
        if self.browser_only {
            return self.fetch_in_browser(url).await;
        }
        let mut last_error = None;
        for attempt in 0..4 {
            match self.fetch_once_text(url).await {
                Ok(result) if result.status == 404 || result.status == 410 => return Ok(result),
                Ok(result) if (result.status == 403 || result.status == 429) && attempt < 3 => {
                    eprintln!(
                        "[http] blocked or throttled, backing off before retry {}: {url}",
                        attempt + 1
                    );
                    tokio::time::sleep(Duration::from_secs(5 * (attempt + 1))).await;
                }
                Ok(result) if result.status >= 500 && attempt < 3 => {
                    tokio::time::sleep(Duration::from_secs(attempt + 1)).await;
                }
                Ok(result) => return Ok(result),
                Err(error) if attempt < 3 => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_secs(attempt + 1)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("failed to fetch after retries: {url}")))
    }

    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let parsed = reqwest::Url::parse(url).context("invalid image URL")?;
        if parsed.scheme() != "https"
            || !matches!(
                parsed.host_str(),
                Some("doctoreto.com" | "statics.doctoreto.com")
            )
        {
            return Err(anyhow!(
                "image URL host is not allowed: {}",
                parsed.host_str().unwrap_or_default()
            ));
        }
        let mut last_error = None;
        for attempt in 0..4 {
            match self.fetch_once(url, true).await {
                Ok(result) if (200..300).contains(&result.status) => return Ok(result.bytes),
                Ok(result)
                    if (result.status == 403 || result.status == 429 || result.status >= 500)
                        && attempt < 3 =>
                {
                    let delay = if result.status == 403 || result.status == 429 {
                        5
                    } else {
                        1
                    };
                    tokio::time::sleep(Duration::from_secs(delay * (attempt + 1))).await;
                }
                Ok(result) => return Err(anyhow!("image HTTP {} at {url}", result.status)),
                Err(error) if attempt < 3 => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_secs(attempt + 1)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("image fetch failed after retries at {url}")))
    }

    pub async fn interact_with_profile_page(
        &self,
        url: &str,
        reveal_phone: bool,
        expand_reviews: bool,
        expand_endorsements: bool,
        expansion_limit: usize,
    ) -> Result<RevealResult> {
        self.assert_site_url(url)?;
        let _permit = self.phone_reveals.acquire().await?;
        self.rate_wait().await;
        let browser = self.browser().await?;
        let url = url.to_owned();
        tokio::task::spawn_blocking(move || {
            let tab = browser.new_tab().context("could not open profile interaction tab")?;
            tab.navigate_to(&url).context("could not open the doctor profile")?.wait_until_navigated()
                .context("doctor profile did not finish loading")?;
            std::thread::sleep(Duration::from_millis(900));
            if reveal_phone {
                let script = r#"(() => {
                  const controls = Array.from(document.querySelectorAll('button,[role="button"],a')).slice(0,120);
                  let clicked = 0;
                  for (const control of controls) {
                    if (clicked >= 5) break;
                    const href = (control.getAttribute('href') || '').toLowerCase();
                    if (href.startsWith('tel:')) continue;
                    const label = [control.innerText || '', control.getAttribute('aria-label') || '', control.title || ''].join(' ');
                    if (!/(شماره|تلفن|تماس)/i.test(label) || !/(نمایش|مشاهده|دریافت|شماره تلفن)/i.test(label)) continue;
                    control.click();
                    clicked++;
                  }
                  return clicked;
                })()"#;
                let _ = tab.evaluate(script, false);
                std::thread::sleep(Duration::from_millis(450));
            }
            if expand_endorsements {
                let script = r#"(() => {
                  const headings = Array.from(document.querySelectorAll('h1,h2,h3,h4'));
                  const heading = headings.find(node => /(نظرات\s*پزشکان|تاییدیه|توصیه\s*نامه)/.test(node.innerText || ''));
                  if (!heading) return false;
                  let region = heading.closest('section,article,[role="region"]') || heading;
                  for (let depth = 0; region && depth < 8; depth++, region = region.parentElement) {
                    const controls = Array.from(region.querySelectorAll('button,[role="button"],a'));
                    const control = controls.find(node => {
                      const label = [node.innerText || '', node.getAttribute('aria-label') || '', node.title || ''].join(' ').trim();
                      return /(مشاهده\s*\d*\s*تاییدیه|مشاهده.*تاییدیه|view.*endorsement|see.*endorsement)/i.test(label);
                    });
                    if (control) {
                      control.click();
                      return true;
                    }
                  }
                  return false;
                })()"#;
                let clicked = tab
                    .evaluate(script, false)
                    .ok()
                    .and_then(|result| result.value)
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                if clicked {
                    std::thread::sleep(Duration::from_millis(600));
                }
            }
            if expand_reviews {
                let script = r#"(() => {
                  const headings = Array.from(document.querySelectorAll('h1,h2,h3,h4'));
                  const heading = headings.find(node => /(نظرات|دیدگاه|تجربه کاربران)/.test(node.innerText || ''));
                  if (!heading) return false;
                  let region = heading.closest('section,article,[role="region"]') || heading;
                  for (let depth = 0; region && depth < 8; depth++, region = region.parentElement) {
                    const controls = Array.from(region.querySelectorAll('button,[role="button"],a'));
                    const control = controls.find(node => {
                      const href = (node.getAttribute('href') || '').toLowerCase();
                      if (href.startsWith('tel:')) return false;
                      const label = [node.innerText || '', node.getAttribute('aria-label') || '', node.title || ''].join(' ').trim();
                      return /(نمایش\s*بیشتر|مشاهده\s*همه|دیدن\s*همه|نمایش\s*همه|نظرات\s*بیشتر|ادامه\s*نظرات|show\s*more|load\s*more|see\s*all)/i.test(label);
                    });
                    if (control) {
                      control.click();
                      return true;
                    }
                  }
                  return false;
                })()"#;
                let limit = expansion_limit.clamp(1, 100);
                for _ in 0..limit {
                    let clicked = tab
                        .evaluate(script, false)
                        .ok()
                        .and_then(|result| result.value)
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    if !clicked {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            let html = tab.get_content().context("could not read the revealed profile")?;
            let mut phones = if reveal_phone { extract_phones_from_html(&html) } else { Vec::new() };
            if reveal_phone
                && let Ok(body) = tab.find_element("body")
                && let Ok(text) = body.call_js_fn("function() { return this.innerText; }", vec![], false)
                && let Some(value) = text.value.and_then(|value| value.as_str().map(str::to_owned))
            {
                phones.extend(extract_phones_after_reveal(&value));
            }
            Ok(RevealResult { phones: unique(phones.into_iter().map(|phone| clean_phone(&phone)).filter(|phone| !phone.is_empty())), html })
        }).await.context("browser phone reveal task stopped")?
    }

    async fn fetch_in_browser(&self, url: &str) -> Result<HttpResult> {
        self.rate_wait().await;
        let browser = self.browser().await?;
        let target = url.to_owned();
        tokio::task::spawn_blocking(move || {
            let tab = browser.new_tab().context("could not open a browser tab")?;
            tab.navigate_to(&target)
                .context("browser could not navigate to the page")?
                .wait_until_navigated()
                .context("browser page did not finish loading")?;
            std::thread::sleep(Duration::from_millis(700));
            let final_url = tab.get_url();
            let text = tab
                .get_content()
                .context("browser could not read the page")?;
            Ok(HttpResult {
                status: 200,
                text,
                final_url,
            })
        })
        .await
        .context("browser fetch task stopped")?
    }

    async fn fetch_once(&self, url: &str, binary: bool) -> Result<RawResult> {
        let _permit = self.requests.acquire().await?;
        self.rate_wait().await;
        let mut request = self.client.get(url).header(header::USER_AGENT, USER_AGENT);
        if binary {
            request = request.header(header::ACCEPT, "image/webp,image/png,image/jpeg,*/*;q=0.8");
        } else {
            request = request
                .header(
                    header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml,text/xml;q=0.9,*/*;q=0.7",
                )
                .header(header::ACCEPT_LANGUAGE, "fa-IR,fa;q=0.9,en;q=0.5");
        }
        let response = request.send().await.context("HTTP request failed")?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        self.rate_observe(status, &retry_after).await;
        let final_url = response.url().to_string();
        let maximum = if binary {
            MAX_IMAGE_BYTES
        } else {
            MAX_PAGE_BYTES
        };
        if response
            .content_length()
            .is_some_and(|length| length > maximum as u64)
        {
            return Err(anyhow!("response exceeded the configured size limit"));
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("could not read HTTP response")?;
            if bytes.len().saturating_add(chunk.len()) > maximum {
                return Err(anyhow!("response exceeded the configured size limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(RawResult {
            status: status.as_u16(),
            final_url,
            bytes,
        })
    }

    async fn rate_wait(&self) {
        let wait = {
            let mut state = self.rate.lock().await;
            let now = Instant::now();
            let start = state.next.max(state.blocked_until).max(now);
            state.next = start + state.interval.max(self.delay);
            if start > now {
                start - now
            } else {
                Duration::ZERO
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    async fn rate_observe(&self, status: StatusCode, retry_after: &str) {
        let mut state = self.rate.lock().await;
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::FORBIDDEN {
            state.successes = 0;
            state.interval = (state.interval * 2).min(Duration::from_secs(2));
            let mut pause = Duration::from_secs(20);
            if let Ok(seconds) = retry_after.trim().parse::<u64>() {
                pause = pause.max(Duration::from_secs(seconds));
            } else if let Ok(date) = DateTime::parse_from_rfc2822(retry_after) {
                let remaining = (date.with_timezone(&Utc) - Utc::now())
                    .to_std()
                    .unwrap_or_default();
                pause = pause.max(remaining);
            }
            state.blocked_until = state.blocked_until.max(Instant::now() + pause);
        } else if status.is_success() || status.is_redirection() {
            state.successes += 1;
            if state.interval > state.base_interval && state.successes >= 100 {
                state.interval = (state.interval / 2).max(state.base_interval);
                state.successes = 0;
            }
        }
    }

    async fn robots_rules(&self) -> &[String] {
        self.robots
            .get_or_init(|| async {
                match self
                    .fetch_once(&format!("{BASE_URL}/robots.txt"), false)
                    .await
                {
                    Ok(response) if (200..300).contains(&response.status) => {
                        parse_robots(&String::from_utf8_lossy(&response.bytes))
                    }
                    Ok(_) => Vec::new(),
                    Err(error) => {
                        eprintln!("[robots] could not read robots.txt: {error}");
                        Vec::new()
                    }
                }
            })
            .await
    }

    async fn browser(&self) -> Result<Arc<Browser>> {
        let browser = self
            .browser
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(new_browser)
                    .await
                    .context("browser startup task stopped")?
                    .map(Arc::new)
            })
            .await?;
        Ok(Arc::clone(browser))
    }

    fn assert_site_url(&self, url: &str) -> Result<()> {
        let parsed = reqwest::Url::parse(url).context("invalid page URL")?;
        if parsed.scheme() != "https" || parsed.host_str() != Some("doctoreto.com") {
            return Err(anyhow!(
                "refusing to fetch outside https://doctoreto.com: {url}"
            ));
        }
        Ok(())
    }

    pub async fn close(&self) {}
}

struct RawResult {
    status: u16,
    final_url: String,
    bytes: Vec<u8>,
}

impl From<RawResult> for HttpResult {
    fn from(result: RawResult) -> Self {
        Self {
            status: result.status,
            text: String::from_utf8_lossy(&result.bytes).into_owned(),
            final_url: result.final_url,
        }
    }
}

impl RawResult {
    fn into_http(self) -> HttpResult {
        self.into()
    }
}

impl SiteClient {
    async fn fetch_once_text(&self, url: &str) -> Result<HttpResult> {
        Ok(self.fetch_once(url, false).await?.into_http())
    }
}

fn parse_robots(text: &str) -> Vec<String> {
    let mut groups: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    let mut agents = Vec::new();
    let mut rules = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "user-agent" => {
                if !agents.is_empty() && !rules.is_empty() {
                    groups.push((std::mem::take(&mut agents), std::mem::take(&mut rules)));
                }
                agents.push(value.trim().to_ascii_lowercase());
            }
            "disallow" | "allow" if !agents.is_empty() => rules.push(format!(
                "{}:{}",
                key.trim().to_ascii_lowercase(),
                value.trim()
            )),
            _ => {}
        }
    }
    if !agents.is_empty() {
        groups.push((agents, rules));
    }
    groups
        .into_iter()
        .filter(|(agents, _)| {
            agents
                .iter()
                .any(|agent| agent == "*" || "doctoretopublicdirectorycollector".contains(agent))
        })
        .flat_map(|(_, rules)| {
            rules
                .into_iter()
                .filter_map(|rule| rule.strip_prefix("disallow:").map(str::to_owned))
                .filter(|rule| !rule.is_empty())
        })
        .collect()
}

pub fn robots_sitemaps(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim().eq_ignore_ascii_case("sitemap")).then(|| value.trim().to_owned())
        })
        .filter_map(|raw| safe_site_url(&raw, BASE_URL))
        .collect()
}
