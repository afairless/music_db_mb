# Architecture — music_db_mb

<!---  Status: live · milestone-1 (M1) implemented 2026-10-03 --->

## 1. Overview

**What.** `music_db_mb` builds and serves a local DuckDB of MusicBrainz music metadata for
the tag_agent project (architecture decision **D10 / Option 1**): a *separate*,
MusicBrainz-derived evidence source that complements the wiki database
(`~/wiki_db/music.duckdb`) without touching it.

**Why.** The wiki DB structurally cannot supply complete tracklists, durations, cast
recordings, or composer graphs (track→album coverage ≈ 5%; no per-album tracklists; no
Avenue Q/Spamalot rows). MusicBrainz ships exactly these as CC0 bulk data. A local DB is the
right call over the live API (hard 1 request/s/client limit). Rust was chosen to match the
sibling pipeline repo (`wiki_music_db`) and to stream the 7 GB dump in constant memory.

**Who.** Consumers are the tag_agent LLM pipeline (via the query library / CLI's JSON
contract today; a `db_layer.py` wrapper later) and operators refreshing the weekly rebuild.

## 2. System diagram

```text
┌──────────────────────┐   ┌───────────────────────┐   ┌──────────────────────────┐
│   ingestion          │   │   transformation      │   │   output                │
│   (scripts/          │──▶│   (mbdb import)       │──▶│   (mbdb query)          │
│    download + tar)   │   │   TSV → DuckDB        │   │   JSON contract         │
└──────────────────────┘   └───────────────────────┘   └──────────────────────────┘
        │  MD5SUMS-verified dumps (formats stay as shipped — pg COPY text)
        ▼
   ~/musicbrainz_db/dumps/         (archive staging, retained between refreshes)
        │  constant-memory bz2/xz stream, column-manifest INSERTs, checkpoint/resume
        ▼
   ~/musicbrainz_db/musicbrainz.duckdb      (single-user, embedded; rebuilt weekly)
        │  name-first queries with exact-match tiers, JSON, no silent filtering
        ▼
        query CLI / src/mbdb/query.rs  ──▶  tag_agent (DbSearch-shaped contract)
```

## 3. Module map

| Module | Responsibility | Depends on | Key types |
|---|---|---|---|
| `src/mbdb/manifest.rs` | Pinned schema-31 column manifests (names, kinds, nullability, FKs, CHECKs) + the corrected load order — the single source of truth for both DDL and the loader | — | `TableManifest`, `Column`, `ALL` |
| `src/mbdb/schema.rs` | DuckDB DDL generator from the manifests (all 50 tables, FKs, source CHECKs re-expressed as DuckDB CHECKs) | `manifest` | `create_schema`, `ddl` |
| `src/mbdb/config.rs` | `config.toml` defaults with CLI overrides; shared by import and query | — | `Settings`, `CliOverrides` |
| `src/mbdb/tsv.rs` | Streaming PostgreSQL COPY-text parser: backslash escapes, `0`/`1` + legacy `t`/`f` booleans, `\N` NULL vs empty string | — | `TsvReader` |
| `src/mbdb/import.rs` | Archive discovery (extension-based bz2/xz), ~100 k-row batch INSERTs per table in a transaction, checkpoint + `--resume`, per-table counters, rejections log, inventory + referential-integrity verification | `manifest`, `tsv`, `schema` | `load`, `LoadReport`, `verify_integrity` |
| `src/mbdb/query.rs` | Six evidence queries (artist, release-group, release, recording, work, wikidata) with exact-first tiers, deterministic ordering, and the §4 JSON contract | opens `musicbrainz.duckdb` directly | `query_*`, `QueryResult<T>`, `QueryOptions`, hit structs |
| `src/main.rs` | CLI dispatch (`import` / `query …`) and report printing | all of the above | `Cli`, `Command` |
| `scripts/download_dumps.sh` | Downloader verified against the shipped `MD5SUMS` (md5sum binary-mode `*` markers) | — | — |
| `tests/schema_smoke.rs` | Real-schema smoke tests: DDL executes, load order resolves FKs, every query binds to the true column set | `schema`, `manifest`, `query` | — |
| `tests/golden/` | Byte-exact JSON contract snapshots for the query layer | — | — |

## 4. Data flow

### 4.1 Import pipeline (ingestion → transformation → output)

