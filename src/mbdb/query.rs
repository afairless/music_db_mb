//! Name-first query surface over the load-target DuckDB (M1).
//!
//! Deterministic evidence surface for tag_agent (plan §4). Every query
//! returns rows matched by an explicit tier order with stable id/gid
//! tiebreakers, surfaces aliases and disambiguation, and never filters
//! rows silently — duplicate names, disambiguation variants and the
//! long tail stay visible so the LLM (not the DB) decides.
//!
//! Match tiers (`query_artist`, plan §4 order):
//!
//! 0. exact `name`        `artist.name == query`
//! 1. exact `sort_name`   `artist.sort_name == query`
//! 2. exact alias         `artist_alias.name == query`
//! 3. substring           `name`/`sort_name` contains `query`
//! 4. substring alias     some `artist_alias.name` contains `query`
//!
//! All comparisons are byte-exact (case-sensitive), matching the dumped
//! text. The SQL gates use DuckDB `contains(haystack, needle)`, which is
//! empirically case-sensitive and byte-identical to Rust `str::contains`
//! (verified 2026-10-01 against DuckDB bundled build), so the row set
//! pulled by SQL and the rank assigned in Rust can never diverge.
//!
//! Output maps MusicBrainz names to the plan's contract: `artist.comment`
//! (the MusicBrainz disambiguation) is exposed as `disambiguation`, and
//! the row `gid` as `mbid`. `artist.type`/`artist_alias.type` FKs target
//! tables outside the imported subset (no `artist_type` rows shipped in
//! the pinned manifests), so type names are not surfaced in M1.

use duckdb::{Connection, params, params_from_iter};
use serde::Serialize;
use std::collections::HashSet;
use thiserror::Error;

/// Errors surfaced by the query layer.
#[derive(Debug, Error)]
pub enum QueryError {
    /// The query string is empty (or all whitespace) after trimming.
    #[error("query must not be empty")]
    EmptyQuery,
    /// A DuckDB statement failed.
    #[error("query failed: {0}")]
    Sql(#[from] duckdb::Error),
}

/// One alias exposed with a hit (plan §4: aliases included).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AliasHit {
    pub name: String,
    pub locale: Option<String>,
    pub primary_for_locale: bool,
}

/// A matched artist (contract shape, plan §4).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtistHit {
    pub kind: &'static str,
    pub name: String,
    /// The artist `gid`, exposed under the tag_agent-facing name.
    pub mbid: String,
    pub sort_name: String,
    /// `artist.comment` — the MusicBrainz disambiguation; empty when none.
    pub disambiguation: String,
    /// All of the artist's aliases, ordered by alias id (deterministic).
    pub aliases: Vec<AliasHit>,
}

impl ArtistHit {
    fn new(
        mbid: String,
        name: String,
        sort_name: String,
        disambiguation: String,
        aliases: Vec<AliasHit>,
    ) -> Self {
        Self {
            kind: "artist",
            name,
            mbid,
            sort_name,
            disambiguation,
            aliases,
        }
    }
}

/// Query options shared across all query kinds.
#[derive(Debug, Clone, Default)]
pub struct QueryOptions {
    /// Cap on the number of matches surfaced (applied after ranking;
    /// `None` surfaces everything).
    pub limit: Option<usize>,
}

/// One query's deterministic output (plan §4 contract shape).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QueryResult<T> {
    pub kind: &'static str,
    /// The query text as sent (after trimming).
    pub query: String,
    pub matches: Vec<T>,
}

/// Match tier, ordered per plan §4; lower wins.
const TIER_NAME_EXACT: u8 = 0;
const TIER_SORT_NAME_EXACT: u8 = 1;
const TIER_ALIAS_EXACT: u8 = 2;
const TIER_NAME_SUBSTRING: u8 = 3;
const TIER_ALIAS_SUBSTRING: u8 = 4;

/// Ranked candidate row (internal).
struct Candidate {
    id: i64,
    gid: String,
    name: String,
    sort_name: String,
    comment: String,
    rank: u8,
}

