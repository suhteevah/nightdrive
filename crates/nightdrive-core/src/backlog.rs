use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use tracing::instrument;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backlog {
    pub version: u32,
    #[serde(default)]
    pub youtube_strikes: u32,
    #[serde(default)]
    pub proposed: Vec<Proposed>,
    #[serde(default)]
    pub approved: Vec<Approved>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposed {
    pub slug: String,
    pub theme: String,
    pub proposed_at: DateTime<Utc>,
    pub promote_at: DateTime<Utc>,
    pub proposed_by: String,
    #[serde(default)]
    pub danger_zone_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approved {
    pub slug: String,
    pub theme: String,
    pub approved_at: DateTime<Utc>,
    #[serde(default)]
    pub danger_zone_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub slug: String,
    pub dropped_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum BacklogError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Constructed by `nightdrive-cli album backlog add` when validating uniqueness
    /// before mutating. Kept here so all backlog domain errors share a type.
    #[error("slug already exists in {section}: {slug}")]
    DuplicateSlug { section: &'static str, slug: String },
}

/// Load + lock-and-mutate. Closure runs while exclusive flock is held.
/// Atomic write: serialised state is fsynced to a `.tmp` sibling then renamed
/// over the target. Readers see only the pre-rename or post-rename file, never
/// a torn write. A separate `.lock` file is used for the flock so no handle is
/// held on the target file during the rename (avoids the Windows rename-over-
/// open-handle restriction).
#[instrument(skip(f))]
pub fn mutate<P, F>(path: P, f: F) -> Result<Backlog, BacklogError>
where
    P: AsRef<Path> + std::fmt::Debug,
    F: FnOnce(&mut Backlog) -> Result<(), BacklogError>,
{
    let path = path.as_ref();
    // Use a sibling lock file so we never hold a handle on the target during rename.
    let lock_path = path.with_extension("json.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock_exclusive()?;

    // Read current contents (separate from the lock file).
    let buf = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut bl: Backlog = if buf.trim().is_empty() {
        Backlog {
            version: 1,
            youtube_strikes: 0,
            proposed: vec![],
            approved: vec![],
            history: vec![],
        }
    } else {
        serde_json::from_str(&buf)?
    };

    f(&mut bl)?;

    let out = serde_json::to_string_pretty(&bl)?;
    // Atomic write: write to .tmp sibling, fsync, rename. On POSIX the rename is
    // atomic; on Windows it overwrites the target as a single op (cross-platform
    // via std::fs::rename since Rust 1.51 on Windows). The flock prevents
    // concurrent `mutate` writers; `load()` readers will only ever see the
    // pre-rename or post-rename file, never a torn write.
    let tmp_path = path.with_extension("json.tmp");
    {
        let mut tmp = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        tmp.write_all(out.as_bytes())?;
        tmp.write_all(b"\n")?;
        tmp.sync_all()?;
    } // tmp file closed here
    std::fs::rename(&tmp_path, path)?;

    #[allow(clippy::incompatible_msrv)] // fs2::FileExt::unlock — not a stdlib item
    lock_file.unlock()?;
    Ok(bl)
}

#[instrument]
pub fn load<P: AsRef<Path> + std::fmt::Debug>(path: P) -> Result<Backlog, BacklogError> {
    let buf = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&buf)?)
}

/// Move any proposed entries whose `promote_at <= now` to the tail of `approved`.
/// Returns the slugs that were promoted.
pub fn promote_expired(bl: &mut Backlog, now: DateTime<Utc>) -> Vec<String> {
    let mut promoted = Vec::new();
    let mut still_proposed = Vec::with_capacity(bl.proposed.len());
    for p in std::mem::take(&mut bl.proposed) {
        if p.promote_at <= now {
            promoted.push(p.slug.clone());
            bl.approved.push(Approved {
                slug: p.slug,
                theme: p.theme,
                approved_at: now,
                danger_zone_keys: p.danger_zone_keys,
            });
        } else {
            still_proposed.push(p);
        }
    }
    bl.proposed = still_proposed;
    promoted
}

/// Pop head of approved. Returns None if empty.
pub fn pop_approved(bl: &mut Backlog) -> Option<Approved> {
    if bl.approved.is_empty() {
        None
    } else {
        Some(bl.approved.remove(0))
    }
}

/// Peek head of approved without consuming. Used by dry-run paths.
pub fn peek_approved(bl: &Backlog) -> Option<&Approved> {
    bl.approved.first()
}
