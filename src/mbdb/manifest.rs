//! Pinned per-table column manifests for the imported MusicBrainz subset.
//!
//! Each entry mirrors one source table from the MusicBrainz schema at
//! SCHEMA_SEQUENCE 31 (the layout verified against the 2026-09-01 sample
//! dump). The source of truth for this file is the official
//! `admin/sql/CreateTables.sql` of the 2026-09-01-era musicbrainz-server
//! (the only later schema change, `old_editor_name` in sequence 32, does not
//! touch any imported table).
//!
//! Dump TSV members ship **without a header row**; the manifests below are
//! the authoritative column lists (names, order, constraints) in the exact
//! schema-31 layout. The loader inserts through these lists
//! (`INSERT INTO t (col1, …, colN)`) so similar-typed columns can never be
//! silently swapped, and the import inventory step checks every member's
//! column count against [`Column`] counts in [`ALL`].
//!
//! Deviations from the source DDL are deliberate and documented inline:
//!
//! - `gid` UUID columns are kept as `VARCHAR` — plan §3: "keep MB's own
//!   identifiers in text columns"; byte-identical round-trip, no parse risk.
//! - `SERIAL` id columns become plain `INTEGER` — the dump TSV carries the
//!   explicit ids, so nothing auto-increments.
//! - `artist.area`, `artist.begin_area`, `artist.end_area` FKs are **not**
//!   emitted: `area` is created later in the corrected load order (artists
//!   load first, area joins at the label/area step) and DuckDB validates FK
//!   targets at DDL time. The join is by integer id at query time — same
//!   rows, no referential constraint.
//! - `release_label.label` FK is **not** emitted for the same reason: `label`
//!   ships in the label/area group, after the release family that owns
//!   `release_label`. The join is by integer id at query time. (`label.area`
//!   **is** kept — `area` is created immediately before `label` in `ALL`.)
//! - FKs whose target is outside the imported subset (e.g. `artist.type` →
//!   `artist_type`, `release.script` → `script`, `release_country.country` →
//!   `country_area`) are omitted for the same DDL-time reason.
//! - The source regex CHECKs on `isrc.isrc` / `iswc.iswc` use Postgres `~`
//!   syntax DuckDB does not support; they are omitted. All other source
//!   CHECKs are re-expressed in [`Check`].
//!
//! The table order of [`ALL`] **is** the corrected load order: FK targets
//! always precede the tables that reference them (e.g. `medium` before
//! `track`, `recording` before `track.recording`).

/// Logical storage kind of a manifest column.
///
/// Drives both the emitted DuckDB DDL (see [`schema.rs`](crate::mbdb::schema))
/// and, in the loader, the text→value casting (e.g. `t`/`f` booleans).
#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub enum Kind {
    /// Source `SMALLINT` — date parts, counts, cardinalities.
    SmallInt,
    /// Source `INTEGER` / `SERIAL` — ids, lengths (ms), counters.
    Int,
    /// Source `BOOLEAN` — dump text `t`/`f`.
    Bool,
    /// Source `VARCHAR` / `CHAR` / `TEXT` — and all `gid` UUID columns,
    /// stored as text per plan §3.
    Text,
    /// Source `TIMESTAMP WITH TIME ZONE` — PG COPY text form
    /// (`2026-09-01 12:34:56.789+00`) verified to round-trip.
    Timestamp,
}

/// One pinned column: name, storage kind, nullability, and default.
#[derive(Debug)]
pub struct Column<'a> {
    /// Column name exactly as shipped in the schema-31 TSV.
    pub name: &'a str,
    /// Storage kind (target DDL type + loader casting).
    pub kind: Kind,
    /// `false` for source `NOT NULL` columns.
    pub nullable: bool,
    /// SQL default to emit in the DDL, when the source column has one.
    pub default: Option<&'a str>,
}

/// A foreign key from a manifest column to another imported table's `id`.
#[derive(Debug)]
pub struct ForeignKey<'a> {
    /// The referencing column.
    pub column: &'a str,
    /// The referenced table (always its `id` column).
    pub target: &'a str,
}

