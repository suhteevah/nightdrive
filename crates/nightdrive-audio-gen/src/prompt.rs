//! Engine-specific prompt formatters. Pure deterministic translation from
//! [`CompositionSpec`] → engine-native prompt strings. No LLM, no I/O.
//!
//! The model-specific prompt syntaxes differ enough that having one global
//! `musicgen_prompt` field on the spec and feeding it to every engine
//! verbatim leaves quality on the table:
//!
//! - **MusicGen** wants a comma-separated descriptor list, ≤60 words,
//!   no section structure. Continuation calls reuse the same prompt for
//!   every segment.
//! - **Stable Audio Open** wants a similar comma-separated list but with
//!   `loopable` / `<N> bpm` hints baked in. Per-segment generation also
//!   wants the same prompt.
//! - **ACE-Step** wants TWO fields: a `caption` (≤512 chars, natural-language
//!   description) **and** a `lyrics` block with `[Section - notes]` tags
//!   that the model uses as structural anchors. For instrumental tracks the
//!   per-section tags carry our `Section.instrumentation` hints as "lyrics" —
//!   ACE-Step doesn't try to *sing* "pad swell + filtered arp," it uses the
//!   line as a structural cue ("we're in the intro section, instrumentation
//!   has this character"). This is the per-section progression that MG was
//!   throwing away.
//!
//! Once we have an audio QC ("listener") agent, the prompt formatters could
//! be augmented to accept critique feedback. Today they're stateless.

use nightdrive_core::CompositionSpec;

/// Format the natural-language description ACE-Step takes as `caption`.
///
/// Strategy: trust the album-composer's `musicgen_prompt` as the descriptive
/// backbone (it already names instruments + mood + production) and ensure an
/// instrumental directive is present. BPM/key are NOT appended — ACE-Step 1.5
/// takes those via its dedicated `bpm`/`keyscale` request fields (its docs say
/// don't put tempo/BPM/key in the caption). The ~510-char cap is enforced with
/// a `…` truncation rather than a silent drop. See docs/acestep-prompting.md.
pub fn format_ace_step_caption(spec: &CompositionSpec) -> String {
    let mut caption = String::with_capacity(512);
    caption.push_str(spec.musicgen_prompt.trim());

    // Anti-vocals nudge — ACE-Step will occasionally generate vocal stabs
    // even on tracks with `[Instrumental]` lyrics if the caption doesn't
    // explicitly forbid them. Belt + suspenders.
    if !contains_case_insensitive(&caption, "no vocals")
        && !contains_case_insensitive(&caption, "instrumental")
    {
        push_separated(&mut caption, "no vocals, instrumental");
    }

    // BPM + key are deliberately NOT appended here. ACE-Step 1.5's docs are
    // explicit: tempo/BPM/key belong in the dedicated `bpm` / `keyscale` request
    // fields (which the sidecar request populates), NOT the caption — repeating
    // them is redundant. (The composer's prose may still mention them inline;
    // that's a separate, future composer-prompt cleanup.) See
    // docs/acestep-prompting.md.
    truncate_to_chars(caption, 510, "…")
}

/// Format the structured `lyrics` field ACE-Step uses for section progression.
/// For instrumental nightdrive tracks each line is a `[Section - notes]`
/// block taken from `spec.sections[]`.
///
/// Example for a 5-section spec:
///
/// ```text
/// [Intro - pad swell + filtered arp]
/// [Verse - + sub bass + soft drums]
/// [Chorus - + lead + sidechain pump]
/// [Bridge - stripped, only pad + bass]
/// [Outro - tape stop fade]
/// ```
///
/// When `sections[]` is empty (defensive case) we fall back to a single
/// `[Instrumental]` tag so the request still has a valid lyrics block.
pub fn format_ace_step_lyrics(spec: &CompositionSpec) -> String {
    // 2026-05-22: ACE-Step was synthesizing the instrumentation-hint text
    // ("pad swell + filtered arp" etc.) as ghost vocal phonemes when those
    // hints were included inside the [Section - hint] brackets. The hints
    // are already carried in `spec.musicgen_prompt` (the caption), so
    // emitting them in the lyrics field is pure leakage. Fix: section name
    // only — ACE-Step still gets the structural anchor it needs, no
    // vocalizable content. Lead with `[Instrumental]` as a global no-vocals
    // cue. See feedback_acestep_lyrics_leak_via_section_hints memory.
    if spec.sections.is_empty() {
        return "[Instrumental]".to_string();
    }

    let mut buf = String::from("[Instrumental]\n");
    buf.reserve(16 * spec.sections.len());
    for section in &spec.sections {
        let name_title = title_case_word(&section.name);
        buf.push_str(&format!("[{name_title}]\n"));
    }
    // Strip the trailing newline so the request is tidy.
    if buf.ends_with('\n') {
        buf.pop();
    }
    buf
}

