//! DTO ↔ schema drift checks.
//!
//! `RecordDecode` DTOs expose `WIRES` — the wire columns they read. Wiring
//! those against the introspected live schema turns "field added to the
//! model, migration never written" into a failing integration test instead
//! of a runtime `MissingField` against production traffic.

use crate::error::{MigrateError, Result};
use crate::schema::introspect::fetch_actual;
use crate::schema::model::Schema;
use arcadedb_protocol::ArcadeDbClient;

/// The DTO wire names with no matching property on the introspected type.
/// Pure — unit-testable without a server. Extra DB columns are fine (a DTO
/// may read a subset of a wide table); the reverse is drift.
pub fn dto_wires_missing<'w>(wires: &[&'w str], schema: &Schema, type_name: &str) -> Vec<&'w str> {
    // `@`-prefixed wires (`@rid`, `@type`, `@in`, `@out`, ...) are engine
    // intrinsics — always present on real records, never schema-managed
    // properties, so they cannot drift against the type definition.
    let intrinsic = |w: &str| w.starts_with('@');
    match schema.type_(type_name) {
        Some(ty) => wires
            .iter()
            .copied()
            .filter(|w| !intrinsic(w) && !ty.properties.contains_key(*w))
            .collect(),
        // A missing type is total drift — every wire is unmatched.
        None => wires.iter().copied().filter(|w| !intrinsic(w)).collect(),
    }
}

/// Integration-tier assert: fetch the live schema and fail listing the DTO
/// columns the database does not carry. Usage:
///
/// ```no_run
/// # async fn go(client: &arcadedb_protocol::ArcadeDbClient) -> arcadedb_migrate::Result<()> {
/// arcadedb_migrate::schema::dto_drift::assert_dto_wires(
///     client, "testdb", "user_shelf", &["item_id", "minutes_total"],
/// ).await
/// # }
/// ```
pub async fn assert_dto_wires(
    client: &ArcadeDbClient,
    db: &str,
    type_name: &str,
    wires: &[&str],
) -> Result<()> {
    let schema = fetch_actual(client, db).await?;
    if !schema.types.contains_key(type_name) {
        return Err(MigrateError::Drift {
            message: format!("DTO drift: type `{type_name}` does not exist in `{db}`"),
        });
    }
    let missing = dto_wires_missing(wires, &schema, type_name);
    if !missing.is_empty() {
        return Err(MigrateError::Drift {
            message: format!(
                "DTO drift: `{type_name}` in `{db}` is missing columns {missing:?} — \
                 add a migration or drop them from the DTO"
            ),
        });
    }
    Ok(())
}

/// Compatibility class of a column/expected type name — the drift check
/// asserts DECODABILITY, not storage equality: the decoder is range-checked
/// across integer widths (`i64` reading `INTEGER` is fine) and float-tolerant
/// (`f64` accepts integer kinds — the engine returns ints for some computed
/// float columns), and `DateTime` also parses RFC-3339 strings. List forms
/// normalize too: the server introspects `LIST OF DOUBLE` columns as
/// `ARRAY_OF_FLOATS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeClass {
    Str,
    Bool,
    Int,
    Float,
    DateTime,
    Map,
    List(&'static TypeClassHelper),
    Unknown,
}

/// Helper indirection so `List` stays `Copy` (a nested `List` collapses to
/// `Unknown` — no legitimate deeper nesting in this schema family).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypeClassHelper(TypeClass);

fn class_of(type_name: &str) -> TypeClass {
    let name = type_name.trim().to_ascii_uppercase();
    if let Some(rest) = name.strip_prefix("LIST OF ") {
        return list_class(rest);
    }
    // SET OF columns decode identically to LIST OF (the wire carries both
    // as `ListValue`; a set-typed DTO field declares LIST OF), so one class.
    if let Some(rest) = name.strip_prefix("SET OF ") {
        return list_class(rest);
    }
    if let Some(rest) = name.strip_prefix("ARRAY_OF") {
        return list_class(rest.trim_start_matches('_'));
    }
    if let Some(rest) = name.strip_prefix("SET_OF") {
        return list_class(rest.trim_start_matches('_'));
    }
    scalar_class(&name)
}

fn scalar_class(name: &str) -> TypeClass {
    match name {
        "STRING" | "STRINGS" | "LINK" => TypeClass::Str,
        "BOOLEAN" | "BOOLEANS" => TypeClass::Bool,
        "INTEGER" | "INTEGERS" | "LONG" | "LONGS" | "SHORT" | "BYTE" => TypeClass::Int,
        "FLOAT" | "FLOATS" | "DOUBLE" | "DOUBLES" => TypeClass::Float,
        "DATETIME" | "DATE" => TypeClass::DateTime,
        "MAP" | "EMBEDDEDMAP" => TypeClass::Map,
        _ => TypeClass::Unknown,
    }
}