/// Query artists by name (plan §4.`query artist --name`).
///
/// Returns an exact-match-first, byte-deterministic list. See the module
/// docs for the tier contract and the case-sensitivity rules.
pub fn query_artist(
    conn: &Connection,
    name: &str,
    options: &QueryOptions,
) -> Result<QueryResult<ArtistHit>, QueryError> {
    let query = name.trim();
    if query.is_empty() {
        return Err(QueryError::EmptyQuery);
    }

    // Gate 1 — artist rows matching on name/sort_name (exact or
    // substring). The `contains` predicates are byte-identical to the
    // Rust ranking below, so the candidate set can never disagree with
    // the assigned tier.
    let mut candidate_ids: HashSet<i64> = HashSet::new();
    {
        let sql = "SELECT id FROM artist \
                   WHERE name = ?1 OR sort_name = ?1 \
                      OR contains(name, ?2) OR contains(sort_name, ?2)";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![query, query], |row| row.get::<_, i64>(0))?;
        for id in rows {
            candidate_ids.insert(id?);
        }
    }

    // Gate 2 — alias rows matching the query. These give both the
    // alias-exact ids (tier 2) and the alias-substring ids (tier 4).
    // The rows themselves are re-fetched per hit later, so only the id
    // sets are kept here.
    let mut alias_exact: HashSet<i64> = HashSet::new();
    let mut alias_substring: HashSet<i64> = HashSet::new();
    {
        let sql = "SELECT artist, name FROM artist_alias \
                   WHERE name = ?1 OR contains(name, ?2)";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![query, query], |row| {
            let artist: i64 = row.get(0)?;
            let alias: String = row.get(1)?;
            Ok((artist, alias))
        })?;
        for row in rows {
            let (artist, alias) = row?;
            if alias == query {
                alias_exact.insert(artist);
            }
            if alias.contains(query) {
                alias_substring.insert(artist);
            }
        }
    }

    candidate_ids.extend(alias_exact.iter().copied());
    candidate_ids.extend(alias_substring.iter().copied());
    if candidate_ids.is_empty() {
        return Ok(QueryResult {
            kind: "artist",
            query: query.to_owned(),
            matches: Vec::new(),
        });
    }

    // Fetch the full rows for every candidate id.
    let ids: Vec<i64> = candidate_ids.into_iter().collect();
    let mut rows = fetch_artist_rows(conn, &ids)?;
    for row in rows.iter_mut() {
        row.rank = if row.name == query {
            TIER_NAME_EXACT
        } else if row.sort_name == query {
            TIER_SORT_NAME_EXACT
        } else if alias_exact.contains(&row.id) {
            TIER_ALIAS_EXACT
        } else if row.name.contains(query) || row.sort_name.contains(query) {
            TIER_NAME_SUBSTRING
        } else {
            TIER_ALIAS_SUBSTRING
        };
    }

    // Deterministic order: tier, then name, then id (stable tiebreaker).
    rows.sort_by(|a, b| (a.rank, a.name.as_str(), a.id).cmp(&(b.rank, b.name.as_str(), b.id)));
    if let Some(limit) = options.limit {
        rows.truncate(limit);
    }

    // Aliases for the surfaced hits, ordered by alias id.
    let hit_ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let mut aliases: std::collections::BTreeMap<i64, Vec<AliasHit>> = Default::default();
    if !hit_ids.is_empty() {
        let sql = format!(
            "SELECT artist, name, locale, primary_for_locale \
             FROM artist_alias WHERE artist IN ({}) ORDER BY id",
            in_placeholders(hit_ids.len())
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(hit_ids.iter().map(|id| duckdb::types::Value::BigInt(*id))),
            |row| {
                let artist: i64 = row.get(0)?;
                let name: String = row.get(1)?;
                let locale: Option<String> = row.get(2)?;
                let primary_for_locale: bool = row.get(3)?;
                Ok((
                    artist,
                    AliasHit {
                        name,
                        locale,
                        primary_for_locale,
                    },
                ))
            },
        )?;
        for row in rows {
            let (artist, alias) = row?;
            aliases.entry(artist).or_default().push(alias);
        }
    }

    let matches = rows
        .into_iter()
        .map(|row| {
            ArtistHit::new(
                row.gid,
                row.name,
                row.sort_name,
                row.comment,
                aliases.remove(&row.id).unwrap_or_default(),
            )
        })
        .collect();

    Ok(QueryResult {
        kind: "artist",
        query: query.to_owned(),
        matches,
    })
}

