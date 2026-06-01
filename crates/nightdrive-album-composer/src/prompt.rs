//! Prompt builder for the album composer. Includes the 3 most-recently-modified
//! album JSONs as few-shot examples so the LLM matches house style.

use crate::schema::AlbumSpec;

pub fn build_prompt(
    theme: &str,
    slug: &str,
    track_count: u32,
    few_shot_examples: &[AlbumSpec],
    danger_zone_keys: &[String],
) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "You are nightdrive's album composer. Output a single JSON object matching the AlbumSpec schema.\n\
         No prose, no markdown fence, no commentary — JSON only.\n\n\
         Theme: {theme}\n\
         Album slug: {slug}\n\
         Track count: {track_count} (exactly).\n\
         Danger-zone theme keys to avoid double-hits in: {danger_zone_keys:?}\n\n\
         Rules:\n\
         - BPM 80-118 per track (slowed cruise + a few peaks).\n\
         - Duration 180-360 seconds per track.\n\
         - Each track has key, role (opener|cruiser|peak|bridge|closer), bpm, duration_seconds,\n\
           mood_tags[], sections[], musicgen_prompt, cover_prompt, key_relationship_to_prior,\n\
           tempo_relationship_to_prior, composer_notes.\n\
         - sections[] MUST be an array of OBJECTS, each exactly\n\
           {{\"name\": string, \"bars\": integer, \"instrumentation\": string}} — never bare strings.\n\
         - Use recurring_motifs to thread the album together (3-5 musical motifs that recur).\n\
         - Compose a narrative_arc (1-2 sentences).\n\
         - bpm_arc[] is the BPM of each track in order.\n\
         - Avoid track titles that ARE both a soundtrack-known title AND a film object/dialogue\n\
           (these would trigger algorithmic claims).\n\n\
         Examples of well-formed album JSONs (match this house style):\n\n"
    ));
    for ex in few_shot_examples {
        p.push_str("```json\n");
        p.push_str(&serde_json::to_string_pretty(ex).unwrap_or_else(|_| "{}".to_string()));
        p.push_str("\n```\n\n");
    }
    p.push_str("Now produce the AlbumSpec JSON for the requested theme + slug.\n");
    p
}
