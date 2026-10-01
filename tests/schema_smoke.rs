//! Smoke tests: the schema DDL executes on a scratch DuckDB.
//!
//! Covers plan §7 milestone-1 acceptance for the schema step: the full DDL
//! runs on a scratch database, the created table list matches the §3 matrix,
//! creation is idempotent, and the corrected load order is load-bearing for
//! FK resolution (`track.recording` and friends).

use duckdb::Connection;

use mbdb::manifest::{ALL, table};
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