/// Format a per-section prompt for the MusicGen continuation chain.
///
/// Where `MusicGenClient` today sends `spec.musicgen_prompt` for every
/// segment, the section-aware path picks the section that covers the
/// segment's start-time and produces a focused prompt that mentions the
/// global track aesthetic *plus* the section's instrumentation hint.
///
/// Returns `None` if `section_idx` is out of bounds — caller falls back to
/// `spec.musicgen_prompt`.
pub fn format_musicgen_section_prompt(
    spec: &CompositionSpec,
    section_idx: usize,
) -> Option<String> {
    let section = spec.sections.get(section_idx)?;
    let aesthetic = trim_first_clause(&spec.musicgen_prompt, 200);
    let inst = section.instrumentation.trim();
    let prompt = if inst.is_empty() {
        format!(
            "{aesthetic}, {} section, {} BPM, {}, instrumental",
            section.name, spec.bpm, spec.musical_key,
        )
    } else {
        format!(
            "{aesthetic}, {} section: {inst}, {} BPM, {}, instrumental",
            section.name, spec.bpm, spec.musical_key,
        )
    };
    Some(truncate_to_chars(prompt, 600, "…"))
}

/// Map a cumulative time-within-track (seconds) to the index of the
/// [`Section`] that covers it. Assumes all sections are equal-bars-per-beat
/// at the spec's BPM, distributed across `spec.duration_seconds`.
///
/// Returns `None` if the spec has no sections.
pub fn section_for_time(spec: &CompositionSpec, t_seconds: f32) -> Option<usize> {
    if spec.sections.is_empty() {
        return None;
    }
    let total_bars: u32 = spec.sections.iter().map(|s| s.bars).sum();
    if total_bars == 0 {
        return Some(0);
    }
    let bar_seconds = if spec.bpm == 0 {
        // Defensive: avoid div-by-zero. Default 100 BPM -> 0.6 s/beat -> 2.4 s/bar @ 4/4.
        2.4
    } else {
        // 4 beats per bar (we assume 4/4 — composer-spec'd time sigs aren't
        // surfaced through `Section` yet). 60s/bpm * 4 beats/bar.
        60.0 / spec.bpm as f32 * 4.0
    };
    let mut bars_seen: u32 = 0;
    for (idx, sec) in spec.sections.iter().enumerate() {
        let section_end_bars = bars_seen + sec.bars;
        let section_end_seconds = section_end_bars as f32 * bar_seconds;
        if t_seconds < section_end_seconds {
            return Some(idx);
        }
        bars_seen = section_end_bars;
    }
    Some(spec.sections.len() - 1) // past the end -> last section
}

// =============================================================================
// String helpers — kept private; tested indirectly via the public functions.
// =============================================================================

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
}

fn push_separated(buf: &mut String, addition: &str) {
    if buf.trim_end().is_empty() {
        buf.push_str(addition);
    } else {
        let last = buf.chars().last().unwrap_or(' ');
        if matches!(last, ',' | '.' | ';' | '\n') {
            buf.push(' ');
        } else {
            buf.push_str(", ");
        }
        buf.push_str(addition);
    }
}

fn truncate_to_chars(s: String, max: usize, ellipsis: &str) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(ellipsis.chars().count())).collect();
    out.push_str(ellipsis);
    out
}

fn title_case_word(s: &str) -> String {
    let mut chars = s.trim().chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase(),
    }
}

