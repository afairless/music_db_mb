# Research: MusicBrainz Database for tag_agent (Option 1 — separate DB)

**Date:** 2026-09-30
**Status:** Proposed — plan for review; not yet implemented.
**Decision source:** `tag_agent/docs/ARCHITECTURE-proposal.md`, D10 (added 2026-09-30) — the tag agent will add a
MusicBrainz evidence source per **Option 1**: a *separate* MusicBrainz-derived database, built by a new repo
`music_db_mb`, leaving the existing wiki DB (`~/wiki_db/music.duckdb`) untouched.

**Locked decisions (user-confirmed 2026-09-30):**

| # | Decision | Choice |
|---|---|---|
| L1 | Toolchain | **Rust** — consistent with the sibling data-pipeline repo (`wiki_music_db`); release CLI; fast streaming of the multi-GB dump. The tag_agent consumer stays Python and reads the DuckDB file directly |
| L2 | Primary input format | **SQL/TSV `mbdump` (fullexport)** — relational tables incl. `track`/`medium`/`recording` length columns, plus `mbdump-derived` for the Wikidata↔MBID link tables |
| L3 | Milestone-1 deliverable | **DB + query surface** — import core MB tables into a DuckDB + a small query layer/CLI mirroring the tag_agent `DbSearch` contract + a DoD battery; tag_agent wiring is a later phase |
| L4 | Refresh strategy | **Weekly fullexport re-import** — `fullexport` regenerates twice a week; re-download + rebuild + atomic swap on a weekly cadence |

**References:**

