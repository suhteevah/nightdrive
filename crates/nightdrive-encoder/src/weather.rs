//! Forecast fetch + radar imagery + deterministic-fake fallback.
//!
//! Matt's 2026-05-11 framing: "if we pull in real time data and timestamp it
//! we can embed older weather data as historical np." So even though the
//! VOD pipeline doesn't NEED live data (the channel is pre-recorded synthwave
//! audio with weather-as-aesthetic), we pull it anyway and archive the raw
//! response per-track. Every VOD becomes a time capsule of the weather where
//! it was generated.
//!
//! ## Per-album region matching (2026-06-05)
//!
//! The TWC overlay (radar + 4-city forecast cycle) must match the album's
//! geographic theme — a Tokyo album shows Tokyo, not Miami. The album slug is
//! embedded in every `TrackId` (`nd-tokyo-cyberpunk-vol-1-001`), so
//! [`region_for`] routes on it:
//!
//! | Album slug contains | Region | Forecast backend | Radar |
//! |---|---|---|---|
//! | (none — US default) | hashed US region | NWS | NWS Ridge2 GIF |
//! | `tokyo` | JAPAN (Tokyo/Yokohama/Osaka/Kyoto) | Open-Meteo | RainViewer |
//! | `soviet`/`sovetskiy` | SOVIET (Moscow/Leningrad/Kyiv/Minsk) | Open-Meteo | RainViewer\* |
//! | `arctic`/`ice-station` | ARCTIC (Reykjavík/Nuuk/Murmansk/Yellowknife) | Open-Meteo | RainViewer\* |
//! | `hong-kong`/`kowloon` | HONG KONG | Open-Meteo | RainViewer |
//! | `shasta`/`telos` | SHASTA (Mt Shasta City/Weed/Dunsmuir/McCloud) | NWS | NWS Ridge2 GIF |
//!
//! \* RainViewer is fed by national radar networks. Japan/Iceland/most of
//! Europe are covered; Russia, Greenland, and high-arctic Canada are NOT —
//! for those the radar inset renders a real dark map of the region with no
//! precip echoes (same as a clear day). The forecast panel always gets real
//! data via Open-Meteo, which is global.
//!
//! ## Forecast backends
//!
//! - **NWS** (`api.weather.gov`): US only. Two-hop `/points` → `/gridpoints`.
//! - **Open-Meteo** (`api.open-meteo.com`): global, keyless. Daily highs/lows
//!   + WMO weather codes. Used for every non-US region.
//!
//! ## Radar GIF
//!
//! - **NWS**: download the pre-rendered Ridge2 `_loop.gif` (light basemap +
//!   colored precip). The encoder negates it (white land → dark, precip →
//!   magenta). `radar_prestyled == false`.
//! - **RainViewer**: transparent precip tiles, recolored to synthwave magenta
//!   and composited over a toned-down OSM night-map basemap into a loop GIF
//!   *here*. Already dark-styled, so the encoder skips the negate chain.
//!   `radar_prestyled == true`.

use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use nightdrive_core::TrackId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, warn};

const USER_AGENT: &str = "nightdrive/0.1 (mmichels88@gmail.com)";

// =============================================================================
// Region registry
// =============================================================================

/// One city in a region's 4-city forecast rotation.
#[derive(Debug, Clone, Copy)]
pub struct CityDef {
    /// Short uppercase name for the on-screen panel header ("TOKYO").
    pub display: &'static str,
    /// Long-form label preserved in the JSON archive ("Tokyo, JP").
    pub full: &'static str,
    pub lat: f64,
    pub lon: f64,
}

/// Which API serves a region's 5-day forecast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForecastBackend {
    /// `api.weather.gov` — US only.
    Nws,
    /// `api.open-meteo.com` — global, keyless.
    OpenMeteo,
}

