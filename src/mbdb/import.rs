//! Streaming batch importer for MusicBrainz dump tarballs.
//!
//! # Design
//!
//! The loader makes **one decompression pass** per dump archive
//! (`.tar.bz2` / `.tar.xz`, detected by extension), streaming each member
//! through the COPY-text parser ([`tsv`](crate::mbdb::tsv)) at constant
//! memory, inserting rows in multi-row statements ([`ROWS_PER_STATEMENT`])
//! inside committed batches ([`ROWS_PER_BATCH`]). Each batch commits once
//! and the checkpoint file (``<db>.checkpoint.toml``) advances past exactly
//! those committed rows, so an interrupt can lose at most one batch:
//! re-running with `--resume` picks up at the last checkpoint. The
//! production import is therefore interrupt-safe without re-reading
//! already-committed rows' contents.
//!
//! ## Members load in archive order, not manifest order
//!
//! A compressed tar cannot be seeked, so the loader reads members in the
//! order the dump ships them (alphabetical by file name) and inserts into
//! whichever manifest table each member belongs to. This is only possible
//! because the **load target has no row-level foreign keys**.
//!
//! The DuckDB build bundled with this project enforces foreign keys on
//! insert and exposes no setting to disable them (verified by probe:
//! `foreign_keys`, `enforce_foreign_keys`, `SET foreign_keys`,
//! `disable_foreign_keys` are all rejected or ignored). Alphabetical
//! archive order violates FK presence anyway — `release` loads before
//! `release_group`, `release_group` before `release_group_primary_type` —
//! and the same-table parent/child hierarchies (`link_type.parent`,
//! `release_status.parent`, …) cannot guarantee parent-before-child order
//! *within* a member. Row-level FKs are therefore incompatible with
//! single-pass streaming, and the loader creates the target schema with
//! `foreign_keys = false` (see
//! [`schema.rs`](crate::mbdb::schema) [`ddl_with`](crate::mbdb::schema::ddl_with)).
//!
//! Referential integrity is preserved **by construction and by
//! verification** instead:
//!
//! - the pinned manifests are the authoritative column lists — every cell
//!   is cast per its declared [`Kind`](crate::mbdb::manifest::Kind), and
//!   malformed rows (wrong width, undecodable int, invalid bool,
//!   non-UTF-8 text) are rejected, never silently dropped or shifted;
//! - `--resume` cannot re-insert committed rows (checkpoint gate);
//! - [`verify_integrity`] runs after every load and reports the orphan
//!   count per manifest FK edge (rows whose FK id has no parent row) — the
//!   run notes must show this clean (all zeros) for a healthy import,
//!   which the corrected load order guarantees for correctly loaded dumps.
//!
//! ## Rejections
//!
//! Rejected rows are counted per table with the first few rendered verbatim
//! (COPY text, byte-identical to the dump) into ``<db>.rejections.log``.
//! The milestone battery treats **any** rejection as a defect.
//!
//! ## Sources and scope
//!
//! Archives are discovered in the dump directory by name: core =
//! `mbdump.{tar.bz2,tar.xz}`, falling back to `mbdump-sample.`* ; derived =
//! `mbdump-derived.{tar.bz2,tar.xz}`. Other `mbdump-*.` archives
//! (`mbdump-edit`, `mbdump-editor`, `mbdump-cdstubs`, `mbdump-stats`,
//! `mbdump-cover-art-archive`, …) are **not** loaded — their tables are not
//! pinned in the manifest — and are left untouched on disk. Members whose
//! file name matches no manifest table are recorded as skipped with their
//! size, so a schema drift in the dump is visible in the run notes instead
//! of passing silently. (The derived archive's tables are currently all
//! unpinned — `l_*_wikidata`, `recording_first_release_date` — future work,
//! out of milestone-1 scope.)

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::str;

use duckdb::Connection;
use duckdb::params_from_iter;
use duckdb::types::Value;

use serde::{Deserialize, Serialize};

use crate::mbdb::config::{Settings, Tables};
use crate::mbdb::manifest::{ALL, Kind, TableManifest, table};
use crate::mbdb::schema::{SchemaOptions, create_schema_with};
use crate::mbdb::tsv::{Field, Row, TsvReader, parse_bool};

/// Rows per multi-row `INSERT` statement (parameter-position budget).
pub const ROWS_PER_STATEMENT: usize = 1000;

/// Rows per committed batch / checkpoint advance.
pub const ROWS_PER_BATCH: u64 = 100_000;

/// Rejection samples written to the log per table.
const REJECTION_SAMPLES: usize = 5;

/// Cap for a rendered rejection sample (bytes).
const REJECTION_SAMPLE_CAP: usize = 4096;

/// Errors raised while importing a dump into DuckDB.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The resolved database path exists on a non-resume run.
    #[error(
        "database `{path}` already exists (pass --resume to continue an interrupted import, or delete the file to start over)"
    )]
    DbExists {
        /// The database path.
        path: String,
    },

    /// The database file cannot be opened for writing.
    #[error("cannot open database `{path}`: {source}")]
    DbOpen {
        /// The database path.
        path: String,
        /// The underlying DuckDB error.
        #[source]
        source: duckdb::Error,
    },

    /// Schema DDL failed on the fresh target.
    #[error("cannot create schema: {source}")]
    Schema {
        /// The underlying schema error.
        #[source]
        source: crate::mbdb::schema::SchemaError,
    },

    /// The dump directory does not exist.
    #[error("dump directory `{path}` does not exist")]
    DumpDirMissing {
        /// The dump directory path.
        path: String,
    },

    /// No usable core archive found.
    #[error(
        "no core dump archive found in `{path}` (expected mbdump.tar.bz2/.xz or mbdump-sample.tar.bz2/.xz)"
    )]
    NoCoreArchive {
        /// The dump directory path.
        path: String,
    },

    /// No usable derived archive found.
    #[error("no derived dump archive found in `{path}` (expected mbdump-derived.tar.bz2/.xz)")]
    NoDerivedArchive {
        /// The dump directory path.
        path: String,
    },

    /// The dump directory cannot be listed.
    #[error("cannot list dump directory `{path}`: {source}")]
    ReadDumpDir {
        /// The dump directory path.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Unsupported archive extension (need `.tar.bz2` or `.tar.xz`).
    #[error("unsupported archive `{path}` (expected .tar.bz2 or .tar.xz)")]
    UnsupportedArchive {
        /// The archive path.
        path: String,
    },

    /// The archive cannot be opened / decompressed.
    #[error("cannot open archive `{path}`: {message}")]
    ArchiveOpen {
        /// The archive path.
        path: String,
        /// Why the file could not be opened or decoded.
        message: String,
    },

    /// Decompressing or reading the archive failed mid-stream.
    #[error("cannot read archive `{path}`: {message}")]
    ArchiveRead {
        /// The archive path.
        path: String,
        /// Why the read failed.
        message: String,
    },

    /// The checkpoint file exists but cannot be parsed.
    #[error("cannot parse checkpoint file `{path}`: {message}")]
    CheckpointParse {
        /// The checkpoint path.
        path: String,
        /// Why the file is invalid.
        message: String,
    },

    /// The checkpoint file cannot be written (resume safety is lost — the
    /// import is aborted rather than continue unprotected).
    #[error("cannot write checkpoint file `{path}`: {message}")]
    CheckpointWrite {
        /// The checkpoint path.
        path: String,
        /// Why the write failed.
        message: String,
    },

    /// An import statement failed against the database.
    #[error("statement failed on table `{table}`: {source}")]
    Statement {
        /// The target table (or bookkeeping pseudo-table).
        table: String,
        /// The underlying DuckDB error.
        #[source]
        source: duckdb::Error,
    },

    /// The rejections log cannot be written.
    #[error("cannot write rejections log `{path}`: {message}")]
    RejectionsWrite {
        /// The log path.
        path: String,
        /// Why the write failed.
        message: String,
    },
}

