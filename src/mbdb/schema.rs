//! DuckDB DDL for the imported MusicBrainz subset.
//!
//! The DDL is **generated** from the pinned manifests in
//! [`manifest.rs`](crate::mbdb::manifest) so the DDL and the column lists the
//! loader inserts through can never drift apart — there is exactly one source
//! of truth for every table, column, constraint, and the corrected load
//! order.
//!
//! Notes on the DuckDB rendering:
//!
//! - Every statement is `CREATE TABLE IF NOT EXISTS` (idempotent — see the
//!   smoke test), with `SERIAL` rendered as `INTEGER` and `gid` UUID columns
//!   as `VARCHAR`, per the manifest's documented deviations.
//! - FKs are emitted as table-level `FOREIGN KEY (col) REFERENCES t(id)`.
//!   DuckDB validates FK targets at DDL time, which is exactly why the
//!   manifest's load order must (and is unit-tested to) put every target
//!   before its referencing table — `track.recording` being the canonical
//!   case from plan §7.
//! - CHECK constraints are re-expressed from the source schema; DuckDB
//!   enforces them on insert with Postgres-compatible NULL semantics, both
//!   verified against a scratch database during the schema step.

use duckdb::Connection;

use super::manifest::{ALL, Check, Kind, TableManifest};

/// Errors from executing the schema DDL.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// A `CREATE TABLE` statement was rejected by DuckDB.
    #[error("failed to create table `{table}`: {source}")]
    Execute {
        /// The table whose statement failed.
        table: String,
        /// The underlying DuckDB error.
        #[source]
        source: duckdb::Error,
    },
}

/// SQL for a manifest [`Check`], mirroring the source schema-31 constraints.
///
/// `isrc`/`iswc` regex CHECKs (`~ E'…'`, Postgres syntax) are not expressible
/// in DuckDB and are deliberately omitted from the manifests.
fn check_sql(check: Check) -> String {
    match check {
        Check::EditsPendingNonNeg => "edits_pending >= 0".to_owned(),
        Check::EndedConsistency => {
            "((end_date_year IS NOT NULL OR end_date_month IS NOT NULL OR ".to_owned()
                + "end_date_day IS NOT NULL) AND ended = TRUE) OR "
                + "(end_date_year IS NULL AND end_date_month IS NULL AND "
                + "end_date_day IS NULL)"
        }
        Check::AliasPrimaryLocality => {
            "(locale IS NULL AND primary_for_locale IS FALSE) OR ".to_owned()
                + "(locale IS NOT NULL)"
        }
        Check::AliasSearchHintsArtist => {
            "(type <> 3) OR (type = 3 AND sort_name = name AND ".to_owned()
                + "begin_date_year IS NULL AND begin_date_month IS NULL AND "
                + "begin_date_day IS NULL AND end_date_year IS NULL AND "
                + "end_date_month IS NULL AND end_date_day IS NULL AND "
                + "primary_for_locale IS FALSE AND locale IS NULL)"
        }
        Check::AliasSearchHintsWork => {
            "(type <> 2) OR (type = 2 AND sort_name = name AND ".to_owned()
                + "begin_date_year IS NULL AND begin_date_month IS NULL AND "
                + "begin_date_day IS NULL AND end_date_year IS NULL AND "
                + "end_date_month IS NULL AND end_date_day IS NULL AND "
                + "primary_for_locale IS FALSE AND locale IS NULL)"
        }
        Check::LengthPositive => "length IS NULL OR length > 0".to_owned(),
        Check::LinkOrderNonNeg => "link_order >= 0".to_owned(),
    }
}

/// The DuckDB type name for a manifest [`Kind`].
fn sql_kind(kind: Kind) -> &'static str {
    match kind {
        Kind::SmallInt => "SMALLINT",
        Kind::Int => "INTEGER",
        Kind::Bool => "BOOLEAN",
        Kind::Text => "VARCHAR",
        Kind::Timestamp => "TIMESTAMPTZ",
    }
}

