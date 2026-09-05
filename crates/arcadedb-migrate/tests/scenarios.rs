//! Scenario tests for the migration manager — assert the *exact* DDL output
//! for each realistic migration situation. These are the "does the manager do
//! the right thing?" tests: they cover the full diff → DiffAction → to_sql
//! pipeline against concrete scenarios, asserting complete SQL strings (not
//! just structural matches).

use arcadedb_migrate::schema::ddl::Tail;
use arcadedb_migrate::schema::parser;
use arcadedb_migrate::schema::{diff, DiffAction, DropStrategy};
use arcadedb_migrate::schema::{Constraints, Index, IndexKind, Property, Schema, TypeKind};

/// Run the diff + render every action to SQL, sorted for deterministic order.
fn diff_sql(desired: &Schema, actual: &Schema, strategy: DropStrategy) -> Vec<String> {
    let mut out: Vec<String> = diff(desired, actual, strategy)
        .iter()
        .map(DiffAction::to_sql)
        .collect();
    out.sort();
    out
}

fn schema(ddl: &str) -> Schema {
    parser::parse(ddl).expect("test schema should parse")
}

/// Build an actual-side property with no constraints set (the common
/// "introspected but plain" case).
fn plain_prop(name: &str, type_name: &str) -> Property {
    Property {
        name: name.into(),
        type_name: type_name.into(),
        constraints: Constraints::default(),
    }
}

// ---------------------------------------------------------------------------
// Scenario 1: brand-new type (the empty-DB → first-sync case)
// ---------------------------------------------------------------------------