1. **Ingestion — download + verify.** `scripts/download_dumps.sh` fetches
   `fullexport/<date>/` `mbdump.tar.bz2` + `mbdump-derived.tar.bz2` and verifies them
   against the shipped `MD5SUMS` before anything else happens. The tarball members are the
   raw input; nothing is transformed here.
2. **Transformation — dump → DuckDB** (`mbdb import`, single pass).
   - *Inventory*: each archive member is logged with its row count and its column count is
     checked against the pinned manifest. Discrepancies are recorded, never tuned around.
   - *Stream*: constant-memory decompression (compressor detected by extension), the
     PG COPY-text parser, and ~100 k-row batch INSERTs via the column manifest inside a
     transaction, with progress checkpoints for `--resume`. Malformed rows go to the
     rejections log (count + first-N samples).
   - *Verify*: per-table loaded counts == inventory counts; zero unexpected rejections;
     `verify_integrity` re-scans every manifest FK edge (79 edges) for orphans. The run
     notes (§R1–§R3) record the measured numbers.
3. **Output — the database file** `~/musicbrainz_db/musicbrainz.duckdb`. The refresh cycle
   rebuilds into `musicbrainz.new.duckdb` and atomically `mv`-swaps it into place, keeping
   `musicbrainz.prev.duckdb` for one cycle (archive, never delete).

### 4.2 Query surface (plan §4)

`mbdb query <kind>` executes a name-first, byte-deterministic query and prints one JSON
document. All queries rank exact matches before aliases before substrings (stable id/gid
tiebreakers), surface aliases + disambiguation, convert ms lengths to seconds, and
**never filter rows silently** — statuses, disambiguated duplicates, and the long tail
stay visible so the tag_agent LLM (not the DB) decides.

| Query | Input | Evidence |
|---|---|---|
| `query artist --name <s>` | name | exact name → sort_name → alias → substrings; aliases, disambiguation |
| `query release-group --name <s> [--artist <s>] [--type <t>]` | name (+filters) | primary + secondary types, first release date, artist credit, aliases |
| `query release --release-group <gid> [--inc-tracks]` | group gid (lookup) | status, date, countries, labels + catalog numbers, credit; per-medium tracklists with seconds + recording mbids |
| `query recording --name <s> [--artist <s>]` | name (+filter) | duration seconds (ms → s), first release date (via the sparse `l_recording_release` bridge — `None` when absent), credit, aliases |
| `query work --name <s> [--composer <s>]` | name (+filter) | ISWCs, raw `work.type` id (name table not imported), composer links (link_type `composer`), aliases |
| `query wikidata --mbid <gid>` | gid (lookup) | Wikidata QIDs via the `url` + `l_<entity>_url` bridge (link_type `wikidata`) |

## 5. Key design decisions

### Decision: separate MusicBrainz DuckDB (D10/Option 1) over fixing the wiki DB

- **Context**: the wiki DB cannot produce complete tracklists/durations or cast
  recordings; reworking it would fight dump-coverage limits (P361 envelope, QID mirrors).
- **Decision**: build a separate `music_db_mb` repo + DuckDB from the MusicBrainz CC0
  `fullexport`, leaving `~/wiki_db/music.duckdb` untouched.
- **Alternatives considered**: on-demand MusicBrainz API (hard 1 req/s + shared search
  pool — judged slow/flaky); JSON dumps (available, but the TSV `fullexport` is L2-locked
  and richer for the relational joins we need).
- **Consequences**:
  - (+) Complete tracklists + durations, cast recordings, composer/work graph, MBID↔Wikidata.
  - (−) A second database to build/refresh; weekly rebuild is a deliberate trade (no
    incremental complexity).

### Decision: weekly full rebuild with atomic swap

- **Context**: fullexport regenerates twice weekly; incremental sync adds complexity.
- **Decision**: re-download + full rebuild into `musicbrainz.new.duckdb`, then atomic
  `mv` swap; retain `musicbrainz.prev.duckdb` for one cycle.
- **Consequences**:
  - (+) A broken rebuild is never visible; rollback = rename one file; the loader's
    checkpoint/resume makes even a 6 h run interrupt-safe.
  - (−) Disk peak ≈ 13–19 GB (dumps + DB + `.prev`); keep ≥ 30 GB free before a refresh
    run (tripwire lesson from §R3).