/// Where a region's radar imagery comes from.
#[derive(Debug, Clone, Copy)]
pub enum RadarSource {
    /// NWS Ridge2 animated loop for a NEXRAD station (e.g. `"KAMX"`). The
    /// resulting GIF is a light basemap + colored precip; the encoder negates
    /// it. `radar_prestyled == false`.
    Nws(&'static str),
    /// RainViewer slippy tile (z/x/y) covering the region. Composited here
    /// into a pre-styled dark+magenta GIF; the encoder overlays without
    /// negating. `radar_prestyled == true`. Works even with no precip (the
    /// dark basemap still renders the region's geography).
    RainViewer { z: u32, x: u32, y: u32 },
}

/// A geographic region the TWC overlay can render.
#[derive(Debug, Clone, Copy)]
pub struct RegionDef {
    /// Stable key stored in `Forecast.region_key` for radar re-lookup.
    pub key: &'static str,
    /// Label shown on the radar panel header ("RADAR · TOKYO").
    pub label: &'static str,
    pub backend: ForecastBackend,
    pub radar: RadarSource,
    /// Exactly 4 cities — the forecast panel cycles them 30s each.
    pub cities: &'static [CityDef],
}

impl RegionDef {
    pub fn radar_prestyled(&self) -> bool {
        matches!(self.radar, RadarSource::RainViewer { .. })
    }
}

/// US regions — selected by `djb2(track_id) % 4` for any album whose slug
/// doesn't match a themed region. Preserves the original behavior.
static US_REGIONS: &[RegionDef] = &[
    RegionDef {
        key: "us-northwest",
        label: "NORTHWEST",
        backend: ForecastBackend::Nws,
        radar: RadarSource::Nws("KATX"),
        cities: &[
            CityDef { display: "SEATTLE", full: "Seattle, WA", lat: 47.6062, lon: -122.3321 },
            CityDef { display: "TACOMA", full: "Tacoma, WA", lat: 47.2529, lon: -122.4443 },
            CityDef { display: "BELLINGHAM", full: "Bellingham, WA", lat: 48.7519, lon: -122.4787 },
            CityDef { display: "EVERETT", full: "Everett, WA", lat: 47.9790, lon: -122.2021 },
        ],
    },
    RegionDef {
        key: "us-northeast",
        label: "NORTHEAST",
        backend: ForecastBackend::Nws,
        radar: RadarSource::Nws("KOKX"),
        cities: &[
            CityDef { display: "NEW YORK", full: "New York, NY", lat: 40.7128, lon: -74.0060 },
            CityDef { display: "NEWARK", full: "Newark, NJ", lat: 40.7357, lon: -74.1724 },
            CityDef { display: "WHITE PLAINS", full: "White Plains, NY", lat: 41.0339, lon: -73.7629 },
            CityDef { display: "BRIDGEPORT", full: "Bridgeport, CT", lat: 41.1865, lon: -73.1952 },
        ],
    },
    RegionDef {
        key: "us-southwest",
        label: "SOUTHWEST",
        backend: ForecastBackend::Nws,
        radar: RadarSource::Nws("KVTX"),
        cities: &[
            CityDef { display: "LOS ANGELES", full: "Los Angeles, CA", lat: 34.0522, lon: -118.2437 },
            CityDef { display: "LONG BEACH", full: "Long Beach, CA", lat: 33.7701, lon: -118.1937 },
            CityDef { display: "SANTA BARBARA", full: "Santa Barbara, CA", lat: 34.4208, lon: -119.6982 },
            CityDef { display: "ANAHEIM", full: "Anaheim, CA", lat: 33.8366, lon: -117.9143 },
        ],
    },
    RegionDef {
        key: "us-southeast",
        label: "SOUTHEAST",
        backend: ForecastBackend::Nws,
        radar: RadarSource::Nws("KAMX"),
        cities: &[
            CityDef { display: "MIAMI", full: "Miami, FL", lat: 25.7617, lon: -80.1918 },
            CityDef { display: "FORT LAUDERDALE", full: "Fort Lauderdale, FL", lat: 26.1224, lon: -80.1373 },
            CityDef { display: "KEY WEST", full: "Key West, FL", lat: 24.5551, lon: -81.7800 },
            CityDef { display: "NAPLES", full: "Naples, FL", lat: 26.1420, lon: -81.7948 },
        ],
    },
];

/// Themed (non-US) regions — matched by album slug substring in `region_for`.
/// RainViewer tile coords picked to frame each metro cluster (verified
/// 2026-06-05; see scratch/jp-radar-test/DESIGN.md).
static JAPAN: RegionDef = RegionDef {
    key: "japan",
    label: "TOKYO",
    backend: ForecastBackend::OpenMeteo,
    radar: RadarSource::RainViewer { z: 7, x: 113, y: 50 },
    cities: &[
        CityDef { display: "TOKYO", full: "Tokyo, JP", lat: 35.6762, lon: 139.6503 },
        CityDef { display: "YOKOHAMA", full: "Yokohama, JP", lat: 35.4437, lon: 139.6380 },
        CityDef { display: "OSAKA", full: "Osaka, JP", lat: 34.6937, lon: 135.5023 },
        CityDef { display: "KYOTO", full: "Kyoto, JP", lat: 35.0116, lon: 135.7681 },
    ],
};

static SOVIET: RegionDef = RegionDef {
    key: "soviet",
    label: "MOSCOW",
    backend: ForecastBackend::OpenMeteo,
    // RainViewer has little/no coverage over Russia — the inset renders a dark
    // map of European Russia with (usually) no echoes. Honest + on-theme.
    radar: RadarSource::RainViewer { z: 6, x: 38, y: 19 },
    cities: &[
        CityDef { display: "MOSCOW", full: "Moscow, RU", lat: 55.7558, lon: 37.6173 },
        CityDef { display: "LENINGRAD", full: "Leningrad, RU", lat: 59.9311, lon: 30.3609 },
        CityDef { display: "KYIV", full: "Kyiv, UA", lat: 50.4501, lon: 30.5234 },
        CityDef { display: "MINSK", full: "Minsk, BY", lat: 53.9006, lon: 27.5590 },
    ],
};

static ARCTIC: RegionDef = RegionDef {
    key: "arctic",
    label: "ARCTIC",
    backend: ForecastBackend::OpenMeteo,
    // Reykjavík tile — the one arctic city RainViewer covers. Others render
    // basemap-only.
    radar: RadarSource::RainViewer { z: 6, x: 28, y: 17 },
    cities: &[
        CityDef { display: "REYKJAVIK", full: "Reykjavik, IS", lat: 64.1466, lon: -21.9426 },
        CityDef { display: "NUUK", full: "Nuuk, GL", lat: 64.1814, lon: -51.6941 },
        CityDef { display: "MURMANSK", full: "Murmansk, RU", lat: 68.9585, lon: 33.0827 },
        CityDef { display: "YELLOWKNIFE", full: "Yellowknife, CA", lat: 62.4540, lon: -114.3718 },
    ],
};

static HONGKONG: RegionDef = RegionDef {
    key: "hongkong",
    label: "HONG KONG",
    backend: ForecastBackend::OpenMeteo,
    radar: RadarSource::RainViewer { z: 7, x: 105, y: 54 },
    cities: &[
        CityDef { display: "HONG KONG", full: "Hong Kong (Central)", lat: 22.2796, lon: 114.1722 },
        CityDef { display: "KOWLOON", full: "Kowloon, HK", lat: 22.3193, lon: 114.1694 },
        CityDef { display: "MACAU", full: "Macau", lat: 22.1987, lon: 113.5439 },
        CityDef { display: "GUANGZHOU", full: "Guangzhou, CN", lat: 23.1291, lon: 113.2644 },
    ],
};

/// Siskiyou Co., CA — the "Lost Worlds" saga launch (Telos beneath Mt. Shasta).
/// Slug-matched like the themed regions, but **NWS-native** (it's US soil —
/// Matt's backyard), so it uses the same forecast + radar path as `US_REGIONS`
/// and the encoder negates the Ridge2 loop (`radar_prestyled == false`).
/// Radar station **KMAX** (Medford, OR NEXRAD) is the NEXRAD covering the
/// Mt. Shasta area of far-northern California.
static SHASTA: RegionDef = RegionDef {
    key: "shasta",
    label: "MT SHASTA",
    backend: ForecastBackend::Nws,
    radar: RadarSource::Nws("KMAX"),
    cities: &[
        CityDef { display: "MT SHASTA", full: "Mt. Shasta City, CA", lat: 41.3099, lon: -122.3106 },
        CityDef { display: "WEED", full: "Weed, CA", lat: 41.4227, lon: -122.3861 },
        CityDef { display: "DUNSMUIR", full: "Dunsmuir, CA", lat: 41.2082, lon: -122.2722 },
        CityDef { display: "MCCLOUD", full: "McCloud, CA", lat: 41.2549, lon: -122.1392 },
    ],
};

/// Slug-matched regions (matched by album-slug substring in [`region_for`]).
/// JAPAN/SOVIET/ARCTIC/HONGKONG are non-US (Open-Meteo + RainViewer); SHASTA is
/// US/NWS. `region_by_key` also searches this list to re-resolve the radar.
static THEMED_REGIONS: &[&RegionDef] = &[&JAPAN, &SOVIET, &ARCTIC, &HONGKONG, &SHASTA];

/// Resolve the region for a track from the album slug embedded in its id.
/// Themed albums match by keyword; everything else hash-picks a US region
/// (the original behavior, preserved for the daily LLM-spec VOD path).
pub fn region_for(track_id: &TrackId) -> &'static RegionDef {
    let id = track_id.as_str().to_ascii_lowercase();
    // Order matters: "neo-tokyo" and "tokyo-cyberpunk" both → JAPAN.
    if id.contains("tokyo") {
        return &JAPAN;
    }
    if id.contains("soviet") || id.contains("sovetskiy") || id.contains("sovetsky") {
        return &SOVIET;
    }
    if id.contains("arctic") || id.contains("ice-station") || id.contains("ice_station")
        || id.contains("hollow") || id.contains("polar")
    {
        // Lost Worlds #2 (Hollow Earth) is the polar-opening descent → Arctic anchor.
        return &ARCTIC;
    }
    if id.contains("hong-kong") || id.contains("hongkong") || id.contains("kowloon") {
        return &HONGKONG;
    }
    // Lost Worlds saga launch — Telos beneath Mt. Shasta (NWS-native US region).
    if id.contains("shasta") || id.contains("telos") || id.contains("siskiyou") {
        return &SHASTA;
    }
    let h = djb2(track_id.as_str());
    &US_REGIONS[(h % US_REGIONS.len() as u64) as usize]
}