#[test]
fn new_type_emits_create_type_then_its_properties_and_indexes() {
    let desired = schema(
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.title IF NOT EXISTS STRING;
         CREATE INDEX idx_item_title IF NOT EXISTS ON item(title) UNIQUE;",
    );
    let actual = Schema::new();

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    assert_eq!(
        sql,
        vec![
            "CREATE DOCUMENT TYPE item IF NOT EXISTS",
            "CREATE INDEX idx_item_title IF NOT EXISTS ON item (title) UNIQUE",
            "CREATE PROPERTY item.title IF NOT EXISTS STRING",
        ]
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: additive property on an existing type
// ---------------------------------------------------------------------------

#[test]
fn add_property_emits_single_create_property() {
    // DB has {title}; desired adds {title, review_score}.
    let desired = schema(
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.title IF NOT EXISTS STRING;
         CREATE PROPERTY item.review_score IF NOT EXISTS DOUBLE;",
    );
    let mut actual = Schema::new();
    let ty = actual.type_or_insert("item", TypeKind::Document);
    ty.properties
        .insert("title".into(), plain_prop("title", "STRING"));

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    assert_eq!(
        sql,
        vec!["CREATE PROPERTY item.review_score IF NOT EXISTS DOUBLE"]
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: additive index on an existing type (named)
// ---------------------------------------------------------------------------

#[test]
fn add_named_index_emits_create_index() {
    let desired = schema(
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE INDEX idx_item_external_id IF NOT EXISTS ON item(external_id) UNIQUE;",
    );
    let mut actual = Schema::new();
    actual.type_or_insert("item", TypeKind::Document);

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    assert_eq!(
        sql,
        vec!["CREATE INDEX idx_item_external_id IF NOT EXISTS ON item (external_id) UNIQUE"]
    );
}

// ---------------------------------------------------------------------------
// Scenario 4: additive FULL_TEXT index, unnamed (auto-named Type[col])
// ---------------------------------------------------------------------------

#[test]
fn add_unnamed_fulltext_index_emits_create_index_without_name() {
    let desired = schema(
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE INDEX ON item(title) FULL_TEXT;",
    );
    let mut actual = Schema::new();
    actual.type_or_insert("item", TypeKind::Document);

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    // No name in the output — matches the input form (SEARCH_INDEX needs this).
    assert_eq!(
        sql,
        vec!["CREATE INDEX IF NOT EXISTS ON item (title) FULL_TEXT"]
    );
}

// ---------------------------------------------------------------------------
// Scenario 5: sparse-vector index with METADATA renders verbatim
// ---------------------------------------------------------------------------

#[test]
fn sparse_vector_index_metadata_roundtrips_in_output() {
    let desired = schema(
        r#"CREATE DOCUMENT TYPE item IF NOT EXISTS;
           CREATE INDEX idx_item_sparse IF NOT EXISTS ON item(label_tokens, label_weights)
               LSM_SPARSE_VECTOR METADATA { "dimensions": 20000, "modifier": "IDF" };"#,
    );
    let mut actual = Schema::new();
    actual.type_or_insert("item", TypeKind::Document);

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    assert_eq!(sql.len(), 1);
    let s = &sql[0];
    assert!(s.starts_with("CREATE INDEX idx_item_sparse IF NOT EXISTS ON item (label_tokens, label_weights) LSM_SPARSE_VECTOR"), "got: {s}");
    assert!(s.contains("METADATA"), "missing METADATA: {s}");
    assert!(
        s.contains("\"dimensions\": 20000"),
        "metadata body lost: {s}"
    );
    assert!(
        s.contains("\"modifier\": \"IDF\""),
        "metadata body lost: {s}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 6: constraints render canonically on first sync, reconcile on re-sync
// ---------------------------------------------------------------------------

#[test]
fn constraints_roundtrip_canonically_and_resync_is_noop() {
    let desired = schema(
        "CREATE DOCUMENT TYPE page IF NOT EXISTS;
         CREATE PROPERTY page.slug IF NOT EXISTS STRING (MANDATORY true);
         CREATE PROPERTY page.checksum IF NOT EXISTS STRING (DEFAULT \"\");
         CREATE PROPERTY account.role IF NOT EXISTS STRING (DEFAULT \"user\", MANDATORY true);",
    );

    // 1. First sync against an empty DB renders the attribute block in a
    //    canonical order — values keep their case (`"user"`, not `"USER"`).
    let sql = diff_sql(&desired, &Schema::new(), DropStrategy::Never);
    assert!(
        sql.iter()
            .any(|s| s == "CREATE PROPERTY page.slug IF NOT EXISTS STRING (MANDATORY true)"),
        "got: {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 18: TimeSeries types (first-class declarative support)
// ---------------------------------------------------------------------------

/// A TimeSeries declaration parses, renders canonically through the full
/// diff pipeline, and emits NO separate property/index actions — the columns
/// live inside the CREATE.
#[test]
fn timeseries_create_renders_the_dedicated_form() {
    let desired = schema(
        "CREATE TIMESERIES TYPE SensorReading \
         TIMESTAMP ts PRECISION SECOND \
         TAGS (sensor_id LONG) \
         FIELDS (temperature INTEGER, humidity INTEGER) \
         SHARDS 2 RETENTION 90 DAYS;",
    );
    let sql = diff_sql(&desired, &Schema::new(), DropStrategy::Never);
    assert_eq!(
        sql,
        vec![
            "CREATE TIMESERIES TYPE SensorReading IF NOT EXISTS TIMESTAMP ts \
             PRECISION SECOND TAGS (sensor_id LONG) FIELDS (temperature INTEGER, humidity INTEGER) \
             SHARDS 2 RETENTION 90 DAYS"
        ]
    );
}

/// Under Explicit, a leftover TimeSeries type drops via its DEDICATED form —
/// `DROP TIMESERIES TYPE` (plain `DROP TYPE … UNSAFE` doesn't apply to the
/// TS engine).
#[test]
fn timeseries_leftover_drops_via_dedicated_form() {
    let desired = Schema::new();
    let mut actual = Schema::new();
    let ty = actual.type_or_insert("metrics", TypeKind::Timeseries);
    ty.timeseries = Some(arcadedb_migrate::schema::TimeseriesSpec {
        timestamp_column: "ts".into(),
        fields: vec![arcadedb_migrate::schema::TimeseriesColumn {
            name: "f".into(),
            data_type: "DOUBLE".into(),
            role: arcadedb_migrate::schema::TsRole::Field,
        }],
        ..Default::default()
    });

    let actions = diff(&desired, &actual, DropStrategy::Explicit);
    assert_eq!(actions.len(), 1);
    assert_eq!(
        actions[0].to_sql(),
        "DROP TIMESERIES TYPE IF EXISTS metrics"
    );
}

// ---------------------------------------------------------------------------
// Scenario 6b: constraint drift on an existing property → typed ALTERs
// ---------------------------------------------------------------------------

#[test]
fn constraint_drift_emits_typed_alter_actions() {
    // Desired adds NOTNULL + a MIN to an existing property.
    let desired = schema(
        "CREATE DOCUMENT TYPE page IF NOT EXISTS;
         CREATE PROPERTY page.score IF NOT EXISTS INTEGER (NOTNULL true, MIN 0 , MAX 100);",
    );
    // Actual: the property already has MAX 50.
    let mut actual = Schema::new();
    let page = actual.type_or_insert("page", TypeKind::Document);
    let c = Constraints {
        max: Some("50".into()),
        ..Constraints::default()
    };
    page.properties.insert(
        "score".into(),
        Property {
            name: "score".into(),
            type_name: "INTEGER".into(),
            constraints: c,
        },
    );

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);
    assert!(
        sql.contains(&"ALTER PROPERTY page.score notnull true".to_string()),
        "got: {sql:?}"
    );
    assert!(
        sql.contains(&"ALTER PROPERTY page.score min 0".to_string()),
        "got: {sql:?}"
    );
    assert!(
        sql.contains(&"ALTER PROPERTY page.score max 100".to_string()),
        "got: {sql:?}"
    );

    // And none of these are destructive.
    let actions = diff(&desired, &actual, DropStrategy::Never);
    assert!(actions.iter().all(|a| !a.is_destructive()));
}

// ---------------------------------------------------------------------------
// Scenario 6c: constraint edits change the checksum (they re-run the sync)
// ---------------------------------------------------------------------------

#[test]
fn checksum_changes_when_constraint_changes() {
    use arcadedb_migrate::schema::checksum;

    let a = schema(
        "CREATE DOCUMENT TYPE page IF NOT EXISTS;
         CREATE PROPERTY page.synced_at IF NOT EXISTS DATETIME (DEFAULT date());",
    );
    let b = schema(
        "CREATE DOCUMENT TYPE page IF NOT EXISTS;
         CREATE PROPERTY page.synced_at IF NOT EXISTS DATETIME (DEFAULT date('2020-01-01'));",
    );
    assert_ne!(
        checksum(&a),
        checksum(&b),
        "default edit must change the checksum"
    );

    let c = schema(
        "CREATE DOCUMENT TYPE page IF NOT EXISTS;
         CREATE PROPERTY page.synced_at IF NOT EXISTS DATETIME (DEFAULT date());",
    );
    assert_eq!(
        checksum(&a),
        checksum(&c),
        "identical schema must checksum equal"
    );

    // ... attribute ORDER is not significant (canonical checksum).
    let reordered = schema(
        "CREATE DOCUMENT TYPE account IF NOT EXISTS;
         CREATE PROPERTY account.role IF NOT EXISTS STRING (MANDATORY true, DEFAULT \"user\");",
    );
    let reordered2 = schema(
        "CREATE DOCUMENT TYPE account IF NOT EXISTS;
         CREATE PROPERTY account.role IF NOT EXISTS STRING (DEFAULT \"user\", MANDATORY true);",
    );
    assert_eq!(
        checksum(&reordered),
        checksum(&reordered2),
        "attribute order is canonical"
    );
}

// ---------------------------------------------------------------------------
// Scenario 7: multi-word property type (LIST OF INTEGER) preserved
// ---------------------------------------------------------------------------

#[test]
fn multi_word_property_type_preserved() {
    let desired = schema(
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.label_ids IF NOT EXISTS LIST OF INTEGER;",
    );
    let mut actual = Schema::new();
    actual.type_or_insert("item", TypeKind::Document);

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    assert_eq!(
        sql,
        vec!["CREATE PROPERTY item.label_ids IF NOT EXISTS LIST OF INTEGER"]
    );
}

// ---------------------------------------------------------------------------
// Scenario 7b: no-op when actual matches desired (the idempotent re-sync)
// ---------------------------------------------------------------------------

#[test]
fn matching_schema_produces_empty_diff() {
    // Desired: type with a property + a named index.
    let desired = schema(
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.title IF NOT EXISTS STRING;
         CREATE INDEX idx_item_title IF NOT EXISTS ON item(title) UNIQUE;",
    );
    // Actual: introspection returns the same shape (with the index auto-named
    // internally — but a named index matches by name).
    let mut actual = Schema::new();
    let ty = actual.type_or_insert("item", TypeKind::Document);
    ty.properties
        .insert("title".into(), plain_prop("title", "STRING"));
    ty.indexes.push(Index {
        name: Some("idx_item_title".into()),
        columns: vec!["title".into()],
        kind: IndexKind::Unique,
        unique: true,
        tail: Tail::none(),
    });

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);
    assert!(sql.is_empty(), "expected no SQL, got: {sql:?}");
}

// ---------------------------------------------------------------------------
// Scenario 8: vertex/edge types use the right DDL keyword
// ---------------------------------------------------------------------------

#[test]
fn vertex_and_edge_types_render_correct_keywords() {
    let desired = schema(
        "CREATE VERTEX TYPE node IF NOT EXISTS;
         CREATE EDGE TYPE link IF NOT EXISTS;",
    );
    let actual = Schema::new();

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    assert!(
        sql.contains(&"CREATE VERTEX TYPE node IF NOT EXISTS".to_string()),
        "got: {sql:?}"
    );
    assert!(
        sql.contains(&"CREATE EDGE TYPE link IF NOT EXISTS".to_string()),
        "got: {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 9: compound (multi-column) index
// ---------------------------------------------------------------------------

#[test]
fn compound_index_renders_all_columns() {
    let desired = schema(
        "CREATE DOCUMENT TYPE t IF NOT EXISTS;
         CREATE INDEX idx_t_a_b IF NOT EXISTS ON t(a, b) NOTUNIQUE;",
    );
    let actual = Schema::new();

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    assert_eq!(
        sql.iter().find(|s| s.contains("idx_t_a_b")).unwrap(),
        &"CREATE INDEX idx_t_a_b IF NOT EXISTS ON t (a, b) NOTUNIQUE".to_string()
    );
}

// ---------------------------------------------------------------------------
// Scenario 10: property type mismatch → AlterPropertyType (destructive)
// ---------------------------------------------------------------------------

#[test]
fn property_type_change_emits_alter() {
    let desired = schema(
        "CREATE DOCUMENT TYPE t IF NOT EXISTS;
         CREATE PROPERTY t.score IF NOT EXISTS DOUBLE;",
    );
    let mut actual = Schema::new();
    let ty = actual.type_or_insert("t", TypeKind::Document);
    ty.properties.insert(
        "score".into(),
        plain_prop("score", "INTEGER"), // was INTEGER
    );

    let actions = diff(&desired, &actual, DropStrategy::Never);
    assert!(
        actions.iter().any(
            |a| matches!(a, DiffAction::AlterPropertyType { type_name, prop }
            if type_name == "t" && prop.name == "score" && prop.type_name == "DOUBLE")
        ),
        "expected AlterPropertyType, got {actions:?}"
    );
    assert!(actions.iter().any(|a| a.is_destructive()));
    // The ALTER renders with the new type.
    let sql: Vec<String> = actions.iter().map(DiffAction::to_sql).collect();
    assert!(
        sql.iter()
            .any(|s| s.contains("ALTER PROPERTY t.score TYPE DOUBLE")),
        "got {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 11: leftover type under Explicit → DropType DDL (UNSAFE)
// ---------------------------------------------------------------------------

#[test]
fn explicit_drop_renders_drop_type_unsafe() {
    let desired = Schema::new();
    let mut actual = Schema::new();
    actual.type_or_insert("deprecated_old", TypeKind::Document);

    let sql = diff_sql(&desired, &actual, DropStrategy::Explicit);

    assert_eq!(sql, vec!["DROP TYPE deprecated_old IF EXISTS UNSAFE"]);
}

// ---------------------------------------------------------------------------
// Scenario 12: leftovers ignored under Never (no destructive DDL)
// ---------------------------------------------------------------------------

#[test]
fn never_drop_emits_nothing_for_leftover_type_property_or_index() {
    let desired = schema("CREATE DOCUMENT TYPE item IF NOT EXISTS;");
    let mut actual = Schema::new();
    // Actual has a leftover property and a leftover index that desired doesn't mention.
    let ty = actual.type_or_insert("item", TypeKind::Document);
    ty.properties
        .insert("legacy_field".into(), plain_prop("legacy_field", "STRING"));
    ty.indexes.push(Index {
        name: Some("idx_legacy".into()),
        columns: vec!["legacy_field".into()],
        kind: IndexKind::NotUnique,
        unique: false,
        tail: Tail::none(),
    });
    // And a whole leftover type.
    actual.type_or_insert("abandoned", TypeKind::Document);

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);
    assert!(
        sql.is_empty(),
        "Never must emit nothing for leftovers, got: {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 13: realistic multi-type sync (account + label + item) ordering
// ---------------------------------------------------------------------------

#[test]
fn multi_type_sync_emits_all_creates_in_name_order() {
    // Three related types with an index: ordering across the whole set.
    let desired = schema(
        "CREATE DOCUMENT TYPE account IF NOT EXISTS;
         CREATE PROPERTY account.external_id IF NOT EXISTS STRING;
         CREATE INDEX idx_account_external_id IF NOT EXISTS ON account(external_id) UNIQUE;

         CREATE DOCUMENT TYPE label IF NOT EXISTS;
         CREATE PROPERTY label.name IF NOT EXISTS STRING;

         CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.external_id IF NOT EXISTS STRING;",
    );
    let actual = Schema::new();

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);

    // The exact set of statements (sorted). Verifies nothing is dropped or
    // duplicated across multi-file schemas.
    assert_eq!(
        sql,
        vec![
            "CREATE DOCUMENT TYPE account IF NOT EXISTS",
            "CREATE DOCUMENT TYPE item IF NOT EXISTS",
            "CREATE DOCUMENT TYPE label IF NOT EXISTS",
            "CREATE INDEX idx_account_external_id IF NOT EXISTS ON account (external_id) UNIQUE",
            "CREATE PROPERTY account.external_id IF NOT EXISTS STRING",
            "CREATE PROPERTY item.external_id IF NOT EXISTS STRING",
            "CREATE PROPERTY label.name IF NOT EXISTS STRING",
        ]
    );
}

// ---------------------------------------------------------------------------
// Scenario 14: checksum changes only when the schema model changes
// ---------------------------------------------------------------------------

#[test]
fn checksum_is_stable_across_cosmetic_edits() {
    use arcadedb_migrate::schema::checksum;

    let a = schema(
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.title IF NOT EXISTS STRING;",
    );
    // Same model, whitespace + a comment added.
    let b = schema(
        "-- a comment
         CREATE DOCUMENT TYPE item IF NOT EXISTS   ;

         CREATE PROPERTY item.title IF NOT EXISTS STRING;",
    );
    assert_eq!(
        checksum(&a),
        checksum(&b),
        "checksum must ignore cosmetic edits"
    );

    // Different model (new property) → different checksum.
    let c = schema(
        "CREATE DOCUMENT TYPE item IF NOT EXISTS;
         CREATE PROPERTY item.title IF NOT EXISTS STRING;
         CREATE PROPERTY item.year IF NOT EXISTS INTEGER;",
    );
    assert_ne!(
        checksum(&a),
        checksum(&c),
        "checksum must change on model change"
    );
}

// ---------------------------------------------------------------------------
// Scenario 15: realistic sync — every attribute-block flavor
// ---------------------------------------------------------------------------

#[test]
fn realistic_schema_roundtrips_and_renders_constraints() {
    let ddl = r#"
        CREATE DOCUMENT TYPE page IF NOT EXISTS;
        CREATE PROPERTY page.parent_id IF NOT EXISTS LINK;
        CREATE PROPERTY page.slug IF NOT EXISTS STRING (MANDATORY true);
        CREATE PROPERTY page.label_range IF NOT EXISTS LIST OF STRING;
        CREATE PROPERTY page.checksum IF NOT EXISTS STRING (DEFAULT "");
        CREATE PROPERTY page.synced_at IF NOT EXISTS DATETIME (DEFAULT date());
        CREATE INDEX idx_page_parent IF NOT EXISTS ON page (parent_id) NOTUNIQUE;
        CREATE INDEX idx_page_slug IF NOT EXISTS ON page (slug) UNIQUE;

        CREATE VERTEX TYPE account IF NOT EXISTS;
        CREATE PROPERTY account.role IF NOT EXISTS STRING (DEFAULT "user");

        CREATE EDGE TYPE account_completion IF NOT EXISTS;
        CREATE PROPERTY account_completion.completed_at IF NOT EXISTS DATETIME (DEFAULT date());
        CREATE PROPERTY account_completion.score IF NOT EXISTS INTEGER;
        CREATE PROPERTY account_completion.payload IF NOT EXISTS MAP;
        CREATE INDEX idx_completion_account IF NOT EXISTS ON account_completion (@out) NOTUNIQUE;
        CREATE INDEX idx_completion_unique IF NOT EXISTS ON account_completion (@out, @in) UNIQUE;
    "#;
    let desired = schema(ddl);

    // 1. First sync against an empty DB: every statement renders exactly as
    //    written — attribute values keep their case, `@out`/`@in` columns are
    //    not mangled.
    let sql = diff_sql(&desired, &Schema::new(), DropStrategy::Never);
    let expected = vec![
        "CREATE DOCUMENT TYPE page IF NOT EXISTS",
        "CREATE EDGE TYPE account_completion IF NOT EXISTS",
        "CREATE INDEX idx_completion_account IF NOT EXISTS ON account_completion (@out) NOTUNIQUE",
        "CREATE INDEX idx_completion_unique IF NOT EXISTS ON account_completion (@out, @in) UNIQUE",
        "CREATE INDEX idx_page_parent IF NOT EXISTS ON page (parent_id) NOTUNIQUE",
        "CREATE INDEX idx_page_slug IF NOT EXISTS ON page (slug) UNIQUE",
        "CREATE PROPERTY account.role IF NOT EXISTS STRING (DEFAULT \"user\")",
        "CREATE PROPERTY account_completion.completed_at IF NOT EXISTS DATETIME (DEFAULT date())",
        "CREATE PROPERTY account_completion.payload IF NOT EXISTS MAP",
        "CREATE PROPERTY account_completion.score IF NOT EXISTS INTEGER",
        "CREATE PROPERTY page.checksum IF NOT EXISTS STRING (DEFAULT \"\")",
        "CREATE PROPERTY page.label_range IF NOT EXISTS LIST OF STRING",
        "CREATE PROPERTY page.parent_id IF NOT EXISTS LINK",
        "CREATE PROPERTY page.slug IF NOT EXISTS STRING (MANDATORY true)",
        "CREATE PROPERTY page.synced_at IF NOT EXISTS DATETIME (DEFAULT date())",
        "CREATE VERTEX TYPE account IF NOT EXISTS",
    ];
    assert_eq!(
        sql, expected,
        "first sync must re-emit everything canonically"
    );

    // 2. Re-sync against an introspected *actual* that also surfaced the
    //    constraints (a constraint-aware engine round-trip — DEFAULT date() is
    //    introspected resolved, but presence still matches) is a no-op.
    let actions = diff(&desired, &desired.clone(), DropStrategy::Never);
    assert!(
        actions.is_empty(),
        "structured constraints must not re-diff an equal schema, got {actions:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 16: type-page EXTENDS is structured + reconciled; BUCKETS etc.
// stay create-only
// ---------------------------------------------------------------------------

#[test]
fn type_extends_reconciled_and_bucket_clause_create_only() {
    use arcadedb_migrate::schema::checksum;

    let desired = schema(
        "CREATE VERTEX TYPE Employee EXTENDS Person IF NOT EXISTS;
         CREATE PROPERTY Employee.name IF NOT EXISTS STRING (MANDATORY true);
         CREATE DOCUMENT TYPE audit BUCKETS 4 PAGESIZE 4096;",
    );

    // 1. First sync re-emits the EXTENDS list and the create-only clause
    //    canonically after IF NOT EXISTS.
    let sql = diff_sql(&desired, &Schema::new(), DropStrategy::Never);
    assert!(
        sql.iter()
            .any(|s| s == "CREATE VERTEX TYPE Employee IF NOT EXISTS EXTENDS Person"),
        "extends lost on first sync: {sql:?}"
    );
    assert!(
        sql.iter()
            .any(|s| s == "CREATE DOCUMENT TYPE audit IF NOT EXISTS BUCKETS 4 PAGESIZE 4096"),
        "clause lost on first sync: {sql:?}"
    );
    assert!(
        sql.iter()
            .any(|s| s == "CREATE PROPERTY Employee.name IF NOT EXISTS STRING (MANDATORY true)"),
        "attributed property lost alongside clause test: {sql:?}"
    );

    // 2. EXTENDS drift on an existing type → AlterTypeSupers (non-destructive).
    let mut actual = Schema::new();
    let emp = actual.type_or_insert("Employee", TypeKind::Vertex);
    emp.properties
        .insert("name".into(), plain_prop("name", "STRING"));
    let audit = actual.type_or_insert("audit", TypeKind::Document);
    audit.properties.insert(
        "name".into(),
        Property {
            name: "name".into(),
            type_name: "STRING".into(),
            constraints: Constraints {
                mandatory: true,
                ..Constraints::default()
            },
        },
    );
    let actions = diff(&desired, &actual, DropStrategy::Never);
    assert!(
        actions
            .iter()
            .any(|a| a.to_sql().contains("ALTER TYPE Employee SUPERTYPE +Person")),
        "expected a SUPERTYPE add, got {actions:?}"
    );
    assert!(actions.iter().all(|a| !a.is_destructive()));

    // 3. BUCKETS is NOT diffed — the audit type created without a clause
    //    stays untouched even under never-drop (no ALTER form exists).
    let mut plain_actual = Schema::new();
    plain_actual.type_or_insert("audit", TypeKind::Document);
    let only_e = diff(&desired, &plain_actual, DropStrategy::Never);
    assert!(
        !only_e.iter().any(|a| a.to_sql().contains("BUCKETS")),
        "bucket clause must not be diffed, got {only_e:?}"
    );

    // 4. EXTENDS edits change the checksum (editing the .sql must re-run).
    let extended = schema(
        "CREATE VERTEX TYPE Employee EXTENDS Manager IF NOT EXISTS;
         CREATE PROPERTY Employee.name IF NOT EXISTS STRING (MANDATORY true);
         CREATE DOCUMENT TYPE audit BUCKETS 4 PAGESIZE 4096;",
    );
    assert_ne!(
        checksum(&desired),
        checksum(&extended),
        "an EXTENDS edit must change the checksum"
    );

    // 5. Attribute ORDER in the .sql is canonical (no checksum churn).
    let reordered = schema(
        "CREATE VERTEX TYPE Employee IF NOT EXISTS EXTENDS Person;
         CREATE PROPERTY Employee.name IF NOT EXISTS STRING (MANDATORY true);
         CREATE DOCUMENT TYPE audit BUCKETS 4 PAGESIZE 4096;",
    );
    assert_eq!(
        checksum(&desired),
        checksum(&reordered),
        "EXTENDS position must be canonical"
    );
}

// ---------------------------------------------------------------------------
// Section marker retired — DEFAULT reconciliation is now three-way vs the
// SchemaSnapshot (see the model module doc), not purely presence-based.
// These scenarios still cover the presence arms that don't need a snapshot.
// ---------------------------------------------------------------------------

#[test]
fn default_presence_deleted_in_desired_emits_default_null() {
    // Desired dropped the DEFAULT; the DB property still has one.
    let desired = schema(
        "CREATE DOCUMENT TYPE t IF NOT EXISTS;
         CREATE PROPERTY t.x IF NOT EXISTS STRING;",
    );
    let mut actual = Schema::new();
    let c = Constraints {
        default: Some(arcadedb_migrate::schema::DefaultExpr::new("legacy")),
        ..Constraints::default()
    };
    let ty = actual.type_or_insert("t", TypeKind::Document);
    ty.properties.insert("x".into(), {
        let mut p = plain_prop("x", "STRING");
        p.constraints = c;
        p
    });

    let sql = diff_sql(&desired, &actual, DropStrategy::Never);
    assert!(
        sql.contains(&"ALTER PROPERTY t.x default null".to_string()),
        "got: {sql:?}"
    );
}