fn list_class(elem: &str) -> TypeClass {
    match scalar_class(elem) {
        TypeClass::Str => TypeClass::List(&STR_LIST),
        TypeClass::Bool => TypeClass::List(&BOOL_LIST),
        TypeClass::Int => TypeClass::List(&INT_LIST),
        TypeClass::Float => TypeClass::List(&FLOAT_LIST),
        _ => TypeClass::Unknown,
    }
}

static STR_LIST: TypeClassHelper = TypeClassHelper(TypeClass::Str);
static BOOL_LIST: TypeClassHelper = TypeClassHelper(TypeClass::Bool);
static INT_LIST: TypeClassHelper = TypeClassHelper(TypeClass::Int);
static FLOAT_LIST: TypeClassHelper = TypeClassHelper(TypeClass::Float);

/// Can a field expecting `expected` decode a column typed `actual`?
fn kinds_compatible(expected: &str, actual: &str) -> bool {
    match (class_of(expected), class_of(actual)) {
        (TypeClass::Unknown, _) | (_, TypeClass::Unknown) => true,
        (TypeClass::Str, TypeClass::Str) => true,
        (TypeClass::Bool, TypeClass::Bool) => true,
        (TypeClass::Int, TypeClass::Int) => true,
        // Floats decode integer columns; NOT the reverse.
        (TypeClass::Float, TypeClass::Float | TypeClass::Int) => true,
        // DateTime also parses RFC-3339 strings.
        (TypeClass::DateTime, TypeClass::DateTime | TypeClass::Str) => true,
        // String fields also render temporal columns as RFC-3339 text —
        // mirror of the decoder's `String` arm.
        (TypeClass::Str, TypeClass::DateTime) => true,
        (TypeClass::Map, TypeClass::Map) => true,
        (TypeClass::List(e), TypeClass::List(a)) => kinds_compatible_elem(e, a),
        _ => false,
    }
}

fn kinds_compatible_elem(e: &TypeClassHelper, a: &TypeClassHelper) -> bool {
    // Element-wise, floats accept ints (same as scalars).
    matches!(
        (e.0, a.0),
        (TypeClass::Str, TypeClass::Str)
            | (TypeClass::Bool, TypeClass::Bool)
            | (TypeClass::Int, TypeClass::Int)
            | (TypeClass::Float, TypeClass::Float | TypeClass::Int)
    )
}

/// Type-level drift: DTO wire columns whose expected type cannot decode the
/// introspected column type (`Vec<(wire, expected, actual)>`). Names are
/// checked separately by [`dto_wires_missing`].
pub fn dto_kind_drift(
    kinds: &[(&str, &str)],
    schema: &Schema,
    type_name: &str,
) -> Vec<(String, String, String)> {
    let Some(ty) = schema.type_(type_name) else {
        return Vec::new();
    };
    kinds
        .iter()
        .filter_map(|(wire, expected)| {
            let prop = ty.properties.get(*wire)?;
            let actual = prop.type_name.as_str();
            (!kinds_compatible(expected, actual)).then(|| {
                (
                    (*wire).to_string(),
                    expected.to_string(),
                    actual.to_string(),
                )
            })
        })
        .collect()
}