### Decision: keep MusicBrainz ids (gid) as text; no `mbid` table

- **Context**: schema-31 exports carry `gid` UUID columns on nearly every substantive table
  (track, medium, artist_credit, link_type included), and there is **no `mbid` mapping
  table** in the dump.
- **Decision**: import `gid` columns as shipped; `link` rows keep only their integer id
  (never invent synthetic MBIDs); the `*_gid_redirect` tables are imported so merged/
  re-pointed ids stay resolvable.
- **Consequences**: (+) the query contract uses the real MBIDs (v1 of this design would
  have invented a mapping layer; here `mbid` == `gid`); (−) id-family lookups are text
  scans, fine for a single-user local DB.

### Decision: the Wikidata bridge is `url` + `l_<entity>_url` (link_type `wikidata`)

- **Context**: the plan anticipated optional `l_*_wikidata` snapshot tables in the derived
  archive; the §R3 full-import inventory showed the schema-31 exports ship **no such
  members** (removed upstream).
- **Decision**: the query layer implements only the verified core path — `url` rows joined
  through `l_<entity>_url` → `link` → `link_type` filtered to name `wikidata`; `l_url_work`
  is alphabetical (entity0 = url, entity1 = work) so its sides are reversed.
- **Consequences**: (+) no dead code for tables that can never exist; the bridge was
  verified live (artist U2 → Q396, The Joshua Tree → Q152873, With or Without You →
  Q1131812); (−) nothing to revisit — the snapshot fast path is documented as absent.

### Decision: pinned schema-31 column manifests as the single source of truth

- **Context**: dump TSVs have no headers and the schema-31 layout differs from classic
  docs; similar-typed columns could silently swap.
- **Decision**: `manifest.rs` pins every imported table's column list (from the official
  DDL), which drives both the DDL generator and the loader's `INSERT INTO t (col…)`
  statements, and the smoke suite binds every query against the *real* generated schema.
- **Consequences**: (+) a latent query/schema mismatch surfaces as a failing smoke test
  (this is exactly how the `release_group_secondary_type_join.id` binder error was caught
  and fixed, 2026-10-03); (−) a schema bump upstream is a manifest change + re-verify, by
  design.

## 6. Limitations

- **Single user, single machine**: the DuckDB file is not a service; concurrent read
  access is fine, concurrent writers are not.
- **No incremental updates**: the weekly full rebuild is the only refresh path — small
  edits between exports surface only after the next rebuild.
- **Sparse join data**: `l_recording_release` ships ~1.3 k rows in the full export, so
  recording first-release dates are `None` for almost all recordings; the field is
  best-effort and never estimated.
- **Opaque FKs where the name table isn't imported**: `work.type` (→ `work_type`),
  `medium.format` (→ `medium_format`), and alias/artist type ids surface as raw integers
  — the name tables ship no members in the pinned subset.
- **Partial dates render as `YYYY-01-01`**: a country row with only `date_year` formats
  month/day as 01 (matching the release-group convention); the battery records this
  nuance rather than tuning around it.

## 7. Security and operations

- **Secrets**: none — the pipeline is public CC0 data over local files; no credentials are
  stored or requested.
- **Data provenance**: downloads are MD5SUMS-verified before import; loaded counts are
  checkpoint-recorded and re-verifiable per table; rejections are logged, never silent.
- **Observability**: the import prints per-table counters, skipped-member inventory, and a
  referential-integrity verdict; run notes (§R1–§R3) track wall-clock, peak RSS, and disk
  tripwires (keep ≥ 30 GB free).
- **Battery**: the definition-of-done checks in `docs/research/2026-09_musicbrainz_db_plan.md`
  §8 are re-run after every full import; measured values are recorded in the run notes.

## 8. Cross-references

- [AGENTS.md](./AGENTS.md) — repo conventions (toolchain, verify gate, error handling)
- [README.md](./README.md) — what the repo does, key decisions, query examples
- [docs/research/2026-09_musicbrainz_db_plan.md](./docs/research/2026-09_musicbrainz_db_plan.md)
  — the plan: target schema §3, query contract §4, pipeline §5, battery §8, run notes
- Consumer: tag_agent — `/home/tr/Documents/tag_agent/docs/ARCHITECTURE-proposal.md`
- Sibling pipeline: wiki_music_db — `/home/tr/Documents/wiki_music_db/`