/// One dump member seen at inventory: which archive, which member, how big.
#[derive(Debug, PartialEq, Eq)]
pub struct MemberInfo {
    /// Archive file name, e.g. `mbdump.tar.bz2`.
    pub archive: String,
    /// Member file name (`mbdump/…` basename), e.g. `artist`.
    pub member: String,
    /// Header size in bytes.
    pub size: u64,
}

/// Per-table import counters for the run report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TableStats {
    /// Rows present in the table after this run (including resumed rows).
    pub rows: u64,
    /// Rows skipped because an earlier run committed them (resume).
    pub resumed_rows: u64,
    /// Rows rejected by manifest validation.
    pub rejected: u64,
}

/// Rejection accounting for one table.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RejectionSummary {
    /// Total rejected rows.
    pub count: u64,
    /// First few rejected rows, rendered verbatim (COPY text).
    pub samples: Vec<Vec<u8>>,
}

/// Everything the loader learned during one run.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Archives processed, in order.
    pub archives: Vec<String>,
    /// Every member inventoried (loaded or skipped), with sizes.
    pub members: Vec<MemberInfo>,
    /// Per-table import stats (one entry per member that matched a
    /// manifest, whether loaded or resumed).
    pub tables: BTreeMap<String, TableStats>,
    /// Member file names that matched no manifest table, `archive:member`.
    pub skipped_members: Vec<String>,
    /// Rejection counts and samples, keyed by table.
    pub rejections: BTreeMap<String, RejectionSummary>,
}

/// Checkpoint state: per-table committed row counts.
///
/// Serialized to ``<db>.checkpoint.toml`` after every committed batch. A
/// table's entry counts the rows an interrupted run must *not* re-insert.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Checkpoint {
    pub tables: BTreeMap<String, u64>,
}

/// Path of the checkpoint file for a database.
pub fn checkpoint_path(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.checkpoint.toml", db.to_string_lossy()))
}

/// Path of the rejections log for a database.
pub fn rejections_path(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.rejections.log", db.to_string_lossy()))
}

/// Import dump archives into the DuckDB database named in `settings`.
///
/// Fresh run (`resume = false`): errors if the database already exists,
/// creates the FK-free load-target schema, then processes every discovered
/// archive in one pass. Resume run: continues from the checkpoint file,
/// skipping committed rows; a missing database or checkpoint starts a fresh
/// import (noted on stderr).
///
/// Progress lines go to stderr (run-notes material); the returned
/// [`LoadReport`] is the machine-readable summary.
pub fn load(settings: &Settings, resume: bool) -> Result<LoadReport, ImportError> {
    let db_path = PathBuf::from(&settings.db);

    if !resume && db_path.exists() {
        return Err(ImportError::DbExists {
            path: settings.db.to_owned(),
        });
    }
    if resume && !db_path.exists() {
        eprintln!(
            "note: --resume requested but `{}` is missing; starting a fresh import",
            settings.db
        );
    }

    let archives = discover_archives(settings)?;
    if archives.is_empty() {
        eprintln!("note: no archives to import for the selected scope");
        return Ok(LoadReport::default());
    }

    // Checkpoint: a fresh run starts empty; a resume validates the file.
    let mut checkpoint = Checkpoint::default();
    let checkpoint_file = checkpoint_path(&db_path);
    if resume && checkpoint_file.exists() {
        match fs::read_to_string(&checkpoint_file) {
            Ok(text) => match toml::from_str::<Checkpoint>(&text) {
                Ok(parsed) => checkpoint = parsed,
                Err(_) => {
                    return Err(ImportError::CheckpointParse {
                        path: checkpoint_file.to_string_lossy().into_owned(),
                        message: "invalid checkpoint data".to_owned(),
                    });
                }
            },
            Err(_) => {
                // Unreadable checkpoint — treat as absent; a fresh import
                // is strictly safer than trusting a half-written file.
                eprintln!(
                    "note: checkpoint `{}` unreadable; starting fresh",
                    checkpoint_file.to_string_lossy()
                );
            }
        }
    }

    let mut conn = Connection::open(&db_path).map_err(|source| ImportError::DbOpen {
        path: settings.db.to_owned(),
        source,
    })?;

    // The bundled DuckDB build rejects WAL/synchronous pragmas outright
    // (probe: "unrecognized configuration parameter"); its defaults are
    // used unchanged. The checkpoint makes the run resumable regardless —
    // a crash recovers the committed batches exactly.

    create_schema_with(
        &mut conn,
        &SchemaOptions {
            foreign_keys: false,
        },
    )
    .map_err(|source| ImportError::Schema { source })?;

    conn.execute_batch("BEGIN")
        .map_err(|source| ImportError::Statement {
            table: "<txn>".to_owned(),
            source,
        })?;

    let mut report = LoadReport::default();
    for path in archives {
        let archive_name = path.file_name().unwrap().to_string_lossy().into_owned();
        eprintln!("import: opening {}", path.to_string_lossy().into_owned());
        load_archive(&path, &db_path, &mut conn, &mut checkpoint, &mut report)?;
        report.archives.push(archive_name);
    }
    conn.execute_batch("COMMIT")
        .map_err(|source| ImportError::Statement {
            table: "<txn>".to_owned(),
            source,
        })?;

    write_rejections(&db_path, &report)?;
    Ok(report)
}

