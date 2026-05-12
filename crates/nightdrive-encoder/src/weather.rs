//! NWS forecast fetch + radar imagery + deterministic-fake fallback.
//!
//! Matt's 2026-05-11 framing: "if we pull in real time data and timestamp it
//! we can embed older weather data as historical np." So even though the
//! VOD pipeline doesn't NEED live data (the channel is pre-recorded synthwave
//! audio with weather-as-aesthetic), we pull it anyway and archive the raw
//! response per-track. Every VOD becomes a time capsule of the weather where
//! it was generated. When the livestream goes live (post-240-min catalog), the
//! same code path serves the live data flow without changes.
//!
//! ## Multi-city per region
//!
//! Each radar region has 4 cities within the NEXRAD station's coverage area.
//! All 4 are fetched in parallel at encode time and the encoder cycles through
//! them every ~30s in the forecast panel — TWC "Local on the 8s" style. The
//! radar GIF stays static (it's already regional). Synthetic fallback also
//! produces 4 city stubs so the cycling layout is consistent regardless of
//! NWS reachability.

use chrono::{DateTime, Utc};
use nightdrive_core::TrackId;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, warn};

const USER_AGENT: &str = "nightdrive/0.1 (mmichels88@gmail.com)";

/// (region_label, radar_station, cities[])
///
/// Each city tuple is (display_name, full_label, lat, lon). `display_name`
/// is what shows on the forecast panel header ("MIAMI"); `full_label` is the
/// long form preserved in the archive ("Miami, FL"). 4 cities per region;
/// the cycling assumes exactly that count.
const REGIONS: &[(&str, &str, &[(&str, &str, f64, f64)])] = &[
    (
        "NORTHWEST",
        "KATX",
        &[
            ("SEATTLE", "Seattle, WA", 47.6062, -122.3321),
            ("TACOMA", "Tacoma, WA", 47.2529, -122.4443),
            ("BELLINGHAM", "Bellingham, WA", 48.7519, -122.4787),
            ("EVERETT", "Everett, WA", 47.9790, -122.2021),
        ],
    ),
    (
        "NORTHEAST",
        "KOKX",
        &[
            ("NEW YORK", "New York, NY", 40.7128, -74.0060),
            ("NEWARK", "Newark, NJ", 40.7357, -74.1724),
            ("WHITE PLAINS", "White Plains, NY", 41.0339, -73.7629),
            ("BRIDGEPORT", "Bridgeport, CT", 41.1865, -73.1952),
        ],
    ),
    (
        "SOUTHWEST",
        "KVTX",
        &[
            ("LOS ANGELES", "Los Angeles, CA", 34.0522, -118.2437),
            ("LONG BEACH", "Long Beach, CA", 33.7701, -118.1937),
            ("SANTA BARBARA", "Santa Barbara, CA", 34.4208, -119.6982),
            ("ANAHEIM", "Anaheim, CA", 33.8366, -117.9143),
        ],
    ),
    (
        "SOUTHEAST",
        "KAMX",
        &[
            ("MIAMI", "Miami, FL", 25.7617, -80.1918),
            ("FORT LAUDERDALE", "Fort Lauderdale, FL", 26.1224, -80.1373),
            ("KEY WEST", "Key West, FL", 24.5551, -81.7800),
            ("NAPLES", "Naples, FL", 26.1420, -81.7948),
        ],
    ),
];

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
    /// Short uppercase name for the on-screen panel header ("MIAMI").
    pub display_name: String,
    /// Long-form label preserved in the JSON archive ("Miami, FL").
    pub full_label: String,
    pub lat: f64,
    pub lon: f64,
    pub days: Vec<DayForecast>,
    /// Raw NWS response when source == Nws for this city. Kept verbatim so
    /// the historical archive is reproducible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_nws: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forecast {
    pub fetched_at: DateTime<Utc>,
    pub source: ForecastSource,
    pub region: String,
    pub radar_station: String,
    /// 4 cities cycling through the on-screen panel, ~30s each. The first
    /// city is the "primary" for any single-city display path.
    pub cities: Vec<CityForecast>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForecastSource {
    Nws,
    Synthetic,
    /// Mixed mode — some cities pulled live, others fell back to synthetic
    /// because their individual NWS calls failed.
    Partial,
}

/// Resolve the (region, radar_station, cities) tuple for this track. Pure
/// function of the track id — region rotates NW/NE/SE/SW based on
/// `djb2(id) % 4`.
pub fn region_for(
    track_id: &TrackId,
) -> (&'static str, &'static str, &'static [(&'static str, &'static str, f64, f64)]) {
    let h = djb2(track_id.as_str());
    let entry = &REGIONS[(h % REGIONS.len() as u64) as usize];
    (entry.0, entry.1, entry.2)
}

