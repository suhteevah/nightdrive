//! nightdrive-core — shared types, errors, observability bootstrap.
//!
//! Every other crate in the workspace depends on this.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

pub mod config;
pub mod observability;
pub mod retry;

// =============================================================================
// IDs
// =============================================================================

/// Stable track identifier, e.g. `nd-20260510-001`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub String);

impl TrackId {
    /// Generate a new id from today + sequence number.
    pub fn new(date: chrono::NaiveDate, sequence: u32) -> Self {
        Self(format!("nd-{}-{:03}", date.format("%Y%m%d"), sequence))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TrackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// =============================================================================
// CompositionSpec — the contract between LLM and the rest of the pipeline
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionSpec {
    pub track_id: TrackId,
    pub title: String,
    pub subgenre: String,
    pub mood_tags: Vec<String>,
    pub bpm: u32,
    pub musical_key: String,
    pub duration_seconds: u32,
    pub sections: Vec<Section>,
    pub musicgen_prompt: String,
    pub cover_prompt: String,
    pub youtube: YoutubeMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub name: String,
    pub bars: u32,
    pub instrumentation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoutubeMetadata {
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub category_id: String,
}

// =============================================================================
// TrackState — pipeline progress for a given track
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackState {
    Pending,
    SpecGenerated,
    AudioRendered,
    CoverRendered,
    AudioMastered,
    VideoEncoded,
    Published,
    Failed,
}

impl TrackState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::SpecGenerated => "spec_generated",
            Self::AudioRendered => "audio_rendered",
            Self::CoverRendered => "cover_rendered",
            Self::AudioMastered => "audio_mastered",
            Self::VideoEncoded => "video_encoded",
            Self::Published => "published",
            Self::Failed => "failed",
        }
    }
}

// =============================================================================
// Artifact paths — every stage knows where to write/read
// =============================================================================

#[derive(Debug, Clone)]
pub struct TrackPaths {
    pub root: PathBuf,
}

impl TrackPaths {
    pub fn new(work_dir: impl Into<PathBuf>, id: &TrackId) -> Self {
        let root = work_dir.into().join("tracks").join(id.as_str());
        Self { root }
    }

    pub fn spec_json(&self) -> PathBuf {
        self.root.join("spec.json")
    }
    pub fn raw_audio_wav(&self) -> PathBuf {
        self.root.join("raw.wav")
    }
    pub fn master_flac(&self) -> PathBuf {
        self.root.join("master.flac")
    }
    pub fn master_mp3(&self) -> PathBuf {
        self.root.join("master.mp3")
    }
    pub fn cover_png(&self) -> PathBuf {
        self.root.join("cover.png")
    }
    pub fn thumbnail_jpg(&self) -> PathBuf {
        self.root.join("thumbnail.jpg")
    }
    pub fn scene_mp4(&self) -> PathBuf {
        self.root.join("scene.mp4")
    }
    pub fn final_mp4(&self) -> PathBuf {
        self.root.join("final.mp4")
    }
}

// =============================================================================
// Errors — domain-level, shared
// =============================================================================

#[derive(Debug, Error)]
pub enum NightdriveError {
    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Audio generation error: {0}")]
    AudioGen(String),

    #[error("Audio mastering error: {0}")]
    AudioMaster(String),

    #[error("Cover art error: {0}")]
    Art(String),

    #[error("Visualizer error: {0}")]
    Visuals(String),

    #[error("Encoder error: {0}")]
    Encoder(String),

    #[error("YouTube upload error: {0}")]
    Youtube(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type NightdriveResult<T> = Result<T, NightdriveError>;
