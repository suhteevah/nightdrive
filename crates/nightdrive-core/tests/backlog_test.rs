use chrono::{Duration, Utc};
use nightdrive_core::backlog::{self, Backlog, Proposed};
use tempfile::tempdir;

// This test deliberately couples to the canonical seed file content from Task 6.
// If the seed changes intentionally, update the assertion. A parse failure here
// means the seed JSON itself is malformed.
#[test]
fn load_seed() {
    let bl: Backlog = serde_json::from_str(include_str!("../../../docs/album-backlog.json"))
        .expect("seed parses");
    assert_eq!(bl.approved.len(), 4);
    assert_eq!(bl.proposed.len(), 0);
    assert_eq!(bl.history.len(), 5);
}

#[test]
fn promote_expired_moves_old_proposals() {
    let now = Utc::now();
    let mut bl = Backlog {
        version: 1,
        youtube_strikes: 0,
        proposed: vec![
            Proposed {
                slug: "expired".into(),
                theme: "x".into(),
                proposed_at: now - Duration::days(2),
                promote_at: now - Duration::hours(1),
                proposed_by: "test".into(),
                danger_zone_keys: vec![],
            },
            Proposed {
                slug: "fresh".into(),
                theme: "y".into(),
                proposed_at: now,
                promote_at: now + Duration::hours(23),
                proposed_by: "test".into(),
                danger_zone_keys: vec![],
            },
        ],
        approved: vec![],
        history: vec![],
    };
    let promoted = backlog::promote_expired(&mut bl, now);
    assert_eq!(promoted, vec!["expired"]);
    assert_eq!(bl.approved.len(), 1);
    assert_eq!(bl.proposed.len(), 1);
    assert_eq!(bl.proposed[0].slug, "fresh");
}

#[test]
fn mutate_round_trips_via_flock() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("b.json");
    backlog::mutate(&path, |bl| {
        bl.version = 1;
        bl.approved.push(backlog::Approved {
            slug: "test-vol-1".into(),
            theme: "t".into(),
            approved_at: Utc::now(),
            danger_zone_keys: vec![],
        });
        Ok(())
    })
    .unwrap();
    let bl = backlog::load(&path).unwrap();
    assert_eq!(bl.approved.len(), 1);
    assert_eq!(bl.approved[0].slug, "test-vol-1");
}
