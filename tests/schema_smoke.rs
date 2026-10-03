//! Smoke tests: the schema DDL executes on a scratch DuckDB.
//!
//! Covers plan §7 milestone-1 acceptance for the schema step: the full DDL
//! runs on a scratch database, the created table list matches the §3 matrix,
//! creation is idempotent, and the corrected load order is load-bearing for
//! FK resolution (`track.recording` and friends).

use duckdb::Connection;

use mbdb::manifest::{ALL, table};
use mbdb::query::{
    QueryOptions, query_artist, query_recording, query_release, query_release_group, query_work,
};
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
            (1, 1, 1987, 3, 9);
         INSERT INTO label (id, gid, name) VALUES
            (1, 'label-island-gid', 'Island');
         INSERT INTO release_label (id, release, label, catalog_number) VALUES
            (1, 1, 1, 'CID U2 6');
         INSERT INTO recording (id, gid, name, artist_credit, length) VALUES
            (1, 'rec-gid-1', 'Where the Streets Have No Name', 1, 337520);
         INSERT INTO medium (id, release, position, name, track_count, gid) VALUES
            (1, 1, 1, '', 1, 'med-gid-1');
         INSERT INTO track (id, gid, recording, medium, position, number, name,
                            artist_credit, length, is_data_track) VALUES
            (1, 'track-gid-1', 1, 1, 1, '1', 'Where the Streets Have No Name',
             1, 337973, FALSE);
         INSERT INTO recording_alias (id, recording, name, sort_name, locale,
                                      primary_for_locale) VALUES
            (1, 1, 'Streets', 'Streets', NULL, FALSE);
         INSERT INTO link_type (id, gid, entity_type0, entity_type1, name,
                                link_phrase, reverse_link_phrase, long_link_phrase) VALUES
            (1, 'lt-rec-rel-gid', 'recording', 'release', 'appears on',
             'appears on', 'appears on', 'appears on');
         INSERT INTO link (id, link_type) VALUES
            (1, 1);
         INSERT INTO l_recording_release (id, link, entity0, entity1) VALUES
            (1, 1, 1, 1);
         INSERT INTO work (id, gid, name, type) VALUES
            (1, 'work-gid-1', 'With or Without You', 17);
         INSERT INTO work_alias (id, work, name, sort_name, locale,
                                 primary_for_locale) VALUES
            (1, 1, 'WOWY', 'WOWY', NULL, FALSE);
         INSERT INTO iswc (id, work, iswc) VALUES
            (1, 1, 'T-011.336.805-0');
         INSERT INTO link_type (id, gid, entity_type0, entity_type1, name,
                                link_phrase, reverse_link_phrase, long_link_phrase) VALUES
            (2, 'lt-composer-gid', 'artist', 'work', 'composer',
             'composed', 'composed by', 'composed');
         INSERT INTO link (id, link_type) VALUES
            (2, 2);
         INSERT INTO l_artist_work (id, link, entity0, entity1) VALUES
            (1, 2, 1, 1);"
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

#[test]
fn smoke_release_query_binds_to_real_schema() {
    let mut conn = Connection::open_in_memory().unwrap();
    create_schema(&mut conn).unwrap();
    seed_query_tables(&mut conn);

    // The release query binds to the real column set: status join,
    // country dates, labels, and the per-medium tracklist joins
    // (release_label.label can be NULL, track.length is nullable, and
    // `release_unknown_country` carries no surrogate id).
    let result = query_release(
        &conn,
        "6f3e9fa6-acd3-4ce0-8f36-1e5aa6a18432",
        true,
        &QueryOptions::default(),
    )
    .unwrap();
    assert_eq!(result.matches.len(), 1);
    let hit = &result.matches[0];
    assert_eq!(hit.status.as_deref(), Some("Official"));
    assert_eq!(hit.date.as_deref(), Some("1987-03-09"));
    assert_eq!(hit.countries, vec!["United States"]);
    assert_eq!(hit.labels.len(), 1);
    assert_eq!(hit.labels[0].name.as_deref(), Some("Island"));
    let tracklists = hit
        .tracklists
        .as_ref()
        .expect("tracklists present with --inc tracks");
    assert_eq!(tracklists.len(), 1);
    let track = &tracklists[0].tracks[0];
    assert_eq!(track.name, "Where the Streets Have No Name");
    assert_eq!(track.duration_seconds.expect("length"), 337.973);
    assert_eq!(track.recording_mbid.as_deref(), Some("rec-gid-1"));
}

#[test]
fn smoke_recording_query_binds_to_real_schema() {
    let mut conn = Connection::open_in_memory().unwrap();
    create_schema(&mut conn).unwrap();
    seed_query_tables(&mut conn);

    // The recording query binds to the real column set: name gate,
    // alias join, artist-credit join, l_recording_release first-date
    // (via link/link_type join), and the nullable length.
    let result = query_recording(
        &conn,
        "Where the Streets Have No Name",
        None,
        &QueryOptions::default(),
    )
    .unwrap();
    assert_eq!(result.matches.len(), 1);
    let hit = &result.matches[0];
    assert_eq!(hit.mbid, "rec-gid-1");
    assert_eq!(hit.duration_seconds.expect("length"), 337.52);
    assert_eq!(hit.first_release_date.as_deref(), Some("1987-03-09"));
    assert_eq!(hit.artist_credit.len(), 1);
    assert_eq!(hit.artist_credit[0].name, "U2");
    assert_eq!(hit.aliases.len(), 1);
    assert_eq!(hit.aliases[0].name, "Streets");

    // The --artist filter binds too.
    let filtered = query_recording(&conn, "Where", Some("U2"), &QueryOptions::default()).unwrap();
    assert_eq!(filtered.matches.len(), 1);
    let none = query_recording(&conn, "Where", Some("Nobody"), &QueryOptions::default()).unwrap();
    assert!(none.matches.is_empty());
}

#[test]
fn smoke_work_query_binds_to_real_schema() {
    let mut conn = Connection::open_in_memory().unwrap();
    create_schema(&mut conn).unwrap();
    seed_query_tables(&mut conn);

    // The work query binds to the real column set: name gate, alias
    // join, iswc, the omitted work_type name table (raw type id
    // surfaces), and the composer link chain through
    // link_type/link/l_artist_work (entity0 = artist, entity1 = work).
    let result = query_work(&conn, "With or Without You", None, &QueryOptions::default()).unwrap();
    assert_eq!(result.matches.len(), 1);
    let hit = &result.matches[0];
    assert_eq!(hit.mbid, "work-gid-1");
    assert_eq!(hit.work_type.expect("work type"), 17);
    assert_eq!(hit.iswc, vec!["T-011.336.805-0"]);
    assert_eq!(hit.composers.len(), 1);
    assert_eq!(hit.composers[0].name, "U2");
    assert_eq!(hit.aliases.len(), 1);
    assert_eq!(hit.aliases[0].name, "WOWY");

    // The --composer filter binds (link_type name = 'composer').
    let filtered = query_work(
        &conn,
        "With or Without You",
        Some("U2"),
        &QueryOptions::default(),
    )
    .unwrap();
    assert_eq!(filtered.matches.len(), 1);
    let none = query_work(
        &conn,
        "With or Without You",
        Some("Nobody"),
        &QueryOptions::default(),
    )
    .unwrap();
    assert!(none.matches.is_empty());
}