/// Source CHECK constraints re-expressed for DuckDB.
///
/// DuckDB enforces CHECKs on insert (verified) with Postgres-compatible
/// NULL semantics (a NULL check expression passes), so the shipped source
/// constraints hold unmodified for valid dump rows.
#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub enum Check {
    /// `edits_pending >= 0` — shipped on every edits-pending column.
    EditsPendingNonNeg,
    /// The end-date/`ended` consistency check on dated entities.
    EndedConsistency,
    /// `(locale IS NULL AND primary_for_locale IS FALSE) OR (locale IS NOT NULL)`.
    AliasPrimaryLocality,
    /// `artist_alias.search_hints_are_empty` (type 3 = search hint).
    AliasSearchHintsArtist,
    /// `work_alias.search_hints_are_empty` (type 2 = search hint).
    AliasSearchHintsWork,
    /// `length IS NULL OR length > 0` — `recording` and `track`.
    LengthPositive,
    /// `link_order >= 0` — every `l_*` link junction.
    LinkOrderNonNeg,
}

/// One imported source table: constraints and column list in ship order.
#[derive(Debug)]
pub struct TableManifest<'a> {
    /// Table name — the target DuckDB table (== source member name).
    pub name: &'a str,
    /// Columns in exact schema-31 order (no TSV header involvement).
    pub columns: &'a [Column<'a>],
    /// Primary-key column(s) as in the source schema.
    pub primary_key: &'a [&'a str],
    /// FKs to other imported tables. Every target must appear **before**
    /// this table in [`ALL`] (load order), so DuckDB's DDL-time FK target
    /// validation always succeeds.
    pub foreign_keys: &'a [ForeignKey<'a>],
    /// Source CHECK constraints emitted into the DuckDB DDL.
    pub checks: &'a [Check],
}