/// Back-compat shim for callers wanting just the primary city.
pub fn region_city_for(
    track_id: &TrackId,
) -> (&'static str, &'static str, f64, f64, &'static str) {
    let (region, station, cities) = region_for(track_id);
    let primary = cities[0];
    (region, primary.1, primary.2, primary.3, station)
}

/// Fetch live forecasts for all 4 cities in the track's region, in parallel.
/// Synthetic fallback per-city: if a single NWS call fails the rest still
/// land. Always returns a 4-city Forecast — the pipeline never blocks on
/// weather.
pub async fn fetch_or_synthesize(track_id: &TrackId) -> Forecast {
    let (region, station, cities) = region_for(track_id);

    // Kick off 4 NWS fetches in parallel via tokio::join_all over async blocks.
    let http = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "couldn't build HTTP client — full synthetic fallback");
            return synthesize_full(track_id, region, station, cities);
        }
    };

    let mut handles = Vec::with_capacity(cities.len());
    for (i, &(disp, full, lat, lon)) in cities.iter().enumerate() {
        let http = http.clone();
        let track_id = track_id.clone();
        handles.push(tokio::spawn(async move {
            let result = fetch_one_city(&http, lat, lon).await;
            match result {
                Ok((days, raw)) => CityForecast {
                    display_name: disp.to_string(),
                    full_label: full.to_string(),
                    lat,
                    lon,
                    days,
                    raw_nws: Some(raw),
                },
                Err(e) => {
                    warn!(
                        city = disp,
                        error = %e,
                        "NWS fetch failed for city — synthesizing fallback for this slot"
                    );
                    synthesize_city(&track_id, disp, full, lat, lon, i)
                }
            }
        }));
    }

    let mut city_results: Vec<CityForecast> = Vec::with_capacity(cities.len());
    for h in handles {
        match h.await {
            Ok(c) => city_results.push(c),
            Err(e) => {
                warn!(error = %e, "join error on city fetch — falling back to synthetic");
                // task panic — synthesize the slot from track_id alone
                let idx = city_results.len();
                let entry = cities[idx];
                city_results.push(synthesize_city(track_id, entry.0, entry.1, entry.2, entry.3, idx));
            }
        }
    }

    // Source label reflects how many cities actually came from NWS.
    let nws_count = city_results.iter().filter(|c| c.raw_nws.is_some()).count();
    let source = match nws_count {
        0 => ForecastSource::Synthetic,
        n if n == city_results.len() => ForecastSource::Nws,
        _ => ForecastSource::Partial,
    };

    Forecast {
        fetched_at: Utc::now(),
        source,
        region: region.to_string(),
        radar_station: station.to_string(),
        cities: city_results,
    }
}

/// Fetch one city's 5-day forecast from NWS. Two-hop: /points to resolve
/// gridpoint, then /gridpoints/.../forecast for the actual periods.
async fn fetch_one_city(
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

    let fc_url =
        format!("https://api.weather.gov/gridpoints/{office}/{grid_x},{grid_y}/forecast");
    let raw: serde_json::Value =
        http.get(&fc_url).send().await?.error_for_status()?.json().await?;

    let periods = raw
        .get("properties")
        .and_then(|p| p.get("periods"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("missing properties.periods"))?;

    let days = periods_to_days(periods)?;
    Ok((days, raw))
}

/// NWS returns "periods" — 12-hour slices with names "Today/Tonight/Monday/
/// Monday Night/...". Pair daytime + nighttime to produce 5 day-rows.
fn periods_to_days(periods: &[serde_json::Value]) -> anyhow::Result<Vec<DayForecast>> {
    let mut days: Vec<DayForecast> = Vec::with_capacity(5);
    let mut i = 0usize;
    while days.len() < 5 && i < periods.len() {
        let p = &periods[i];
        let is_day = p.get("isDaytime").and_then(|v| v.as_bool()).unwrap_or(true);
        if !is_day {
            i += 1;
            continue;
        }
        let name = p
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("---")
            .to_string();
        let day_temp = p
            .get("temperature")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("period missing temperature"))? as i32;
        let short = p
            .get("shortForecast")
            .and_then(|v| v.as_str())
            .unwrap_or("");
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

fn short_day_name(verbose: &str) -> String {
    let upper = verbose.to_ascii_uppercase();
    let prefixes = [("MON", "MON"), ("TUE", "TUE"), ("WED", "WED"), ("THU", "THU"),
        ("FRI", "FRI"), ("SAT", "SAT"), ("SUN", "SUN"), ("TODAY", "TDY"), ("THIS", "TDY")];
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
    } else if s.contains("cloud") || s.contains("fog") || s.contains("haz")
        || s.contains("overcast")
    {
        'o'
    } else {
        '*'
    }
}

/// Synthesize a fallback forecast for ALL cities — used when the HTTP client
/// can't even be constructed.
fn synthesize_full(
    track_id: &TrackId,
    region: &str,
    station: &str,
    cities: &[(&str, &str, f64, f64)],
) -> Forecast {
    let city_results: Vec<CityForecast> = cities
        .iter()
        .enumerate()
        .map(|(i, &(d, f, lat, lon))| synthesize_city(track_id, d, f, lat, lon, i))
        .collect();
    Forecast {
        fetched_at: Utc::now(),
        source: ForecastSource::Synthetic,
        region: region.to_string(),
        radar_station: station.to_string(),
        cities: city_results,
    }
}

/// Synthesize a single city's deterministic-fake 5-day forecast.
fn synthesize_city(
    track_id: &TrackId,
    display: &str,
    full: &str,
    lat: f64,
    lon: f64,
    city_idx: usize,
) -> CityForecast {
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
        display_name: display.to_string(),
        full_label: full.to_string(),
        lat,
        lon,
        days,
        raw_nws: None,
    }
}

