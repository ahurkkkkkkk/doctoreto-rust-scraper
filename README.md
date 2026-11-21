Author: ahura / @mabae

# Doctoreto scraper

This Rust command line program discovers public doctor profiles, saves restartable checkpoints, and writes city folders with Excel, Word, PDF, JSON, and profile photos.

## Build and run

Install Rust and Chrome or Edge. Run commands from this folder:

```powershell
cargo run --release -- --help
cargo run --release -- --discover-only
cargo run --release -- --limit 25
```

The default output folder is `output/doctoreto`. Use `--output PATH` to choose another location. `--discover-only` checks the directory and saves a discovery checkpoint without fetching profile pages.

## Environment settings

Edit `.env` before running. The included file lists every setting. `SCRAPE_ALL` sets the default for all data fields. Each `SCRAPE_<PART>` value accepts `default`, `true`, or `false`; `default` inherits the master setting, and a boolean overrides it.

| Setting | Data selected |
| --- | --- |
| `SCRAPE_PROFILE_IDENTITY` | Name, title, honor, and medical license number |
| `SCRAPE_PROFILE_PHOTO` | Avatar URL and downloaded, embedded profile photo |
| `SCRAPE_SPECIALTIES` | Doctor specialties |
| `SCRAPE_CONTACT_DETAILS` | Phone numbers |
| `SCRAPE_REVEAL_HIDDEN_PHONES` | Click the site's phone reveal control when present |
| `SCRAPE_CLINIC_ADDRESSES` | Clinic addresses from clinic cards only |
| `SCRAPE_CLINIC_COORDINATES` | Clinic latitude and longitude from doctor-specific clinic data |
| `SCRAPE_ABOUT` | Biography HTML and text |
| `SCRAPE_PROFILE_STATISTICS` | Rating, recommendations, completed visits, and featured state |
| `SCRAPE_APPOINTMENTS` | Waiting time, next times, and on-call state |
| `SCRAPE_CONSULTATIONS` | Online consultation section |
| `SCRAPE_RECOMMENDATION_CARDS` | Recommended doctor and service cards |
| `SCRAPE_SERVICE_PRICE_CARDS` | Service prices and price cards |
| `SCRAPE_SERVICES` | Services, conditions, and symptom tags |
| `SCRAPE_INSURANCES` | Insurance information |
| `SCRAPE_DOCTOR_ENDORSEMENTS` | Doctor recommendations and endorsements |
| `SCRAPE_PATIENT_REVIEWS` | Patient review sections and loaded reviews |
| `SCRAPE_FAQS` | Frequently asked questions |
| `SCRAPE_EDITORIAL_GUIDE` | Doctoreto editorial guide content |
| `SCRAPE_RELATED_DOCTORS` | Related doctor cards |
| `SCRAPE_SOCIAL_LINKS` | Social profile links |
| `SCRAPE_OTHER_PAGE_SECTIONS` | Sections not matched to a named category |
| `SCRAPE_PAGE_SECTIONS` | Keep categorized heading and section text |
| `SCRAPE_FULL_PAGE_TEXT` | Keep the complete visible page text |
| `SCRAPE_STRUCTURED_DATA` | Keep embedded Next.js profile and page query data |
| `SCRAPE_RAW_HTML` | Keep the full profile HTML; off by default due to file size |

The complete page text and embedded structured data are broad captures. Turn those settings off when you need the individual section switches to remove content from the exports. Raw HTML is also a complete page capture and can make the output very large.

When `SCRAPE_CLINIC_COORDINATES=false`, the clinic coordinate field and coordinate keys in structured data are removed. Turn off `SCRAPE_RAW_HTML` as well if you do not want the original page markup, which can contain the source coordinates.

`EXPAND_REVIEW_LIST=true` clicks review list expansion controls when they are available. `REVIEW_EXPANSION_LIMIT` sets the maximum clicks per profile from 1 to 100. The default is 10 and expansion is off by default.

`EXPAND_DOCTOR_ENDORSEMENTS=true` clicks the control that opens all doctor endorsements when it is available. This is off by default to avoid an extra browser interaction for every profile with endorsements.

`CITIES_ENABLED=*` allows all discovered cities. Replace `*` with a comma separated list of city names or slugs to create output only for those cities. `CITIES_DISABLED` is a comma separated exclusion list and takes precedence over the allowlist. Persian city names and URL slugs are accepted.

For example, set `CITIES_ENABLED=tehran,mashhad` to include only those cities, or set `CITIES_DISABLED=tehran` to exclude Tehran while leaving other discovered cities enabled. City restrictions also limit listing discovery. Sitemap-only profile links without a city listing hint are skipped when a city restriction is active.

System environment variables override values from `.env`. If no `.env` is found in the current folder, the program loads the project `.env` beside `Cargo.toml`.

## Command options

| Option | Purpose |
| --- | --- |
| `--output PATH` | Choose the output folder |
| `--concurrency N` | Set concurrent requests, from 1 to 256 |
| `--requests-per-second N` | Set the shared request cap, from 1 to 24 |
| `--delay-ms N` | Add a delay before requests |
| `--limit N` | Limit new profile pages for this run |
| `--discover-only` | Discover and save profile URLs without fetching profiles |
| `--city SLUG` | Export one enabled city |
| `--no-images` | Skip profile photo downloads |
| `--browser-fetch` | Fetch pages through the local Chrome or Edge browser |
| `--fresh` | Clear the discovery and profile checkpoints |

## Output

Each city gets a numbered folder such as `1-tehran`. It contains an Excel workbook with compact right-to-left sheets, a Word report, a matching PDF, a complete JSON export, and an `images` folder. Profile photos are embedded in all three document formats. Exports split into parts of at most 400 doctors.

Clinic addresses are extracted from clinic address cards only. Biography text is never used to fill the address field. Clinic coordinates use place-level latitude and longitude and exclude city and province centroids. When hidden phones are requested, the browser clicks the site's reveal control. The scraper does not bypass access controls or anti-bot checks.
