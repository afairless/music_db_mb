# music_db_mb — MusicBrainz database builder

Builds and serves a local DuckDB of MusicBrainz metadata for the tag_agent project, per
**Option 1** of the tag_agent architecture: a *separate* MusicBrainz-derived database that
complements (never replaces) the wiki-based database.

**Status:** **implemented (M1)** — DB + query surface live (`~/musicbrainz_db/musicbrainz.duckdb`,
2026-09-30-002222 export); battery measured 2026-10-03 (run notes §R4). See
[`docs/research/2026-09_musicbrainz_db_plan.md`](docs/research/2026-09_musicbrainz_db_plan.md) (status: Implemented).

## What this repo does

- Downloads MusicBrainz `fullexport` TSV dumps (`mbdump` ≈ 7 GB + `mbdump-derived` ≈ 496 MB; core data is **CC0**), refreshed **weekly**
- Imports the core music tables (artist, release group, release, medium, track, recording, work, label, links, aliases, Wikidata↔MBID link tables) into a **DuckDB**
- Exposes a `query` CLI / library with the tag_agent `DbSearch`-shaped JSON contract (name-first, exact-match-first, no silent filtering)

## Key decisions (user-confirmed 2026-09-30)

| Decision | Choice |
|---|---|
| Toolchain | Rust (consistent with sibling repo `wiki_music_db`) |
| Primary input | SQL/TSV `fullexport` dumps |
| M1 deliverable | DB + query surface + DoD battery |
| Refresh | Weekly fullexport re-import with atomic swap |

## Layout

```text
docs/research/2026-09_musicbrainz_db_plan.md   # the plan (status: Proposed)
~/musicbrainz_db/dumps/                         # downloaded dumps
~/musicbrainz_db/musicbrainz.duckdb             # built database (config default)
```

## Query surface

`mbdb query <kind> …` prints one JSON document per run (the §4 contract): name-first,
exact-match-first ranking with stable id/gid tiebreakers, aliases + disambiguation surfaced,
and **no silent filtering** — statuses, disambiguated duplicates, and the long tail stay
visible for the LLM to judge. Durations are seconds (ms → s) matching the mp3-side convention.

```text
# Artists — exact name first, then sort_name/alias, then substring
mbdb query artist --name U2

# Release groups — types, first-release-date, artist credit
mbdb query release-group --name "The Joshua Tree" [--artist U2] [--type Album]

# Releases of a group — status/date/country/label; --inc-tracks adds tracklists
mbdb query release --release-group 6f3e9fa6-be7a-3de8-a2b2-2072ece8a54d [--inc-tracks]

# Recordings — duration seconds, first release date (best effort), artist credit
mbdb query recording --name "Where the Streets Have No Name" [--artist U2]

# Works — ISWCs, raw type id, composer links
mbdb query work --name "With or Without You" [--composer Bono]

# Wikidata bridge — MBID → QID via the verified url + l_<entity>_url path
mbdb query wikidata --mbid 6f3e9fa6-be7a-3de8-a2b2-2072ece8a54d
```

Every query honors `--limit N` (applied after ranking). Call the same functions from Rust via
`src/mbdb/query.rs` (`mbdb::query::query_*)`.

## Related projects

- Consumer: tag_agent → `/home/tr/Documents/tag_agent/docs/ARCHITECTURE-proposal.md` (decision D10)
- Sibling pipeline: wiki_music_db → `/home/tr/Documents/wiki_music_db/`
- Upstream data: https://data.metabrainz.org (MusicBrainz `fullexport` / `sample` / `json-dumps`)