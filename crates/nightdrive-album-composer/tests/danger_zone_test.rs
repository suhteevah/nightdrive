use nightdrive_album_composer::danger_zone::{check_titles, DangerZoneFile, ThemeZone};
use std::collections::HashMap;

fn fixture() -> DangerZoneFile {
    let mut themes = HashMap::new();
    themes.insert("tron".into(), ThemeZone {
        soundtrack_titles: vec!["Derez".into(), "End of Line".into()],
        film_objects:      vec!["Derez".into(), "Light Cycle".into()],
    });
    themes.insert("blade_runner".into(), ThemeZone {
        soundtrack_titles: vec!["Tears in Rain".into()],
        film_objects:      vec!["Tears in Rain".into(), "Spinner".into()],
    });
    DangerZoneFile { version: 1, themes }
}

#[test]
fn double_hit_is_blocked() {
    let z = fixture();
    let hits = check_titles(&["Derez", "Cruiser One"], &z, &vec!["tron".into()]);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].track_title, "Derez");
}

#[test]
fn single_hit_passes() {
    let z = fixture();
    // "Light Cycle" is film_object but not soundtrack_title -> single hit, allowed
    let hits = check_titles(&["Light Cycle"], &z, &vec!["tron".into()]);
    assert!(hits.is_empty());
}

#[test]
fn case_insensitive() {
    let z = fixture();
    let hits = check_titles(&["derez"], &z, &vec!["tron".into()]);
    assert_eq!(hits.len(), 1);
}

#[test]
fn cross_theme_hits_only_in_enabled_themes() {
    let z = fixture();
    let hits = check_titles(&["Tears in Rain"], &z, &vec!["tron".into()]);
    assert!(hits.is_empty(), "blade_runner not enabled, so no hit");
    let hits = check_titles(&["Tears in Rain"], &z, &vec!["blade_runner".into()]);
    assert_eq!(hits.len(), 1);
}
