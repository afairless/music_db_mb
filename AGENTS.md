# music_db_mb

## Purpose

Builds and serves a local, single-user DuckDB database of MusicBrainz metadata for the
tag_agent project (decision D10): a *separate* MusicBrainz-derived evidence source that
complements the wiki database without touching it. Streams the MusicBrainz `fullexport`
TSV dumps (~7 GB core + ~497 MB derived, CC0) into a 50-table DuckDB, then exposes a
deterministic name-first query surface (`mbdb query …`) with the tag_agent `DbSearch`-shaped
JSON contract. Refreshed weekly by a full rebuild + atomic swap.

## Stack

- **Language**: Rust (edition 2024)
- **Database**: DuckDB 1.x (embedded, via the `duckdb` crate with `bundled` feature)
- **CLI**: `clap` 4.x with derive macros
- **Config**: `toml` + `serde`
- **Archives**: `bzip2` / `xz2` (streaming decompression) + `tar`
- **Serialization**: `serde_json`
- **Error handling**: `thiserror` (library)
- **Testing**: `cargo test` + `tempfile`
- **Linting**: `cargo clippy --all-targets --all-features -- -D warnings`
- **Formatting**: `rustfmt`
- **Package manager**: `cargo`

## Project Structure

```text
src/
  main.rs            — CLI entry point; import + query subcommand dispatch
  mbdb.rs            — Library module root
  lib.rs             — Public exports
  mbdb/config.rs     — TOML configuration loading + path expansion (import + query share it)
  mbdb/schema.rs     — DuckDB DDL generator (source CHECK constraints re-expressed)
  mbdb/manifest.rs   — Pinned schema-31 column manifests + load order (the source of truth)
  mbdb/tsv.rs        — PostgreSQL COPY-text streaming parser (backslash escapes, \N NULL)
  mbdb/import.rs     — Batch loader: bz2/xz discovery, ~100 k-row INSERTs, checkpoint/resume,
                       rejections log, referential-integrity verification
  mbdb/query.rs      — Name-first query surface: artist / release-group / release / recording /
                       work / wikidata (JSON contract shapes + tier ranking)
scripts/
  download_dumps.sh  — Checksum-gated dump downloader (MD5SUMS verification)
tests/
  schema_smoke.rs    — Real-schema smoke: DDL executes, load order resolves FKs, and every
                       query binds to the true 50-table column set
  golden/            — JSON contract snapshots for the query layer
docs/
  research/          — Feature research and implementation plan
```

## Commands

| Action | Command |
|---|---|
| Build | `cargo build` |
| Build release | `cargo build --release` |
| Test all | `cargo test` |
| Test single | `cargo test test_name` |
| Lint | `cargo clippy --all-targets --all-features -- -D warnings` |
| Format check | `cargo fmt --check` |
| Format fix | `cargo fmt` |
| Download dumps | `scripts/download_dumps.sh` |
| Run import | `cargo run --release -- import [--dump-dir <dir>] [--db <file>] [--resume]` |
| Run query | `cargo run --release -- query artist --name <name>` |
| Query help | `cargo run --release -- query --help` |

## Conventions

### Rust

- All public items MUST have doc comments (`///` or `//!`).
- Errors use `thiserror` for library code; `anyhow` is not used (the binary propagates
  domain errors directly).
- No `unwrap()`/`expect()` in production code — use `?` to propagate errors; `expect` is
  acceptable in tests and fixtures.
- Follow the `rust-dev` skill for Rust-specific conventions.
- Follow the `testing-guide` skill for test structure (Arrange-Act-Assert, None-One-Many).
- Follow the `incremental-development` skill for the commit workflow (implement → test →
  lint → fmt → release build → commit, one step at a time).

### Error handling and data contracts

- Missing optional data (aliases, dates, statuses, lengths, labels) is stored/returned as
  NULL / empty vectors — never fabricated or estimated.
- The query layer **never filters silently**: statuses, disambiguated duplicates, and the
  long tail stay visible; the LLM decides (the wiki suite's soft-evidence rule).
- Dump schema is pinned per table in `manifest.rs` (schema 31). Discrepancies are recorded,
  never tuned around.
- Malformed dump rows go to the rejections log (count + first-N samples), never silently
  dropped; every import run must report zero unexpected rejections.
- Queries are byte-deterministic: exact-match-first tiers with stable id/gid tiebreakers.

### Commit messages

- Use conventional commits (the `conventional-commit` skill).
- Format: `type(scope): description` — e.g., `feat(query): add release query with tracklists`.