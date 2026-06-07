# Lost Worlds Saga — shared-universe album series (design)

**Date:** 2026-06-06
**Status:** APPROVED (brainstorm, Matt 2026-06-06). Production pending GPU windows.
**Decision:** A 5-album **shared universe** — interconnected, not standalone. "It's all
one thing (naturally, as it is IRL)." Built BEFORE recycling existing subgenres.

## Premise

The antediluvian advanced civilizations and where they went when their worlds fell:
**under the mountain, under the earth, under the sea, or out to the stars.** One mythos,
five entry points. A listener who falls into one album gets pulled through all five.

Yakub was proposed and **dropped** — racial-origin conspiracy myth = hate-speech /
demonetization risk on a monetized channel. The esoteric "hidden origins" energy is
carried safely by the lost-world themes below instead.

## The five albums (saga order)

| # | Slug | Role in arc | Sonic identity | Weather anchor |
|---|------|-------------|----------------|----------------|
| 1 | `telos-shasta-vol-1` | **Launch / keystone.** Lemurian refuge — survivors of sunken Lemuria hiding in the crystal city of Telos beneath Mt. Shasta. Elegy → transcendence. | Warm, alpine, vortex-mystical; mid-slow BPM, choir/pad, crystal arps | **Siskiyou Co., CA (real NWS):** Mt. Shasta City / Weed / Dunsmuir / McCloud. Matt's backyard — authentic launch. Lenticular-cloud lore = art + weather gold. |
| 2 | `hollow-earth-vol-1` | The descent. Vernian / Admiral-Byrd polar-opening expedition, deeper-and-darker, prehistoric inner world. | Driving, propulsive, descending BPM arc; darker | **Arctic** (polar entrance — reuses the built Arctic region) |
| 3 | `agartha-vol-1` | The inner civilization Telos belongs to — luminous Agarthan capital, inner sun, Shambhala, vril warmth. The payoff/arrival. | Meditative, transcendent, telluric, warm; slow | **Arctic / stylized inner-sun** |
| 4 | `atlantis-vol-1` | The Atlantic sibling to Lemuria's Pacific — concentric ring-city, orichalcum/crystal tech, the deluge. | Oceanic, vast, majestic-then-drowned; deep-pressure low end, bioluminescent | **Mid-Atlantic / Azores / Pillars of Hercules** |
| 5 | `gate-of-ra-vol-1` | Cosmic capstone — they went *out*. Ancient-astronaut gate/ring portal, Ra & Anubis as star-travelers. | Most propulsive + sci-fi; sequenced arps, "travel" energy, higher BPM | **Giza / Cairo desert** |

Telos launches (personal, US real-weather); Gate of Ra caps it (cosmic). Diptych spine:
Hollow Earth (descent) ⇄ Agartha (arrival) is the structural heart.

## Shared-universe rules

- **Cross-album sync handoffs:** each album's track 12 hands off (key/motif) to the next
  album's track 1, so the catalog plays as one continuous saga.
- **Recurring motifs:** a small set of leitmotifs (e.g. a "descent" interval, a "crystal"
  arp figure, an "inner-sun" pad chord) recur across albums, transposed per album key.
- **Key relationships:** album tonal centers chosen to chain (album-composer enforces the
  per-track key arc within each; saga-level chain set at compose time).
- **Visual continuity:** shared SDXL palette/motif family (the lone figure, the threshold/
  portal, the glow-from-below) so covers read as one series.

## Naming cautions (danger-zone discipline)

Per `feedback_album_title_danger_zone` + the Derez/Recognizer rename lesson:
- **Gate of Ra:** mythology-generic ONLY. Use Ra, Anubis, "the gate," glyph-rings,
  Heliopolis. **Never** the literal title "Stargate" or franchise proper nouns
  (Goa'uld, SG-1, exact chevron lingo) — MGM trademark + David Arnold soundtrack.
- **Atlantis:** avoid Disney "Atlantis: The Lost Empire" track/character names.
- **Hollow Earth:** avoid Verne "Journey to the Center of the Earth" film/score titles.
- Run each album's titles through danger-zone validation at compose time.

## Weather system dependency (already shipped 2026-06-06)

The per-album region weather system (Japan/Soviet/Arctic + Hong Kong; Open-Meteo global
forecast + RainViewer/OSM radar; slug-routed in `nightdrive-encoder::weather`) makes the
weather anchors above automatic. New anchors to register before each drop:
- Telos → US Siskiyou Co. (NWS native — add a US sub-region or coords)
- Hollow Earth / Agartha → Arctic (built)
- Atlantis → mid-Atlantic (Open-Meteo coords; RainViewer ocean = basemap-only)
- Gate of Ra → Egypt/Giza (Open-Meteo; RainViewer desert = mostly basemap)

## Production order

1. Telos first (compose → covers → render → staggered drop) on the next GPU window.
2. Then Hollow Earth → Agartha (diptych) → Atlantis → Gate of Ra.
3. Each album: `album-composer` spec → SDXL covers → ACE-Step render → encode (JP-style
   weather per anchor) → `publish-staggered` (6/day GCP cap) → sync-drop anchor.