/// All imported tables in corrected load order.
///
/// Order contract (plan §3/§7): artists → credits/aliases → release-group
/// family → releases → recording → medium → track → works → label/area →
/// link types/link/url → link junctions → gid redirects — with FK targets
/// strictly preceding referencing tables, so `track.recording` (among all
/// others) resolves at DDL time.
pub const ALL: &[TableManifest] = &[
    // --- artists ---
    TableManifest {
        name: "artist",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "sort_name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "begin_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            // type/gender FKs target tables outside the subset — omitted.
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            // area/begin_area/end_area FKs omitted — see module docs.
            Column {
                name: "area",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "gender",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "comment",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "ended",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
            Column {
                name: "begin_area",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_area",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[],
        checks: &[Check::EditsPendingNonNeg, Check::EndedConsistency],
    },
    TableManifest {
        name: "artist_credit",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "artist_count",
                kind: Kind::SmallInt,
                nullable: false,
                default: None,
            },
            Column {
                name: "ref_count",
                kind: Kind::Int,
                nullable: true,
                default: Some("0"),
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[],
        checks: &[Check::EditsPendingNonNeg],
    },
    TableManifest {
        name: "artist_credit_name",
        columns: &[
            Column {
                name: "artist_credit",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "position",
                kind: Kind::SmallInt,
                nullable: false,
                default: None,
            },
            Column {
                name: "artist",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "join_phrase",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["artist_credit", "position"],
        foreign_keys: &[
            ForeignKey {
                column: "artist_credit",
                target: "artist_credit",
            },
            ForeignKey {
                column: "artist",
                target: "artist",
            },
        ],
        checks: &[],
    },
    TableManifest {
        name: "artist_alias",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "artist",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "locale",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            // type → artist_alias_type, outside the subset — omitted.
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "sort_name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "begin_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "primary_for_locale",
                kind: Kind::Bool,
                nullable: false,
                default: Some("false"),
            },
            Column {
                name: "ended",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "artist",
            target: "artist",
        }],
        checks: &[
            Check::EditsPendingNonNeg,
            Check::EndedConsistency,
            Check::AliasPrimaryLocality,
            Check::AliasSearchHintsArtist,
        ],
    },
    // --- release-group family ---
    TableManifest {
        name: "release_group_primary_type",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "parent",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "child_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "description",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "parent",
            target: "release_group_primary_type",
        }],
        checks: &[],
    },
    TableManifest {
        name: "release_group_secondary_type",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "parent",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "child_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "description",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "parent",
            target: "release_group_secondary_type",
        }],
        checks: &[],
    },
    TableManifest {
        name: "release_group",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "artist_credit",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "comment",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "artist_credit",
                target: "artist_credit",
            },
            ForeignKey {
                column: "type",
                target: "release_group_primary_type",
            },
        ],
        checks: &[Check::EditsPendingNonNeg],
    },
    TableManifest {
        name: "release_group_alias",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "release_group",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "locale",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "sort_name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "begin_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "primary_for_locale",
                kind: Kind::Bool,
                nullable: false,
                default: Some("false"),
            },
            Column {
                name: "ended",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "release_group",
            target: "release_group",
        }],
        checks: &[
            Check::EditsPendingNonNeg,
            Check::EndedConsistency,
            Check::AliasPrimaryLocality,
        ],
    },
    TableManifest {
        name: "release_group_secondary_type_join",
        columns: &[
            Column {
                name: "release_group",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "secondary_type",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: false,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["release_group", "secondary_type"],
        foreign_keys: &[
            ForeignKey {
                column: "release_group",
                target: "release_group",
            },
            ForeignKey {
                column: "secondary_type",
                target: "release_group_secondary_type",
            },
        ],
        checks: &[],
    },
    // --- releases ---
    TableManifest {
        name: "release_status",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "parent",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "child_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "description",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "parent",
            target: "release_status",
        }],
        checks: &[],
    },
    TableManifest {
        name: "release_packaging",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "parent",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "child_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "description",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "parent",
            target: "release_packaging",
        }],
        checks: &[],
    },
    TableManifest {
        name: "release",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "artist_credit",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "release_group",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "status",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "packaging",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            // language/script FKs target tables outside the subset — omitted.
            Column {
                name: "language",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "script",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "barcode",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "comment",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "quality",
                kind: Kind::SmallInt,
                nullable: false,
                default: Some("-1"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "artist_credit",
                target: "artist_credit",
            },
            ForeignKey {
                column: "release_group",
                target: "release_group",
            },
            ForeignKey {
                column: "status",
                target: "release_status",
            },
            ForeignKey {
                column: "packaging",
                target: "release_packaging",
            },
        ],
        checks: &[Check::EditsPendingNonNeg],
    },
    TableManifest {
        name: "release_country",
        columns: &[
            Column {
                name: "release",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            // country → country_area (a view over area) — outside the subset.
            Column {
                name: "country",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
        ],
        primary_key: &["release", "country"],
        foreign_keys: &[ForeignKey {
            column: "release",
            target: "release",
        }],
        checks: &[],
    },
    TableManifest {
        name: "release_unknown_country",
        columns: &[
            Column {
                name: "release",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
        ],
        primary_key: &["release"],
        foreign_keys: &[ForeignKey {
            column: "release",
            target: "release",
        }],
        checks: &[],
    },
    TableManifest {
        name: "release_label",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "release",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            // label → label, created later in the corrected load order
            // (label/area group ships after releases) — FK omitted for the
            // same DDL-time reason as artist.area; joined by integer id at
            // query time.
            Column {
                name: "label",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "catalog_number",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "release",
            target: "release",
        }],
        checks: &[],
    },
    // --- recordings ---
    TableManifest {
        name: "recording",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "artist_credit",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "length",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "comment",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "video",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "artist_credit",
            target: "artist_credit",
        }],
        checks: &[Check::EditsPendingNonNeg, Check::LengthPositive],
    },
    TableManifest {
        name: "recording_alias",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "recording",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "locale",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "sort_name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "begin_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "primary_for_locale",
                kind: Kind::Bool,
                nullable: false,
                default: Some("false"),
            },
            Column {
                name: "ended",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "recording",
            target: "recording",
        }],
        checks: &[
            Check::EditsPendingNonNeg,
            Check::EndedConsistency,
            Check::AliasPrimaryLocality,
        ],
    },
    TableManifest {
        name: "isrc",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "recording",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "isrc",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "recording",
            target: "recording",
        }],
        checks: &[Check::EditsPendingNonNeg],
    },
    // --- mediums and tracks ---
    TableManifest {
        name: "medium",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "release",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "position",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            // format → medium_format, outside the subset — omitted.
            Column {
                name: "format",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "track_count",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "release",
            target: "release",
        }],
        checks: &[Check::EditsPendingNonNeg],
    },
    TableManifest {
        name: "track",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            // The FK this load order exists to resolve: recording first.
            Column {
                name: "recording",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "medium",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "position",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "number",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "artist_credit",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "length",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "is_data_track",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "recording",
                target: "recording",
            },
            ForeignKey {
                column: "medium",
                target: "medium",
            },
            ForeignKey {
                column: "artist_credit",
                target: "artist_credit",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LengthPositive],
    },
    // --- works ---
    TableManifest {
        name: "work",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            // type → work_type, outside the subset — omitted.
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "comment",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[],
        checks: &[Check::EditsPendingNonNeg],
    },
    TableManifest {
        name: "work_alias",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "work",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "locale",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "sort_name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "begin_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "primary_for_locale",
                kind: Kind::Bool,
                nullable: false,
                default: Some("false"),
            },
            Column {
                name: "ended",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "work",
            target: "work",
        }],
        checks: &[
            Check::EditsPendingNonNeg,
            Check::EndedConsistency,
            Check::AliasPrimaryLocality,
            Check::AliasSearchHintsWork,
        ],
    },
    TableManifest {
        name: "iswc",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "work",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "iswc",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: false,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "work",
            target: "work",
        }],
        checks: &[],
    },
    // --- labels and areas ---
    // `area` loads first: `label.area` FKs to it, so the target must exist
    // at DDL time (DuckDB validates FK targets on CREATE).
    TableManifest {
        name: "area",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            // type → area_type, outside the subset — omitted.
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "begin_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "ended",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
            Column {
                name: "comment",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[],
        checks: &[Check::EditsPendingNonNeg, Check::EndedConsistency],
    },
    TableManifest {
        name: "label",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "begin_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "label_code",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            // type → label_type, outside the subset — omitted.
            Column {
                name: "type",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "area",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "comment",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "ended",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
        ],
        primary_key: &["id"],
        // `area` was created immediately above — the FK target resolves.
        foreign_keys: &[ForeignKey {
            column: "area",
            target: "area",
        }],
        checks: &[Check::EditsPendingNonNeg, Check::EndedConsistency],
    },
    // --- links ---
    TableManifest {
        name: "link_type",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "parent",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "child_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity_type0",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity_type1",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "description",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "link_phrase",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "reverse_link_phrase",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "long_link_phrase",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "is_deprecated",
                kind: Kind::Bool,
                nullable: false,
                default: Some("false"),
            },
            Column {
                name: "has_dates",
                kind: Kind::Bool,
                nullable: false,
                default: Some("true"),
            },
            Column {
                name: "entity0_cardinality",
                kind: Kind::SmallInt,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity1_cardinality",
                kind: Kind::SmallInt,
                nullable: false,
                default: Some("0"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "parent",
            target: "link_type",
        }],
        checks: &[],
    },
    TableManifest {
        name: "link_attribute_type",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "parent",
                kind: Kind::Int,
                nullable: true,
                default: None,
            },
            Column {
                name: "root",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "child_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "name",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "description",
                kind: Kind::Text,
                nullable: true,
                default: None,
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "parent",
                target: "link_attribute_type",
            },
            ForeignKey {
                column: "root",
                target: "link_attribute_type",
            },
        ],
        checks: &[],
    },
    TableManifest {
        name: "link",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link_type",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "begin_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "begin_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_year",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_month",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "end_date_day",
                kind: Kind::SmallInt,
                nullable: true,
                default: None,
            },
            Column {
                name: "attribute_count",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "ended",
                kind: Kind::Bool,
                nullable: false,
                default: Some("FALSE"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[ForeignKey {
            column: "link_type",
            target: "link_type",
        }],
        checks: &[Check::EndedConsistency],
    },
    TableManifest {
        name: "url",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "url",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[],
        checks: &[Check::EditsPendingNonNeg],
    },
    // --- link junctions (entities sorted alphabetically in each name) ---
    TableManifest {
        name: "l_artist_release",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "artist",
            },
            ForeignKey {
                column: "entity1",
                target: "release",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_artist_release_group",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "artist",
            },
            ForeignKey {
                column: "entity1",
                target: "release_group",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_artist_recording",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "artist",
            },
            ForeignKey {
                column: "entity1",
                target: "recording",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_artist_work",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "artist",
            },
            ForeignKey {
                column: "entity1",
                target: "work",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_artist_url",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "artist",
            },
            ForeignKey {
                column: "entity1",
                target: "url",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_label_release",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "label",
            },
            ForeignKey {
                column: "entity1",
                target: "release",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_recording_release",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "recording",
            },
            ForeignKey {
                column: "entity1",
                target: "release",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_recording_url",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "recording",
            },
            ForeignKey {
                column: "entity1",
                target: "url",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_recording_work",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "recording",
            },
            ForeignKey {
                column: "entity1",
                target: "work",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_release_group_url",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "release_group",
            },
            ForeignKey {
                column: "entity1",
                target: "url",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    TableManifest {
        name: "l_release_url",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "release",
            },
            ForeignKey {
                column: "entity1",
                target: "url",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    // The official link-table name is `l_url_work` (entity0 = url) — the
    // plan §3 draft spelled it `l_work_url`, which does not exist in the
    // schema-31 DDL; the verified core name is pinned here.
    TableManifest {
        name: "l_url_work",
        columns: &[
            Column {
                name: "id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "link",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity0",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "entity1",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "edits_pending",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "last_updated",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
            Column {
                name: "link_order",
                kind: Kind::Int,
                nullable: false,
                default: Some("0"),
            },
            Column {
                name: "entity0_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
            Column {
                name: "entity1_credit",
                kind: Kind::Text,
                nullable: false,
                default: Some("''"),
            },
        ],
        primary_key: &["id"],
        foreign_keys: &[
            ForeignKey {
                column: "link",
                target: "link",
            },
            ForeignKey {
                column: "entity0",
                target: "url",
            },
            ForeignKey {
                column: "entity1",
                target: "work",
            },
        ],
        checks: &[Check::EditsPendingNonNeg, Check::LinkOrderNonNeg],
    },
    // --- gid redirects (MBID re-pointing provenance) ---
    TableManifest {
        name: "artist_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "artist",
        }],
        checks: &[],
    },
    TableManifest {
        name: "artist_credit_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "artist_credit",
        }],
        checks: &[],
    },
    TableManifest {
        name: "release_group_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "release_group",
        }],
        checks: &[],
    },
    TableManifest {
        name: "release_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "release",
        }],
        checks: &[],
    },
    TableManifest {
        name: "recording_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "recording",
        }],
        checks: &[],
    },
    TableManifest {
        name: "work_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "work",
        }],
        checks: &[],
    },
    TableManifest {
        name: "label_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "label",
        }],
        checks: &[],
    },
    TableManifest {
        name: "url_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "url",
        }],
        checks: &[],
    },
    TableManifest {
        name: "area_gid_redirect",
        columns: &[
            Column {
                name: "gid",
                kind: Kind::Text,
                nullable: false,
                default: None,
            },
            Column {
                name: "new_id",
                kind: Kind::Int,
                nullable: false,
                default: None,
            },
            Column {
                name: "created",
                kind: Kind::Timestamp,
                nullable: true,
                default: Some("NOW()"),
            },
        ],
        primary_key: &["gid"],
        foreign_keys: &[ForeignKey {
            column: "new_id",
            target: "area",
        }],
        checks: &[],
    },
];

