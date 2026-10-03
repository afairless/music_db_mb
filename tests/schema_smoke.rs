//! Smoke tests: the schema DDL executes on a scratch DuckDB.
//!
//! Covers plan §7 milestone-1 acceptance for the schema step: the full DDL
//! runs on a scratch database, the created table list matches the §3 matrix,
//! creation is idempotent, and the corrected load order is load-bearing for
//! FK resolution (`track.recording` and friends).

use duckdb::Connection;

use mbdb::manifest::{ALL, table};
use mbdb::query::{QueryOptions, query_artist, query_release_group};
use mbdb::schema::{create_schema, ddl};

/// Query helper: count rows in `information_schema.tables` matching a name.
fn table_count(conn: &Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = 'main' AND table_name = ?",
        [name],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn smoke_create_schema_creates_every_matrix_table() {
    let mut conn = Connection::open_in_memory().unwrap();
    create_schema(&mut conn).unwrap();

    // Every manifest table exists in the scratch database.
    for table in ALL {
        assert_eq!(table_count(&conn, table.name), 1, "{}", table.name);
    }
    // And nothing more than the matrix (50 tables).
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'main'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total, 50);
}

#[test]
fn smoke_create_schema_is_idempotent() {
    let mut conn = Connection::open_in_memory().unwrap();
    create_schema(&mut conn).unwrap();
    // Second pass must succeed too (IF NOT EXISTS guards every statement).
    create_schema(&mut conn).unwrap();
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'main'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total, 50);
}