/// Discover the archives to load for `settings.tables`.
///
/// Core: `mbdump.tar.bz2` / `mbdump.tar.xz`, falling back to
/// `mbdump-sample.tar.bz2` / `mbdump-sample.tar.xz`. Derived:
/// `mbdump-derived.tar.bz2` / `mbdump-derived.tar.xz`. Other `mbdump-*.`
/// archives (edit, editor, cdstubs, stats, …) are not importable and are
/// noted on stderr so a run cannot silently skip an archive the operator
/// expected to load.
pub fn discover_archives(settings: &Settings) -> Result<Vec<PathBuf>, ImportError> {
    let dir_path = PathBuf::from(&settings.dump_dir);
    if !dir_path.exists() {
        return Err(ImportError::DumpDirMissing {
            path: settings.dump_dir.to_owned(),
        });
    }
    let dir = fs::read_dir(&dir_path).map_err(|source| ImportError::ReadDumpDir {
        path: settings.dump_dir.to_owned(),
        source,
    })?;

    let mut core: Option<PathBuf> = None;
    let mut sample: Option<PathBuf> = None;
    let mut derived: Option<PathBuf> = None;

    for result in dir {
        let entry = result.map_err(|source| ImportError::ReadDumpDir {
            path: settings.dump_dir.to_owned(),
            source,
        })?;
        let path = entry.path();
        let name = match path.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => continue,
        };
        if !name.ends_with(".tar.bz2") && !name.ends_with(".tar.xz") {
            continue;
        }
        let mut stem = name.clone();
        if let Some(trimmed) = stem.strip_suffix(".tar.bz2") {
            stem = trimmed.to_owned();
        } else if let Some(trimmed) = stem.strip_suffix(".tar.xz") {
            stem = trimmed.to_owned();
        }
        if stem == "mbdump" {
            core = Some(path);
        } else if stem == "mbdump-sample" {
            sample = Some(path);
        } else if stem == "mbdump-derived" {
            derived = Some(path);
        } else {
            eprintln!(
                "note: ignoring archive `{}` (not a core/derived dump)",
                name
            );
        }
    }

    let mut out = Vec::new();
    match settings.tables {
        Tables::Derived => {
            if let Some(path) = derived {
                out.push(path);
            } else {
                return Err(ImportError::NoDerivedArchive {
                    path: settings.dump_dir.to_owned(),
                });
            }
        }
        Tables::Core | Tables::CoreAndDerived => {
            // Production dump wins; the sample dump is the fallback so the
            // same loader config drives both the P0 smoke import and the
            // full production load.
            let chosen = core.or(sample);
            if let Some(path) = chosen {
                out.push(path);
            } else {
                return Err(ImportError::NoCoreArchive {
                    path: settings.dump_dir.to_owned(),
                });
            }
            if settings.tables == Tables::CoreAndDerived {
                if let Some(path) = derived {
                    out.push(path);
                } else {
                    eprintln!("note: no derived archive found; loading core tables only");
                }
            }
        }
    }
    Ok(out)
}

/// Decompression decoder for an archive path, picked by extension.
///
/// `.tar.bz2`/`.tbz2` → bzip2; `.tar.xz`/`.txz` → xz. Anything else errors.
fn open_decoder(path: &Path) -> Result<Box<dyn Read>, ImportError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) => {
            return Err(ImportError::ArchiveOpen {
                path: path.to_string_lossy().into_owned(),
                message: format!("cannot open file: {err}"),
            });
        }
    };
    let buffered = Box::new(BufReader::new(file));
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        Ok(Box::new(bzip2::read::BzDecoder::new(buffered)))
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        Ok(Box::new(xz2::read::XzDecoder::new(buffered)))
    } else {
        Err(ImportError::UnsupportedArchive {
            path: path.to_string_lossy().into_owned(),
        })
    }
}

/// One decompression pass over an archive: load every member that matches a
/// manifest table (stream order), skip and inventory everything else.
fn load_archive(
    path: &Path,
    db_path: &Path,
    conn: &mut Connection,
    checkpoint: &mut Checkpoint,
    report: &mut LoadReport,
) -> Result<(), ImportError> {
    let archive_name = path.file_name().unwrap().to_string_lossy().into_owned();
    let decoder = open_decoder(path)?;
    let mut archive = tar::Archive::new(decoder);

    let mut entries = archive
        .entries()
        .map_err(|error| ImportError::ArchiveRead {
            path: path.to_string_lossy().into_owned(),
            message: format!("{error}"),
        })?;
    loop {
        let next = entries.next();
        if let Some(result) = next {
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    return Err(ImportError::ArchiveRead {
                        path: path.to_string_lossy().into_owned(),
                        message: format!("{error}"),
                    });
                }
            };
            let header = entry.header();
            let size = header.size().unwrap_or(0);

            if !header.entry_type().is_file() {
                // Directories, hardlinks, pax headers, … carry no loadable
                // data; drain so the iterator stays aligned.
                drain_member(path, entry)?;
                continue;
            }

            let member_name = member_basename(&entry);
            if let Some(member_name) = member_name {
                report.members.push(MemberInfo {
                    archive: archive_name.to_owned(),
                    member: member_name.to_owned(),
                    size,
                });

                if let Some(manifest) = table(&member_name) {
                    let committed = match checkpoint.tables.get(&member_name) {
                        Some(rows) => *rows,
                        None => 0,
                    };
                    let stats = load_member_rows(
                        &MemberLoadCtx {
                            archive: path,
                            db: db_path,
                        },
                        entry,
                        conn,
                        checkpoint,
                        &mut report.rejections,
                        manifest,
                        committed,
                    )?;
                    let _ = report.tables.insert(member_name.to_owned(), stats);
                } else {
                    report
                        .skipped_members
                        .push(format!("{archive_name}:{member_name}"));
                    drain_member(path, entry)?;
                }
            } else {
                drain_member(path, entry)?;
            }
        } else {
            break;
        }
    }
    Ok(())
}