/// Look a region back up by its stored `key` (used when only a `Forecast` is
/// in hand). Falls back to the first US region if the key is unknown.
fn region_by_key(key: &str) -> &'static RegionDef {
    for r in US_REGIONS {
        if r.key == key {
            return r;
        }
    }
    for r in THEMED_REGIONS {
        if r.key == key {
            return r;
        }
    }
    &US_REGIONS[0]
}

/// Back-compat shim for callers wanting just the primary city tuple.
pub fn region_city_for(track_id: &TrackId) -> (&'static str, &'static str, f64, f64, &'static str) {
    let region = region_for(track_id);
    let primary = region.cities[0];
    let radar_id = match region.radar {
        RadarSource::Nws(s) => s,
        RadarSource::RainViewer { .. } => "rainviewer",
    };
    (region.label, primary.full, primary.lat, primary.lon, radar_id)
}

// =============================================================================
// Forecast types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayForecast {
    pub name: String,
    /// One-char synthwave-style condition glyph: `*` sunny, `o` cloudy, `~` rain.
    pub glyph: char,
    pub current: i32,
    pub high: i32,
    pub low: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CityForecast {
    /// Short uppercase name for the on-screen panel header ("TOKYO").
    pub display_name: String,
    /// Long-form label preserved in the JSON archive ("Tokyo, JP").
    pub full_label: String,
    pub lat: f64,
    pub lon: f64,
    pub days: Vec<DayForecast>,
    /// Raw upstream response when this city was pulled live. Kept verbatim so
    /// the historical archive is reproducible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forecast {
    pub fetched_at: DateTime<Utc>,
    pub source: ForecastSource,
    /// Human label shown on the radar panel header.
    pub region: String,
    /// Stable region key — used to re-resolve the radar source.
    #[serde(default)]
    pub region_key: String,
    /// NWS station id, or `rainviewer:z/x/y` for non-US regions. Archived.
    pub radar_station: String,
    /// True when the radar GIF is already dark-styled (RainViewer path) and
    /// the encoder must NOT run the NWS negate/chromakey chain.
    #[serde(default)]
    pub radar_prestyled: bool,
    /// 4 cities cycling through the on-screen panel, ~30s each.
    pub cities: Vec<CityForecast>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForecastSource {
    Nws,
    OpenMeteo,
    Synthetic,
    /// Some cities live, others fell back to synthetic.
    Partial,
}

// =============================================================================
// Forecast fetch
// =============================================================================

/// Fetch live forecasts for all 4 cities in the track's region, in parallel.
/// Per-city synthetic fallback: a single failed call doesn't sink the rest.
/// Always returns a 4-city `Forecast` — the pipeline never blocks on weather.
pub async fn fetch_or_synthesize(track_id: &TrackId) -> Forecast {
    let region = region_for(track_id);
    let radar_station = match region.radar {
        RadarSource::Nws(s) => s.to_string(),
        RadarSource::RainViewer { z, x, y } => format!("rainviewer:{z}/{x}/{y}"),
    };

    let http = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "couldn't build HTTP client — full synthetic fallback");
            return synthesize_full(track_id, region, &radar_station);
        }
    };

    let mut handles = Vec::with_capacity(region.cities.len());
    for (i, city) in region.cities.iter().enumerate() {
        let http = http.clone();
        let track_id = track_id.clone();
        let city = *city;
        let backend = region.backend;
        handles.push(tokio::spawn(async move {
            let result = match backend {
                ForecastBackend::Nws => fetch_one_city_nws(&http, city.lat, city.lon).await,
                ForecastBackend::OpenMeteo => {
                    fetch_one_city_open_meteo(&http, city.lat, city.lon).await
                }
            };
            match result {
                Ok((days, raw)) => CityForecast {
                    display_name: city.display.to_string(),
                    full_label: city.full.to_string(),
                    lat: city.lat,
                    lon: city.lon,
                    days,
                    raw: Some(raw),
                },
                Err(e) => {
                    warn!(
                        city = city.display,
                        error = %e,
                        "live fetch failed for city — synthesizing fallback for this slot"
                    );
                    synthesize_city(&track_id, city, i)
                }
            }
        }));
    }

    let mut city_results: Vec<CityForecast> = Vec::with_capacity(region.cities.len());
    for (idx, h) in handles.into_iter().enumerate() {
        match h.await {
            Ok(c) => city_results.push(c),
            Err(e) => {
                warn!(error = %e, "join error on city fetch — synthesizing slot");
                city_results.push(synthesize_city(track_id, region.cities[idx], idx));
            }
        }
    }

    let live_count = city_results.iter().filter(|c| c.raw.is_some()).count();
    let source = match (live_count, region.backend) {
        (0, _) => ForecastSource::Synthetic,
        (n, ForecastBackend::Nws) if n == city_results.len() => ForecastSource::Nws,
        (n, ForecastBackend::OpenMeteo) if n == city_results.len() => ForecastSource::OpenMeteo,
        _ => ForecastSource::Partial,
    };

    Forecast {
        fetched_at: Utc::now(),
        source,
        region: region.label.to_string(),
        region_key: region.key.to_string(),
        radar_station,
        radar_prestyled: region.radar_prestyled(),
        cities: city_results,
    }
}