- [tag_agent ARCHITECTURE-proposal.md](../../../tag_agent/docs/ARCHITECTURE-proposal.md) — the consuming design; DbSearch contract (D5), soft-evidence model, P1 fixtures
- [wiki_music_db research index](../../../wiki_music_db/docs/research/) — house conventions for pipeline repos (Rust, DuckDB, config.toml, scripts, DoD batteries)
- MusicBrainz sources (all verified live 2026-09-30): `https://data.metabrainz.org/pub/musicbrainz/data/` (fullexport + json-dumps + sample), [MusicBrainz_Database](https://musicbrainz.org/doc/MusicBrainz_Database), [MusicBrainz_API/Rate_Limiting](https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting)

---

## 1. Context and purpose

The wiki DB's structural weaknesses for tag_agent are known and measured:

- `track_album` covers **1,992 of 40,017 tracks (~5%)** — Wikidata lacks track→album edges for ~94% of tracks.
- **Complete tracklists and durations are absent** (`duration_seconds` 13,037 of 40,017; no per-album tracklist).
- **Broadway cast recordings are absent as items** (Avenue Q, Spamalot, Ragtime; "Annie" only via confusing "Annie Get Your Gun" rows) — the corpus literally contains these shows.
- Canonical songs degrade into junk `artist` rows (e.g. Sunday Bloody Sunday `Q1779491` classified `PROP:P136,P175`).

MusicBrainz fixes exactly these gaps. Verified against the live service, one API call returns what the wiki DB
cannot produce by any rebuild:

```
The Joshua Tree (RG) — primary-type Album — first-release-date 1987-03-09 — artist-credit: U2
  1. Where the Streets Have No Name   337.5 s
  2. I Still Haven't Found What I'm Looking For  278.1 s
  3. With or Without You              296.2 s
  ... 11 tracks total, each with recording MBID + length in ms
```

And the shows missing from Wikidata exist here ("Avenue Q: The Musical", secondary-type Soundtrack,
2003-10-06, plus two more Avenue Q release groups).

**Scope boundary:** this repo builds and serves a *local dataset + query surface*. It does **not** embed the
tag_agent harness, Levenshtein/distance logic (stays in tag_agent), or the wiki DB (separate file, untouched).

## 2. Verified facts about MusicBrainz data access (2026-09-30)

| Fact | Detail |
|---|---|
| **Bulk download exists** | `fullexport/` regenerated **twice a week** (LATEST = 20260930). `mbdump.tar.bz2` = **7 GB** TSV (core tables). `mbdump-derived.tar.bz2` = **496 MB** (derivations incl. Wikidata↔MBID link tables). Optional: edit 16 G, editor 81 M, cdstubs 61 M, stats 116 M, cover-art 161 M — **none needed for M1** |
| **JSON dumps exist** (not primary) | `json-dumps/` per-entity NDJSON, refreshed daily: release-group 1 G, release 23 G (embeds tracklists), work 666 M, recording 33 M, etc. Secondary/cross-check only (L2 locked to TSV) |
| **Sample DB** | `sample/` monthly small dump — the P0 import smoke fixture |
| **License** | Core data **CC0 (public domain)** — stated on the Database page and by the CC0 `COPYING` shipped in the JSON dump; free to merge with the CC0-derived wiki data. Supplementary (tags/ratings/annotations/stats) is CC BY-NC-SA — out of scope for M1 |
| **API is not for bulk** | Hard cap **1 request/second/client**; the *search* zone sits behind a stricter shared pool (observed `503 busy`, `x-ratelimit-zone: search`). This is why on-demand-only was judged slow/flaky in the option analysis; a local DB is the right call |
| **Wikidata join key** | `mbdump-derived` ships `l_artist_wikidata`, `l_release_group_wikidata`, `l_release_wikidata`, `l_recording_wikidata`, `l_work_wikidata` etc. — the reconciliation bridge to the wiki DB and to QID labels (exact filenames verified at P0) |
| **Known coverage caveat** | Long-tail gaps exist in MB too ("Ragtime, Music From The Motion Picture" did not surface in the top RG hits) — the battery includes per-corpus-dir spot checks; MB is a *strong additional* source, not a perfect one |

## 3. Target schema (source → target)

Import the **normalized MB subset that the tag-agent use case (and future reconciliation) needs**, mirroring
source tables 1:1. No entity-classification filter is applied — MB is already curated; we surface, never
silently drop (data-contract convention from the wiki pip-suite: record status, don't filter).

ID strategy: keep MB's own identifiers in text columns. Entities with UUID primary keys (artist, release_group,
release, recording, label) expose their `gid` directly. Entities with **integer** primary keys (area, work,
medium, track, link, artist_credit, link_type) keep the int PK *and* the `mbid` mapping table is imported so the
query layer can expose `gid`-shaped identifiers (a text `mbid` column added by join at query time — do not fake
UUIDs in the importer).

| Source file (mbdump/) | Target table (notes) | Why needed |
|---|---|---|
| `artist` | `artist` (name, sort_name, type person/group, area, begin/end, disambiguation, comment) | artist matching |
| `artist_credit`, `artist_credit_name` | same names | display credits on RG/release/track/recording |
| `artist_alias` (+ aliases of release_group/work/recording) | `artist_alias`, `release_group_alias`, `work_alias`, `recording_alias` | name variants for fuzzier matching |
| `release_group` + `release_group_primary_type`, `release_group_secondary_type`, `…_secondary_type_join` | same | the "album/single/EP/soundtrack/cast" evidence |
| `release` + `release_status`, `release_packaging`, `release_label` | same | per-release dates, countries, catalog numbers |
| `medium`, `track` | same (position, track_count, number, name, length ms, recording ref) | **the tracklist evidence the wiki DB lacks** |
| `recording` (+ `recording_first_release_date`) | same | durations ms, first-release, ISRC |
| `work` | same (title, iswc, type) | classical works / compositions |
| `label` | same | record-label evidence |
| `area` | minimal (id, name, type) | country/area display |
| `link`, `link_type`, `link_attribute_type` | same | typed relationships (performer, composer, member-of…) |
| `l_artist_release`, `l_artist_release_group`, `l_artist_recording`, `l_artist_work`, `l_recording_work`, `l_release_label`, `l_work_release`, `l_recording_release` (+ `link_type` filtered to useful types as a view) | renamed only | the album→tracks / composer→work graph |
| `mbid` | same | gid mapping for int-PK entities |
| (*derived*) `l_artist_wikidata`, `l_release_group_wikidata`, `l_release_wikidata`, `l_recording_wikidata`, `l_work_wikidata` (+ `url`, `l_*_url` if trivial) | same | **reconciliation bridge to the wiki DB / QID labels** |

Explicitly **not** imported in M1: edit history, editor tables, annotations, tags/ratings, stats, CD stubs,
cover-art archive, wikidocs. Full table inventory + observed row counts recorded in run notes at P1 (Step 0 of
the import verifies the file lists against this plan; discrepancies get recorded, not silently tuned around).

## 4. Query surface & JSON contract (M1)

A Rust `query` CLI mirroring the wiki_db CLI style plus a small library in `src/mbdb/query.rs` that tag_agent's
Python `db_layer.py` can later call via subprocess or by opening the DuckDB directly.

Queries (deterministic, name-first, **exact-match-first** — the Defect-B lesson from the wiki suite applies here):

- `query artist --name <s>` → exact `name` first, then sort_name/alias matches; include aliases + disambiguation
- `query release-group --name <s> [--artist <s>] [--type Album|Single|EP|... ]` → type(s), first-release-date, artist-credit
- `query release --release-group <mbid|gid> [--inc tracks]` → releases (status/date/country/label) and per-medium tracklists with lengths
- `query recording --name <s> [--artist <s>]`
- `query work --name <s> [--composer <s>]`
- `query wikidata --mbid <mbid>` → linked Wikidata QID (from derived tables), for later wiki-DB reconciliation

Result shape (JSON contract aligned with tag_agent `DbHit`; distance/Lloyd scoring stays client-side):

```jsonc
{"kind": "release_group", "name": "The Joshua Tree", "mbid": "6f3e9fa6-…",
 "types": ["Album"], "first_release_date": "1987-03-09",
 "artist_credit": [{"name": "U2", "mbid": "a3cb23fc-…", "disambiguation": "Irish rock band"}],
 "aliases": [], "status": "approved"}
```

No row is silently filtered: `release.status` (Official/Promotion/Bootlog…) and disambiguated duplicates are
always surfaced so the LLM (not the DB) decides — same soft-evidence rule as the wiki DB.

## 5. Download and import pipeline

- `scripts/download_dumps.sh` → fetches `fullexport/LATEST` `mbdump.tar.bz2` + `mbdump-derived.tar.bz2` into the
  dump dir (mirrors the wiki_db `download_dump.sh` house pattern). Weekly cron/systemd timer runs
  download+rebuild+swap (Step 7).
- `mbdb import --dump-dir <dir> --db <file>` — single pass:
  1. Inventory: list members of each tarball; log row counts per source file (recorded in run notes).
  2. Stream: for each imported table member, decompress (constant-memory bz2 stream), parse the **MDQ TSV
     escaping** (backslash escapes; `t`/`f` booleans; **empty string = NULL** — verify both against the sample
     dump in P0), batch-insert (~100 k rows/batch) into the target DuckDB inside a transaction checkpointing
     progress for `--resume` (interrupt-safe; roughly 30-180 min wall clock for the 7 GB dump on this machine).
  3. Derived pass: same for the `mbdump-derived` link tables.
  4. Verify: row counts per table vs Step 1 inventory; zero unexpected rejections.
- **Refresh (weekly):** rebuild into `musicbrainz.new.duckdb`, then atomic `mv` swap to `musicbrainz.duckdb`,
  retaining `musicbrainz.prev.duckdb` for one cycle (the archive-not-delete rule from the wiki suite).

## 6. Storage and configuration

```text
/home/tr/Documents/music_db_mb/          # repo (new; this plan)
  Cargo.toml, config.toml, scripts/, src/mbdb/{import,query,download}…, tests/, docs/research/
/home/tr/musicbrainz_db/                 # data root (repo-adjacent, like ~/wiki_db)
  dumps/            # mbdump*.tar.bz2 (7 GB + 496 MB; staging deleted after import)
  musicbrainz.duckdb
```

config.toml defaults (CLI overrides win):

```toml
dump_dir = "~/musicbrainz_db/dumps"
db = "~/musicbrainz_db/musicbrainz.duckdb"
tables = "core+derived"        # overridable subset for tests
```

Disk budget check (2026-09-30: 59 GB free): dump 7 GB (retained between refreshes) + DuckDB ~3-6 GB +
`musicbrainz.prev` ~3-6 GB ≈ **13-19 GB peak** — fits with margin; staging streams and is deleted per run.

## 7. Milestones and commit table

Mirrors the wiki_db implementation-plan convention: each step a small logical unit, tests before code where
practicable, the house verify gate (test → clippy → fmt → release build) before each commit.

| # | Commit message (conventional) | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 0 | *(pre-work, no commit)* | Repo scaffold + plan doc commit | `git init`; README.md; config.toml; `docs/research/2026-09_musicbrainz_db_plan.md` (this doc); `df -h /home/tr` ≥ 40 GB free | — |
| 1 | `feat(schema): define target tables and load order` | Schema as embedded SQL/migration | `src/mbdb/schema.rs` — full target-DDL for §3 tables; load-order graph (artists → credits/aliases → RG/release → mediums → tracks → recordings/works → links → derived) | Schema DDL executed on a scratch duckdb; table list matches the §3 matrix |
| 2 | `feat(import): MDQ TSV streaming importer with resume` | Streaming parser + loader | `src/mbdb/tsv.rs` (MDQ escaping, `t/f` booleans, empty=NULL), `src/mbdb/import.rs` (batch INSERTs, checkpoint/resume, per-table counters) | Fixture TSV incl. tabs/newlines/backslashes/booleans/NULL; interrupted-run resume test; counts match fixture |
| 3 | `feat(import): download script + sample-dump smoke` | Download + P0 smoke import | `scripts/download_dumps.sh`; import the **sample** dump (monthly `sample/`) into a scratch DB | Sample import row-counts recorded (run notes §R2); zero unexpected rejections |
| 4 | *(no commit)* | Full release import run | `scripts/download_dumps.sh` + `mbdb import` against `fullexport` LATEST (dump 7 GB, ~30-180 min) | Inventory vs loaded counts per table; run notes §R3 |
| 5 | `feat(query): name-first query layer + JSON contract` | Query surface (M1) | `src/mbdb/query.rs` + CLI (`query artist/release-group/release/recording/work/wikidata`); exact-match-first ordering; aliases; no silent filtering of status/duplicates | Unit tests: exact vs alias vs substring ordering; disambiguated duplicate surfacing; contract JSON snapshots (golden files) |
| 6 | `docs(research): record battery results and mark plan implemented` | DoD battery + close-out | Re-run §8 battery; record measured values in run notes; swap/release build; mark this doc `Status: Implemented` with closing commit refs | Battery table filled; release build green |

**Verify gate (steps 1-5):** `cargo test` all green → `cargo clippy -- -D warnings` zero warnings →
`cargo fmt --check` clean → `cargo build --release` succeeds → commit.

**Out of scope (tracked as later phases):** tag_agent `db_layer.py` integration (a `DbSearch`-shaped second
surface), wiki-DB reconciliation via the Wikidata link tables, Levenshtein/distance scoring, and the web/UI.

## 8. Definition-of-Done battery (M1, exact expectations)

Run against `musicbrainz.duckdb` after Step 4. Goldens codify *verified facts* from the live sources; any
measured mismatch is recorded with root cause (never tuned around):

| # | Check | Expect | Measured at run (fill in) |
|---|---|---|---|
| 1 | Import completeness | Imported rows == source-file inventory rows per table (recorded) | |
| 2 | `query artist --name U2` | Exact-match band U2 **first**; disambiguation "Irish rock band"; aliases surfaced (e.g. 유투) | |
| 3 | `query release-group --name "The Joshua Tree"` | RG = Album, first-release-date 1987-03-09, artist-credit U2 | |
| 4 | `query release --release-group <that RG> --inc tracks` | 11 tracks; #1 "Where the Streets Have No Name" ≈ 337.5 s; #3 "With or Without You" | |
| 5 | `query release-group --name "Avenue Q"` | ≥ 1 RG including "Avenue Q: The Musical" (Soundtrack, 2003-10-06); other Avenue Q RGs present | |
| 6 | `query release-group --name Ragtime` | RG list incl. "Ragtime" (1981 film) or "Ragtime (musical)" if present — record coverage verdict per corpus dir | |
| 7 | Wikidata bridge | `l_release_group_wikidata` + `l_artist_wikidata` + `l_work_wikidata` > 0 rows; `query wikidata --mbid 6f3e9fa6-…` links The Joshua Tree | |
| 8 | Corpus spot-checks | Each `/home/tr/mp3_files/` dir: ≥ 0 sensible RG hits; record any no-evidence dirs (both sources may miss long-tail) | |
| 9 | CLI contract | JSON outputs parse against the §4 snapshot goldens; exact-match-first ordering verified incl. >100-row spillover case | |

## 9. Pitfalls

- **MDQ TSV encoding is not plain TSV** — backslash-escaped fields; booleans are `t`/`f`; NULL is an empty
  string (verify against the sample dump in Step 1 before trusting any assumption; music titles *do* contain
  tabs/newlines in the wild).
- **Integer-PK entities** (track, medium, work, link, area, artist_credit) require the `mbid` table for
  stable identifiers — do not invent UUIDs in the importer.
- **Disk budget:** 59 GB free at planning time; keep the dump between refreshes, stream imports, delete staging,
  retain one `.prev` — flag the run if free space < 25 GB (mirror the wiki suite's tripwire habit).
- **Weekly full rebuild is the point** — no incremental complexity; an atomic swap keeps a broken rebuild from
  ever being visible.
- **Never filter silently:** release `status` (Official/Promotion/Bootlog), disambiguated duplicates, and
  multi-release RGs stay visible; the tag_agent LLM decides.
- **Recording `length` is ms in MB** — expose seconds in the JSON contract to match the mp3-side convention.
- **Long-tail gaps remain** (Ragtime film soundtrack did not surface in a live top-hit check): the battery
  measures coverage per corpus dir; a miss is recorded as a finding, not a tuning target.

## 10. Open items / risks

1. Exact member filenames inside `mbdump-derived.tar.bz2` (the `l_*_wikidata` set) — confirmed by inventory at
   Step 2 (plan assumes the documented set; discrepancies get recorded).
2. Whether to retain the dump 7 GB between refreshes vs re-download weekly — decision at first refresh run
   (bandwidth vs storage trade-off; both are acceptable today).
3. Import wall-clock (30-180 min estimate) — measured and recorded at Step 4; affects only scheduling.
4. tag_agent integration and wiki-DB reconciliation are deliberately deferred — revisit after this battery
   passes and the tag_agent `db_layer.py` milestone is open.

## Run notes

Reserved for execution-time measurements (inventory counts, sample-import counts, full-import counts,
battery results, refresh-cycle record) per the house documentation conventions.