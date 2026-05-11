//! NWS forecast fetch + deterministic-fake fallback.
//!
//! Matt's 2026-05-11 framing: "if we pull in real time data and timestamp it
//! we can embed older weather data as historical np." So even though the
//! VOD pipeline doesn't NEED live data (the channel is pre-recorded synthwave
//! audio with weather-as-aesthetic), we pull it anyway and archive the raw
//! response per-track. Every VOD becomes a time capsule of the weather where
//! it was generated. When the livestream goes live (post-240-min catalog), the
//! same code path serves the live data flow without changes.
//!
//! ## Region → city mapping
//!
//! The radar region label (NW/NE/SE/SW, picked by track_id hash) maps to one
//! representative city for the forecast pull. This ties the radar narrative
//! to the forecast narrative — "Northwest radar shows ... Seattle's 5-day."
//!
//! ## Fallback
//!
//! If NWS is unreachable, the user-agent is wrong, the grid lookup fails, or
//! any of the network steps error out, we synthesize a deterministic
//! placeholder forecast from `track_id`. The pipeline never blocks on weather.

use chrono::{DateTime, Utc};
use nightdrive_core::TrackId;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, warn};

/// NWS API requires a contact email in the User-Agent header. They reach out
/// at this address if our requests start misbehaving — that's the protocol.
const USER_AGENT: &str = "nightdrive/0.1 (mmichels88@gmail.com)";

/// (region_label, city, latitude, longitude, radar_station). The lat/lon points
/// are downtown-ish; NWS rounds to its grid cells anyway. radar_station is the
/// 4-letter NEXRAD site code used to fetch the Ridge2 animated radar loop —
/// KATX (Seattle), KOKX (NYC/Upton), KVTX (Los Angeles), KAMX (Miami).
const REGION_CITIES: &[(&str, &str, f64, f64, &str)] = &[
    ("NORTHWEST", "Seattle, WA", 47.6062, -122.3321, "KATX"),
    ("NORTHEAST", "New York, NY", 40.7128, -74.0060, "KOKX"),
    ("SOUTHWEST", "Los Angeles, CA", 34.0522, -118.2437, "KVTX"),
    ("SOUTHEAST", "Miami, FL", 25.7617, -80.1918, "KAMX"),
];

/// One day's worth of forecast data, exactly what the on-screen panel needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayForecast {
    pub name: String,
    /// One-char synthwave-style condition glyph: `*` sunny, `o` cloudy, `~` rain.
    pub glyph: char,
    pub current: i32,
    pub high: i32,
    pub low: i32,
}

/// Forecast bundle written to `paths.root/forecast.json` per track. The raw
/// NWS response is preserved verbatim alongside the derived display values so
/// future tooling can re-process without re-fetching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forecast {
    pub fetched_at: DateTime<Utc>,
    pub source: ForecastSource,
    pub region: String,
    pub city: String,
    pub lat: f64,
    pub lon: f64,
    pub days: Vec<DayForecast>,
    /// Raw NWS response when source == Nws. `null` when we fell back to
    /// synthetic. Kept verbatim so the historical archive is reproducible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_nws: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForecastSource {
    Nws,
    Synthetic,
}

/// Resolve the (region, city, lat, lon, radar_station) tuple for this track.
/// Pure function of the track id — region rotates NW/NE/SE/SW based on
/// `djb2(id) % 4`.
pub fn region_city_for(
    track_id: &TrackId,
) -> (&'static str, &'static str, f64, f64, &'static str) {
    let h = djb2(track_id.as_str());
    let entry = REGION_CITIES[(h % REGION_CITIES.len() as u64) as usize];
    (entry.0, entry.1, entry.2, entry.3, entry.4)
}