/// NWS 5-day forecast. Two-hop: `/points` → `/gridpoints/.../forecast`.
async fn fetch_one_city_nws(
    http: &reqwest::Client,
    lat: f64,
    lon: f64,
) -> anyhow::Result<(Vec<DayForecast>, serde_json::Value)> {
    let points_url = format!("https://api.weather.gov/points/{lat:.4},{lon:.4}");
    debug!(%points_url, "NWS points lookup");
    let points: serde_json::Value = http
        .get(&points_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let office = points
        .get("properties")
        .and_then(|p| p.get("gridId"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing properties.gridId"))?;
    let grid_x = points
        .get("properties")
        .and_then(|p| p.get("gridX"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("missing properties.gridX"))?;
    let grid_y = points
        .get("properties")
        .and_then(|p| p.get("gridY"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("missing properties.gridY"))?;

    let fc_url = format!("https://api.weather.gov/gridpoints/{office}/{grid_x},{grid_y}/forecast");
    let raw: serde_json::Value =
        http.get(&fc_url).send().await?.error_for_status()?.json().await?;

    let periods = raw
        .get("properties")
        .and_then(|p| p.get("periods"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("missing properties.periods"))?;

    let days = nws_periods_to_days(periods)?;
    Ok((days, raw))
}

/// Open-Meteo 5-day daily forecast — global, keyless. Returns Fahrenheit
/// highs/lows + WMO weather codes.
async fn fetch_one_city_open_meteo(
    http: &reqwest::Client,
    lat: f64,
    lon: f64,
) -> anyhow::Result<(Vec<DayForecast>, serde_json::Value)> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
         &daily=weathercode,temperature_2m_max,temperature_2m_min\
         &temperature_unit=fahrenheit&timezone=auto&forecast_days=5"
    );
    debug!(%url, "Open-Meteo lookup");
    let raw: serde_json::Value =
        http.get(&url).send().await?.error_for_status()?.json().await?;

    let daily = raw
        .get("daily")
        .ok_or_else(|| anyhow::anyhow!("missing daily"))?;
    let times = daily.get("time").and_then(|v| v.as_array());
    let codes = daily.get("weathercode").and_then(|v| v.as_array());
    let maxes = daily.get("temperature_2m_max").and_then(|v| v.as_array());
    let mins = daily.get("temperature_2m_min").and_then(|v| v.as_array());
    let (times, codes, maxes, mins) = match (times, codes, maxes, mins) {
        (Some(t), Some(c), Some(mx), Some(mn)) => (t, c, mx, mn),
        _ => return Err(anyhow::anyhow!("malformed Open-Meteo daily block")),
    };

    let mut days = Vec::with_capacity(5);
    for i in 0..times.len().min(5) {
        let date = times[i].as_str().unwrap_or("");
        let code = codes.get(i).and_then(|v| v.as_i64()).unwrap_or(0);
        let high = maxes.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0).round() as i32;
        let low = mins.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0).round() as i32;
        days.push(DayForecast {
            name: weekday_abbrev(date),
            glyph: wmo_glyph(code),
            current: high,
            high,
            low,
        });
    }
    if days.is_empty() {
        return Err(anyhow::anyhow!("Open-Meteo returned no days"));
    }
    Ok((days, raw))
}

/// NWS "periods" are 12-hour slices ("Today/Tonight/Monday/..."). Pair
/// daytime + nighttime to produce up to 5 day-rows.
fn nws_periods_to_days(periods: &[serde_json::Value]) -> anyhow::Result<Vec<DayForecast>> {
    let mut days: Vec<DayForecast> = Vec::with_capacity(5);
    let mut i = 0usize;
    while days.len() < 5 && i < periods.len() {
        let p = &periods[i];
        let is_day = p.get("isDaytime").and_then(|v| v.as_bool()).unwrap_or(true);
        if !is_day {
            i += 1;
            continue;
        }
        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("---").to_string();
        let day_temp = p
            .get("temperature")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("period missing temperature"))? as i32;
        let short = p.get("shortForecast").and_then(|v| v.as_str()).unwrap_or("");
        let night_temp = periods
            .get(i + 1)
            .and_then(|np| np.get("temperature"))
            .and_then(|v| v.as_i64())
            .map(|t| t as i32)
            .unwrap_or(day_temp - 12);

        days.push(DayForecast {
            name: short_day_name(&name),
            glyph: condition_glyph(short),
            current: day_temp,
            high: day_temp,
            low: night_temp.min(day_temp - 1),
        });
        i += 2;
    }
    Ok(days)
}

fn weekday_abbrev(date: &str) -> String {
    match NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(d) => match d.weekday() {
            Weekday::Mon => "MON",
            Weekday::Tue => "TUE",
            Weekday::Wed => "WED",
            Weekday::Thu => "THU",
            Weekday::Fri => "FRI",
            Weekday::Sat => "SAT",
            Weekday::Sun => "SUN",
        }
        .to_string(),
        Err(_) => "---".to_string(),
    }
}