/// Build the `CREATE TABLE IF NOT EXISTS` statement for one manifest table.
///
/// Returns the complete standalone statement; the caller applies the
/// statements in [`ALL`](crate::mbdb::manifest::ALL) order so every FK target
/// exists when its referencing table is created.
pub fn ddl(table: &TableManifest) -> String {
    let mut lines =
        Vec::with_capacity(table.columns.len() + table.foreign_keys.len() + table.checks.len() + 2);

    for col in table.columns {
        let name = col.name;
        let kind = sql_kind(col.kind);
        let not_null = if col.nullable { "" } else { " NOT NULL" };
        let default_clause = match col.default {
            Some(d) => " DEFAULT ".to_owned() + d,
            None => "".to_owned(),
        };
        lines.push(format!("    {name} {kind}{not_null}{default_clause}"));
    }

    if !table.primary_key.is_empty() {
        let mut cols = "".to_owned();
        for (i, col) in table.primary_key.iter().enumerate() {
            if i > 0 {
                cols += ", ";
            }
            cols += col;
        }
        lines.push(format!("    PRIMARY KEY ({cols})"));
    }

    for fk in table.foreign_keys {
        let column = fk.column;
        let target = fk.target;
        lines.push(format!(
            "    FOREIGN KEY ({column}) REFERENCES {target}(id)"
        ));
    }

    for check in table.checks {
        let expr = check_sql(*check);
        lines.push(format!("    CHECK ({expr})"));
    }

    let name = table.name;
    let body = lines.join(",\n");
    format!("CREATE TABLE IF NOT EXISTS {name} (\n{body}\n);")
}

/// Execute the full schema DDL against a connection, in corrected load order.
///
/// Idempotent — safe to call repeatedly (every statement is guarded with
/// `IF NOT EXISTS`).
pub fn create_schema(conn: &mut Connection) -> Result<(), SchemaError> {
    for table in ALL {
        let statement = ddl(table);
        conn.execute_batch(&statement)
            .map_err(|e| SchemaError::Execute {
                table: table.name.to_owned(),
                source: e,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mbdb::manifest;

    #[test]
    fn test_ddl_artist_statement_shape() {
        let statement = ddl(manifest::table("artist").unwrap());
        assert!(statement.starts_with("CREATE TABLE IF NOT EXISTS artist (\n"));
        assert!(statement.ends_with(");"));
        assert!(statement.contains("    id INTEGER NOT NULL"));
        assert!(statement.contains("    gid VARCHAR NOT NULL"));
        assert!(statement.contains("    comment VARCHAR NOT NULL DEFAULT ''"));
        assert!(statement.contains("    PRIMARY KEY (id)"));
        assert!(statement.contains("    CHECK (edits_pending >= 0)"));
        // ended-consistency CHECK spans lines
        assert!(statement.contains("ended = TRUE"));
    }

    #[test]
    fn test_ddl_statement_per_table() {
        // Every manifest table yields a non-trivial, self-contained statement.
        for table in ALL {
            let statement = ddl(table);
            assert!(statement.starts_with("CREATE TABLE IF NOT EXISTS "));
            assert!(statement.ends_with(");"));
            assert!(statement.contains(table.name));
        }
    }

    #[test]
    fn test_ddl_composite_primary_keys() {
        let statement = ddl(manifest::table("artist_credit_name").unwrap());
        assert!(statement.contains("PRIMARY KEY (artist_credit, position)"));
        let statement = ddl(manifest::table("release_country").unwrap());
        assert!(statement.contains("PRIMARY KEY (release, country)"));
    }

    #[test]
    fn test_ddl_foreign_keys_emitted() {
        let statement = ddl(manifest::table("track").unwrap());
        assert!(statement.contains("FOREIGN KEY (recording) REFERENCES recording(id)"));
        assert!(statement.contains("FOREIGN KEY (medium) REFERENCES medium(id)"));
        assert!(statement.contains("FOREIGN KEY (artist_credit) REFERENCES artist_credit(id)"));
        // gid-redirect provenance FK
        let statement = ddl(manifest::table("artist_gid_redirect").unwrap());
        assert!(statement.contains("FOREIGN KEY (new_id) REFERENCES artist(id)"));
    }
}
