# Research: MusicBrainz Database for tag_agent (Option 1 — separate DB)

**Date:** 2026-09-30
**Status:** Proposed — reviewed 2026-09-30 (facts verified against the 2026-09-01 sample dump); corrections applied; not yet implemented.
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
| **Sample DB** | `sample/` monthly small dump — the P0 import smoke fixture. **Format: `mbdump-sample.tar.xz` (xz, ~358 MB), not bz2** — the loader must handle both compressors |
| **Dump text format** | PostgreSQL COPY text mode: tab-separated; backslash escapes (`\\`, `\t`, `\n`, `\r`, `\b`, `\f`); **NULL = the exact field `\N`** (a genuinely empty field is a real empty string); booleans `t`/`f`. Verified against sample rows 2026-09-30 (see §3 note) |
| **Checksums** | Each `fullexport/<date>/` dir ships `MD5SUMS` + `SHA256SUMS` — verify downloads against them (wiki `download_dump.sh` house pattern) |
| **License** | Core data **CC0 (public domain)** — stated on the Database page and by the CC0 `COPYING` shipped in the JSON dump; free to merge with the CC0-derived wiki data. Supplementary (tags/ratings/annotations/stats) is CC BY-NC-SA — out of scope for M1 |
| **API is not for bulk** | Hard cap **1 request/second/client**; the *search* zone sits behind a stricter shared pool (observed `503 busy`, `x-ratelimit-zone: search`). This is why on-demand-only was judged slow/flaky in the option analysis; a local DB is the right call |
| **Wikidata join key** | **Primary bridge (verified in the core dump): `url` + `l_<entity>_url` link tables**, filtered to the `wikidata` link type (`url.url` = `https://www.wikidata.org/wiki/Q…`). Derived `l_*_wikidata` snapshot tables, if present in `mbdump-derived`, are an optional fast path — filenames still unverified (§10.1) |
| **Known coverage caveat** | Long-tail gaps exist in MB too ("Ragtime, Music From The Motion Picture" did not surface in the top RG hits) — the battery includes per-corpus-dir spot checks; MB is a *strong additional* source, not a perfect one |

## 3. Target schema (source → target)