/// WMO weather code → synthwave glyph. Codes per the Open-Meteo / WMO 4677
/// table: 0-1 clear, 2-3 cloud, 45/48 fog, 51-67 drizzle/rain, 71-86
/// snow/showers, 95-99 thunder.
fn wmo_glyph(code: i64) -> char {
    match code {
        0 | 1 => '*',
        2 | 3 | 45 | 48 => 'o',
        51..=67 | 71..=86 | 95..=99 => '~',
        _ => '*',
    }
}

fn short_day_name(verbose: &str) -> String {
    let upper = verbose.to_ascii_uppercase();
    let prefixes = [
        ("MON", "MON"), ("TUE", "TUE"), ("WED", "WED"), ("THU", "THU"),
        ("FRI", "FRI"), ("SAT", "SAT"), ("SUN", "SUN"), ("TODAY", "TDY"), ("THIS", "TDY"),
    ];
    for (prefix, short) in &prefixes {
        if upper.starts_with(prefix) {
            return short.to_string();
        }
    }
    upper.chars().take(3).collect()
}

fn condition_glyph(short: &str) -> char {
    let s = short.to_ascii_lowercase();
    if s.contains("rain") || s.contains("shower") || s.contains("drizzle")
        || s.contains("storm") || s.contains("thunder")
    {
        '~'
    } else if s.contains("cloud") || s.contains("fog") || s.contains("haz") || s.contains("overcast")
    {
        'o'
    } else {
        '*'
    }
}