/// Download the NWS Ridge2 animated radar loop GIF for `region`'s station
/// and save it to `dest`. Returns `Ok(dest)` on success, `Err` on any HTTP
/// failure (caller falls back to no-radar — the inset stays empty). The
/// downloaded GIF is the historical record of what radar looked like at
/// render time; once on disk it sits next to forecast.json in paths.root.
pub async fn fetch_radar_gif(
    region: &str,
    dest: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let station = REGION_CITIES
        .iter()
        .find(|e| e.0 == region)
        .map(|e| e.4)
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

/// Try NWS first, fall back to synthetic-deterministic on any failure. Always
/// returns a valid 5-day forecast — the pipeline never blocks on weather.
pub async fn fetch_or_synthesize(track_id: &TrackId) -> Forecast {
    let (region, city, lat, lon, _station) = region_city_for(track_id);
    match fetch_nws(track_id, region, city, lat, lon).await {
        Ok(f) => f,
        Err(e) => {
            warn!(
                track_id = %track_id,
                region,
                city,
                error = %e,
                "NWS unreachable — falling back to deterministic synthetic forecast"
            );
            synthesize(track_id, region, city, lat, lon)
        }
    }
}

/// Real NWS round-trip: /points → /gridpoints/{office}/{x},{y}/forecast →
/// flatten the next ~10 periods into 5 day-rows with hi/lo/current. Saves the
/// raw NWS response into `Forecast.raw_nws` for the historical archive.
async fn fetch_nws(
    track_id: &TrackId,
    region: &str,
    city: &str,
    lat: f64,
    lon: f64,
) -> anyhow::Result<Forecast> {
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()?;

    // Step 1: resolve lat/lon → grid office + cell coordinates.
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
        .ok_or_else(|| anyhow::anyhow!("missing properties.gridId in /points response"))?;
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

    // Step 2: fetch the forecast for the resolved gridpoint.
    let fc_url =
        format!("https://api.weather.gov/gridpoints/{office}/{grid_x},{grid_y}/forecast");
    debug!(%fc_url, "NWS forecast pull");
    let raw: serde_json::Value = http
        .get(&fc_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let periods = raw
        .get("properties")
        .and_then(|p| p.get("periods"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("missing properties.periods array"))?;

    let days = periods_to_days(track_id, periods)?;

    Ok(Forecast {
        fetched_at: Utc::now(),
        source: ForecastSource::Nws,
        region: region.to_string(),
        city: city.to_string(),
        lat,
        lon,
        days,
        raw_nws: Some(raw),
    })
}

/// NWS returns "periods" — each is a 12-ish hour slice with name like
/// "Today", "Tonight", "Monday", "Monday Night", etc. To produce 5 day-rows
/// with hi/lo/current we pair daytime + nighttime periods.
fn periods_to_days(
    track_id: &TrackId,
    periods: &[serde_json::Value],
) -> anyhow::Result<Vec<DayForecast>> {
    let mut days: Vec<DayForecast> = Vec::with_capacity(5);
    let mut i = 0usize;
    while days.len() < 5 && i < periods.len() {
        let p = &periods[i];
        let is_day = p.get("isDaytime").and_then(|v| v.as_bool()).unwrap_or(true);
        if !is_day {
            // Skip an opening "Tonight" — we want pairs that start with day.
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

        // Look at the next period for the matching night low.
        let night_temp = periods
            .get(i + 1)
            .and_then(|np| np.get("temperature"))
            .and_then(|v| v.as_i64())
            .map(|t| t as i32)
            .unwrap_or(day_temp - 12);

        // For the first day-row, "current" is the daytime forecast high
        // (NWS doesn't expose live current temperature here; that's a
        // separate /stations/.../observations call). For the future rows,
        // "current" doesn't really map; we fill it with the day's high
        // again to keep the panel format consistent.
        let current = day_temp;
        let high = day_temp;
        let low = night_temp.min(day_temp - 1);

        days.push(DayForecast {
            name: short_day_name(&name),
            glyph: condition_glyph(short),
            current,
            high,
            low,
        });
        i += 2;
    }
    // Pad up to 5 in case NWS returned fewer periods than expected (network
    // weirdness, end-of-grid).
    while days.len() < 5 {
        let seed = djb2(track_id.as_str()).wrapping_add(days.len() as u64);
        days.push(synthetic_day(seed, days.len()));
    }
    Ok(days)
}

/// NWS period names are verbose ("Monday", "Monday Night", "This Afternoon").
/// Squash to 3-char day names that fit the VT323 panel layout.
fn short_day_name(verbose: &str) -> String {
    let upper = verbose.to_ascii_uppercase();
    if upper.starts_with("MON") {
        "MON".into()
    } else if upper.starts_with("TUE") {
        "TUE".into()
    } else if upper.starts_with("WED") {
        "WED".into()
    } else if upper.starts_with("THU") {
        "THU".into()
    } else if upper.starts_with("FRI") {
        "FRI".into()
    } else if upper.starts_with("SAT") {
        "SAT".into()
    } else if upper.starts_with("SUN") {
        "SUN".into()
    } else if upper.starts_with("TODAY") || upper.starts_with("THIS") {
        "TDY".into()
    } else {
        // Fall back to the first 3 letters of whatever NWS gave us.
        upper.chars().take(3).collect()
    }
}

/// Map NWS shortForecast strings to the three-glyph synthwave palette.
fn condition_glyph(short: &str) -> char {
    let s = short.to_ascii_lowercase();
    if s.contains("rain")
        || s.contains("shower")
        || s.contains("drizzle")
        || s.contains("storm")
        || s.contains("thunder")
    {
        '~'
    } else if s.contains("cloud") || s.contains("fog") || s.contains("haz") || s.contains("overcast")
    {
        'o'
    } else {
        '*'
    }
}

/// Deterministic-fake forecast — same shape, same fields, no network. Used
/// when NWS is unreachable or for offline tests.
pub fn synthesize(
    track_id: &TrackId,
    region: &str,
    city: &str,
    lat: f64,
    lon: f64,
) -> Forecast {
    const DAYS: [&str; 5] = ["MON", "TUE", "WED", "THU", "FRI"];
    let base = djb2(track_id.as_str());
    let days = DAYS
        .iter()
        .enumerate()
        .map(|(i, &name)| {
            let seed = base.wrapping_add((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut d = synthetic_day(seed, i);
            d.name = name.to_string();
            d
        })
        .collect();
    Forecast {
        fetched_at: Utc::now(),
        source: ForecastSource::Synthetic,
        region: region.to_string(),
        city: city.to_string(),
        lat,
        lon,
        days,
        raw_nws: None,
    }
}

fn synthetic_day(seed: u64, idx: usize) -> DayForecast {
    const GLYPHS: [char; 4] = ['*', 'o', '~', '*'];
    let glyph = GLYPHS[((seed >> 8) & 0x3) as usize];
    let current = 60 + ((seed >> 16) % 25) as i32;
    let high_offset = 4 + ((seed >> 24) % 9) as i32;
    let low_offset = 6 + ((seed >> 32) % 9) as i32;
    DayForecast {
        name: ["MON", "TUE", "WED", "THU", "FRI"]
            .get(idx)
            .copied()
            .unwrap_or("DAY")
            .to_string(),
        glyph,
        current,
        high: current + high_offset,
        low: current - low_offset,
    }
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
    fn region_city_round_trips() {
        let id = TrackId::new(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(), 1);
        let (region, city, lat, lon, _station) = region_city_for(&id);
        assert!(["NORTHWEST", "NORTHEAST", "SOUTHWEST", "SOUTHEAST"].contains(&region));
        assert!(!city.is_empty());
        assert!(lat > 20.0 && lat < 50.0); // continental US range
        assert!(lon > -130.0 && lon < -65.0);
    }

    #[test]
    fn synthetic_is_deterministic() {
        let id = TrackId::new(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(), 1);
        let (region, city, lat, lon, _station) = region_city_for(&id);
        let a = synthesize(&id, region, city, lat, lon);
        let b = synthesize(&id, region, city, lat, lon);
        for (x, y) in a.days.iter().zip(&b.days) {
            assert_eq!(x.current, y.current);
            assert_eq!(x.high, y.high);
            assert_eq!(x.low, y.low);
        }
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