Import the **normalized MB subset that the tag-agent use case (and future reconciliation) needs**, mirroring
source tables 1:1. No entity-classification filter is applied — MB is already curated; we surface, never
silently drop (data-contract convention from the wiki pip-suite: record status, don't filter).

ID strategy: keep MB's own identifiers in text columns. Verified against the 2026-09-01 sample dump
(SCHEMA_SEQUENCE 31, checked 2026-09-30): **nearly every substantive table carries a `gid` UUID column
directly** — artist, release_group, release, recording, label, work, area, track, **medium**,
**artist_credit**, and **link_type** all ship one; only `link` (and pure junction rows) have an integer id
with no gid. There is **no `mbid` mapping table** in the dump — do not design around one. Import `gid`
columns as-is for the entities that have them; `link` rows keep only their int id (never invent a synthetic
MBID). The `*_gid_redirect` tables (e.g. `artist_gid_redirect`) are cheap provenance and should be imported too
so re-pointed/merged MBIDs stay resolvable at query time.

Column layout: dump TSVs have **no header row** and the schema-31 layout differs from the classic docs (e.g.
`track` = 12 columns and carries an explicit `recording` FK plus `gid`; `work` = 7; `artist_credit` = 7 incl.
`gid`). The schema step pins a **column manifest per table** (names, FKs, order) taken from the official DDL
at SCHEMA_SEQUENCE 31; the loader inserts via `INSERT INTO t (col1, …, colN)` using the manifest so similar-
typed columns can never be silently swapped. The import inventory step checks each member's column count
against the manifest.

| Source file (mbdump/) | Target table (notes) | Why needed |
|---|---|---|
| `artist` | `artist` (name, sort_name, type person/group, area, begin/end, disambiguation, comment) | artist matching |
| `artist_credit`, `artist_credit_name` | same names | display credits on RG/release/track/recording |
| `artist_alias` (+ aliases of release_group/work/recording) | `artist_alias`, `release_group_alias`, `work_alias`, `recording_alias` | name variants for fuzzier matching |
| `release_group` + `release_group_primary_type`, `release_group_secondary_type`, `…_secondary_type_join` | same | the "album/single/EP/soundtrack/cast" evidence |
| `release` + `release_status`, `release_packaging`, `release_label`, `release_country`, `release_unknown_country` | same | per-release status, dates (via the country/unknown-country junctions — verified core tables), countries, catalog numbers |
| `medium`, `track` | same (position, track_count, number, name, length ms, recording FK) | **the tracklist evidence the wiki DB lacks**; both carry `gid` in schema 31 |
| `recording` (+ `recording_first_release_date` *when verified* in the derived tarball) + `isrc` | same (durations ms, first-release) | recording evidence; ISRC lives in its own `isrc` table (verified core), not in `recording` |
| `work` + `iswc` | same (title, type) | classical works / compositions; ISWC lives in its own `iswc` table (verified core), not in `work` |
| `label` | same | record-label evidence |
| `area` | minimal (id, gid, name, type) | country/area display; `area` carries a `gid` in schema 31 |
| `link`, `link_type`, `link_attribute_type` | same | typed relationships (performer, composer, member-of…); `link_type` has a `gid`, `link` does not (no synthetic MBIDs) |
| `l_artist_release`, `l_artist_release_group`, `l_artist_recording`, `l_artist_work`, `l_recording_work`, `l_recording_release`, `l_label_release` (+ `link_type` filtered to useful types as a view) | renamed only | the album→tracks / composer→work / label graph. **Verified names:** `l_label_release` exists; `l_release_label` and `l_work_release` do not — there is no work↔release link table (reach it via `l_recording_work ⋈ l_recording_release`) |
| `url` + `l_artist_url`, `l_release_group_url`, `l_release_url`, `l_recording_url`, `l_work_url` | same | **Wikidata bridge (primary, verified core)**: `link_type` rows named `wikidata` pin `url.url` = `https://www.wikidata.org/wiki/Q…` → QID |
| `*_gid_redirect` (artist, artist_credit, release_group, release, recording, work, label, url, area) | same | MBID re-pointing/merge provenance; keeps query-layer gid lookups total |
| (*derived, optional*) `l_*_wikidata` (artist/release_group/release/recording/work) | same | fast-path snapshot of the url bridge — import only if present at inventory (§10.1); the sample dump contains core members only, so this path is first exercised at the full import (milestone 4) |

Explicitly **not** imported in M1: edit history, editor tables, annotations, tags/ratings, stats, CD stubs,
cover-art archive, wikidocs. Full table inventory + observed row counts recorded in run notes (import step 1
verifies the file lists and column counts against this plan; discrepancies get recorded, not silently tuned around).

## 4. Query surface & JSON contract (M1)

A Rust `query` CLI mirroring the wiki_db CLI style plus a small library in `src/mbdb/query.rs` that tag_agent's
Python `db_layer.py` can later call via subprocess or by opening the DuckDB directly.

Queries (deterministic, name-first, **exact-match-first** — the Defect-B lesson from the wiki suite applies
here; all ORDER BY clauses include a stable id/gid tiebreaker so output stays byte-deterministic):

- `query artist --name <s>` → exact `name` first, then sort_name/alias matches; include aliases + disambiguation
- `query release-group --name <s> [--artist <s>] [--type Album|Single|EP|... ]` → type(s), first-release-date, artist-credit
- `query release --release-group <mbid|gid> [--inc tracks]` → releases (status; dates via `release_country`/`release_unknown_country`; country; label) and per-medium tracklists with lengths
- `query recording --name <s> [--artist <s>]`
- `query work --name <s> [--composer <s>]`
- `query wikidata --mbid <mbid>` → linked Wikidata QID, for later wiki-DB reconciliation — primary path `url` + `l_<entity>_url` filtered to `link_type` name `wikidata`, QID extracted from the url (`https://www.wikidata.org/wiki/Q…`); the snapshot `l_*_wikidata` tables are used only if imported

Result shape (JSON contract aligned with tag_agent `DbHit`; distance/Lloyd scoring stays client-side):

```jsonc
{"kind": "release_group", "name": "The Joshua Tree", "mbid": "6f3e9fa6-…",
 "types": ["Album"], "first_release_date": "1987-03-09",
 "artist_credit": [{"name": "U2", "mbid": "a3cb23fc-…", "disambiguation": "Irish rock band"}],
 "aliases": []}
```

No row is silently filtered: `release.status` (Official/Promotion/Bootleg…), disambiguated duplicates, and
multi-release RGs are always surfaced so the LLM (not the DB) decides — same soft-evidence rule as the wiki DB.
(There is no status on a release-group row; a stale `"status": "approved"` field was dropped from the contract
example during review.)

## 5. Download and import pipeline

- `scripts/download_dumps.sh` → fetches `fullexport/LATEST` `mbdump.tar.bz2` + `mbdump-derived.tar.bz2` into the
  dump dir, **verified against the shipped `MD5SUMS`** before import (mirrors the wiki_db `download_dump.sh`
  house pattern) and with a descriptive User-Agent. Weekly cron/systemd timer runs download+rebuild+swap
  (the refresh cycle below).
- `mbdb import --dump-dir <dir> --db <file>` — single pass:
  1. Inventory: list members of each tarball; log row counts per source file; **check each member's column
     count against the pinned §3 column manifest** (recorded in run notes). Discrepancies are recorded, never
     tuned around.
  2. Stream: for each imported table member, decompress (constant-memory stream, **bz2 for the production
     tarballs, xz for the sample dump** — detect by file extension), parse the **PostgreSQL COPY text format**
     (backslash escapes `\\`/`\t`/`\n`/`\r`/`\b`/`\f`; `t`/`f` booleans; **NULL = the exact field `\N`** —
     a genuinely empty field is a real empty string; verified against sample rows 2026-09-30), batch-insert
     (~100 k rows/batch) into the target DuckDB via the column manifest inside a transaction checkpointing
     progress for `--resume` (interrupt-safe; roughly 30-180 min wall clock for the 7 GB dump on this machine).
     Malformed rows go to a **rejections log** (count + first-N sample rows per table), never silently dropped.
  3. Derived pass: same for the `mbdump-derived` tables (imported only if present at inventory; the core
     `url`/`l_*_url` Wikidata bridge and `release_country` dates do not depend on them). The sample dump has
     no derived members, so this pass is first exercised at the full import (milestone 4).
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
| --- | --- | --- | --- | --- |
| 0 | *(pre-work, no commit)* | Repo scaffold + plan doc commit | `git init`; README.md; AGENTS.md (house conventions: Rust/verify gate/error handling, per the wiki repo); config.toml; `docs/research/2026-09_musicbrainz_db_plan.md` (this doc); `df -h /home/tr` ≥ 40 GB free | — |
| 1 | `feat(schema): define target tables, column manifests, and load order` | Schema as embedded SQL/migration | `src/mbdb/schema.rs` + `src/mbdb/manifest.rs` — full target-DDL for §3 tables with `gid` columns as shipped (schema 31, no `mbid` table); per-table column manifests (no TSV headers); load-order graph (artists → credits/aliases → RG → release → recording → medium → track → work → label/area → links → derived, so the `track.recording` FK resolves) | Schema DDL executed on a scratch duckdb; table list matches the §3 matrix; manifest column counts verified against the sample dump |
| 2 | `feat(import): PG-COPY TSV streaming importer with resume` | Streaming parser + loader | `src/mbdb/tsv.rs` (backslash escapes, `0/1` booleans — see §R2 deviation, `\N` = NULL), `src/mbdb/import.rs` (batch INSERTs via manifest, checkpoint/resume, per-table counters, rejections log) | Fixture TSV incl. tabs/newlines/backslashes/booleans/`\N`/empty-string; interrupted-run resume test; counts match fixture |
| 3 | `feat(import): download script + sample-dump smoke` | Download + P0 smoke import | `scripts/download_dumps.sh` (MD5SUMS verification) with bz2/xz detection; import the **sample** dump (`sample/` — note: `.tar.xz`) into a scratch DB | Sample import row-counts recorded (run notes §R2); zero unexpected rejections |
| 4 | *(no commit)* | Full release import run | `scripts/download_dumps.sh` + `mbdb import` against `fullexport` LATEST (dump 7 GB, ~30-180 min) | Inventory vs loaded counts per table; column-manifest checks pass; run notes §R3 |
| 5 | `feat(query): name-first query layer + JSON contract` | Query surface (M1) | `src/mbdb/query.rs` + CLI (`query artist/release-group/release/recording/work/wikidata`); exact-match-first ordering with stable id/gid tiebreakers; aliases; no silent filtering of status/duplicates | Unit tests: exact vs alias vs substring ordering; disambiguated duplicate surfacing; contract JSON snapshots (golden files) |
| 6 | `docs(architecture): document the MB DB and query surface` | ARCHITECTURE.md | `ARCHITECTURE.md` — data flow (import: ingestion/verify; query: output), module map, key decisions (separate DB, weekly atomic swap, wikidata url bridge); cross-refs AGENTS.md/README | Doc review; cross-references resolve |
| 7 | `docs(research): record battery results and mark plan implemented` | DoD battery + close-out | Re-run §8 battery; record measured values in run notes; swap/release build; mark this doc `Status: Implemented` with closing commit refs | Battery table filled; release build green |

**Verify gate (steps 1-6):** `cargo test` all green → `cargo clippy -- -D warnings` zero warnings →
`cargo fmt --check` clean → `cargo build --release` succeeds → commit.

**Out of scope (tracked as later phases):** tag_agent `db_layer.py` integration (a `DbSearch`-shaped second
surface), wiki-DB reconciliation via the Wikidata link tables, Levenshtein/distance scoring, and the web/UI.

## 8. Definition-of-Done battery (M1, exact expectations)

Run against `musicbrainz.duckdb` after Step 4. Goldens codify *verified facts* from the live sources; any
measured mismatch is recorded with root cause (never tuned around):

| # | Check | Expect | Measured at run (fill in) |
| --- | --- | --- | --- |
| 1 | Import completeness | Imported rows == source-file inventory rows per table (recorded) | |
| 2 | `query artist --name U2` | Exact-match band U2 **first**; disambiguation "Irish rock band"; aliases surfaced (e.g. 유투) | |
| 3 | `query release-group --name "The Joshua Tree"` | RG = Album, first-release-date 1987-03-09, artist-credit U2 | |
| 4 | `query release --release-group <that RG> --inc tracks` | 11 tracks; #1 "Where the Streets Have No Name" ≈ 337.5 s; #3 "With or Without You" | |
| 5 | `query release-group --name "Avenue Q"` | ≥ 1 RG including "Avenue Q: The Musical" (Soundtrack, 2003-10-06); other Avenue Q RGs present | |
| 6 | `query release-group --name Ragtime` | RG list incl. "Ragtime" (1981 film) or "Ragtime (musical)" if present — record coverage verdict per corpus dir | |
| 7 | Wikidata bridge | `url` + `l_*_url` rows with `link_type` = wikidata > 0 for artist/release_group/work; `query wikidata --mbid 6f3e9fa6-…` resolves The Joshua Tree to its `https://www.wikidata.org/wiki/Q…` | |
| 8 | Corpus spot-checks | For each `/home/tr/mp3_files/` dir, record the top 1-3 sensible RG hits; list any dir with **no** evidence as a finding (both sources may miss long-tail — measured, not tuned around) | |
| 9 | CLI contract | JSON outputs parse against the §4 snapshot goldens; exact-match-first ordering verified incl. >100-row spillover case | |

## 9. Pitfalls

- **Dump TSV encoding is PostgreSQL COPY text, not plain TSV** — backslash-escaped fields (`\\`, `\t`, `\n`,
  `\r`, `\b`, `\f`); booleans are `t`/`f`; **NULL is the exact field `\N`** (an empty field is a real empty
  string — verified against the 2026-09-01 sample; music titles *do* contain tabs/newlines in the wild).
- **Schema 31 differs from the classic docs**: track/medium/work/area/artist_credit/link_type all carry `gid`
  columns, `track` has an explicit `recording` FK, and there is **no `mbid` table**. Column manifests pinned at
  the schema step (no TSV headers) and verified against the dump inventory.
- **Link-table names follow alphabetical entity order**: `l_label_release`, `l_recording_release`, `l_artist_*`,
  … — `l_release_label` / `l_work_release` do not exist. No work↔release link table ships; derive it via
  `l_recording_work ⋈ l_recording_release`.
- **Disk budget:** 59 GB free at planning time; keep the dump between refreshes, stream imports, delete staging,
  retain one `.prev` — flag the run if free space < 25 GB (mirror the wiki suite's tripwire habit).
- **Weekly full rebuild is the point** — no incremental complexity; an atomic swap keeps a broken rebuild from
  ever being visible.
- **Never filter silently:** release `status` (Official/Promotion/Bootleg), disambiguated duplicates, and
  multi-release RGs stay visible; the tag_agent LLM decides.
- **Recording `length` is ms in MB** — expose seconds in the JSON contract to match the mp3-side convention.
- **Long-tail gaps remain** (Ragtime film soundtrack did not surface in a live top-hit check): the battery
  measures coverage per corpus dir; a miss is recorded as a finding, not a tuning target.

Status tags: **[verified]** = checked against the 2026-09-01 sample dump on 2026-09-30 (review pass);
**[open]** = needs confirmation during this plan's execution.

1. **[verified→de-risked]** Exact member filenames inside `mbdump-derived.tar.bz2` (the optional `l_*_wikidata`
   set and `recording_first_release_date`) — confirmed by inventory at import step 1; discrepancies get
   recorded. **No longer blocking:** the Wikidata bridge and RG/release first-release dates have verified
   core-table paths (`url` + `l_*_url`; `release_country`/`release_unknown_country`); derived tables are
   imported if and only if present.
2. **[open]** Whether to retain the dump 7 GB between refreshes vs re-download weekly — decision at first refresh
   run (bandwidth vs storage trade-off; both are acceptable today).
3. **[open]** Import wall-clock (30-180 min estimate) — measured and recorded at milestone 4; affects only
   scheduling.
4. **[open]** tag_agent integration and wiki-DB reconciliation are deliberately deferred — revisit after this
   battery passes and the tag_agent `db_layer.py` milestone is open.

## Run notes

Execution-time measurements (inventory counts, sample-import counts, full-import counts, battery
results, refresh-cycle record) per the house documentation conventions.

### §R1 — Sample-dump inventory (2026-10-01 dump, schema 31)

- Archive: `sample/20261001-000001/mbdump-sample.tar.xz` (346 MB compressed, 1.7 GB decompressed,
  239 members; confirms `mbdump-sample.tar.xz` — **xz, not bz2** — for the P0 smoke import).
- Members: 5 metadata files at the tar root (`TIMESTAMP`, `COPYING`, `README`, `REPLICATION_SEQUENCE`,
  `SCHEMA_SEQUENCE`) + `mbdump/…` tables. The sample ships **core members only**; no derived members
  (`l_*_wikidata`, `recording_first_release_date`, …), so the derived path is first exercised at the
  full import (milestone-4 TODO).
- Member order in the tar is alphabetical by file stem, independent of load-order (_release_ before
  _release_group_, _release_group_ before _release_group_primary_type_, …) — the root cause of the
  FK-less load-target decision (module header of `src/mbdb/import.rs`).
- Counts below spot-verified against the full export expectations (e.g. `release_group_primary_type`
  = 5, `release_status` = 7, `release_packaging` = 24).

### §R2 — P0 smoke import (milestone-1 step 3)

- Reference:`mbdb import --tables core --dump-dir <dumps-sample> --db smoke.duckdb`
  (`target/release/mbdb`, single pass, constant memory: peak RSS not instrumented).
- **Duration: 16m02s** wall for the smoke set (1.7 GB decompressed, ~11.8 M pinned rows) — the full
  export at ~7 GB raw should land within the plan's 30–180 min budget.
- Loaded **51 tables, 14,669,585 rows** (all pinned members present): zero rejections, checkpoint
  totals identical to table row counts, `referential integrity: CLEAN` (zero orphans across every
  manifest FK edge). Per-table counts:

  | table | rows | table | rows |
  |---|---|---|---|
  | area | 17 867 | isrc | 644 959 |
  | area_gid_redirect | 69 | iswc | 69 115 |
  | artist | 262 044 | l_artist_recording | 1 945 209 |
  | artist_alias | 158 782 | l_artist_release | 15 962 |
  | artist_credit | 292 775 | l_artist_release_group | 162 |
  | artist_credit_gid_redirect | 8 499 | l_artist_url | 41 977 |
  | artist_credit_name | 672 097 | l_artist_work | 371 213 |
  | artist_gid_redirect | 40 945 | l_label_release | 6 748 |
  | label | 24 677 | l_recording_release | 85 |
  | label_gid_redirect | 3 958 | l_recording_url | 141 839 |
  | link | 256 940 | l_recording_work | 2 764 609 |
  | link_attribute_type | 1 169 | l_release_group_url | 1 306 |
  | link_type | 697 | l_release_url | 6 901 |
  | medium | 48 281 | l_url_work | 56 950 |
  | recording | 2 979 704 | release | 29 281 |
  | recording_alias | 410 371 | release_country | 51 641 |
  | recording_gid_redirect | 1 818 388 | release_gid_redirect | 4 132 |
  | release_group | 23 132 | release_label | 34 063 |
  | release_group_alias | 2 609 | release_packaging | 24 |
  | release_group_gid_redirect | 10 658 | release_status | 7 |
  | release_group_primary_type | 5 | release_unknown_country | 1 757 |
  | release_group_secondary_type | 12 | track | 584 064 |
  | release_group_secondary_type_join | 7 947 | url | 228 611 |
  | work | 245 164 | url_gid_redirect | 919 |
  | work_alias | 62 079 | work_gid_redirect | 99 403 |

  (sum matches the checkpoint file; `duckdb_tables()` + `count(*)` cross-checked 2026-10-01.)

- **Deviation from plan (verified against the actual 2026-10-01 sample):** §3 states dump booleans are
  `t`/`f`; the shipped dump renders every boolean (`ended`, `video`, `is_data_track`,
  `primary_for_locale`, …) as **`0`/`1`** — zero `t`/`f` values appear in any member (grep-verified
  across `track`, `recording`, `artist`, `area`, `link`, `isrc`, `release_group_secondary_type_join`).
  `tsv::parse_bool` therefore accepts `0`/`1` (canonical for schema-31 exports) and keeps `t`/`f` as
  legacy tolerance. The first smoke attempt (parser accepting only `t`/`f`) rejected ~4.5 M rows on
  exactly the boolean columns; the fixed run above is rejection-free.
- Also spot-verified against the dump while diagnosing: `track` gained `is_data_track` (12 columns,
  in the manifest), aliases keep a `sort_name` column in the export (16 columns), and `artist` peaks at
  19 columns — the pinned manifests match the shipped members field-for-field.