/// Fetch artist rows for the given ids (order not guaranteed; ranking
/// re-sorts).
fn fetch_artist_rows(conn: &Connection, ids: &[i64]) -> Result<Vec<Candidate>, QueryError> {
    let sql = format!(
        "SELECT id, gid, name, sort_name, comment FROM artist \
         WHERE id IN ({})",
        in_placeholders(ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(ids.iter().map(|id| duckdb::types::Value::BigInt(*id))),
        |row| {
            Ok(Candidate {
                id: row.get(0)?,
                gid: row.get(1)?,
                name: row.get(2)?,
                sort_name: row.get(3)?,
                comment: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                rank: 0,
            })
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Build `?,?,?,…` for an `IN` clause.
fn in_placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// In-memory DuckDB mirroring the loader's target surface for the
    /// artist tables (only the columns the query layer reads).
    fn fixture_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE artist (
                 id INTEGER PRIMARY KEY,
                 gid VARCHAR NOT NULL,
                 name VARCHAR NOT NULL,
                 sort_name VARCHAR NOT NULL,
                 comment VARCHAR NOT NULL DEFAULT ''
             );
             CREATE TABLE artist_alias (
                 id INTEGER PRIMARY KEY,
                 artist INTEGER NOT NULL,
                 name VARCHAR NOT NULL,
                 locale VARCHAR,
                 primary_for_locale BOOLEAN NOT NULL DEFAULT FALSE
             );
             INSERT INTO artist (id, gid, name, sort_name, comment) VALUES
                (1, 'a3cb23fc-acd3-4ce0-8f36-1e5aa6a18432', 'U2', 'U2', 'Irish rock band'),
                (2, '0e4e5b5a-1c1c-4a4f-9a58-b1b8cfc8a9e3', 'U2', 'U2', ''),
                (3, '5b9b1d0f-8f7e-4f3e-9c9f-2b3d7f2b1e1e', 'P!nk', 'Pink', 'American singer'),
                (4, '22222222-2222-4222-8222-222222222222', 'The Beatles', 'The Beatles', 'The Fab Four'),
                (5, '33333333-3333-4333-8333-333333333333', 'Nine Inch Nails', 'Nine Inch Nails', '');
             INSERT INTO artist_alias (id, artist, name, locale, primary_for_locale) VALUES
                (1, 1, '유투', 'ko', TRUE),
                (2, 4, 'Beatles, The', NULL, TRUE),
                (3, 5, 'NIN', NULL, FALSE);",
        )
        .expect("seed fixture");
        conn
    }

    #[test]
    fn exact_name_first_with_duplicates_and_aliases() {
        let conn = fixture_conn();
        let result = query_artist(&conn, "U2", &QueryOptions::default()).unwrap();
        // Both U2 rows are exact-name matches (tier 0); the stable id
        // tiebreaker orders id 1 (Irish rock band) before id 2.
        assert_eq!(result.matches.len(), 2);
        let first = &result.matches[0];
        assert_eq!(first.name, "U2");
        assert_eq!(first.disambiguation, "Irish rock band");
        assert_eq!(first.sort_name, "U2");
        assert_eq!(first.mbid, "a3cb23fc-acd3-4ce0-8f36-1e5aa6a18432");
        assert_eq!(first.aliases.len(), 1);
        assert_eq!(first.aliases[0].name, "유투");
        assert_eq!(first.aliases[0].locale.as_deref(), Some("ko"));
        assert!(first.aliases[0].primary_for_locale);
        let second = &result.matches[1];
        assert_eq!(second.disambiguation, "");
        assert!(second.aliases.is_empty());
    }

    #[test]
    fn sort_name_exact_tier_precedes_substring() {
        let conn = fixture_conn();
        // "Pink" matches neither the name ("P!nk") nor sort_name
        // substring; the sort_name is an exact match (tier 1).
        let result = query_artist(&conn, "Pink", &QueryOptions::default()).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].name, "P!nk");
        assert_eq!(result.matches[0].sort_name, "Pink");
    }

    #[test]
    fn alias_exact_tier_surfaces_parent_artist() {
        let conn = fixture_conn();
        // "Beatles, The" is the alias, not the artist name (tier 2).
        let result = query_artist(&conn, "Beatles, The", &QueryOptions::default()).unwrap();
        assert_eq!(result.matches.len(), 1);
        let hit = &result.matches[0];
        assert_eq!(hit.name, "The Beatles");
        assert_eq!(hit.disambiguation, "The Fab Four");
        assert!(hit.aliases.iter().any(|a| a.name == "Beatles, The"));
    }

    #[test]
    fn substring_tier_matches_inside_name() {
        let conn = fixture_conn();
        let result = query_artist(&conn, "Beatles", &QueryOptions::default()).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].name, "The Beatles");
    }

    #[test]
    fn matching_is_case_sensitive() {
        let conn = fixture_conn();
        // "beatle" is not a case-sensitive substring of "The Beatles".
        let result = query_artist(&conn, "beatle", &QueryOptions::default()).unwrap();
        assert!(result.matches.is_empty());
    }

    #[test]
    fn alias_substring_only_tier() {
        let conn = fixture_conn();
        // "NI" appears in the alias "NIN" but nowhere in the artist's
        // name or sort_name ("Nine Inch Nails") — tier 4.
        let result = query_artist(&conn, "NI", &QueryOptions::default()).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].name, "Nine Inch Nails");
    }

    #[test]
    fn query_is_trimmed() {
        let conn = fixture_conn();
        let result = query_artist(&conn, "  U2  ", &QueryOptions::default()).unwrap();
        assert_eq!(result.query, "U2");
        assert_eq!(result.matches.len(), 2);
    }

    #[test]
    fn empty_query_is_an_error() {
        let conn = fixture_conn();
        assert!(matches!(
            query_artist(&conn, "", &QueryOptions::default()),
            Err(QueryError::EmptyQuery)
        ));
        assert!(matches!(
            query_artist(&conn, "   ", &QueryOptions::default()),
            Err(QueryError::EmptyQuery)
        ));
    }

    #[test]
    fn limit_applies_after_ranking() {
        let conn = fixture_conn();
        let result = query_artist(&conn, "U2", &QueryOptions { limit: Some(1) }).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].disambiguation, "Irish rock band");
    }

    #[test]
    fn spillover_more_than_100_rows_is_never_truncated() {
        let conn = fixture_conn();
        // One exact row plus 200 substring rows — the >100-row spillover
        // case the battery checks for truncation and exact-first order.
        let mut extra =
            String::from("INSERT INTO artist (id, gid, name, sort_name, comment) VALUES ");
        for i in 0..200 {
            extra.push_str(&format!(
                "({}, 'spillover-{i}', 'Test Artist {i:03}', 'Test Artist {i:03}', ''),",
                1000 + i
            ));
        }
        extra.push_str("(2000, 'spillover-exact', 'Test Artist', 'Test Artist', '');");
        conn.execute_batch(&extra).unwrap();

        let result = query_artist(&conn, "Test Artist", &QueryOptions::default()).unwrap();
        assert!(result.matches.len() > 100, "spillover must not truncate");
        assert_eq!(result.matches.len(), 201);
        // Exact-match-first: the plain name rises above every substring.
        assert_eq!(result.matches[0].name, "Test Artist");
        assert_eq!(result.matches[0].mbid, "spillover-exact");
        // Deterministic: (tier, name, id) — the tail is lexicographic.
        let names: Vec<&str> = result
            .matches
            .iter()
            .skip(1)
            .map(|hit| hit.name.as_str())
            .collect();
        assert_eq!(names[0], "Test Artist 000");
        assert_eq!(names[199], "Test Artist 199");
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn golden_json_contract_for_artist_query() {
        let conn = fixture_conn();
        let result = query_artist(&conn, "U2", &QueryOptions::default()).unwrap();
        let actual = serde_json::to_string(&result).unwrap();
        let expected = fs::read_to_string(golden_path("query_artist_u2.json")).unwrap();
        assert_eq!(format!("{actual}\n"), expected);
    }

    fn golden_path(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        root.join("tests").join("golden").join(name)
    }
}