#[test]
fn smoke_load_order_resolves_track_recording_fk() {
    // DuckDB validates FK targets at DDL time: `track` alone cannot be
    // created (its parents recording/medium/artist_credit do not exist yet)…
    let mut conn = Connection::open_in_memory().unwrap();
    let track = table("track").unwrap();
    let track_statement = ddl(track);
    let err = conn.execute_batch(&track_statement);
    assert!(err.is_err());

    // …whereas the corrected load order (`create_schema`) makes the FK
    // resolvable, proving the order is load-bearing rather than cosmetic.
    create_schema(&mut conn).unwrap();
    assert_eq!(table_count(&conn, "track"), 1);

    // And a full row satisfies the pinned checks and FK target (spot-check
    // the canonical case): recording exists, medium exists, artist_credit
    // exists, and a track row referencing them inserts cleanly.
    let _: i64 = conn
        .query_row(
            "INSERT INTO artist_credit (id, name, artist_count, gid) \
             VALUES (1, 'U2', 1, 'aaaaaaaa-1111-2222-3333-444444444444') \
             RETURNING id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let _: i64 = conn
        .query_row(
            "INSERT INTO recording (id, gid, name, artist_credit) \
             VALUES (1, 'bbbbbbbb-1111-2222-3333-444444444444', 'With or Without You', 1) \
             RETURNING id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let _: i64 = conn
        .query_row(
            "INSERT INTO release_group (id, gid, name, artist_credit) \
             VALUES (1, 'eeeeeeee-1111-2222-3333-444444444444', 'The Joshua Tree', 1) \
             RETURNING id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let _: i64 = conn
        .query_row(
            "INSERT INTO release (id, gid, name, artist_credit, release_group, quality) \
             VALUES (1, 'dddddddd-1111-2222-3333-444444444444', 'The Joshua Tree', 1, 1, -1) \
             RETURNING id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let _: i64 = conn
        .query_row(
            "INSERT INTO medium (id, release, position, name, track_count, gid) \
             VALUES (1, 1, 1, '', 11, 'cccccccc-1111-2222-3333-444444444444') \
             RETURNING id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let _: i64 = conn
        .query_row(
            "INSERT INTO track (id, gid, recording, medium, position, number, \
                                name, artist_credit) \
             VALUES (1, 'ffffffff-1111-2222-3333-444444444444', 1, 1, 1, '1', \
                     'With or Without You', 1) \
             RETURNING id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count(&conn, "track"), 1);
}

// ------------------------------------------------------------------ //
//  Query binding against the real schema                             //
// ------------------------------------------------------------------ //

/// Seed the real 50-table DDL with the rows the name-first query layer
/// reads. Column sets here are the pinned schema-31 shapes (no fake
/// columns, no skipped CHECK constraints), so a SQL statement that binds
/// to the wrong column set fails this test rather than in production.
fn seed_query_tables(conn: &mut Connection) {
    conn.execute_batch(
        "INSERT INTO artist (id, gid, name, sort_name, comment) VALUES
            (1, 'a3cb23fc-acd3-4ce0-8f36-1e5aa6a18432', 'U2', 'U2', 'Irish rock band'),
            (2, '22222222-2222-4222-8222-222222222222', 'The Beatles', 'The Beatles', 'The Fab Four');
         INSERT INTO artist_alias (id, artist, name, sort_name, locale, primary_for_locale) VALUES
            (1, 1, '유투', '유투', 'ko', TRUE),
            (2, 2, 'Beatles, The', 'Beatles, The', NULL, FALSE);
         INSERT INTO artist_credit (id, name, artist_count, gid) VALUES
            (1, 'U2', 1, 'ac-u2-gid');
         INSERT INTO artist_credit_name (artist_credit, position, artist, name, join_phrase) VALUES
            (1, 1, 1, 'U2', '');
         INSERT INTO release_group_primary_type (id, name, gid) VALUES
            (1, 'Album', 'pt-album-gid');
         INSERT INTO release_group_secondary_type (id, name, gid) VALUES
            (1, 'Cast', 'st-cast-gid');
         INSERT INTO release_group (id, gid, name, artist_credit, type) VALUES
            (1, '6f3e9fa6-acd3-4ce0-8f36-1e5aa6a18432', 'The Joshua Tree', 1, 1);
         INSERT INTO release_group_alias (id, release_group, name, sort_name, locale, primary_for_locale) VALUES
            (1, 1, '约书亚树', '约书亚树', 'zh-cn', TRUE);
         -- The shipped join table carries NO surrogate id: the secondary-
         -- type fetch must order by the natural key (regression guard for
         -- the old `rgstj.id` binder error on the live database).
         INSERT INTO release_group_secondary_type_join (release_group, secondary_type) VALUES
            (1, 1);
         INSERT INTO release_status (id, name, parent, child_order, description, gid) VALUES
            (1, 'Official', NULL, 0, NULL, 'rs-official-gid');
         INSERT INTO area (id, gid, name, type) VALUES
            (1, 'area-us-gid', 'United States', NULL);
         INSERT INTO release (id, gid, name, artist_credit, release_group, status) VALUES
            (1, 'rel-joshua-tree-gid', 'The Joshua Tree', 1, 1, 1);
         INSERT INTO release_country (release, country, date_year, date_month, date_day) VALUES
            (1, 1, 1987, 3, 9);"
    )
    .unwrap();
}

#[test]
fn smoke_queries_bind_to_real_schema() {
    let mut conn = Connection::open_in_memory().unwrap();
    create_schema(&mut conn).unwrap();
    seed_query_tables(&mut conn);

    // The artist query binds to the real column set (comment →
    // disambiguation, aliases via artist_alias).
    let artist = query_artist(&conn, "U2", &QueryOptions::default()).unwrap();
    assert_eq!(artist.matches.len(), 1);
    assert_eq!(artist.matches[0].disambiguation, "Irish rock band");
    assert_eq!(artist.matches[0].aliases.len(), 1);

    // The release-group query binds to the real column set — above all
    // the secondary-type join without an id column.
    let rg = query_release_group(
        &conn,
        "The Joshua Tree",
        None,
        None,
        &QueryOptions::default(),
    )
    .unwrap();
    assert_eq!(rg.matches.len(), 1);
    assert_eq!(rg.matches[0].mbid, "6f3e9fa6-acd3-4ce0-8f36-1e5aa6a18432");
    assert_eq!(rg.matches[0].types, vec!["Album", "Cast"]);
    assert_eq!(
        rg.matches[0].first_release_date.as_deref(),
        Some("1987-03-09")
    );
}