/// Look up a table manifest by name.
///
/// Returns `None` for tables outside the imported subset (there is no
/// `mbid` table in schema 31 — none will ever be found here).
pub fn table(name: &str) -> Option<&TableManifest<'_>> {
    ALL.iter().find(|t| t.name == name)
}

/// The number of imported tables (mirrors the §3 matrix size).
pub fn table_count() -> usize {
    ALL.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §3 matrix, exactly, in the corrected load order.
    const EXPECTED_ORDER: &[&str] = &[
        "artist",
        "artist_credit",
        "artist_credit_name",
        "artist_alias",
        "release_group_primary_type",
        "release_group_secondary_type",
        "release_group",
        "release_group_alias",
        "release_group_secondary_type_join",
        "release_status",
        "release_packaging",
        "release",
        "release_country",
        "release_unknown_country",
        "release_label",
        "recording",
        "recording_alias",
        "isrc",
        "medium",
        "track",
        "work",
        "work_alias",
        "iswc",
        "area",
        "label",
        "link_type",
        "link_attribute_type",
        "link",
        "url",
        "l_artist_release",
        "l_artist_release_group",
        "l_artist_recording",
        "l_artist_work",
        "l_artist_url",
        "l_label_release",
        "l_recording_release",
        "l_recording_url",
        "l_recording_work",
        "l_release_group_url",
        "l_release_url",
        "l_url_work",
        "artist_gid_redirect",
        "artist_credit_gid_redirect",
        "release_group_gid_redirect",
        "release_gid_redirect",
        "recording_gid_redirect",
        "work_gid_redirect",
        "label_gid_redirect",
        "url_gid_redirect",
        "area_gid_redirect",
    ];

    #[test]
    fn test_all_matches_schema31_matrix_in_load_order() {
        assert_eq!(ALL.len(), EXPECTED_ORDER.len());
        for (actual, expected) in ALL.iter().zip(EXPECTED_ORDER) {
            assert_eq!(actual.name, *expected);
        }
        assert_eq!(table_count(), 50);
    }

    #[test]
    fn test_no_mbid_table() {
        // Schema 31 has no `mbid` mapping table — do not design around one.
        assert!(table("mbid").is_none());
    }

    #[test]
    fn test_every_table_resolvable() {
        for entry in ALL {
            assert_eq!(table(entry.name).unwrap().name, entry.name);
        }
    }

    #[test]
    fn test_column_counts_pinned() {
        // Column counts from the official schema-31 DDL (2026-09-01 sample
        // dump layout). Guards against accidental manifest edits; the full
        // import inventory re-checks counts against the real dump members.
        let expected: &[(&str, usize)] = &[
            ("area", 14),
            ("area_gid_redirect", 3),
            ("artist", 19),
            ("artist_alias", 16),
            ("artist_credit", 7),
            ("artist_credit_gid_redirect", 3),
            ("artist_credit_name", 5),
            ("artist_gid_redirect", 3),
            ("isrc", 5),
            ("iswc", 5),
            ("l_artist_recording", 9),
            ("l_artist_release", 9),
            ("l_artist_release_group", 9),
            ("l_artist_url", 9),
            ("l_artist_work", 9),
            ("l_label_release", 9),
            ("l_recording_release", 9),
            ("l_recording_url", 9),
            ("l_recording_work", 9),
            ("l_release_group_url", 9),
            ("l_release_url", 9),
            ("l_url_work", 9),
            ("label", 16),
            ("label_gid_redirect", 3),
            ("link", 11),
            ("link_attribute_type", 8),
            ("link_type", 16),
            ("medium", 9),
            ("recording", 9),
            ("recording_alias", 16),
            ("recording_gid_redirect", 3),
            ("release", 14),
            ("release_country", 5),
            ("release_gid_redirect", 3),
            ("release_group", 8),
            ("release_group_alias", 16),
            ("release_group_gid_redirect", 3),
            ("release_group_primary_type", 6),
            ("release_group_secondary_type", 6),
            ("release_group_secondary_type_join", 3),
            ("release_label", 5),
            ("release_packaging", 6),
            ("release_status", 6),
            ("release_unknown_country", 4),
            ("track", 12),
            ("url", 5),
            ("url_gid_redirect", 3),
            ("work", 7),
            ("work_alias", 16),
            ("work_gid_redirect", 3),
        ];
        assert_eq!(expected.len(), ALL.len());
        for (name, count) in expected {
            let manifest_table = table(name).unwrap();
            assert_eq!(manifest_table.columns.len(), *count);
        }
        for table in ALL {
            let mut found = false;
            for (name, count) in expected {
                if *name == table.name {
                    assert_eq!(table.columns.len(), *count);
                    found = true;
                }
            }
            let name = table.name;
            assert!(found, "no pinned count for `{name}`");
        }
    }

    #[test]
    fn test_track_manifest_columns_schema31() {
        let track = table("track").unwrap();
        assert_eq!(track.columns.len(), 12);
        let names = &[
            "id",
            "gid",
            "recording",
            "medium",
            "position",
            "number",
            "name",
            "artist_credit",
            "length",
            "edits_pending",
            "last_updated",
            "is_data_track",
        ];
        for (column, expected) in track.columns.iter().zip(names) {
            assert_eq!(column.name, *expected);
        }
    }

    #[test]
    fn test_work_manifest_columns_schema31() {
        let work = table("work").unwrap();
        assert_eq!(work.columns.len(), 7);
        let names = &[
            "id",
            "gid",
            "name",
            "type",
            "comment",
            "edits_pending",
            "last_updated",
        ];
        for (column, expected) in work.columns.iter().zip(names) {
            assert_eq!(column.name, *expected);
        }
    }

    #[test]
    fn test_artist_credit_manifest_columns_schema31() {
        let artist_credit = table("artist_credit").unwrap();
        assert_eq!(artist_credit.columns.len(), 7);
        assert!(artist_credit.columns.iter().any(|c| c.name == "gid"));
        assert_eq!(artist_credit.primary_key.len(), 1);
        assert_eq!(artist_credit.primary_key[0], "id");
    }

    #[test]
    fn test_link_has_no_gid_but_link_type_does() {
        // Plan §3: `link` rows keep only their integer id; `link_type` ships
        // a gid in schema 31.
        let link = table("link").unwrap();
        for col in link.columns {
            assert!(col.name != "gid");
        }
        let link_type = table("link_type").unwrap();
        assert!(link_type.columns.iter().any(|c| c.name == "gid"));
    }

    #[test]
    fn test_medium_has_gid() {
        assert!(
            table("medium")
                .unwrap()
                .columns
                .iter()
                .any(|c| c.name == "gid")
        );
    }

    #[test]
    fn test_load_order_satisfies_all_foreign_keys() {
        // Every FK target is imported and appears strictly before the
        // referencing table, so DuckDB's DDL-time FK validation never fails.
        // This is the invariant behind "corrected load order".
        for (i, table) in ALL.iter().enumerate() {
            for fk in table.foreign_keys {
                let target_index = ALL.iter().position(|t| t.name == fk.target);
                match target_index {
                    Some(j) => {
                        let child = table.name;
                        let target = fk.target;
                        // `j <= i`: strictly-before for every FK, except a
                        // self-reference (child == target, a parent/root
                        // hierarchy) where the target is the table itself.
                        assert!(j <= i, "{child} references {target} but loads after it",);
                    }
                    None => {
                        let child = table.name;
                        let target = fk.target;
                        panic!("{child} references unknown table `{target}`");
                    }
                }
            }
        }
    }

    #[test]
    fn test_gid_redirect_shapes() {
        let redirects = &[
            "artist_gid_redirect",
            "artist_credit_gid_redirect",
            "release_group_gid_redirect",
            "release_gid_redirect",
            "recording_gid_redirect",
            "work_gid_redirect",
            "label_gid_redirect",
            "url_gid_redirect",
            "area_gid_redirect",
        ];
        for name in redirects {
            let table = table(name).unwrap();
            assert_eq!(table.columns.len(), 3);
            assert_eq!(table.columns[0].name, "gid");
            assert_eq!(table.columns[1].name, "new_id");
            assert_eq!(table.columns[2].name, "created");
            assert_eq!(table.primary_key.len(), 1);
            assert_eq!(table.primary_key[0], "gid");
            assert_eq!(table.foreign_keys.len(), 1);
            assert_eq!(table.foreign_keys[0].column, "new_id");
        }
    }
}