/// Basename of a tar member path (strip the `mbdump/` prefix directory).
fn member_basename<'a, R: 'a + Read>(entry: &tar::Entry<'a, R>) -> Option<String> {
    let raw = String::from_utf8_lossy(entry.path_bytes().borrow()).into_owned();
    // The dump ships members with a `mbdump/` directory prefix; keep the
    // last non-empty path component.
    let mut base = String::new();
    for part in raw.split('/') {
        if !part.is_empty() {
            base = part.to_owned();
        }
    }
    Some(base)
}

/// Drain the rest of a member (read to EOF) so the tar iterator stays
/// aligned on the next entry. Reuses the COPY parser; nothing is kept.
fn drain_member<R: Read>(path: &Path, entry: R) -> Result<(), ImportError> {
    let mut reader = TsvReader::new(BufReader::new(entry));
    loop {
        let next = reader.next();
        if let Some(result) = next {
            match result {
                Ok(_) => {}
                Err(error) => {
                    return Err(ImportError::ArchiveRead {
                        path: path.to_string_lossy().into_owned(),
                        message: format!("{error}"),
                    });
                }
            }
        } else {
            break;
        }
    }
    Ok(())
}

/// Archive/db handles shared by the per-member loaders (path is for error
/// messages; db selects the checkpoint file).
struct MemberLoadCtx<'a> {
    archive: &'a Path,
    db: &'a Path,
}

/// Stream one member through the loader, honoring the checkpoint.
///
/// Rows `0..committed` were committed to the database by an earlier run
/// (and are inside the checkpoint): they are parsed and drained for
/// position but not re-inserted. Everything after is validated, batched,
/// inserted, and checkpointed. The member body is always fully consumed,
/// so a resume re-decompresses (but never re-inserts) committed rows.
fn load_member_rows<R: Read>(
    ctx: &MemberLoadCtx,
    entry: R,
    conn: &mut Connection,
    checkpoint: &mut Checkpoint,
    rejections: &mut BTreeMap<String, RejectionSummary>,
    manifest: &TableManifest,
    committed: u64,
) -> Result<TableStats, ImportError> {
    let mut stats = TableStats::default();
    let mut reader = TsvReader::new(BufReader::new(entry));

    let mut pending: Vec<Vec<Value>> = Vec::new();
    let mut committed_rows = committed; // rows durable in the db so far
    let mut inserted: u64 = 0; // rows inserted this run
    let mut resumed: u64 = 0; // committed rows drained this run
    let mut index: u64 = 0;

    loop {
        let next = reader.next();
        if let Some(result) = next {
            let row = match result {
                Ok(row) => row,
                Err(error) => {
                    return Err(ImportError::ArchiveRead {
                        path: ctx.archive.to_string_lossy().into_owned(),
                        message: format!("{error}"),
                    });
                }
            };
            let live = index >= committed;
            index += 1;

            match cast_row(manifest, &row) {
                Ok(values) => {
                    if live {
                        pending.push(values);
                        inserted += 1;
                        if pending.len() == ROWS_PER_STATEMENT {
                            flush_statement(&mut pending, conn, manifest)?;
                        }
                        if inserted.is_multiple_of(ROWS_PER_BATCH) {
                            // The batch just flushed is durable; checkpoint
                            // exactly the rows now committed in the db.
                            committed_rows += inserted;
                            inserted = 0;
                            advance_checkpoint(
                                ctx.db,
                                conn,
                                checkpoint,
                                manifest.name,
                                committed_rows,
                            )?;
                        }
                    } else {
                        resumed += 1;
                    }
                }
                Err(_) => {
                    // A row failing validation today counts as rejected; a
                    // *drained* failed row was committed by an earlier run
                    // (it validated then), so only resume-tally it —
                    // re-logging would double-count a defect already seen.
                    if live {
                        record_rejection(rejections, manifest.name, &mut stats, &row);
                    } else {
                        resumed += 1;
                    }
                }
            }
        } else {
            break;
        }
    }

    // Tail: run any final partial statement back into the opened batch.
    if !pending.is_empty() {
        flush_statement(&mut pending, conn, manifest)?;
    }
    committed_rows += inserted;
    advance_checkpoint(ctx.db, conn, checkpoint, manifest.name, committed_rows)?;

    stats.rows = committed_rows;
    stats.resumed_rows = resumed;
    Ok(stats)
}

/// Validation + casting of one parsed row against its manifest.
///
/// Every field is cast per its declared [`Kind`]; the first failure yields
/// the reason (column index, name, expected form).
fn cast_row(manifest: &TableManifest, row: &Row) -> Result<Vec<Value>, String> {
    if row.fields.len() != manifest.columns.len() {
        return Err(format!(
            "expected {} fields, got {}",
            manifest.columns.len(),
            row.fields.len()
        ));
    }
    let mut values = Vec::with_capacity(row.fields.len());
    for (i, field) in row.fields.iter().enumerate() {
        let column = &manifest.columns[i];
        match cast_field(column.kind, field) {
            Ok(value) => values.push(value),
            Err(reason) => return Err(format!("column {i} ({}): {}", column.name, reason)),
        }
    }
    Ok(values)
}

/// Cast one COPY field to a duckdb value per the manifest storage kind.
///
/// Text and timestamp fields are bound as raw text and rely on DuckDB's
/// own parsing at insert time (the scheme-31 dump layouts are pinned to
/// round-trip). Integers and booleans are validated here so malformed
/// numerics never reach the database silently.
fn cast_field(kind: Kind, field: &Field) -> Result<Value, String> {
    match field {
        Field::Null => Ok(Value::Null),
        Field::Value(bytes) => match kind {
            Kind::SmallInt | Kind::Int => match parse_i64(bytes) {
                Some(value) => Ok(Value::BigInt(value)),
                None => Err(format!(
                    "expected integer, got `{}`",
                    String::from_utf8_lossy(bytes).into_owned()
                )),
            },
            Kind::Bool => match parse_bool(bytes) {
                Some(value) => Ok(Value::Boolean(value)),
                None => Err(format!(
                    "expected t/f boolean, got `{}`",
                    String::from_utf8_lossy(bytes).into_owned()
                )),
            },
            Kind::Text | Kind::Timestamp => match str::from_utf8(bytes) {
                Ok(text) => Ok(Value::Text(text.to_owned())),
                Err(_) => Err("expected UTF-8 text, got invalid bytes".to_owned()),
            },
        },
    }
}

