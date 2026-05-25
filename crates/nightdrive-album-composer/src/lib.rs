//! nightdrive-album-composer — generates a 12-track album JSON for a given theme.

pub mod danger_zone;
pub mod prompt;
pub mod schema;

use nightdrive_openclaw_main::{ask_main, GatewayConfig};
use schema::AlbumSpec;
use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use tracing::{info, instrument, warn};

#[derive(Debug, thiserror::Error)]
pub enum ComposerError {
    #[error("openclaw main: {0}")]
    Llm(#[from] nightdrive_openclaw_main::OpenclawMainError),
    #[error("danger-zone strike after {attempts} attempt(s): {hits:?}")]
    DangerZoneBlocked { attempts: u32, hits: Vec<danger_zone::Hit> },
    #[error("invalid JSON from LLM (attempt {attempt}): {reason}")]
    InvalidJson { attempt: u32, reason: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("danger-zone load: {0}")]
    DangerZoneLoad(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

pub struct ComposeRequest {
    pub slug: String,
    pub theme: String,
    pub track_count: u32,
    pub danger_zone_keys: Vec<String>,
    pub albums_dir: PathBuf,
    pub danger_zone_path: PathBuf,
    pub max_retries: u32,
}

#[instrument(skip(cfg, req), fields(slug = %req.slug, track_count = req.track_count, max_retries = req.max_retries))]
pub async fn compose(cfg: &GatewayConfig, req: &ComposeRequest) -> Result<AlbumSpec, ComposerError> {
    if req.max_retries == 0 {
        return Err(ComposerError::InvalidRequest("max_retries must be >= 1".into()));
    }

    // Few-shot examples: 3 most-recent album JSONs. Prompt passed via stdin
    // (see nightdrive-openclaw-main::ask_main) so 128KB execve arg cap doesn't apply.
    let examples = load_recent_examples(&req.albums_dir, 3)?;
    info!(examples_loaded = examples.len(), "composer: few-shot examples loaded");

    let zones = danger_zone::load(&req.danger_zone_path)
        .map_err(|e| ComposerError::DangerZoneLoad(e.to_string()))?;

    let mut last_hits: Vec<danger_zone::Hit> = Vec::new();
    for attempt in 0..req.max_retries {
        let prompt_text = prompt::build_prompt(
            &req.theme,
            &req.slug,
            req.track_count,
            &examples,
            &req.danger_zone_keys,
        );
        info!(attempt, "composer: asking openclaw main");
        // Transient network errors propagate immediately to the caller; retry/fallback
        // (e.g. to LiteLLM Sonnet) is the caller's responsibility — see nightdrive-cli
        // Task 15's drop-next flow. We do NOT retry LLM transport here.
        let reply = ask_main(cfg, &prompt_text).await?;
        let json = strip_fence(&reply);
        let spec: AlbumSpec = serde_json::from_str(&json).map_err(|e| ComposerError::InvalidJson {
            attempt,
            reason: e.to_string(),
        })?;

        let titles: Vec<&str> = spec.tracks.iter().map(|t| t.title.as_str()).collect();
        let hits = danger_zone::check_titles(&titles, &zones, &req.danger_zone_keys);
        if hits.is_empty() {
            info!(slug = %req.slug, attempt, "composer: clean spec");
            return Ok(spec);
        }
        warn!(?hits, attempt, "composer: danger-zone hits, retrying");
        last_hits = hits;
    }

    Err(ComposerError::DangerZoneBlocked {
        attempts: req.max_retries,
        hits: last_hits,
    })
}

fn strip_fence(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

fn load_recent_examples(dir: &Path, n: usize) -> Result<Vec<AlbumSpec>, ComposerError> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(vec![]);
    };
    let mut entries: Vec<_> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort_by_key(|e| Reverse(e.metadata().and_then(|m| m.modified()).ok()));
    let mut out = Vec::new();
    for e in entries.into_iter().take(n) {
        let path = e.path();
        let buf = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<AlbumSpec>(&buf) {
            Ok(spec) => out.push(spec),
            Err(err) => tracing::warn!(path = %path.display(), %err, "load_recent_examples: skipping malformed album JSON"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::schema::AlbumSpec;

    #[test]
    fn all_existing_albums_parse() {
        for slug in [
            "atompunk-drive-vol-1",
            "neo-tokyo-drive-vol-1",
            "tron-drive-vol-1",
            "sunset-drive-vol-1",
            "sovetskiy-drive-vol-1",
        ] {
            let path = format!(
                "{}/../../docs/albums/{}.json",
                env!("CARGO_MANIFEST_DIR"),
                slug
            );
            let buf = std::fs::read_to_string(&path).expect(&path);
            let spec: AlbumSpec =
                serde_json::from_str(&buf).unwrap_or_else(|e| panic!("parse {slug}: {e}"));
            assert_eq!(spec.album_slug, slug, "slug mismatch in {slug}");
        }
    }
}