// =============================================================================
// Synthetic fallback
// =============================================================================

fn synthesize_full(track_id: &TrackId, region: &RegionDef, radar_station: &str) -> Forecast {
    let city_results: Vec<CityForecast> = region
        .cities
        .iter()
        .enumerate()
        .map(|(i, c)| synthesize_city(track_id, *c, i))
        .collect();
    Forecast {
        fetched_at: Utc::now(),
        source: ForecastSource::Synthetic,
        region: region.label.to_string(),
        region_key: region.key.to_string(),
        radar_station: radar_station.to_string(),
        radar_prestyled: region.radar_prestyled(),
        cities: city_results,
    }
}

fn synthesize_city(track_id: &TrackId, city: CityDef, city_idx: usize) -> CityForecast {
    const DAYS: [&str; 5] = ["MON", "TUE", "WED", "THU", "FRI"];
    let base = djb2(track_id.as_str())
        .wrapping_add((city_idx as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    let days = DAYS
        .iter()
        .enumerate()
        .map(|(i, &name)| {
            let seed = base.wrapping_add((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            const GLYPHS: [char; 4] = ['*', 'o', '~', '*'];
            let glyph = GLYPHS[((seed >> 8) & 0x3) as usize];
            let current = 60 + ((seed >> 16) % 25) as i32;
            let high_offset = 4 + ((seed >> 24) % 9) as i32;
            let low_offset = 6 + ((seed >> 32) % 9) as i32;
            DayForecast {
                name: name.to_string(),
                glyph,
                current,
                high: current + high_offset,
                low: current - low_offset,
            }
        })
        .collect();
    CityForecast {
        display_name: city.display.to_string(),
        full_label: city.full.to_string(),
        lat: city.lat,
        lon: city.lon,
        days,
        raw: None,
    }
}

// =============================================================================
// Radar
// =============================================================================

/// Produce `radar.gif` at `dest` for the forecast's region. NWS regions
/// download the pre-rendered Ridge2 loop; RainViewer regions composite a
/// pre-styled dark+magenta loop with `ffmpeg`. Best-effort: errors bubble so
/// the encoder can fall back to the empty inset.
pub async fn fetch_radar_gif(
    fc: &Forecast,
    dest: &Path,
    ffmpeg: &Path,
) -> anyhow::Result<PathBuf> {
    let region = region_by_key(&fc.region_key);
    match region.radar {
        RadarSource::Nws(station) => download_nws_loop(station, dest).await,
        RadarSource::RainViewer { z, x, y } => {
            build_rainviewer_gif(z, x, y, dest, ffmpeg).await
        }
    }
}

async fn download_nws_loop(station: &str, dest: &Path) -> anyhow::Result<PathBuf> {
    let url = format!("https://radar.weather.gov/ridge/standard/{station}_loop.gif");
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()?;
    let bytes = http.get(&url).send().await?.error_for_status()?.bytes().await?;
    tokio::fs::write(dest, &bytes).await?;
    debug!(station, url, bytes = bytes.len(), dest = %dest.display(), "NWS radar loop downloaded");
    Ok(dest.to_path_buf())
}

/// Build a synthwave radar loop from RainViewer precip tiles over a CARTO dark
/// basemap. Precip is recolored to magenta (`lutrgb`) and composited over a
/// brightened/cyan-tinted basemap; the basemap guarantees the inset always
/// shows the region's geography even when there's no precip. Validated
/// 2026-06-05 (scratch/jp-radar-test/DESIGN.md).
async fn build_rainviewer_gif(
    z: u32,
    x: u32,
    y: u32,
    dest: &Path,
    ffmpeg: &Path,
) -> anyhow::Result<PathBuf> {
    const MAX_FRAMES: usize = 12;
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()?;

    // 1. Frame index.
    let maps: serde_json::Value = http
        .get("https://api.rainviewer.com/public/weather-maps.json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let host = maps
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("rainviewer: missing host"))?;
    let past = maps
        .get("radar")
        .and_then(|r| r.get("past"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("rainviewer: missing radar.past"))?;
    let frames: Vec<&str> = past
        .iter()
        .rev()
        .take(MAX_FRAMES)
        .rev()
        .filter_map(|f| f.get("path").and_then(|v| v.as_str()))
        .collect();
    if frames.is_empty() {
        return Err(anyhow::anyhow!("rainviewer: no past frames available"));
    }

    // Work dir next to the destination.
    let work = dest
        .parent()
        .map(|p| p.join("_radar_frames"))
        .unwrap_or_else(|| PathBuf::from("_radar_frames"));
    tokio::fs::create_dir_all(&work).await?;

    // 2. Basemap (one OSM tile, 256px — scaled to 512 in the filter). OSM is
    //    used over CARTO because CARTO's dark tiles are near-featureless for
    //    coastal metros (Tokyo Bay reads as flat black); OSM has a bold,
    //    legible coastline + place labels that the filter tones down to a dark
    //    navy night-map. One tile per album render is well within OSM's
    //    fair-use (the client sends a contactful User-Agent).
    let base_png = work.join("base.png");
    let base_url = format!("https://tile.openstreetmap.org/{z}/{x}/{y}.png");
    let base_bytes = http.get(&base_url).send().await?.error_for_status()?.bytes().await?;
    tokio::fs::write(&base_png, &base_bytes).await?;

    // 3. Precip frames (transparent), numbered for ffmpeg's image2 sequence.
    let mut n = 0usize;
    for path in &frames {
        let url = format!("{host}{path}/512/{z}/{x}/{y}/4/1_1.png");
        match http.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(resp) => match resp.bytes().await {
                Ok(b) => {
                    let frame_png = work.join(format!("p_{n:03}.png"));
                    tokio::fs::write(&frame_png, &b).await?;
                    n += 1;
                }
                Err(e) => warn!(error = %e, "rainviewer: frame body read failed — skipping"),
            },
            Err(e) => warn!(error = %e, "rainviewer: frame fetch failed — skipping"),
        }
    }
    if n == 0 {
        return Err(anyhow::anyhow!("rainviewer: no precip frames fetched"));
    }

    // 4. Single ffmpeg pass: recolor precip → magenta, composite over the
    //    brightened basemap, two-pass palette for a clean looped GIF.
    let seq = work.join("p_%03d.png");
    // Precip → solid synthwave magenta (alpha preserved). Basemap → scaled to
    // 512, muted + darkened (colorlevels output cap) + navy/cyan tinted into a
    // night-map. Two-pass palette for a clean looped GIF. Tuned 2026-06-05
    // against live Tokyo tiles (scratch/jp-radar-test).
    let filter = "[1:v]format=rgba,lutrgb=r=255:g=42:b=168[pm];\
         [0:v]scale=512:512,format=rgba,eq=saturation=0.4,\
         colorlevels=romax=0.50:gomax=0.46:bomax=0.64,\
         colorbalance=rs=-0.10:bs=0.28:bm=0.16:bh=0.08[bm];\
         [bm][pm]overlay=shortest=1,split[a][b];\
         [a]palettegen=stats_mode=single[pal];\
         [b][pal]paletteuse=new=1";
    let status = Command::new(ffmpeg)
        .args(["-y", "-loglevel", "error", "-loop", "1", "-i"])
        .arg(&base_png)
        .args(["-framerate", "4", "-i"])
        .arg(&seq)
        .args(["-filter_complex", filter, "-frames:v", &n.to_string(), "-loop", "0"])
        .arg(dest)
        .status()
        .await?;
    if !status.success() {
        return Err(anyhow::anyhow!("ffmpeg radar gif assembly failed: {status}"));
    }

    // Best-effort cleanup of the scratch frames.
    let _ = tokio::fs::remove_dir_all(&work).await;

    debug!(z, x, y, frames = n, dest = %dest.display(), "RainViewer radar loop composited");
    Ok(dest.to_path_buf())
}

fn djb2(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn tid(slug_seed: u32) -> TrackId {
        TrackId::new(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(), slug_seed)
    }

    #[test]
    fn every_region_has_four_cities() {
        for r in US_REGIONS.iter().chain(THEMED_REGIONS.iter().map(|r| *r)) {
            assert_eq!(r.cities.len(), 4, "region {} must have 4 cities", r.key);
        }
    }

    #[test]
    fn region_for_default_is_us() {
        let region = region_for(&tid(1));
        assert!(region.key.starts_with("us-"), "non-themed id should hash to a US region");
        assert_eq!(region.backend, ForecastBackend::Nws);
        assert!(!region.radar_prestyled());
    }

    #[test]
    fn tokyo_slug_routes_to_japan() {
        // Album tracks carry the slug in the id (TrackId is a pub String).
        let id = TrackId("nd-tokyo-cyberpunk-vol-1-001".to_string());
        let region = region_for(&id);
        assert_eq!(region.key, "japan");
        assert_eq!(region.backend, ForecastBackend::OpenMeteo);
        assert!(region.radar_prestyled());
        assert_eq!(region.cities[0].display, "TOKYO");
    }

    #[test]
    fn soviet_and_arctic_route() {
        let s = TrackId("nd-sovetskiy-drive-vol-1-003".to_string());
        assert_eq!(region_for(&s).key, "soviet");
        let a = TrackId("nd-arctic-ice-station-vol-1-002".to_string());
        assert_eq!(region_for(&a).key, "arctic");
    }

    #[test]
    fn shasta_slug_routes_to_nws_native_region() {
        // Lost Worlds saga launch — the slug carries both "telos" and "shasta".
        let id = TrackId("nd-telos-shasta-vol-1-001".to_string());
        let region = region_for(&id);
        assert_eq!(region.key, "shasta");
        // US soil → NWS forecast + Ridge2 radar the encoder still negates.
        assert_eq!(region.backend, ForecastBackend::Nws);
        assert!(!region.radar_prestyled());
        assert!(matches!(region.radar, RadarSource::Nws("KMAX")));
        assert_eq!(region.cities[0].display, "MT SHASTA");
    }

    #[test]
    fn region_by_key_roundtrips() {
        assert_eq!(region_by_key("japan").label, "TOKYO");
        assert_eq!(region_by_key("shasta").label, "MT SHASTA");
        assert_eq!(region_by_key("us-southeast").label, "SOUTHEAST");
        assert_eq!(region_by_key("nonsense").key, "us-northwest");
    }

    #[test]
    fn wmo_glyph_classifies() {
        assert_eq!(wmo_glyph(0), '*');
        assert_eq!(wmo_glyph(3), 'o');
        assert_eq!(wmo_glyph(48), 'o');
        assert_eq!(wmo_glyph(61), '~');
        assert_eq!(wmo_glyph(95), '~');
    }

    #[test]
    fn weekday_abbrev_parses() {
        assert_eq!(weekday_abbrev("2026-06-08"), "MON");
        assert_eq!(weekday_abbrev("garbage"), "---");
    }

    #[test]
    fn synthesize_full_produces_4_cities() {
        let id = tid(1);
        let region = region_for(&id);
        let fc = synthesize_full(&id, region, "KATX");
        assert_eq!(fc.cities.len(), 4);
        assert_eq!(fc.source, ForecastSource::Synthetic);
        for city in &fc.cities {
            assert_eq!(city.days.len(), 5);
            for day in &city.days {
                assert!((60..=84).contains(&day.current));
                assert!(day.high > day.current);
                assert!(day.low < day.current);
            }
        }
    }

    #[test]
    fn synthetic_is_deterministic() {
        let id = tid(1);
        let region = region_for(&id);
        let a = synthesize_full(&id, region, "KATX");
        let b = synthesize_full(&id, region, "KATX");
        for (cx, cy) in a.cities.iter().zip(&b.cities) {
            assert_eq!(cx.display_name, cy.display_name);
            for (x, y) in cx.days.iter().zip(&cy.days) {
                assert_eq!(x.current, y.current);
                assert_eq!(x.high, y.high);
            }
        }
    }

    #[test]
    fn different_cities_have_different_forecasts() {
        let id = tid(1);
        let region = region_for(&id);
        let fc = synthesize_full(&id, region, "KATX");
        assert_ne!(fc.cities[0].days[0].current, fc.cities[1].days[0].current);
    }

    #[test]
    fn condition_glyph_classifies() {
        assert_eq!(condition_glyph("Sunny"), '*');
        assert_eq!(condition_glyph("Partly Cloudy"), 'o');
        assert_eq!(condition_glyph("Showers Likely"), '~');
        assert_eq!(condition_glyph("Patchy Fog"), 'o');
    }
}