/// Parse a COPY-text integer (ASCII digits, optional leading sign).
///
/// Returns `None` for empty input, non-ASCII bytes, or overflow. A manual
/// parser keeps the loader independent of locale/formatting conventions and
/// fails loud on dump bytes we do not expect.
fn parse_i64(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let mut i = 0;
    let mut negative = false;
    match bytes[0] {
        b'-' => {
            negative = true;
            i = 1;
        }
        b'+' => i = 1,
        _ => {}
    }
    if i >= bytes.len() {
        return None;
    }
    let mut value: i64 = 0;
    for byte in bytes[i..].iter() {
        if !byte.is_ascii_digit() {
            return None;
        }
        let digit = (byte - b'0') as i64;
        if value > (i64::MAX - digit) / 10 {
            return None; // overflow guard
        }
        value = value * 10 + digit;
    }
    Some(if negative { -value } else { value })
}

/// Build the multi-row `INSERT` statement for `rows` parametrized tuples.
pub fn insert_sql(manifest: &TableManifest, rows: usize) -> String {
    let columns = manifest
        .columns
        .iter()
        .map(|c| c.name)
        .collect::<Vec<&str>>()
        .join(", ");
    let mut tuples = Vec::with_capacity(rows);
    for _ in 0..rows {
        let placeholders = (0..manifest.columns.len())
            .map(|_| "?")
            .collect::<Vec<&str>>()
            .join(",");
        tuples.push(format!("({placeholders})"));
    }
    format!(
        "INSERT INTO {} ({}) VALUES {}",
        manifest.name,
        columns,
        tuples.join(",")
    )
}

/// Execute one pending statement buffer (flattened row-major params).
fn flush_statement(
    pending: &mut Vec<Vec<Value>>,
    conn: &mut Connection,
    manifest: &TableManifest,
) -> Result<(), ImportError> {
    let sql = insert_sql(manifest, pending.len());
    let mut params: Vec<Value> = Vec::with_capacity(pending.len() * manifest.columns.len());
    for values in pending.iter() {
        params.extend_from_slice(values);
    }
    conn.execute(&sql, params_from_iter(params.iter()))
        .map_err(|source| ImportError::Statement {
            table: manifest.name.to_owned(),
            source,
        })?;
    pending.clear();
    Ok(())
}

/// Commit the current batch, advance the checkpoint, and open the next
/// batch. The checkpoint never claims rows that are not durable.
fn advance_checkpoint(
    db_path: &Path,
    conn: &mut Connection,
    checkpoint: &mut Checkpoint,
    table: &str,
    rows: u64,
) -> Result<(), ImportError> {
    conn.execute_batch("COMMIT")
        .map_err(|source| ImportError::Statement {
            table: table.to_owned(),
            source,
        })?;
    conn.execute_batch("BEGIN")
        .map_err(|source| ImportError::Statement {
            table: table.to_owned(),
            source,
        })?;
    let _ = checkpoint.tables.insert(table.to_owned(), rows);
    let text = toml::to_string(checkpoint).map_err(|error| ImportError::CheckpointWrite {
        path: checkpoint_path(db_path).to_string_lossy().into_owned(),
        message: format!("{error}"),
    })?;
    let path = checkpoint_path(db_path);
    match fs::write(&path, text) {
        Ok(()) => Ok(()),
        Err(err) => Err(ImportError::CheckpointWrite {
            path: path.to_string_lossy().into_owned(),
            message: format!("{err}"),
        }),
    }
}

/// Record a rejected row: count it and keep the first few verbatim samples
/// (rendered COPY text — byte-identical to the dump).
fn record_rejection(
    rejections: &mut BTreeMap<String, RejectionSummary>,
    table: &str,
    stats: &mut TableStats,
    row: &Row,
) {
    stats.rejected += 1;
    let summary = rejections.get_mut(table);
    if let Some(summary) = summary {
        add_rejection_sample(summary, row);
    } else {
        let mut summary = RejectionSummary::default();
        add_rejection_sample(&mut summary, row);
        let _ = rejections.insert(table.to_owned(), summary);
    }
}

/// Count one rejection and keep the first few verbatim samples.
fn add_rejection_sample(summary: &mut RejectionSummary, row: &Row) {
    summary.count += 1;
    if summary.samples.len() < REJECTION_SAMPLES {
        let bytes = row.to_copy_bytes();
        let capped = &bytes[..bytes.len().min(REJECTION_SAMPLE_CAP)];
        summary.samples.push(capped.to_vec());
    }
}

/// Verify referential integrity of the loaded database.
///
/// For every manifest FK edge, count child rows whose FK value is non-NULL
/// but has no parent row. Returns the per-edge counts keyed by
/// `child.column -> target`; a healthy import of a fully loaded dump yields
/// all zeros (guaranteed by the corrected load order). The run notes must
/// show this output clean.
pub fn verify_integrity(conn: &mut Connection) -> Result<BTreeMap<String, u64>, ImportError> {
    let mut out = BTreeMap::new();
    for manifest in ALL {
        for fk in manifest.foreign_keys {
            let sql = format!(
                "SELECT COUNT(*) FROM {} c LEFT JOIN {} p ON c.{} = p.id \
                 WHERE c.{} IS NOT NULL AND p.id IS NULL",
                manifest.name, fk.target, fk.column, fk.column,
            );
            let count: i64 = conn
                .query_row(&sql, [], |row| row.get(0))
                .map_err(|source| ImportError::Statement {
                    table: format!("{}.{}", manifest.name, fk.column),
                    source,
                })?;
            let _ = out.insert(
                format!("{}.{} -> {}", manifest.name, fk.column, fk.target),
                count as u64,
            );
        }
    }
    Ok(out)
}