/// Take the first comma-separated clause of `s` (up to `max` chars). Used to
/// extract the "aesthetic backbone" of the album-composer's musicgen_prompt
/// without dragging the entire 80-word descriptor into each section prompt.
fn trim_first_clause(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Grab up to the first 2-3 commas so we keep "genre BPM key" structure
    // when the composer-prompt is "synthwave 88 BPM A minor, hazy DX7 pads, ..."
    let mut split = trimmed.splitn(4, ',');
    let mut head = String::new();
    for (i, part) in split.by_ref().enumerate() {
        if i == 0 {
            head.push_str(part.trim());
        } else if i < 3 {
            head.push_str(", ");
            head.push_str(part.trim());
        } else {
            break;
        }
    }
    truncate_to_chars(head, max, "…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightdrive_core::{Section, TrackId, YoutubeMetadata};

    fn sample_spec_5_section() -> CompositionSpec {
        CompositionSpec {
            track_id: TrackId("nd-20260516-001".to_string()),
            title: "Apex".to_string(),
            subgenre: "synthwave".to_string(),
            mood_tags: vec!["nocturnal".to_string(), "driving".to_string()],
            bpm: 108,
            musical_key: "D major".to_string(),
            duration_seconds: 240,
            sections: vec![
                Section { name: "intro".to_string(),  bars: 4,  instrumentation: "pad swell + filtered arp".to_string() },
                Section { name: "verse".to_string(),  bars: 16, instrumentation: "+ sub bass + soft drums".to_string() },
                Section { name: "chorus".to_string(), bars: 16, instrumentation: "+ lead + sidechain pump".to_string() },
                Section { name: "bridge".to_string(), bars: 8,  instrumentation: "stripped, only pad + bass".to_string() },
                Section { name: "outro".to_string(),  bars: 8,  instrumentation: "tape stop fade".to_string() },
            ],
            musicgen_prompt:
                "synthwave 108 BPM D major peak track, lush DX7 pad, bright analog lead, \
                 sidechained sub bass, gated reverb drums, neon-soaked driving energy, instrumental"
                    .to_string(),
            cover_prompt: "synthwave 1985 album cover".to_string(),
            youtube: YoutubeMetadata {
                title: "Apex".to_string(),
                description: "".to_string(),
                tags: vec![],
                category_id: "10".to_string(),
            },
        }
    }

    #[test]
    fn ace_step_caption_does_not_append_bpm_or_key() {
        // ACE-Step 1.5: tempo/BPM/key belong in the dedicated bpm/keyscale
        // request fields, not the caption. With a prose that omits them, the
        // caption must not introduce them.
        let mut spec = sample_spec_5_section();
        spec.musicgen_prompt =
            "synthwave peak track, lush DX7 pad, bright analog lead, sidechained sub bass, instrumental"
                .to_string();
        let cap = format_ace_step_caption(&spec);
        assert!(!cap.to_ascii_lowercase().contains("bpm"), "caption must not append BPM: {cap}");
        assert!(!cap.contains("D major"), "caption must not append key: {cap}");
        // The composer's instrumental directive survives.
        assert!(cap.contains("instrumental"));
        // Caption respects the ACE-Step length ceiling.
        assert!(cap.chars().count() <= 512, "caption longer than ACE-Step's 512-char cap");
    }

    #[test]
    fn ace_step_caption_adds_no_vocals_when_missing() {
        let mut spec = sample_spec_5_section();
        spec.musicgen_prompt = "synthwave with bright pads".to_string();
        let cap = format_ace_step_caption(&spec);
        assert!(cap.contains("no vocals"));
        assert!(cap.contains("instrumental"));
    }

    #[test]
    fn ace_step_lyrics_emits_section_blocks() {
        // 2026-05-22: format intentionally NO LONGER includes the
        // instrumentation hint after the dash — ACE-Step was vocalizing
        // those phrases as ghost lyrics. Section name only; [Instrumental]
        // at top as global no-vocals cue.
        let spec = sample_spec_5_section();
        let lyrics = format_ace_step_lyrics(&spec);
        let lines: Vec<&str> = lyrics.split('\n').collect();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], "[Instrumental]");
        assert_eq!(lines[1], "[Intro]");
        assert_eq!(lines[2], "[Verse]");
        assert_eq!(lines[3], "[Chorus]");
        assert_eq!(lines[4], "[Bridge]");
        assert_eq!(lines[5], "[Outro]");
    }

    #[test]
    fn ace_step_lyrics_falls_back_to_instrumental_tag() {
        let mut spec = sample_spec_5_section();
        spec.sections.clear();
        assert_eq!(format_ace_step_lyrics(&spec), "[Instrumental]");
    }

    #[test]
    fn section_for_time_picks_the_right_section() {
        // Spec: 4+16+16+8+8 = 52 bars at 108 BPM (4/4) -> 2.222 s/bar -> 115.5 s total
        let spec = sample_spec_5_section();
        let bar_s = 60.0 / 108.0 * 4.0;
        assert_eq!(section_for_time(&spec, 0.0), Some(0));            // intro start
        assert_eq!(section_for_time(&spec, bar_s * 3.5), Some(0));    // still in intro
        assert_eq!(section_for_time(&spec, bar_s * 4.5), Some(1));    // verse
        assert_eq!(section_for_time(&spec, bar_s * 20.5), Some(2));   // chorus
        assert_eq!(section_for_time(&spec, bar_s * 51.0), Some(4));   // outro
        assert_eq!(section_for_time(&spec, 1_000_000.0), Some(4));    // past end -> last
    }

    #[test]
    fn musicgen_section_prompt_includes_instrumentation_and_metadata() {
        let spec = sample_spec_5_section();
        let p = format_musicgen_section_prompt(&spec, 2).expect("chorus exists");
        assert!(p.contains("chorus section: + lead + sidechain pump"));
        assert!(p.contains("108 BPM"));
        assert!(p.contains("D major"));
        assert!(p.contains("instrumental"));
    }

    #[test]
    fn musicgen_section_prompt_out_of_bounds_returns_none() {
        let spec = sample_spec_5_section();
        assert!(format_musicgen_section_prompt(&spec, 99).is_none());
    }
}
