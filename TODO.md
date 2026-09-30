# Implementation Plan: MusicBrainz DB for tag_agent — M1 (DB + query surface)

Source: `docs/research/2026-09_musicbrainz_db_plan.md`

Builds the separate MusicBrainz-derived DuckDB + `query` CLI per **Option 1** of the tag_agent
architecture (decision D10). Repo is Rust (`wiki_music_db` house conventions), input is the
MusicBrainz `fullexport` SQL/TSV dumps (CC0), refresh is a weekly rebuild with atomic swap.

**Deviations from the plan's milestone table (§7) — granularity only, no scope change:**

- Plan step 2 (parser + loader) split into two commits: `src/mbdb/tsv.rs` parser, then
  `src/mbdb/import.rs` loader.
- Config loader (`src/mbdb/config.rs`) promoted to its own small step (shared by the import and
  query CLIs — foundation before consumers).
- Plan step 5 (whole query surface) split into one commit per query after a shared core commit:
  core + `query artist` → `release-group` → `release` (tracklists) → `recording` → `work` →
  `wikidata`, each with its own goldens.
- Plan §7 step 6 (`docs(architecture): ARCHITECTURE.md`) added per the 2026-09-30 review pass — house docs
  convention (AGENTS + ARCHITECTURE + README trio) — before the battery close-out.

Pre-work checks (done 2026-09-30): `cargo/rustc` 1.91.1 present; `df -h /home/tr` = 59 GB free
(≥ 40 GB tripwire, enough for the 13–19 GB peak budget); repo at commit `480aa9b` ("add initial
plan") with README + plan doc committed and a clean tree; data root `~/musicbrainz_db/` to be
created by the download script (step 6).

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `chore: scaffold cargo project skeleton` | Project skeleton | `Cargo.toml` (duckdb, clap, toml deps), `config.toml` (`dump_dir`/`db`/`tables` defaults), `.gitignore`, `tests/` dir | — (smoke: `cargo build`) |
| 2 | `feat(config): load settings with CLI overrides` | Config loader | `src/mbdb/config.rs` — defaults from `config.toml`, CLI args win; consumed by import/query CLIs | Unit |
| 3 | `feat(schema): define target tables, column manifests, and load order` | Schema DDL | `src/mbdb/schema.rs` + `src/mbdb/manifest.rs` — full DDL for §3 tables with `gid` columns as shipped (schema 31, no `mbid` table), per-table column manifests (no TSV headers), corrected load order (artists → credits/aliases → RG → release → recording → medium → track → work → label/area → links → derived; `track.recording` FK resolves) | Smoke, Unit |
| 4 | `feat(import): add PG-COPY TSV streaming parser` | PG-COPY TSV parser | `src/mbdb/tsv.rs` — backslash escapes (`\\`/`\t`/`\n`/`\r`/`\b`/`\f`), `t`/`f` booleans, **NULL = `\N`**, empty field = empty string (verified against the 2026-09-01 sample dump) | Unit |
| 5 | `feat(import): batch load with checkpoint and resume` | Streaming loader | `src/mbdb/import.rs` — constant-memory bz2/xz stream (detect by extension), ~100k-row batch INSERTs via the column manifest in a transaction, checkpoint/`--resume`, per-table counters, rejections log (count + first-N rows), inventory + manifest verification | Unit |
| 6 | `feat(import): download dumps and smoke-import sample` | Download + P0 smoke | `scripts/download_dumps.sh` (verify against shipped `MD5SUMS`); sample-dump (`.tar.xz`) import into scratch DB; run notes §R2 (counts, zero unexpected rejections) | Smoke |
| 7 | — (no commit; operational run) | Full release import | `mbdump.tar.bz2` (7 GB) + `mbdump-derived` (496 MB) into `musicbrainz.duckdb`; inventory vs loaded counts per table; run notes §R3; wall-clock 30–180 min | — |
| 8 | `feat(query): add JSON contract and artist query` | Query core + `query artist` | `src/mbdb/query.rs` (DbHit-shaped JSON, exact-match-first ordering with stable id/gid tiebreakers, aliases + disambiguation, no silent filtering), CLI dispatch + `query artist`; golden snapshot incl. >100-row spillover | Unit |
| 9 | `feat(query): add release-group query` | RG evidence | `query release-group --name/--artist/--type` → types, first-release-date, artist-credit (e.g. The Joshua Tree / Avenue Q); goldens | Unit |
| 10 | `feat(query): add release query with tracklists` | Release + tracklist evidence | `query release --release-group --inc tracks` → releases (status/date/country/label) and per-medium tracklists with lengths in seconds; goldens | Unit |
| 11 | `feat(query): add recording query` | Recording evidence | `query recording --name [--artist]` → durations ms→s, first-release, artist-credit; goldens | Unit |
| 12 | `feat(query): add work query` | Work evidence | `query work --name [--composer]` → title, iswc, type, composer links; goldens | Unit |
| 13 | `feat(query): add wikidata query` | Wikidata bridge | `query wikidata --mbid` → QID via `url` + `l_<entity>_url` filtered to `link_type` = wikidata (verified core path); snapshot `l_*_wikidata` only if imported; README query examples | Unit |
| 14 | `docs(architecture): document the MB DB and query surface` | ARCHITECTURE.md | `ARCHITECTURE.md` — data flow (import/query stages), module map, key decisions (separate DB, weekly atomic swap, wikidata url bridge); cross-refs AGENTS/README | — |
| 15 | `docs(research): record battery results and mark plan implemented` | DoD battery + close-out | §8 battery table measured; README/plan status → Implemented with closing commit refs; release build green | — |

**Verify gate (steps 1–6, 8–14):** `cargo test` green → `cargo clippy -- -D warnings` zero warnings →
`cargo fmt --check` clean → `cargo build --release` succeeds → commit.

**Out of scope (later phases, per plan §7 / §10):** tag_agent `db_layer.py` wiring, wiki-DB
reconciliation via the Wikidata link tables, distance/Levenshtein scoring, web/UI.