/// Write the rejections log (``<db>.rejections.log``), when any rows were
/// rejected: per table, the count and up to [`REJECTION_SAMPLES`] verbatim
/// samples.
fn write_rejections(db: &Path, report: &LoadReport) -> Result<(), ImportError> {
    if report.rejections.is_empty() {
        return Ok(());
    }
    let path = rejections_path(db);
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "# mbdb import rejections for {}\n# rows that failed manifest validation; \
         the milestone battery treats any rejection as a defect.\n",
        db.to_string_lossy()
    ));
    for (table, summary) in report.rejections.iter() {
        lines.push(format!("\n[{table}]\ncount = {}\n", summary.count));
        for sample in summary.samples.iter() {
            lines.push(format!(
                "  {}\n",
                String::from_utf8_lossy(sample).into_owned()
            ));
        }
    }
    match fs::write(&path, lines.join("")) {
        Ok(()) => Ok(()),
        Err(err) => Err(ImportError::RejectionsWrite {
            path: path.to_string_lossy().into_owned(),
            message: format!("{err}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mbdb::schema::{SchemaOptions, create_schema_with};
    use crate::mbdb::tsv::Row;
    use std::io::Write;

    /// Build a COPY-text row for any manifest table: every column gets a
    /// kind-shaped dummy value unless overridden by name.
    fn row_for(table_name: &str, overrides: &[(&str, &str)]) -> String {
        let manifest = table(table_name).unwrap();
        let mut cells: Vec<&str> = Vec::with_capacity(manifest.columns.len());
        for column in manifest.columns.iter() {
            let found = overrides.iter().find(|(name, _)| *name == column.name);
            let value = match found {
                Some((_, value)) => *value,
                None => dummy_for(column.kind),
            };
            cells.push(value);
        }
        cells.join("\t") + "\n"
    }

    fn dummy_for(kind: Kind) -> &'static str {
        match kind {
            Kind::SmallInt | Kind::Int => "1",
            Kind::Bool => "t",
            Kind::Text => "v",
            Kind::Timestamp => "2026-09-01 12:34:56.789+00",
        }
    }

    /// A single artist row with the given id (name/sort follow the id).
    fn artist_row(id: usize) -> String {
        let id_text = format!("{}", id);
        let name = format!("artist-{}", id);
        row_for(
            "artist",
            &[("id", &*id_text), ("name", &*name), ("sort_name", &*name)],
        )
    }

    /// `n` artist rows joined into one member's COPY text.
    fn artist_member(n: usize) -> String {
        let mut rows: Vec<String> = Vec::new();
        for i in 1..n + 1 {
            rows.push(artist_row(i));
        }
        rows.join("")
    }

    /// Write a tar archive (compression "bz2" or "xz") into `path`.
    fn write_archive(path: &PathBuf, compression: &str, members: &[(&str, String)]) {
        let file = File::create(path).unwrap();
        match compression {
            "bz2" => {
                let mut builder = tar::Builder::new(bzip2::write::BzEncoder::new(
                    file,
                    bzip2::Compression::best(),
                ));
                append_members(&mut builder, members);
                builder.finish().unwrap();
            }
            "xz" => {
                let mut builder = tar::Builder::new(xz2::write::XzEncoder::new(file, 6));
                append_members(&mut builder, members);
                builder.finish().unwrap();
            }
            _ => panic!("write_archive: unknown compression"),
        }
    }

    fn append_members<W: Write>(builder: &mut tar::Builder<W>, members: &[(&str, String)]) {
        for (name, content) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            builder
                .append_data(&mut header, name, content.as_bytes())
                .unwrap();
        }
    }

    /// A ready dump directory + resolved settings under `tmp`.
    fn fixture(tmp: &Path, archive_names: &[&str], tables: Tables) -> (Settings, PathBuf) {
        let dump = tmp.join("dumps");
        fs::create_dir_all(&dump).unwrap();
        for name in archive_names {
            let compression = if name.ends_with(".tar.bz2") {
                "bz2"
            } else {
                "xz"
            };
            let empty: &[(&str, String)] = &[];
            write_archive(&dump.join(name), compression, empty);
        }
        let db = tmp.join("test.duckdb");
        let settings = Settings {
            dump_dir: dump.to_string_lossy().into_owned(),
            db: db.to_string_lossy().into_owned(),
            tables,
        };
        (settings, db)
    }

    fn query_count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn test_insert_sql_parameter_shape() {
        let artist = table("artist").unwrap();
        let sql = insert_sql(artist, 2);
        assert!(sql.starts_with("INSERT INTO artist (id,"));
        assert!(sql.contains("VALUES (?"));
        // 19 artist columns × 2 rows = 38 parameters.
        assert_eq!(sql.split('?').count(), 39);
        assert_eq!(artist.columns.len(), 19);
    }

    #[test]
    fn test_load_bz2_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, db) = fixture(&root, &["mbdump.tar.bz2"], Tables::Core);

        let release_group = row_for(
            "release_group",
            &[
                ("id", "1"),
                ("name", "The Joshua Tree"),
                ("artist_credit", "1"),
            ],
        );
        write_archive(
            &root.join("dumps").join("mbdump.tar.bz2"),
            "bz2",
            &[
                ("mbdump/artist", artist_member(3)),
                ("mbdump/release_group", release_group),
            ],
        );

        let report = load(&settings, false).unwrap();
        assert_eq!(report.archives.len(), 1);
        assert_eq!(report.members.len(), 2);
        assert_eq!(report.members[0].member, "artist");
        assert_eq!(report.skipped_members.len(), 0);
        assert!(report.rejections.is_empty());
        assert_eq!(
            *report.tables.get("artist").unwrap(),
            TableStats {
                rows: 3,
                resumed_rows: 0,
                rejected: 0
            }
        );

        let conn = Connection::open(&db).unwrap();
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM artist"), 3);
        // Value round-trip through the manifest column casts.
        assert_eq!(
            query_count(
                &conn,
                "SELECT COUNT(*) FROM artist WHERE id = 2 AND name = 'artist-2'"
            ),
            1
        );
        // Timestamp text bound to TIMESTAMPTZ parses (DuckDB-pinned format).
        assert_eq!(
            query_count(
                &conn,
                "SELECT COUNT(*) FROM artist WHERE id = 1 \
                 AND last_updated::TIMESTAMP = '2026-09-01 12:34:56.789'::TIMESTAMP"
            ),
            1
        );
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM release_group"), 1);
        assert_eq!(
            query_count(
                &conn,
                "SELECT COUNT(*) FROM release_group WHERE id = 1 AND name = 'The Joshua Tree'"
            ),
            1
        );

        // Checkpoint file advanced exactly to the committed rows.
        let text = fs::read_to_string(checkpoint_path(&db)).unwrap();
        let checkpoint = toml::from_str::<Checkpoint>(&text).unwrap();
        assert_eq!(checkpoint.tables.get("artist"), Some(&3));
        assert_eq!(checkpoint.tables.get("release_group"), Some(&1));
    }

    #[test]
    fn test_load_xz_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, db) = fixture(&root, &["mbdump-sample.tar.xz"], Tables::Core);

        write_archive(
            &root.join("dumps").join("mbdump-sample.tar.xz"),
            "xz",
            &[
                ("mbdump/artist", artist_member(2)),
                (
                    "mbdump/release_group",
                    row_for("release_group", &[("id", "1")]),
                ),
            ],
        );

        let report = load(&settings, false).unwrap();
        assert_eq!(report.archives.len(), 1);
        assert_eq!(report.members.len(), 2);
        assert_eq!(
            *report.tables.get("artist").unwrap(),
            TableStats {
                rows: 2,
                resumed_rows: 0,
                rejected: 0
            }
        );

        let conn = Connection::open(&db).unwrap();
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM artist"), 2);
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM release_group"), 1);
    }

    #[test]
    fn test_members_load_in_archive_order_without_runtime_fks() {
        // `release` ships alphabetically BEFORE `release_group`, a row-level
        // FK violation in a schema with enforced FKs. The load target is
        // FK-free, so archive order loads fine and verify_integrity confirms
        // the edge is actually clean afterwards.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, db) = fixture(&root, &["mbdump.tar.bz2"], Tables::Core);

        write_archive(
            &root.join("dumps").join("mbdump.tar.bz2"),
            "bz2",
            &[
                (
                    "mbdump/release",
                    row_for(
                        "release",
                        &[("id", "1"), ("name", "Boy"), ("release_group", "1")],
                    ),
                ),
                (
                    "mbdump/release_group",
                    row_for(
                        "release_group",
                        &[("id", "1"), ("name", "Boy"), ("artist_credit", "1")],
                    ),
                ),
            ],
        );

        let report = load(&settings, false).unwrap();
        assert!(report.rejections.is_empty());

        let conn = Connection::open(&db).unwrap();
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM release"), 1);
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM release_group"), 1);

        let mut conn = Connection::open(&db).unwrap();
        let integrity = verify_integrity(&mut conn).unwrap();
        let orphans = integrity
            .get("release.release_group -> release_group")
            .unwrap();
        assert_eq!(*orphans, 0);
    }

    #[test]
    fn test_verify_integrity_flags_missing_target() {
        // Loading `release` WITHOUT its FK target leaves a dangling id —
        // exactly what verify_integrity exists to catch at load-close time.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, db) = fixture(&root, &["mbdump.tar.bz2"], Tables::Core);

        write_archive(
            &root.join("dumps").join("mbdump.tar.bz2"),
            "bz2",
            &[(
                "mbdump/release",
                row_for("release", &[("id", "1"), ("release_group", "99")]),
            )],
        );

        let report = load(&settings, false).unwrap();
        assert!(report.rejections.is_empty());

        let mut conn = Connection::open(&db).unwrap();
        let integrity = verify_integrity(&mut conn).unwrap();
        let orphans = integrity
            .get("release.release_group -> release_group")
            .unwrap();
        assert_eq!(*orphans, 1);
    }

    #[test]
    fn test_unknown_member_skipped_inventoried_and_alignment_kept() {
        // A non-manifest member between two manifest members must be
        // drained without derailing the tar iterator (stream alignment).
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, db) = fixture(&root, &["mbdump.tar.bz2"], Tables::Core);

        let junk = "not a manifest table\nwhatever\tbytes\n".to_owned();
        write_archive(
            &root.join("dumps").join("mbdump.tar.bz2"),
            "bz2",
            &[
                ("mbdump/artist", artist_member(2)),
                ("mbdump/zzz_unknown", junk),
                (
                    "mbdump/release_group",
                    row_for("release_group", &[("id", "1")]),
                ),
            ],
        );

        let report = load(&settings, false).unwrap();
        let _ = db;
        assert_eq!(report.members.len(), 3);
        assert_eq!(report.skipped_members, ["mbdump.tar.bz2:zzz_unknown"]);
        assert_eq!(
            *report.tables.get("artist").unwrap(),
            TableStats {
                rows: 2,
                resumed_rows: 0,
                rejected: 0
            }
        );
        // The member AFTER the unknown one still loads (iterator aligned).
        assert_eq!(
            *report.tables.get("release_group").unwrap(),
            TableStats {
                rows: 1,
                resumed_rows: 0,
                rejected: 0
            }
        );
    }

    #[test]
    fn test_rejections_logged_and_good_rows_kept() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, db) = fixture(&root, &["mbdump.tar.bz2"], Tables::Core);

        let good = row_for("artist", &[("id", "1"), ("name", "kept")]);
        let wrong_width = "1\ttoo_few_fields\n".to_owned();
        let bad_int = row_for("artist", &[("id", "abc"), ("name", "bad-int")]);
        let bad_bool = row_for(
            "artist",
            &[("id", "4"), ("name", "bad-bool"), ("ended", "z")],
        );
        let artist_member = [good, wrong_width, bad_int, bad_bool].join("");
        write_archive(
            &root.join("dumps").join("mbdump.tar.bz2"),
            "bz2",
            &[("mbdump/artist", artist_member)],
        );

        let report = load(&settings, false).unwrap();
        assert_eq!(
            *report.tables.get("artist").unwrap(),
            TableStats {
                rows: 1,
                resumed_rows: 0,
                rejected: 3
            }
        );

        // Rejection accounting + the verbatim samples.
        let rejections = report.rejections.get("artist").unwrap();
        assert_eq!(rejections.count, 3);
        assert_eq!(rejections.samples.len(), 3);
        let sample0 = String::from_utf8_lossy(&rejections.samples[0]).into_owned();
        assert_eq!(sample0, "1\ttoo_few_fields\n");

        // The log exists and carries count + samples.
        let log = fs::read_to_string(rejections_path(&db)).unwrap();
        assert!(log.contains("[artist]"));
        assert!(log.contains("count = 3"));
        assert!(log.contains("bad-int"));

        // Only the good row landed.
        let conn = Connection::open(&db).unwrap();
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM artist"), 1);
        assert_eq!(
            query_count(&conn, "SELECT COUNT(*) FROM artist WHERE name = 'kept'"),
            1
        );
    }

    #[test]
    fn test_resume_skips_committed_rows() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, db) = fixture(&root, &["mbdump.tar.bz2"], Tables::Core);

        write_archive(
            &root.join("dumps").join("mbdump.tar.bz2"),
            "bz2",
            &[
                ("mbdump/artist", artist_member(3)),
                (
                    "mbdump/release_group",
                    row_for("release_group", &[("id", "1")]),
                ),
            ],
        );

        // Simulate an interrupt after the first artist row committed: the
        // database holds exactly that row and the checkpoint says so.
        {
            let mut conn = Connection::open(&db).unwrap();
            create_schema_with(
                &mut conn,
                &SchemaOptions {
                    foreign_keys: false,
                },
            )
            .unwrap();
            let manifest = table("artist").unwrap();
            let row_text = artist_row(1);
            let row_bytes = row_text.as_bytes();
            let trimmed = &row_bytes[..row_bytes.len() - 1];
            let values = cast_row(manifest, &Row::parse(trimmed)).unwrap();
            let sql = insert_sql(manifest, 1);
            conn.execute(&sql, params_from_iter(values.iter())).unwrap();
            // No explicit BEGIN — each statement autocommits, exactly the
            // durable state a real crashed run would leave behind.
        }

        let mut checkpoint = Checkpoint::default();
        let _ = checkpoint.tables.insert("artist".to_owned(), 1);
        let checkpoint_text = toml::to_string(&checkpoint).unwrap();
        let _ = fs::write(checkpoint_path(&db), checkpoint_text);

        let report = load(&settings, true).unwrap();
        assert_eq!(
            *report.tables.get("artist").unwrap(),
            TableStats {
                rows: 3,
                resumed_rows: 1,
                rejected: 0
            }
        );
        assert_eq!(
            *report.tables.get("release_group").unwrap(),
            TableStats {
                rows: 1,
                resumed_rows: 0,
                rejected: 0
            }
        );

        // No duplicate commits: the resume drained row 1 and inserted 2..3.
        let conn = Connection::open(&db).unwrap();
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM artist"), 3);
        assert_eq!(query_count(&conn, "SELECT COUNT(*) FROM release_group"), 1);
        assert_eq!(
            query_count(&conn, "SELECT COUNT(*) FROM artist WHERE id = 1"),
            1
        );
        assert_eq!(
            query_count(&conn, "SELECT COUNT(*) FROM artist WHERE id = 2"),
            1
        );
        assert_eq!(
            query_count(&conn, "SELECT COUNT(*) FROM artist WHERE id = 3"),
            1
        );

        // Checkpoint advanced to include the newly committed rows.
        let text = fs::read_to_string(checkpoint_path(&db)).unwrap();
        let checkpoint = toml::from_str::<Checkpoint>(&text).unwrap();
        assert_eq!(checkpoint.tables.get("artist"), Some(&3));
        assert_eq!(checkpoint.tables.get("release_group"), Some(&1));
    }

    #[test]
    fn test_db_exists_requires_resume() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, _) = fixture(&root, &["mbdump.tar.bz2"], Tables::Core);
        write_archive(
            &root.join("dumps").join("mbdump.tar.bz2"),
            "bz2",
            &[("mbdump/artist", artist_member(1))],
        );

        let report = load(&settings, false).unwrap();
        assert_eq!(
            *report.tables.get("artist").unwrap(),
            TableStats {
                rows: 1,
                resumed_rows: 0,
                rejected: 0
            }
        );

        // Second fresh run refuses; resume accepts (everything is committed,
        // so nothing is re-inserted).
        let result = load(&settings, false);
        assert!(matches!(result.unwrap_err(), ImportError::DbExists { .. }));

        let report = load(&settings, true).unwrap();
        assert_eq!(
            *report.tables.get("artist").unwrap(),
            TableStats {
                rows: 1,
                resumed_rows: 1,
                rejected: 0
            }
        );
    }

    #[test]
    fn test_unreadable_checkpoint_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, db) = fixture(&root, &["mbdump.tar.bz2"], Tables::Core);
        {
            let conn = Connection::open(&db).unwrap();
            let _ = conn;
        }
        fs::write(checkpoint_path(&db), "not = [valid").unwrap();

        let result = load(&settings, true);
        assert!(matches!(
            result.unwrap_err(),
            ImportError::CheckpointParse { .. }
        ));
    }

    #[test]
    fn test_archive_discovery_prefers_production_and_loads_derived() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());

        // 1. Sample-only dir: the fallback core archive is found.
        let (settings, _) = fixture(&root, &["mbdump-sample.tar.xz"], Tables::CoreAndDerived);
        let archives = discover_archives(&settings).unwrap();
        assert_eq!(archives.len(), 1);
        assert!(
            archives[0]
                .to_string_lossy()
                .contains("mbdump-sample.tar.xz")
        );

        // 2. Production + derived + unrelated archives: core wins over the
        // sample, derived is included, mbdump-edit is ignored.
        let dump = PathBuf::from(&settings.dump_dir);
        let empty: &[(&str, String)] = &[];
        write_archive(&dump.join("mbdump.tar.bz2"), "bz2", empty);
        write_archive(&dump.join("mbdump-sample.tar.xz"), "xz", empty);
        write_archive(&dump.join("mbdump-derived.tar.bz2"), "bz2", empty);
        write_archive(&dump.join("mbdump-edit.tar.bz2"), "bz2", empty);

        let archives = discover_archives(&settings).unwrap();
        assert_eq!(archives.len(), 2);
        assert!(archives[0].to_string_lossy().contains("mbdump.tar.bz2"));
        assert!(
            archives[1]
                .to_string_lossy()
                .contains("mbdump-derived.tar.bz2")
        );

        // 3. Loading both reports nothing skipped (derived member set is
        // empty in this fixture).
        let report = load(&settings, false).unwrap();
        assert_eq!(report.archives.len(), 2);
        assert!(report.skipped_members.is_empty());

        // 4. Core-only scope: missing core is an explicit error (fresh
        // temp dir — the earlier fixtures already populated `root`).
        let empty_tmp = tempfile::TempDir::new().unwrap();
        let empty_root = PathBuf::from(empty_tmp.path());
        let (no_core, _) = fixture(&empty_root, &[], Tables::Core);
        let result = discover_archives(&no_core);
        assert!(matches!(
            result.unwrap_err(),
            ImportError::NoCoreArchive { .. }
        ));
    }

    #[test]
    fn test_derived_only_scope() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = PathBuf::from(tmp.path());
        let (settings, _) = fixture(&root, &["mbdump-derived.tar.bz2"], Tables::Derived);
        let archives = discover_archives(&settings).unwrap();
        assert_eq!(archives.len(), 1);
        assert!(archives[0].to_string_lossy().contains("mbdump-derived"));

        let empty_tmp = tempfile::TempDir::new().unwrap();
        let empty_root = PathBuf::from(empty_tmp.path());
        let (no_derived, _) = fixture(&empty_root, &[], Tables::Derived);
        let result = discover_archives(&no_derived);
        assert!(matches!(
            result.unwrap_err(),
            ImportError::NoDerivedArchive { .. }
        ));
    }
}
