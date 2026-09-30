# music_db_mb — MusicBrainz database builder

Builds and serves a local DuckDB of MusicBrainz metadata for the tag_agent project, per
**Option 1** of the tag_agent architecture: a *separate* MusicBrainz-derived database that
complements (never replaces) the wiki-based database.

**Status:** planning — see `docs/research/2026-09_musicbrainz_db_plan.md` (Proposed, 2026-09-30).

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

## Related projects

- Consumer: tag_agent → `/home/tr/Documents/tag_agent/docs/ARCHITECTURE-proposal.md` (decision D10)
- Sibling pipeline: wiki_music_db → `/home/tr/Documents/wiki_music_db/`
- Upstream data: https://data.metabrainz.org (MusicBrainz `fullexport` / `sample` / `json-dumps`)