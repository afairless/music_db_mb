//! mbdb — MusicBrainz-derived DuckDB builder CLI (milestone-1 import).
//!
//! Downloads are handled by `scripts/download_dumps.sh` (checksum-gated);
//! this binary turns the verified tarballs into the load-target DuckDB
//! database. Query subcommands arrive in later steps.

use clap::{Args, Parser, Subcommand};
use duckdb::Connection;
use std::path::PathBuf;

use mbdb::config::{CliOverrides, Config, DEFAULT_CONFIG_PATH, Settings};
use mbdb::import::{LoadReport, load, rejections_path, verify_integrity};
use mbdb::query::{QueryOptions, query_artist, query_release_group};

/// The mbdb command-line interface.
#[derive(Debug, Parser)]
#[command(name = "mbdb")]
#[command(about = "MusicBrainz-derived DuckDB builder (milestone-1 import)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Supported subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Import MusicBrainz dump tarballs into a DuckDB database.
    Import(ImportArgs),
    /// Query the MusicBrainz DuckDB database.
    Query(QueryArgs),
}

/// Arguments for `mbdb query`.
#[derive(Debug, Args)]
struct QueryArgs {
    /// Override the config file path (default: ./config.toml).
    #[arg(long, value_name = "PATH")]
    config: Option<String>,

    /// Override the database path (config: db).
    #[arg(long, value_name = "PATH")]
    db: Option<String>,

    #[command(subcommand)]
    kind: QueryKind,
}

/// Query kinds (one per evidence surface, plan §4).
#[derive(Debug, Subcommand)]
enum QueryKind {
    /// Query artists by name (exact first, then sort_name/alias, then
    /// substring) with aliases and disambiguation surfaced.
    Artist(ArtistArgs),
    /// Query release groups by name with types, first-release-date, and
    /// artist credit.
    ReleaseGroup(ReleaseGroupArgs),
}

/// Arguments for `mbdb query release-group`.
#[derive(Debug, Args)]
struct ReleaseGroupArgs {
    /// Release-group name to search for (case-sensitive).
    #[arg(long, value_name = "NAME")]
    name: String,

    /// Filter by artist credit name (case-sensitive substring).
    #[arg(long, value_name = "NAME")]
    artist: Option<String>,

    /// Filter by primary type name (exact, e.g. Album, Single, EP).
    #[arg(long, value_name = "TYPE")]
    rg_type: Option<String>,

    /// Cap the number of matches printed (default: all).
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
}

/// Arguments for `mbdb query artist`.
#[derive(Debug, Args)]
struct ArtistArgs {
    /// Artist name to search for (case-sensitive).
    #[arg(long, value_name = "NAME")]
    name: String,

    /// Cap the number of matches printed (default: all).
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
}

/// Arguments for `mbdb import`.
#[derive(Debug, Args)]
struct ImportArgs {
    /// Override the config file path (default: ./config.toml).
    #[arg(long, value_name = "PATH")]
    config: Option<String>,

    /// Override the dump directory (config: dump_dir).
    #[arg(long, value_name = "DIR")]
    dump_dir: Option<String>,

    /// Override the output database path (config: db).
    #[arg(long, value_name = "PATH")]
    db: Option<String>,

    /// Import scope: core | derived | core+derived (config: tables).
    #[arg(long, value_name = "SCOPE")]
    tables: Option<String>,

    /// Resume an interrupted import from its checkpoint.
    #[arg(long)]
    resume: bool,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Import(args) => run_import(&args),
        Command::Query(args) => run_query(&args),
    }
}

fn run_query(args: &QueryArgs) {
    let config_path = match args.config.as_ref() {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(DEFAULT_CONFIG_PATH),
    };
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    let overrides = CliOverrides {
        dump_dir: None,
        db: args.db.clone(),
        tables: None,
    };
    let settings = match Settings::resolve(config.as_ref(), &overrides) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };

    let conn = match Connection::open(PathBuf::from(&settings.db)) {
        Ok(conn) => conn,
        Err(error) => {
            eprintln!("error: could not open {}: {error}", settings.db);
            std::process::exit(1);
        }
    };
    match &args.kind {
        QueryKind::Artist(artist) => {
            let options = QueryOptions {
                limit: artist.limit,
            };
            let result = query_artist(&conn, &artist.name, &options);
            let result = match result {
                Ok(r) => r,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            };
            println!(
                "{}",
                serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_owned())
            );
        }
        QueryKind::ReleaseGroup(rg) => {
            let options = QueryOptions { limit: rg.limit };
            let result = query_release_group(
                &conn,
                &rg.name,
                rg.artist.as_deref(),
                rg.rg_type.as_deref(),
                &options,
            );
            let result = match result {
                Ok(r) => r,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            };
            println!(
                "{}",
                serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_owned())
            );
        }
    }
}

fn run_import(args: &ImportArgs) {
    let config_path = match args.config.as_ref() {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(DEFAULT_CONFIG_PATH),
    };
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    let overrides = CliOverrides {
        dump_dir: args.dump_dir.clone(),
        db: args.db.clone(),
        tables: args.tables.clone(),
    };
    let settings = match Settings::resolve(config.as_ref(), &overrides) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "import: dump-dir {} · db {}",
        settings.dump_dir, settings.db
    );
    let report = match load(&settings, args.resume) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    print_report(&settings, &report);
}

fn print_report(settings: &Settings, report: &LoadReport) {
    if report.archives.is_empty() {
        println!(
            "no archives imported (check --dump-dir {})",
            settings.dump_dir
        );
        return;
    }

    println!(
        "imported {} archive(s): {}",
        report.archives.len(),
        report.archives.join(", ")
    );

    let db_path = PathBuf::from(&settings.db);
    let mut orphan_total: u64 = 0;
    // Referential-integrity pass on the finished load target. The run
    // notes must show this clean (zero orphans per manifest FK edge).
    let mut conn = match Connection::open(&db_path) {
        Ok(conn) => conn,
        Err(error) => {
            eprintln!(
                "warning: could not reopen {} for integrity check ({})",
                settings.db, error
            );
            return;
        }
    };
    match verify_integrity(&mut conn) {
        Ok(edges) => {
            for (_, count) in edges.iter() {
                orphan_total += *count;
            }
        }
        Err(error) => eprintln!("warning: integrity check failed: {error}"),
    }

    for (table, stats) in report.tables.iter() {
        println!(
            "  loaded {table}: {} rows ({} resumed, {} rejected)",
            stats.rows, stats.resumed_rows, stats.rejected
        );
    }
    if !report.skipped_members.is_empty() {
        println!(
            "  skipped (no pinned manifest): {}",
            report.skipped_members.join(", ")
        );
    }
    if report.rejections.is_empty() {
        println!("rejections: none");
    } else {
        println!(
            "rejections: {} table(s) — see {}",
            report.rejections.len(),
            rejections_path(&db_path).to_string_lossy()
        );
        for (table, summary) in report.rejections.iter() {
            println!("  {table}: {} rejected row(s)", summary.count);
        }
    }
    if orphan_total == 0 {
        println!("referential integrity: CLEAN");
    } else {
        println!("referential integrity: {orphan_total} orphan(s) — inspect the run");
    }
}
