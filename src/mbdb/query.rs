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

/// One entry in an artist credit (plan §4 contract).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtistCreditEntry {
    pub name: String,
    pub mbid: String,
    pub disambiguation: String,
}

/// A matched release group (contract shape, plan §4).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReleaseGroupHit {
    pub kind: &'static str,
    pub name: String,
    /// The release_group `gid`, exposed as `mbid`.
    pub mbid: String,
    /// Primary and secondary type names (e.g. `["Album", "Soundtrack"]`).
    pub types: Vec<String>,
    /// Earliest release date for any release in this group, formatted
    /// as `YYYY-MM-DD`. `None` when no date is known.
    pub first_release_date: Option<String>,
    /// The artist credit for this release group.
    pub artist_credit: Vec<ArtistCreditEntry>,
    /// All of the release group's aliases, ordered by alias id.
    pub aliases: Vec<AliasHit>,
}

/// Query release groups by name (plan §4.`query release-group --name`).
///
/// Optional filters: `--artist` (matches against artist credit name, same
/// exact-first semantics) and `--type` (matches primary type name exactly).
///
/// Match tiers:
///
/// 0. exact `name`          `release_group.name == query`
/// 1. exact alias           `release_group_alias.name == query`
/// 2. substring             `release_group.name contains query`
/// 3. substring alias       some `release_group_alias.name contains query`
pub fn query_release_group(
    conn: &Connection,
    name: &str,
    artist: Option<&str>,
    rg_type: Option<&str>,
    options: &QueryOptions,
) -> Result<QueryResult<ReleaseGroupHit>, QueryError> {
    let query = name.trim();
    if query.is_empty() {
        return Err(QueryError::EmptyQuery);
    }

    // Gate 1 — release_group rows matching on name (exact or substring).
    let mut candidate_ids: HashSet<i64> = HashSet::new();
    {
        let sql = "SELECT id FROM release_group \
                   WHERE name = ?1 OR contains(name, ?2)";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![query, query], |row| row.get::<_, i64>(0))?;
        for id in rows {
            candidate_ids.insert(id?);
        }
    }

    // Gate 2 — alias rows matching the query.
    let mut alias_exact: HashSet<i64> = HashSet::new();
    let mut alias_substring: HashSet<i64> = HashSet::new();
    {
        let sql = "SELECT release_group, name FROM release_group_alias \
                   WHERE name = ?1 OR contains(name, ?2)";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![query, query], |row| {
            let rg: i64 = row.get(0)?;
            let alias: String = row.get(1)?;
            Ok((rg, alias))
        })?;
        for row in rows {
            let (rg, alias) = row?;
            if alias == query {
                alias_exact.insert(rg);
            }
            if alias.contains(query) {
                alias_substring.insert(rg);
            }
        }
    }

    candidate_ids.extend(alias_exact.iter().copied());
    candidate_ids.extend(alias_substring.iter().copied());
    if candidate_ids.is_empty() {
        return Ok(QueryResult {
            kind: "release_group",
            query: query.to_owned(),
            matches: Vec::new(),
        });
    }

    // Filter by --artist (artist credit name contains the filter).
    if let Some(artist_filter) = artist {
        let trimmed = artist_filter.trim();
        if !trimmed.is_empty() {
            let ids = candidate_ids.into_iter().collect::<Vec<_>>();
            let mut filtered: HashSet<i64> = HashSet::new();
            // Use plain `?` throughout (DuckDB counts each `?`
            // positionally). The IN clause `?`s come first, then the
            // two name/contains params.
            let in_clause = in_placeholders(ids.len());
            let sql = format!(
                "SELECT DISTINCT rg.id \
                 FROM release_group rg \
                 JOIN artist_credit_name acn ON rg.artist_credit = acn.artist_credit \
                 WHERE rg.id IN ({}) \
                   AND (acn.name = ? OR contains(acn.name, ?))",
                in_clause
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_params: Vec<duckdb::types::Value> = Vec::new();
            for id in ids {
                all_params.push(duckdb::types::Value::BigInt(id));
            }
            all_params.push(duckdb::types::Value::Text(trimmed.to_string()));
            all_params.push(duckdb::types::Value::Text(trimmed.to_string()));
            let rows = stmt.query_map(params_from_iter(all_params.iter()), |row| {
                row.get::<_, i64>(0)
            })?;
            for id in rows {
                filtered.insert(id?);
            }
            candidate_ids = filtered;
            if candidate_ids.is_empty() {
                return Ok(QueryResult {
                    kind: "release_group",
                    query: query.to_owned(),
                    matches: Vec::new(),
                });
            }
        }
    }

    // Filter by --type (primary type name exact match).
    if let Some(type_filter) = rg_type {
        let trimmed = type_filter.trim();
        if !trimmed.is_empty() {
            let ids = candidate_ids.into_iter().collect::<Vec<_>>();
            let mut filtered: HashSet<i64> = HashSet::new();
            let in_clause = in_placeholders(ids.len());
            let sql = format!(
                "SELECT rg.id \
                 FROM release_group rg \
                 JOIN release_group_primary_type rgt ON rg.type = rgt.id \
                 WHERE rg.id IN ({}) AND rgt.name = ?",
                in_clause
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_params: Vec<duckdb::types::Value> = Vec::new();
            for id in ids {
                all_params.push(duckdb::types::Value::BigInt(id));
            }
            all_params.push(duckdb::types::Value::Text(trimmed.to_string()));
            let rows = stmt.query_map(params_from_iter(all_params.iter()), |row| {
                row.get::<_, i64>(0)
            })?;
            for id in rows {
                filtered.insert(id?);
            }
            candidate_ids = filtered;
            if candidate_ids.is_empty() {
                return Ok(QueryResult {
                    kind: "release_group",
                    query: query.to_owned(),
                    matches: Vec::new(),
                });
            }
        }
    }

    // Fetch the full rows for every candidate id.
    let ids: Vec<i64> = candidate_ids.into_iter().collect();
    let mut rows = fetch_release_group_rows(conn, &ids)?;
    for row in rows.iter_mut() {
        row.rank = if row.name == query {
            TIER_NAME_EXACT
        } else if alias_exact.contains(&row.id) {
            TIER_ALIAS_EXACT
        } else if row.name.contains(query) {
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

    // Batch-fetch the extra columns for the surfaced hits.
    let hit_ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let primary_types = fetch_primary_types(conn, &hit_ids)?;
    let secondary_types = fetch_secondary_types(conn, &hit_ids)?;
    let first_dates = fetch_first_release_dates(conn, &hit_ids)?;
    let credit = fetch_artist_credits(conn, &hit_ids)?;
    let aliases = fetch_release_group_aliases(conn, &hit_ids)?;

    let matches = rows
        .into_iter()
        .map(|row| {
            let mut types: Vec<String> = Vec::new();
            if let Some(v) = primary_types.get(&row.id) {
                types.extend(v.clone());
            }
            if let Some(v) = secondary_types.get(&row.id) {
                types.extend(v.clone());
            }
            let first_date: Option<String> = match first_dates.get(&row.id) {
                Some(v) => v.clone(),
                None => None,
            };
            let credit_vec: Vec<ArtistCreditEntry> = match credit.get(&row.id) {
                Some(v) => v.clone(),
                None => Vec::new(),
            };
            let alias_vec: Vec<AliasHit> = match aliases.get(&row.id) {
                Some(v) => v.clone(),
                None => Vec::new(),
            };
            ReleaseGroupHit {
                kind: "release_group",
                name: row.name.clone(),
                mbid: row.gid.clone(),
                types,
                first_release_date: first_date,
                artist_credit: credit_vec,
                aliases: alias_vec,
            }
        })
        .collect();

    Ok(QueryResult {
        kind: "release_group",
        query: query.to_owned(),
        matches,
    })
}

/// Ranked release-group candidate row (internal).
struct RgCandidate {
    id: i64,
    gid: String,
    name: String,
    rank: u8,
}

/// Fetch release-group rows for the given ids.
fn fetch_release_group_rows(
    conn: &Connection,
    ids: &[i64],
) -> Result<Vec<RgCandidate>, QueryError> {
    let sql = format!(
        "SELECT id, gid, name FROM release_group WHERE id IN ({})",
        in_placeholders(ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(ids.iter().map(|id| duckdb::types::Value::BigInt(*id))),
        |row| {
            Ok(RgCandidate {
                id: row.get(0)?,
                gid: row.get(1)?,
                name: row.get(2)?,
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

/// Map from release-group id to its primary-type name.
fn fetch_primary_types(
    conn: &Connection,
    rg_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Vec<String>>, QueryError> {
    let mut out: std::collections::BTreeMap<i64, Vec<String>> = Default::default();
    if rg_ids.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "SELECT rg.id, rgt.name \
         FROM release_group rg \
         JOIN release_group_primary_type rgt ON rg.type = rgt.id \
         WHERE rg.id IN ({})",
        in_placeholders(rg_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(rg_ids.iter().map(|id| duckdb::types::Value::BigInt(*id))),
        |row| {
            let rg_id: i64 = row.get(0)?;
            let type_name: String = row.get(1)?;
            Ok((rg_id, type_name))
        },
    )?;
    for row in rows {
        let (rg_id, type_name) = row?;
        out.entry(rg_id).or_default().push(type_name);
    }
    Ok(out)
}

/// Map from release-group id to its secondary-type names.
fn fetch_secondary_types(
    conn: &Connection,
    rg_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Vec<String>>, QueryError> {
    let mut out: std::collections::BTreeMap<i64, Vec<String>> = Default::default();
    if rg_ids.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "SELECT rgstj.release_group, rgst.name \
         FROM release_group_secondary_type_join rgstj \
         JOIN release_group_secondary_type rgst ON rgstj.secondary_type = rgst.id \
         WHERE rgstj.release_group IN ({}) \
         ORDER BY rgstj.release_group, rgstj.secondary_type",
        in_placeholders(rg_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(rg_ids.iter().map(|id| duckdb::types::Value::BigInt(*id))),
        |row| {
            let rg_id: i64 = row.get(0)?;
            let type_name: String = row.get(1)?;
            Ok((rg_id, type_name))
        },
    )?;
    for row in rows {
        let (rg_id, type_name) = row?;
        out.entry(rg_id).or_default().push(type_name);
    }
    Ok(out)
}

/// Map from release-group id to its first release date (earliest across
/// `release_country` and `release_unknown_country`).
fn fetch_first_release_dates(
    conn: &Connection,
    rg_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Option<String>>, QueryError> {
    let mut out: std::collections::BTreeMap<i64, Option<String>> = Default::default();
    if rg_ids.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "SELECT rg_id, MIN(date_str) AS first_date \
         FROM ( \
           SELECT r.release_group AS rg_id, \
               printf('%04d-%02d-%02d', rc.date_year, \
                   COALESCE(rc.date_month, 1), COALESCE(rc.date_day, 1)) AS date_str \
           FROM release r \
           JOIN release_country rc ON r.id = rc.release \
           WHERE r.release_group IN ({}) AND rc.date_year IS NOT NULL \
           UNION ALL \
           SELECT r.release_group AS rg_id, \
               printf('%04d-%02d-%02d', ruc.date_year, \
                   COALESCE(ruc.date_month, 1), COALESCE(ruc.date_day, 1)) AS date_str \
           FROM release r \
           JOIN release_unknown_country ruc ON r.id = ruc.release \
           WHERE r.release_group IN ({}) AND ruc.date_year IS NOT NULL \
         ) sub \
         GROUP BY rg_id",
        in_placeholders(rg_ids.len()),
        in_placeholders(rg_ids.len()),
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut all_params: Vec<duckdb::types::Value> = Vec::new();
    for id in rg_ids {
        all_params.push(duckdb::types::Value::BigInt(*id));
    }
    for id in rg_ids {
        all_params.push(duckdb::types::Value::BigInt(*id));
    }
    let rows = stmt.query_map(params_from_iter(all_params.iter()), |row| {
        let rg_id: i64 = row.get(0)?;
        let date_str: Option<String> = row.get(1)?;
        Ok((rg_id, date_str))
    })?;
    for row in rows {
        let (rg_id, date_str) = row?;
        // Only keep if not None (MIN() of all-NULL group returns NULL).
        if let Some(d) = date_str {
            out.insert(rg_id, Some(d));
        }
    }
    // Ensure every input id has a map entry (even when None).
    for id in rg_ids {
        if !out.contains_key(id) {
            out.insert(*id, None);
        }
    }
    Ok(out)
}

/// Map from release-group id to its artist credit (ordered by position).
fn fetch_artist_credits(
    conn: &Connection,
    rg_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Vec<ArtistCreditEntry>>, QueryError> {
    let mut out: std::collections::BTreeMap<i64, Vec<ArtistCreditEntry>> = Default::default();
    if rg_ids.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "SELECT rg.id AS rg_id, a.name AS artist_name, a.gid, a.comment \
         FROM release_group rg \
         JOIN artist_credit_name acn ON rg.artist_credit = acn.artist_credit \
         JOIN artist a ON acn.artist = a.id \
         WHERE rg.id IN ({}) \
         ORDER BY rg.id, acn.position",
        in_placeholders(rg_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(rg_ids.iter().map(|id| duckdb::types::Value::BigInt(*id))),
        |row| {
            let rg_id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            let gid: String = row.get(2)?;
            let comment: Option<String> = row.get::<_, Option<String>>(3)?;
            Ok((
                rg_id,
                ArtistCreditEntry {
                    name,
                    mbid: gid,
                    disambiguation: comment.unwrap_or_default(),
                },
            ))
        },
    )?;
    for row in rows {
        let (rg_id, entry) = row?;
        out.entry(rg_id).or_default().push(entry);
    }
    Ok(out)
}

/// Map from release-group id to its aliases (ordered by alias id).
fn fetch_release_group_aliases(
    conn: &Connection,
    rg_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Vec<AliasHit>>, QueryError> {
    let mut out: std::collections::BTreeMap<i64, Vec<AliasHit>> = Default::default();
    if rg_ids.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "SELECT release_group, name, locale, primary_for_locale \
         FROM release_group_alias \
         WHERE release_group IN ({}) \
         ORDER BY id",
        in_placeholders(rg_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(rg_ids.iter().map(|id| duckdb::types::Value::BigInt(*id))),
        |row| {
            let rg_id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            let locale: Option<String> = row.get(2)?;
            let primary_for_locale: bool = row.get(3)?;
            Ok((
                rg_id,
                AliasHit {
                    name,
                    locale,
                    primary_for_locale,
                },
            ))
        },
    )?;
    for row in rows {
        let (rg_id, alias) = row?;
        out.entry(rg_id).or_default().push(alias);
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

    // ------------------------------------------------------------------ //
    //  Release-group query tests                                         //
    // ------------------------------------------------------------------ //

    /// In-memory DuckDB mirroring the tables the release-group query
    /// needs: release_group, artist/artist_credit/artist_credit_name,
    /// primary/secondary types, aliases, and release/release_country.
    fn rg_fixture_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE artist (
                 id INTEGER PRIMARY KEY,
                 gid VARCHAR NOT NULL,
                 name VARCHAR NOT NULL,
                 sort_name VARCHAR NOT NULL,
                 comment VARCHAR NOT NULL DEFAULT ''
             );
             CREATE TABLE artist_credit (
                 id INTEGER PRIMARY KEY,
                 name VARCHAR NOT NULL,
                 artist_count SMALLINT NOT NULL,
                 gid VARCHAR NOT NULL
             );
             CREATE TABLE artist_credit_name (
                 artist_credit INTEGER NOT NULL,
                 position SMALLINT NOT NULL,
                 artist INTEGER NOT NULL,
                 name VARCHAR NOT NULL,
                 join_phrase VARCHAR NOT NULL DEFAULT ''
             );
             CREATE TABLE release_group_primary_type (
                 id INTEGER PRIMARY KEY,
                 name VARCHAR NOT NULL,
                 gid VARCHAR NOT NULL
             );
             CREATE TABLE release_group_secondary_type (
                 id INTEGER PRIMARY KEY,
                 name VARCHAR NOT NULL,
                 gid VARCHAR NOT NULL
             );
             CREATE TABLE release_group (
                 id INTEGER PRIMARY KEY,
                 gid VARCHAR NOT NULL,
                 name VARCHAR NOT NULL,
                 artist_credit INTEGER NOT NULL,
                 type INTEGER,
                 comment VARCHAR NOT NULL DEFAULT ''
             );
             CREATE TABLE release_group_alias (
                 id INTEGER PRIMARY KEY,
                 release_group INTEGER NOT NULL,
                 name VARCHAR NOT NULL,
                 locale VARCHAR,
                 primary_for_locale BOOLEAN NOT NULL DEFAULT FALSE
             );
             CREATE TABLE release_group_secondary_type_join (
                 release_group INTEGER NOT NULL,
                 secondary_type INTEGER NOT NULL
             );
             CREATE TABLE release (
                 id INTEGER PRIMARY KEY,
                 gid VARCHAR NOT NULL,
                 name VARCHAR NOT NULL,
                 release_group INTEGER NOT NULL,
                 artist_credit INTEGER NOT NULL
             );
             CREATE TABLE release_country (
                 release INTEGER NOT NULL,
                 country INTEGER NOT NULL,
                 date_year SMALLINT,
                 date_month SMALLINT,
                 date_day SMALLINT
             );
             CREATE TABLE release_unknown_country (
                 release INTEGER PRIMARY KEY,
                 date_year SMALLINT,
                 date_month SMALLINT,
                 date_day SMALLINT
             );
             INSERT INTO artist (id, gid, name, sort_name, comment) VALUES
                (1, 'a3cb23fc-acd3-4ce0-8f36-1e5aa6a18432', 'U2', 'U2', 'Irish rock band'),
                (2, 'art-avenue-q-gid', 'Various Artists', 'Various Artists', '');
             INSERT INTO artist_credit (id, name, artist_count, gid) VALUES
                (1, 'U2', 1, 'ac-u2-gid'),
                (2, 'Various Artists', 1, 'ac-va-gid');
             INSERT INTO artist_credit_name (artist_credit, position, artist, name, join_phrase) VALUES
                (1, 1, 1, 'U2', ''),
                (2, 1, 2, 'Various Artists', '');
             INSERT INTO release_group_primary_type (id, name, gid) VALUES
                (1, 'Album', 'pt-album-gid'),
                (2, 'Single', 'pt-single-gid'),
                (3, 'Soundtrack', 'pt-soundtrack-gid');
             INSERT INTO release_group_secondary_type (id, name, gid) VALUES
                (1, 'Cast', 'st-cast-gid'),
                (2, 'Compilation', 'st-comp-gid');
             INSERT INTO release_group (id, gid, name, artist_credit, type, comment) VALUES
                (1, '6f3e9fa6-acd3-4ce0-8f36-1e5aa6a18432', 'The Joshua Tree', 1, 1, ''),
                (2, 'rg-avenue-q-musical-gid', 'Avenue Q: The Musical', 2, 3, 'Cast recording'),
                (3, 'rg-another-rg-gid', 'Another Album', 1, 1, '');
             INSERT INTO release_group_alias (id, release_group, name, locale, primary_for_locale) VALUES
                (1, 1, '约书亚树', 'zh-cn', TRUE),
                (2, 2, 'Avenue Q (musical)', NULL, FALSE);
             INSERT INTO release_group_secondary_type_join (release_group, secondary_type) VALUES
                (2, 1);
             INSERT INTO release (id, gid, name, release_group, artist_credit) VALUES
                (1, 'rel-joshua-tree-gid', 'The Joshua Tree', 1, 1),
                (2, 'rel-avenue-q-gid', 'Avenue Q: The Musical (Original Cast)', 2, 2);
             INSERT INTO release_country (release, country, date_year, date_month, date_day) VALUES
                (1, 1, 1987, 3, 9),
                (2, 1, 2003, 10, 6);",
        )
        .expect("seed rg fixture");
        conn
    }

    #[test]
    fn rg_exact_name_first() {
        let conn = rg_fixture_conn();
        let result = query_release_group(
            &conn,
            "The Joshua Tree",
            None,
            None,
            &QueryOptions::default(),
        )
        .unwrap();
        assert_eq!(result.matches.len(), 1);
        let hit = &result.matches[0];
        assert_eq!(hit.name, "The Joshua Tree");
        assert_eq!(hit.mbid, "6f3e9fa6-acd3-4ce0-8f36-1e5aa6a18432");
        assert_eq!(hit.types, vec!["Album"]);
        assert_eq!(hit.first_release_date, Some(String::from("1987-03-09")));
        assert_eq!(hit.artist_credit.len(), 1);
        assert_eq!(hit.artist_credit[0].name, "U2");
        assert_eq!(hit.aliases.len(), 1);
        assert_eq!(hit.aliases[0].name, "约书亚树");
        assert_eq!(hit.aliases[0].locale.as_deref(), Some("zh-cn"));
    }

    #[test]
    fn rg_substring_matches_name() {
        let conn = rg_fixture_conn();
        let result =
            query_release_group(&conn, "Joshua", None, None, &QueryOptions::default()).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].name, "The Joshua Tree");
    }

    #[test]
    fn rg_alias_exact_surfaces_parent() {
        let conn = rg_fixture_conn();
        // "约书亚树" is the alias, not the RG name.
        let result =
            query_release_group(&conn, "约书亚树", None, None, &QueryOptions::default()).unwrap();
        assert_eq!(result.matches.len(), 1);
        let hit = &result.matches[0];
        assert_eq!(hit.name, "The Joshua Tree");
        assert!(hit.aliases.iter().any(|a| a.name == "约书亚树"));
    }

    #[test]
    fn rg_alias_substring_matches_parent() {
        let conn = rg_fixture_conn();
        // "musical" appears in the alias "Avenue Q (musical)" but not
        // in the RG name "Avenue Q: The Musical".
        let result =
            query_release_group(&conn, "musical", None, None, &QueryOptions::default()).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].name, "Avenue Q: The Musical");
    }

    #[test]
    fn rg_filter_by_artist() {
        let conn = rg_fixture_conn();
        // "The Joshua Tree" is by U2; "Another Album" is also by U2.
        let result =
            query_release_group(&conn, "The", None, None, &QueryOptions::default()).unwrap();
        assert_eq!(result.matches.len(), 2); // "The Joshua Tree" + substring hits

        // With --artist U2, only U2's RGs remain.
        let result =
            query_release_group(&conn, "The", Some("U2"), None, &QueryOptions::default()).unwrap();
        assert!(!result.matches.is_empty());
        for hit in result.matches {
            assert!(
                hit.artist_credit.iter().any(|ac| ac.name == "U2"),
                "all hits should have U2 in the artist credit"
            );
        }
    }

    #[test]
    fn rg_filter_by_type() {
        let conn = rg_fixture_conn();
        let result = query_release_group(
            &conn,
            "Avenue",
            None,
            Some("Soundtrack"),
            &QueryOptions::default(),
        )
        .unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].name, "Avenue Q: The Musical");
        assert!(result.matches[0].types.iter().any(|t| t == "Soundtrack"));
        assert!(result.matches[0].types.iter().any(|t| t == "Cast"));
    }

    #[test]
    fn rg_filter_by_type_no_match() {
        let conn = rg_fixture_conn();
        // "The Joshua Tree" is an Album, not a Single.
        let result =
            query_release_group(&conn, "The", None, Some("Single"), &QueryOptions::default())
                .unwrap();
        assert_eq!(result.matches.len(), 0);
    }

    #[test]
    fn rg_first_release_date() {
        let conn = rg_fixture_conn();
        let result = query_release_group(
            &conn,
            "Avenue Q: The Musical",
            None,
            None,
            &QueryOptions::default(),
        )
        .unwrap();
        assert_eq!(result.matches.len(), 1);
        let hit = &result.matches[0];
        assert_eq!(hit.first_release_date.as_deref(), Some("2003-10-06"),);
    }

    #[test]
    fn rg_no_match_returns_empty() {
        let conn = rg_fixture_conn();
        let result = query_release_group(
            &conn,
            "Nonexistent Album That Does Not Exist",
            None,
            None,
            &QueryOptions::default(),
        )
        .unwrap();
        assert_eq!(result.matches.len(), 0);
        assert_eq!(result.query, "Nonexistent Album That Does Not Exist");
    }

    #[test]
    fn rg_empty_query_is_an_error() {
        let conn = rg_fixture_conn();
        assert!(matches!(
            query_release_group(&conn, "", None, None, &QueryOptions::default()),
            Err(QueryError::EmptyQuery)
        ));
        assert!(matches!(
            query_release_group(&conn, "   ", None, None, &QueryOptions::default()),
            Err(QueryError::EmptyQuery)
        ));
    }

    #[test]
    fn rg_limit_applies_after_ranking() {
        let conn = rg_fixture_conn();
        let result =
            query_release_group(&conn, "A", None, None, &QueryOptions { limit: Some(1) }).unwrap();
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn rg_golden_json_contract() {
        let conn = rg_fixture_conn();
        let result = query_release_group(
            &conn,
            "The Joshua Tree",
            None,
            None,
            &QueryOptions::default(),
        )
        .unwrap();
        let actual = serde_json::to_string(&result).unwrap();
        let expected =
            fs::read_to_string(golden_path("query_release_group_joshua_tree.json")).unwrap();
        assert_eq!(format!("{actual}\n"), expected);
    }
}