/// Integration-tier assert: the DTO's expected column types can decode what
/// the live schema actually stores. Companion to [`assert_dto_wires`].
pub async fn assert_dto_kinds(
    client: &ArcadeDbClient,
    db: &str,
    type_name: &str,
    kinds: &[(&str, &str)],
) -> Result<()> {
    let schema = fetch_actual(client, db).await?;
    let drift = dto_kind_drift(kinds, &schema, type_name);
    if drift.is_empty() {
        return Ok(());
    }
    let details: Vec<String> = drift
        .iter()
        .map(|(w, e, a)| format!("`{w}`: expects {e}, column is {a}"))
        .collect();
    Err(MigrateError::Drift {
        message: format!(
            "DTO kind drift on `{type_name}` in `{db}`: {} — the decoder would          reject these columns at runtime",
            details.join("; ")
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::model::{Property, Type};

    fn schema_with(type_name: &str, props: &[&str]) -> Schema {
        use crate::schema::model::TypeKind;
        let mut schema = Schema::new();
        let ty = Type {
            name: type_name.to_string(),
            kind: TypeKind::Document,
            extends: Vec::new(),
            clause: Default::default(),
            properties: props
                .iter()
                .map(|p| {
                    (
                        p.to_string(),
                        Property {
                            name: p.to_string(),
                            type_name: "STRING".to_string(),
                            constraints: Default::default(),
                        },
                    )
                })
                .collect(),
            indexes: Vec::new(),
            timeseries: None,
        };
        schema.types.insert(type_name.to_string(), ty);
        schema
    }

    #[test]
    fn missing_wires_are_the_difference() {
        let schema = schema_with(
            "user_shelf",
            &["item_id", "minutes_total", "last_seen"],
        );
        assert!(dto_wires_missing(
            &["item_id", "minutes_total", "last_seen"],
            &schema,
            "user_shelf"
        )
        .is_empty());

        // Extra DB columns are fine (partial DTOs)…
        assert!(dto_wires_missing(&["item_id"], &schema, "user_shelf").is_empty());

        // `@`-prefixed wires are intrinsics — never schema columns.
        assert!(dto_wires_missing(&["@rid", "item_id"], &schema, "user_shelf").is_empty());

        // …DTO columns missing from the DB are drift.
        assert_eq!(
            dto_wires_missing(&["item_id", "synced_at"], &schema, "user_shelf"),
            vec!["synced_at"]
        );
    }

    #[test]
    fn compat_matrix_matches_decoder_tolerance() {
        // Integers cross widths.
        assert!(kinds_compatible("LONG", "INTEGER"));
        assert!(kinds_compatible("INTEGER", "LONG"));
        // Floats accept ints; ints reject floats.
        assert!(kinds_compatible("DOUBLE", "LONG"));
        assert!(!kinds_compatible("LONG", "DOUBLE"));
        // The server's ARRAY_OF_* spelling is the same list class.
        assert!(kinds_compatible("LIST OF DOUBLE", "ARRAY_OF_FLOATS"));
        assert!(kinds_compatible("LIST OF INTEGER", "ARRAY_OF_INTEGERS"));
        // SET OF columns decode identically (both are wire ListValue), so a
        // set-backed DTO field declaring LIST OF stays compatible.
        assert!(kinds_compatible("LIST OF INTEGER", "SET OF INTEGER"));
        assert!(kinds_compatible("LIST OF STRING", "SET_OF_STRINGS"));
        assert!(!kinds_compatible("LIST OF STRING", "ARRAY_OF_INTEGERS"));
        // Cross-class is drift.
        assert!(!kinds_compatible("STRING", "LONG"));
        assert!(!kinds_compatible("BOOLEAN", "INTEGER"));
        // DateTime tolerates string columns (RFC-3339 parse path)…
        assert!(kinds_compatible("DATETIME", "STRING"));
        // …and String fields render temporal columns as RFC-3339 text.
        assert!(kinds_compatible("STRING", "DATETIME"));
        // Unknown server types never false-positive.
        assert!(kinds_compatible("STRING", "WHATEVER"));
    }

    #[test]
    fn kind_drift_reports_only_incompatible_columns() {
        let schema = schema_with_typed(
            "t",
            &[
                ("a", "INTEGER"),
                ("b", "LONG"),
                ("c", "STRING"),
                ("d", "ARRAY_OF_INTEGERS"),
            ],
        );
        let kinds = [
            ("a", "LONG"),            // compatible (int widths)
            ("b", "LONG"),            // exact
            ("c", "LONG"),            // DRIFT: string column, long field
            ("d", "LIST OF INTEGER"), // compatible (ARRAY_OF spelling)
            ("ghost", "LONG"),        // name-level — not kind drift's business
        ];
        let drift = dto_kind_drift(&kinds, &schema, "t");
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].0, "c");
        assert_eq!(drift[0].1, "LONG");
        assert_eq!(drift[0].2, "STRING");
    }

    fn schema_with_typed(type_name: &str, props: &[(&str, &str)]) -> Schema {
        use crate::schema::model::{Property, Type, TypeKind};
        let mut schema = Schema::new();
        let ty = Type {
            name: type_name.to_string(),
            kind: TypeKind::Document,
            extends: Vec::new(),
            clause: Default::default(),
            properties: props
                .iter()
                .map(|(p, t)| {
                    (
                        p.to_string(),
                        Property {
                            name: p.to_string(),
                            type_name: t.to_string(),
                            constraints: Default::default(),
                        },
                    )
                })
                .collect(),
            indexes: Vec::new(),
            timeseries: None,
        };
        schema.types.insert(type_name.to_string(), ty);
        schema
    }

    #[test]
    fn missing_type_is_total_drift() {
        let schema = Schema::new();
        assert_eq!(
            dto_wires_missing(&["a", "b"], &schema, "ghost"),
            vec!["a", "b"]
        );
    }
}
