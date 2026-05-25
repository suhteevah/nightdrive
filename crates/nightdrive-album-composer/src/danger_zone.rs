//! Danger-zone check: reject album track titles that double-hit canonical
//! soundtracks AND film objects/dialogue. Single-hit titles are allowed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DangerZoneFile {
    pub version: u32,
    pub themes: HashMap<String, ThemeZone>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeZone {
    #[serde(default)]
    pub soundtrack_titles: Vec<String>,
    #[serde(default)]
    pub film_objects: Vec<String>,
}

#[derive(Clone)]
pub struct Hit {
    pub track_title: String,
    pub matched_soundtrack: String,
    pub matched_film: String,
    pub theme_key: String,
}

impl std::fmt::Debug for Hit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Hit {{ title={:?}, soundtrack={:?}, film={:?}, theme={:?} }}",
            self.track_title, self.matched_soundtrack, self.matched_film, self.theme_key
        )
    }
}

#[tracing::instrument]
pub fn load<P: AsRef<Path> + std::fmt::Debug>(path: P) -> Result<DangerZoneFile, anyhow::Error> {
    let buf = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&buf)?)
}

/// A track title is a "hit" if (a) it appears in soundtrack_titles AND (b) appears in film_objects
/// for any of the supplied theme keys. Returns all double-hits across all enabled themes.
pub fn check_titles(titles: &[&str], zones: &DangerZoneFile, enabled_themes: &[String]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let norm = |s: &str| s.to_lowercase();
    for t in titles {
        let tn = norm(t);
        for theme_key in enabled_themes {
            let Some(zone) = zones.themes.get(theme_key) else { continue };
            let st_hit = zone.soundtrack_titles.iter().find(|s| norm(s) == tn);
            let fo_hit = zone.film_objects.iter().find(|s| norm(s) == tn);
            if let (Some(s), Some(f)) = (st_hit, fo_hit) {
                hits.push(Hit {
                    track_title: t.to_string(),
                    matched_soundtrack: s.clone(),
                    matched_film: f.clone(),
                    theme_key: theme_key.clone(),
                });
            }
        }
    }
    hits
}
