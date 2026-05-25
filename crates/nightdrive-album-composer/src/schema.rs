//! Album JSON schema. Mirrors the existing docs/albums/<slug>.json shape so the
//! composer's output drops directly into the same on-disk format the rest of
//! the pipeline already consumes (orchestrator run-album, arranger, cover gen).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumSpec {
    pub album_slug: String,
    pub title: String,
    pub theme: String,
    pub track_count: u32,
    pub tonic_progression: String,
    pub bpm_arc: Vec<u32>,
    pub narrative_arc: String,
    /// Recurring motifs are rich objects in the existing album JSONs; keep as
    /// serde_json::Value to avoid schema drift.
    pub recurring_motifs: Vec<serde_json::Value>,
    pub tracks: Vec<TrackSpec>,
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackSpec {
    pub track_number: u32,
    pub title: String,
    pub role: String,
    pub key: String,
    pub bpm: u32,
    pub duration_seconds: u32,
    pub mood_tags: Vec<String>,
    pub sections: Vec<Section>,
    pub musicgen_prompt: String,
    pub cover_prompt: String,
    pub key_relationship_to_prior: String,
    pub tempo_relationship_to_prior: String,
    pub composer_notes: String,
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub name: String,
    pub bars: u32,
    pub instrumentation: String,
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}