/// Download the NWS Ridge2 animated radar loop GIF for `region`'s station
/// and save it to `dest`. Returns Ok(dest) on success.
pub async fn fetch_radar_gif(
    region: &str,
    dest: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let station = REGIONS
        .iter()
        .find(|e| e.0 == region)
        .map(|e| e.1)
        .ok_or_else(|| anyhow::anyhow!("unknown region for radar: {region}"))?;
    let url = format!("https://radar.weather.gov/ridge/standard/{station}_loop.gif");
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()?;
    let bytes = http
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    tokio::fs::write(dest, &bytes).await?;
    debug!(
        station,
        url,
        bytes = bytes.len(),
        dest = %dest.display(),
        "NWS radar loop downloaded"
    );
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

    #[test]
    fn region_has_four_cities() {
        let id = TrackId::new(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(), 1);
        let (_, _, cities) = region_for(&id);
        assert_eq!(cities.len(), 4, "every region must have exactly 4 cities");
    }

    #[test]
    fn synthesize_full_produces_4_cities() {
        let id = TrackId::new(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(), 1);
        let (region, station, cities) = region_for(&id);
        let fc = synthesize_full(&id, region, station, cities);
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
        let id = TrackId::new(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(), 1);
        let (region, station, cities) = region_for(&id);
        let a = synthesize_full(&id, region, station, cities);
        let b = synthesize_full(&id, region, station, cities);
        for (cx, cy) in a.cities.iter().zip(&b.cities) {
            assert_eq!(cx.display_name, cy.display_name);
            for (x, y) in cx.days.iter().zip(&cy.days) {
                assert_eq!(x.current, y.current);
                assert_eq!(x.high, y.high);
                assert_eq!(x.low, y.low);
            }
        }
    }

    #[test]
    fn different_cities_have_different_forecasts() {
        let id = TrackId::new(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(), 1);
        let (region, station, cities) = region_for(&id);
        let fc = synthesize_full(&id, region, station, cities);
        // Each city's seed is offset by `city_idx * golden-ratio constant`, so
        // adjacent cities should produce visibly different numbers.
        let c0 = &fc.cities[0];
        let c1 = &fc.cities[1];
        assert_ne!(
            c0.days[0].current, c1.days[0].current,
            "adjacent cities must produce distinct synthetic forecasts"
        );
    }

    #[test]
    fn condition_glyph_classifies() {
        assert_eq!(condition_glyph("Sunny"), '*');
        assert_eq!(condition_glyph("Partly Cloudy"), 'o');
        assert_eq!(condition_glyph("Showers Likely"), '~');
        assert_eq!(condition_glyph("Thunderstorm"), '~');
        assert_eq!(condition_glyph("Clear"), '*');
        assert_eq!(condition_glyph("Patchy Fog"), 'o');
    }

    #[test]
    fn short_day_name_squashes() {
        assert_eq!(short_day_name("Monday"), "MON");
        assert_eq!(short_day_name("Monday Night"), "MON");
        assert_eq!(short_day_name("This Afternoon"), "TDY");
        assert_eq!(short_day_name("Today"), "TDY");
        assert_eq!(short_day_name("Saturday Night"), "SAT");
    }
}
