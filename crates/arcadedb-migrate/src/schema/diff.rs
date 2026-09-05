//! Diff desired-vs-actual schema → a list of reconciliation [`DiffAction`]s.
//!
//! The diff is additive-first: missing types/properties/indexes become
//! `Create*` actions. Destructive `Drop*` actions are opt-in via
//! [`DropStrategy`], and renames are NEVER inferred (they look like drop+add —
//! that's what override migrations exist for).
//!
//! Property constraints are compared field-by-field and each change becomes a
//! typed `ALTER PROPERTY` action (one constraint per statement, matching the
//! engine's ALTER grammar). Constraint removals follow the engine's reality:
//!
//! - booleans (`mandatory`/`notnull`/`readonly`/`external`): set with `true`,
//!   cleared with `false` → always reconciled.
//! - `min`/`max`/`regexp`: settable, but there is **no unset form**
//!   → changed values are reconciled, removals are silently skipped (an
//!   override's job).
//! - `default` (see the module doc in [`super::model`] for the full
//!   three-way rule): the engine resolves expressions at create time, so
//!   desired *text* can never equal introspected *value*. Presence is
//!   reconciled (`default <expr>` / `default null`), and when both sides *and*
//!   the recorded [`SchemaSnapshot`] have a default, the source text and the
//!   resolved values are compared against the snapshot: source edits and DB
//!   drift both re-apply the desired expression.
//!
//! Type-level `EXTENDS` diffs to `ALTER TYPE t SUPERTYPE +x` / `-x`. Everything
//! in [`Type::clause`] and [`Index::metadata`] is create-time-only and not
//! diffed (see the [`ddl`](super::ddl) module-for-the-contract docs in [`super::model`]).

use super::ddl::Tail;
use super::model::{
    quote_value, Constraints, DefaultExpr, Index, Property, Schema, SchemaSnapshot, Type, TypeKind,
};

/// A single reconciliation action, renderable to DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffAction {
    /// Create a type that doesn't exist yet. `extends` is the structural
    /// super-type list; `clause` is the create-time-only clause tail
    /// (`BUCKETS n`, ...) re-emitted so a fresh DB gets the same declaration
    /// as the .sql file.
    CreateType {
        name: String,
        kind: TypeKind,
        extends: Vec<String>,
        clause: Tail,
    },
    /// Add a property to an existing type (or a freshly created one).
    CreateProperty { type_name: String, prop: Property },
    /// Create an index.
    CreateIndex { type_name: String, index: Index },
    /// Change a property's data type. The migrator never renders this to SQL —
    /// ArcadeDB cannot retype in place, so `Migrator::sync` converts the
    /// action into a warning directing the operator to write a rebuild
    /// override. It exists as a typed action so drift is *detected* (and
    /// classified destructive) rather than silently ignored.
    AlterPropertyType { type_name: String, prop: Property },
    /// Set one property constraint. Renders
    /// `ALTER PROPERTY {type}.{prop} {constraint} {value}` — the engine takes
    /// one attribute per ALTER statement, so each changed constraint is its
    /// own action. `constraint` is the engine keyword (`mandatory`, `notnull`,
    /// `readonly`, `external`, `min`, `max`, `regexp`); `value` the canonical
    /// new value (`true`/`false` or a bare/quote-wrapped string).
    AlterPropertyConstraint {
        type_name: String,
        property_name: String,
        constraint: &'static str,
        value: String,
    },
    /// Reconcile a property `DEFAULT` by **presence** (values never match —
    /// the engine resolves expressions). `None` means "remove" →
    /// `ALTER PROPERTY t.x default null`; `Some(expr)` → `default <expr>`.
    AlterPropertyDefault {
        type_name: String,
        property_name: String,
        expr: Option<DefaultExpr>,
    },
    /// Reconcile the super-type list (`EXTENDS`): `to_add` renders
    /// `ALTER TYPE t SUPERTYPE +a`, `to_remove` → `-r` (one statement each).
    AlterTypeSupers {
        type_name: String,
        to_add: Vec<String>,
        to_remove: Vec<String>,
    },
    /// Drop a type that exists in the DB but not the desired schema. The kind
    /// selects the drop form — TimeSeries types require their dedicated
    /// `DROP TIMESERIES TYPE` statement.
    DropType { name: String, kind: TypeKind },
    /// Drop a property.
    DropProperty { type_name: String, name: String },
    /// Drop an index.
    DropIndex { type_name: String, index: Index },
}

impl DiffAction {
    /// Whether this action is destructive (removes data / schema objects).
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            DiffAction::DropType { .. }
                | DiffAction::DropProperty { .. }
                | DiffAction::DropIndex { .. }
                | DiffAction::AlterPropertyType { .. }
        )
    }

    /// Render to ArcadeDB DDL.
    pub fn to_sql(&self) -> String {
        match self {
            DiffAction::CreateType {
                name,
                kind,
                extends,
                clause,
            } => {
                let extends_sql = if extends.is_empty() {
                    String::new()
                } else {
                    format!(" EXTENDS {}", extends.join(", "))
                };
                format!(
                    "CREATE {kw} TYPE {name} IF NOT EXISTS{extends_sql}{suffix}",
                    kw = kind.ddl_keyword(),
                    suffix = clause.render_suffix(),
                )
            }
            DiffAction::CreateProperty { type_name, prop } => {
                format!(
                    "CREATE PROPERTY {type_name}.{prop_name} IF NOT EXISTS {clause}",
                    prop_name = prop.name,
                    clause = prop.type_clause(),
                )
            }
            DiffAction::CreateIndex { type_name, index } => render_create_index(type_name, index),
            DiffAction::AlterPropertyType { type_name, prop } => {
                // TYPE changes only; the constraints are reconciled by their
                // own AlterProperty* actions.
                format!(
                    "ALTER PROPERTY {type_name}.{prop_name} TYPE {ty}",
                    prop_name = prop.name,
                    ty = prop.type_name
                )
            }
            DiffAction::AlterPropertyConstraint {
                type_name,
                property_name,
                constraint,
                value,
            } => format!("ALTER PROPERTY {type_name}.{property_name} {constraint} {value}"),
            DiffAction::AlterPropertyDefault {
                type_name,
                property_name,
                expr,
            } => match expr {
                Some(e) => format!(
                    "ALTER PROPERTY {type_name}.{property_name} default {}",
                    e.text()
                ),
                None => format!("ALTER PROPERTY {type_name}.{property_name} default null"),
            },
            DiffAction::AlterTypeSupers {
                type_name,
                to_add,
                to_remove,
            } => {
                let mut stmts = Vec::new();
                for s in to_add {
                    stmts.push(format!("ALTER TYPE {type_name} SUPERTYPE +{s}"));
                }
                for s in to_remove {
                    stmts.push(format!("ALTER TYPE {type_name} SUPERTYPE -{s}"));
                }
                stmts.join("\n")
            }
            DiffAction::DropType { name, kind } => match kind {
                TypeKind::Timeseries => format!("DROP TIMESERIES TYPE IF EXISTS {name}"),
                _ => format!("DROP TYPE {name} IF EXISTS UNSAFE"),
            },
            DiffAction::DropProperty { type_name, name } => {
                format!("DROP PROPERTY {type_name}.{name} IF EXISTS FORCE")
            }
            DiffAction::DropIndex { type_name, index } => {
                // Auto-named indexes are referenced as `Type[col]`; named ones
                // by their name. Backtick-quote to handle the bracket form.
                let ident = index
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{type_name}[{}]", index.columns.join(",")));
                format!("DROP INDEX `{ident}` IF EXISTS")
            }
        }
    }
}

/// Render a `CREATE INDEX` statement matching the parser's input form.
fn render_create_index(type_name: &str, index: &Index) -> String {
    let cols = index.columns.join(", ");
    let name_clause = index
        .name
        .as_deref()
        .map(|n| format!("{n} "))
        .unwrap_or_default();
    let mut sql = format!(
        "CREATE INDEX {name_clause}IF NOT EXISTS ON {type_name} ({cols}) {kind}",
        kind = index.kind.ddl_keyword()
    );
    if let Some(meta) = index.metadata() {
        sql.push_str(&format!(" METADATA {{ {meta} }}"));
    }
    sql
}

/// How to treat objects present in the DB but absent from the desired schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DropStrategy {
    /// Never emit drop actions. The safest default — leftover objects are left
    /// alone. Handles the common additive-change case fully.
    #[default]
    Never,
    /// Emit drop actions for objects in actual-but-not-desired. The migrator
    /// surfaces these for explicit confirmation (`--apply-destructive`).
    Explicit,
}

/// Compute the diff: what DDL actions reconcile `actual` toward `desired`.
///
/// `ignored_types` marks schema objects owned by override migrations (derived
/// from their recorded statements): an override-created helper type is
/// intentional, not drift, so under [`DropStrategy::Explicit`] it is NOT
/// dropped. Long-lived objects should still be declared in the desired schema;
/// overrides should only rebuild/rename objects the schema also declares.
pub fn diff(desired: &Schema, actual: &Schema, drop_strategy: DropStrategy) -> Vec<DiffAction> {
    diff_with_snapshot(desired, actual, None, drop_strategy, &Default::default())
}

/// [`diff`], with the recorded applied-state [`SchemaSnapshot`] available for
/// three-way default reconciliation (and, in the future, any other field where
/// desired-text cannot be compared to introspected-value directly), and the
/// override-owned type names that must survive `DropStrategy::Explicit`. See
/// the module doc in [`super::model`] for the default rule.
pub fn diff_with_snapshot(
    desired: &Schema,
    actual: &Schema,
    snapshot: Option<&SchemaSnapshot>,
    drop_strategy: DropStrategy,
    ignored_types: &std::collections::BTreeSet<String>,
) -> Vec<DiffAction> {
    let mut actions = Vec::new();

    for (name, desired_ty) in &desired.types {
        match actual.types.get(name) {
            None => {
                // Whole type missing → create it. For TimeSeries types the
                // columns live inside the CREATE (spec), so no separate
                // property/index actions are emitted.
                actions.push(DiffAction::CreateType {
                    name: name.clone(),
                    kind: desired_ty.kind,
                    extends: desired_ty.extends.clone(),
                    clause: if desired_ty.kind == TypeKind::Timeseries {
                        Tail::from_text(
                            &desired_ty
                                .timeseries
                                .as_ref()
                                .map(|spec| spec.render_suffix())
                                .unwrap_or_default(),
                        )
                    } else {
                        desired_ty.clause_tail().clone()
                    },
                });
                if desired_ty.kind != TypeKind::Timeseries {
                    for prop in desired_ty.properties.values() {
                        actions.push(DiffAction::CreateProperty {
                            type_name: name.clone(),
                            prop: prop.clone(),
                        });
                    }
                    for idx in &desired_ty.indexes {
                        actions.push(DiffAction::CreateIndex {
                            type_name: name.clone(),
                            index: idx.clone(),
                        });
                    }
                }
            }
            Some(actual_ty) => {
                if desired_ty.kind == TypeKind::Timeseries || actual_ty.kind == TypeKind::Timeseries
                {
                    // TimeSeries declarations have no ALTER forms at all —
                    // structural reconciliation is impossible; spec drift is
                    // surfaced as a rebuild warning by the migrator (see
                    // `diff_statements`). Emit nothing structurally.
                    continue;
                }
                // Type exists — reconcile its super-types, properties + indexes.
                diff_supers(&mut actions, name, desired_ty, actual_ty);
                diff_properties(&mut actions, name, desired_ty, actual_ty, snapshot);
                diff_indexes(&mut actions, name, desired_ty, actual_ty);
            }
        }
    }

    if matches!(drop_strategy, DropStrategy::Explicit) {
        for (name, actual_ty) in &actual.types {
            if !desired.types.contains_key(name) {
                if ignored_types.contains(name) {
                    // Override-owned: intentional, not drift. Never dropped.
                    continue;
                }
                actions.push(DiffAction::DropType {
                    name: name.clone(),
                    kind: actual_ty.kind,
                });
            }
        }
    }

    actions
}

/// Reconcile the super-type list: adds and removals become separate
/// `ALTER TYPE t SUPERTYPE +x` / `-x` actions.
fn diff_supers(actions: &mut Vec<DiffAction>, name: &str, desired: &Type, actual: &Type) {
    let to_add: Vec<String> = desired
        .extends
        .iter()
        .filter(|e| !actual.extends.contains(e))
        .cloned()
        .collect();
    let to_remove: Vec<String> = actual
        .extends
        .iter()
        .filter(|e| !desired.extends.contains(e))
        .cloned()
        .collect();
    if !to_add.is_empty() || !to_remove.is_empty() {
        actions.push(DiffAction::AlterTypeSupers {
            type_name: name.to_string(),
            to_add,
            to_remove,
        });
    }
}

fn diff_properties(
    actions: &mut Vec<DiffAction>,
    type_name: &str,
    desired: &Type,
    actual: &Type,
    snapshot: Option<&SchemaSnapshot>,
) {
    for (pname, desired_prop) in &desired.properties {
        match actual.properties.get(pname) {
            None => actions.push(DiffAction::CreateProperty {
                type_name: type_name.to_string(),
                prop: desired_prop.clone(),
            }),
            Some(actual_prop) => {
                // Type mismatch — surface as an explicit alter (dangerous; the
                // migrator requires confirmation for is_destructive() actions).
                if actual_prop.type_name != desired_prop.type_name {
                    actions.push(DiffAction::AlterPropertyType {
                        type_name: type_name.to_string(),
                        prop: desired_prop.clone(),
                    });
                }
                // Constraint drift, independent of the type change.
                diff_constraints(
                    actions,
                    type_name,
                    pname,
                    &desired_prop.constraints,
                    &actual_prop.constraints,
                    snapshot,
                );
            }
        }
    }
    // Property drops are intentionally not emitted here even under Explicit —
    // dropping a property requires FORCE and is almost always better done via
    // an override. Surfaced only if/when a real need appears.
}

/// Per-constraint diff. See the module doc for the engine-grounding of each
/// reconcile/skip decision.
fn diff_constraints(
    actions: &mut Vec<DiffAction>,
    type_name: &str,
    property_name: &str,
    d: &Constraints,
    a: &Constraints,
    snapshot: Option<&SchemaSnapshot>,
) {
    // Booleans: set with true, cleared with false.
    for (kw, d_val, a_val) in [
        ("mandatory", d.mandatory, a.mandatory),
        ("notnull", d.not_null, a.not_null),
        ("readonly", d.readonly, a.readonly),
        ("external", d.external, a.external),
    ] {
        if d_val != a_val {
            actions.push(DiffAction::AlterPropertyConstraint {
                type_name: type_name.to_string(),
                property_name: property_name.to_string(),
                constraint: kw,
                value: d_val.to_string(),
            });
        }
    }

    // String constraints: changed → ALTER (the value is re-quoted on render if
    // needed). Removed in desired → skipped: the engine has no unset form for
    // min/max/regexp (`min null` errors), so removing one is an override's job.
    for (kw, d_val, a_val) in [
        ("min", &d.min, &a.min),
        ("max", &d.max, &a.max),
        ("regexp", &d.regexp, &a.regexp),
    ] {
        if let Some(dv) = d_val {
            if Some(dv) != a_val.as_ref() {
                actions.push(DiffAction::AlterPropertyConstraint {
                    type_name: type_name.to_string(),
                    property_name: property_name.to_string(),
                    constraint: kw,
                    value: quote_value(dv),
                });
            }
        }
    }

    // default — three-way, see the model module doc. Removal in desired →
    // `default null` (the engine's unset form); absence in
    // actual → set. When both present, the snapshot is the only reference that
    // distinguishes "the source changed" from "the DB drifted" from "nothing
    // happened":
    match (&d.default, &a.default) {
        (Some(de), None) => actions.push(DiffAction::AlterPropertyDefault {
            type_name: type_name.to_string(),
            property_name: property_name.to_string(),
            expr: Some(de.clone()),
        }),
        (None, Some(_)) => actions.push(DiffAction::AlterPropertyDefault {
            type_name: type_name.to_string(),
            property_name: property_name.to_string(),
            expr: None,
        }),
        (Some(de), Some(_)) => {
            // Both present. With no snapshot record (first sync backfilling the
            // feature), the presence-only case: leave an existing default alone.
            let Some(snap) = snapshot else { return };
            let resolved = snap.resolved_default(type_name, property_name);
            // Expression defaults (`date()`, `uuid()`, …) compare by source
            // text only: their introspected form is engine-version-dependent
            // (26.9.1 echoes the stored text), so a value comparison is not
            // a stable settle signal — drift only when the .sql edited the
            // expression. Literal defaults do compare
            // by value (a manual `ALTER PROPERTY` drifts `42` → `99`,
            // restore from source).
            let is_expression = de.text().contains('(');
            let drifted = !is_expression
                && (resolved.is_none() || a.default.as_ref().map(|v| v.text()) != resolved);
            // Source change: the .sql edited the expression text since we last
            // applied it (e.g. `date()` → `date('2020-01-01')`).
            let source_changed =
                snap.applied_default_expr(type_name, property_name) != Some(de.text());
            if drifted || source_changed {
                actions.push(DiffAction::AlterPropertyDefault {
                    type_name: type_name.to_string(),
                    property_name: property_name.to_string(),
                    expr: Some(de.clone()),
                });
            }
        }
        (None, None) => {}
    }
}

fn diff_indexes(actions: &mut Vec<DiffAction>, type_name: &str, desired: &Type, actual: &Type) {
    for desired_idx in &desired.indexes {
        let exists = actual.indexes.iter().any(|a| indexes_match(a, desired_idx));
        if !exists {
            actions.push(DiffAction::CreateIndex {
                type_name: type_name.to_string(),
                index: desired_idx.clone(),
            });
        }
    }
    // Index drops (like property drops) are intentionally conservative — use
    // an override to drop an index deliberately.
}

/// Do two indexes refer to the same logical index? Named indexes match by
/// name; unnamed ones match by (columns, kind).
fn indexes_match(a: &Index, b: &Index) -> bool {
    match (&a.name, &b.name) {
        (Some(an), Some(bn)) => an == bn,
        _ => a.columns == b.columns && a.kind == b.kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::parser;
    use crate::schema::IndexKind;
    use crate::schema::SchemaSnapshot;

    fn schema_from(ddl: &str) -> Schema {
        parser::parse(ddl).unwrap()
    }

    /// Build the recorded-state half of a snapshot for type `t`, property `x`:
    /// `resolved` is what the engine surfaced after our last apply; `expr` is
    /// the desired `DEFAULT` source text we applied then.
    fn snapshot_with(resolved: Option<&str>, expr: Option<&str>) -> SchemaSnapshot {
        let mut state = Schema::new();
        let c = Constraints {
            default: resolved.map(DefaultExpr::new),
            ..Constraints::default()
        };
        state
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "STRING".into(),
                    constraints: c,
                },
            );
        SchemaSnapshot {
            checksum: "abc".into(),
            state,
            default_exprs: expr
                .map(|e| [("t.x".to_string(), e.to_string())].into_iter().collect())
                .unwrap_or_default(),
        }
    }

    fn diff_with(
        desired: &Schema,
        actual: &Schema,
        snapshot: Option<&SchemaSnapshot>,
    ) -> Vec<String> {
        diff_with_snapshot(
            desired,
            actual,
            snapshot,
            DropStrategy::Never,
            &Default::default(),
        )
        .iter()
        .map(DiffAction::to_sql)
        .collect()
    }

    #[test]
    fn empty_actual_creates_everything() {
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING;",
        );
        let actions = diff(&desired, &Schema::new(), DropStrategy::Never);
        assert!(actions
            .iter()
            .any(|a| matches!(a, DiffAction::CreateType { name, .. } if name == "t")));
        assert!(actions.iter().any(|a| matches!(a, DiffAction::CreateProperty { type_name, prop } if type_name == "t" && prop.name == "x")));
    }

    #[test]
    fn matching_actual_is_noop() {
        let desired = schema_from("CREATE DOCUMENT TYPE t IF NOT EXISTS;");
        let mut actual = Schema::new();
        actual.type_or_insert("t", TypeKind::Document);
        let actions = diff(&desired, &actual, DropStrategy::Never);
        assert!(actions.is_empty(), "expected no actions, got {actions:?}");
    }

    #[test]
    fn additive_property_detected() {
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING;
             CREATE PROPERTY t.y IF NOT EXISTS INTEGER;",
        );
        let mut actual = Schema::new();
        let ty = actual.type_or_insert("t", TypeKind::Document);
        ty.properties.insert(
            "x".into(),
            Property {
                name: "x".into(),
                type_name: "STRING".into(),
                constraints: Constraints::default(),
            },
        );
        let actions = diff(&desired, &actual, DropStrategy::Never);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            DiffAction::CreateProperty { type_name, prop } if type_name == "t" && prop.name == "y"
        ));
    }

    #[test]
    fn drop_strategy_never_leaves_leftovers() {
        let desired = Schema::new();
        let mut actual = Schema::new();
        actual.type_or_insert("leftover", TypeKind::Document);
        let actions = diff(&desired, &actual, DropStrategy::Never);
        assert!(actions.is_empty(), "Never should not drop, got {actions:?}");
    }

    #[test]
    fn drop_strategy_explicit_surfaces_leftover_type() {
        let desired = Schema::new();
        let mut actual = Schema::new();
        actual.type_or_insert("leftover", TypeKind::Document);
        let actions = diff(&desired, &actual, DropStrategy::Explicit);
        assert!(actions
            .iter()
            .any(|a| matches!(a, DiffAction::DropType { name, .. } if name == "leftover")));
    }

    #[test]
    fn unnamed_index_matched_by_columns_and_kind() {
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE INDEX ON t(title) FULL_TEXT;",
        );
        let mut actual = Schema::new();
        let ty = actual.type_or_insert("t", TypeKind::Document);
        ty.indexes.push(Index {
            name: Some("t[title]".into()), // introspection auto-names it
            columns: vec!["title".into()],
            kind: IndexKind::FullText,
            unique: false,
            tail: Tail::none(),
        });
        let actions = diff(&desired, &actual, DropStrategy::Never);
        assert!(
            actions.is_empty(),
            "unnamed index should match the auto-named one, got {actions:?}"
        );
    }

    #[test]
    fn create_index_sql_roundtrips_metadata() {
        let action = DiffAction::CreateIndex {
            type_name: "item".into(),
            index: Index {
                name: Some("idx_sparse".into()),
                columns: vec!["tokens".into(), "weights".into()],
                kind: IndexKind::LsmSparseVector,
                unique: false,
                tail: Tail::from_text("\"dimensions\": 20000"),
            },
        };
        let sql = action.to_sql();
        assert!(sql.contains(
            "CREATE INDEX idx_sparse IF NOT EXISTS ON item (tokens, weights) LSM_SPARSE_VECTOR"
        ));
        assert!(sql.contains("METADATA"));
        assert!(sql.contains("\"dimensions\": 20000"));
    }

    #[test]
    fn drop_index_with_name_renders_name() {
        let action = DiffAction::DropIndex {
            type_name: "item".into(),
            index: Index {
                name: Some("idx_external_id".into()),
                columns: vec!["external_id".into()],
                kind: IndexKind::Unique,
                unique: true,
                tail: Tail::none(),
            },
        };
        assert_eq!(action.to_sql(), "DROP INDEX `idx_external_id` IF EXISTS");
    }

    #[test]
    fn drop_index_unnamed_renders_type_bracket_columns() {
        // ArcadeDB auto-names unnamed indexes as `Type[col]`, so the DROP must
        // reference exactly that form (regression: this used to render without
        // the type prefix — `DROP INDEX `[col]`` — which hits the wrong index).
        let action = DiffAction::DropIndex {
            type_name: "item".into(),
            index: Index {
                name: None,
                columns: vec!["title".into(), "year".into()],
                kind: IndexKind::FullText,
                unique: false,
                tail: Tail::none(),
            },
        };
        assert_eq!(action.to_sql(), "DROP INDEX `item[title,year]` IF EXISTS");
    }

    #[test]
    fn constraint_drift_emits_typed_alters() {
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING (MANDATORY true, NOTNULL true, MIN 5, REGEXP \"^[a-z]+\");",
        );
        // Actual: same property, constraints off (MANDATORY unset, MIN differs,
        // regexp set).
        let mut actual = Schema::new();
        let ty = actual.type_or_insert("t", TypeKind::Document);
        let c = Constraints {
            min: Some("1".into()),
            regexp: Some("^[a-z]+".into()),
            ..Constraints::default()
        };
        ty.properties.insert(
            "x".into(),
            Property {
                name: "x".into(),
                type_name: "STRING".into(),
                constraints: c,
            },
        );

        let actions = diff(&desired, &actual, DropStrategy::Never);
        let sql: Vec<String> = actions.iter().map(DiffAction::to_sql).collect();
        assert!(
            sql.contains(&"ALTER PROPERTY t.x mandatory true".to_string()),
            "got {sql:?}"
        );
        assert!(
            sql.contains(&"ALTER PROPERTY t.x notnull true".to_string()),
            "got {sql:?}"
        );
        assert!(
            sql.contains(&"ALTER PROPERTY t.x min 5".to_string()),
            "got {sql:?}"
        );
        assert!(
            !sql.iter().any(|s| s.contains("regexp")),
            "unchanged regexp must not alter: {sql:?}"
        );
        // None of these are destructive.
        assert!(actions.iter().all(|a| !a.is_destructive()));
    }

    #[test]
    fn boolean_constraint_cleared_with_false() {
        // Desired: property x with no mandatory (false). Actual: x is mandatory.
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING;",
        );
        let mut actual = Schema::new();
        let ty = actual.type_or_insert("t", TypeKind::Document);
        let c = Constraints {
            mandatory: true,
            ..Constraints::default()
        };
        ty.properties.insert(
            "x".into(),
            Property {
                name: "x".into(),
                type_name: "STRING".into(),
                constraints: c,
            },
        );
        let actions = diff(&desired, &actual, DropStrategy::Never);
        let sql: Vec<String> = actions.iter().map(DiffAction::to_sql).collect();
        assert!(
            sql.contains(&"ALTER PROPERTY t.x mandatory false".to_string()),
            "got {sql:?}"
        );
    }

    #[test]
    fn default_presence_reconciled_both_ways() {
        // Desired has a default; actual property (same type) has none → set it.
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING (DEFAULT \"user\");",
        );
        let mut actual = Schema::new();
        actual
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "STRING".into(),
                    constraints: Constraints::default(),
                },
            );
        let actions = diff(&desired, &actual, DropStrategy::Never);
        let sql: Vec<String> = actions.iter().map(DiffAction::to_sql).collect();
        assert!(
            sql.contains(&"ALTER PROPERTY t.x default \"user\"".to_string()),
            "got {sql:?}"
        );

        // Desired keeps the property but dropped the DEFAULT attribute; actual
        // still has one → remove it.
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING;",
        );
        let mut actual = Schema::new();
        let c = Constraints {
            default: Some(DefaultExpr::new("admin")),
            ..Constraints::default()
        };
        actual
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "STRING".into(),
                    constraints: c,
                },
            );
        let actions = diff(&desired, &actual, DropStrategy::Never);
        let sql: Vec<String> = actions.iter().map(DiffAction::to_sql).collect();
        assert!(
            sql.contains(&"ALTER PROPERTY t.x default null".to_string()),
            "got {sql:?}"
        );
    }

    #[test]
    fn both_present_unchanged_default_is_noop() {
        // Desired, actual and snapshot all agree the default is `42`: nothing
        // to do — this is the steady state after a literal default was applied.
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING (DEFAULT 42);",
        );
        let mut actual = Schema::new();
        let c = Constraints {
            default: Some(DefaultExpr::new("42")),
            ..Constraints::default()
        };
        actual
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "STRING".into(),
                    constraints: c,
                },
            );
        let snap = snapshot_with(Some("42"), Some("42"));
        let sql = diff_with(&desired, &actual, Some(&snap));
        assert!(
            sql.is_empty(),
            "settled default must not alter, got {sql:?}"
        );
    }

    #[test]
    fn source_default_edit_reapplies_the_expression() {
        // The .sql changed the default expression (e.g. `date()` →
        // `date('2020-01-01')`). Desired and actual both have a default, the
        // DB value is unchanged — only the snapshot's recorded *source text*
        // reveals the edit.
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS DATETIME (DEFAULT date('2020-01-01'));",
        );
        let mut actual = Schema::new();
        let resolved = DefaultExpr::new("2020-08-09T00:00:00+00:00");
        let c = Constraints {
            default: Some(resolved),
            ..Constraints::default()
        };
        actual
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "DATETIME".into(),
                    constraints: c,
                },
            );
        let snap = snapshot_with(Some("2020-08-09T00:00:00+00:00"), Some("date()"));
        let sql = diff_with(&desired, &actual, Some(&snap));
        assert!(
            sql.contains(&"ALTER PROPERTY t.x default date('2020-01-01')".to_string()),
            "source default edit must re-apply, got {sql:?}"
        );
    }

    #[test]
    fn expression_default_with_changing_value_is_settled() {
        // `date()` is re-resolved by the engine on every introspection: the
        // actual DB value (15:00) differs from the snapshot's recorded value
        // (14:58) even though nothing changed. Value drift must NOT re-fire the
        // ALTER — only a .sql edit (source text change) should.
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS DATETIME (DEFAULT date());",
        );
        let mut actual = Schema::new();
        let c = Constraints {
            default: Some(DefaultExpr::new("2026-08-20T15:00:00+00:00")),
            ..Constraints::default()
        };
        actual
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "DATETIME".into(),
                    constraints: c,
                },
            );
        let snap = snapshot_with(Some("2026-08-20T14:58:00+00:00"), Some("date()"));
        let sql = diff_with(&desired, &actual, Some(&snap));
        assert!(
            sql.is_empty(),
            "expression default must settle despite the re-resolved value, got {sql:?}"
        );
    }

    #[test]
    fn db_default_drift_restores_from_source() {
        // Someone manually changed the DB default (e.g. `ALTER PROPERTY`).
        // Desired and snapshot both say 42; actual resolved to 99 → restore.
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS INTEGER (DEFAULT 42);",
        );
        let mut actual = Schema::new();
        let c = Constraints {
            default: Some(DefaultExpr::new("99")),
            ..Constraints::default()
        };
        actual
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "INTEGER".into(),
                    constraints: c,
                },
            );
        let snap = snapshot_with(Some("42"), Some("42"));
        let sql = diff_with(&desired, &actual, Some(&snap));
        assert!(
            sql.contains(&"ALTER PROPERTY t.x default 42".to_string()),
            "DB default drift must restore from source, got {sql:?}"
        );
    }

    #[test]
    fn both_present_without_snapshot_is_presence_only() {
        // First sync backfilling the snapshot feature: no record yet → a
        // both-present default is left alone (as pre-snapshot behavior).
        let desired = schema_from(
            "CREATE DOCUMENT TYPE t IF NOT EXISTS;
             CREATE PROPERTY t.x IF NOT EXISTS STRING (DEFAULT \"user\");",
        );
        let mut actual = Schema::new();
        let c = Constraints {
            default: Some(DefaultExpr::new("user")),
            ..Constraints::default()
        };
        actual
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "STRING".into(),
                    constraints: c,
                },
            );
        let sql = diff_with(&desired, &actual, None);
        assert!(
            sql.is_empty(),
            "no snapshot record → presence-only, got {sql:?}"
        );
    }

    #[test]
    fn min_removed_in_desired_is_skipped() {
        let desired = schema_from("CREATE DOCUMENT TYPE t IF NOT EXISTS;");
        let mut actual = Schema::new();
        let c = Constraints {
            min: Some("1".into()),
            ..Constraints::default()
        };
        actual
            .type_or_insert("t", TypeKind::Document)
            .properties
            .insert(
                "x".into(),
                Property {
                    name: "x".into(),
                    type_name: "STRING".into(),
                    constraints: c,
                },
            );
        let actions = diff(&desired, &actual, DropStrategy::Never);
        assert!(
            actions.is_empty(),
            "min removal must be skipped, got {actions:?}"
        );
    }

    #[test]
    fn extends_drift_emits_supers_alters() {
        let desired = schema_from("CREATE VERTEX TYPE A EXTENDS B, C IF NOT EXISTS;");
        let mut actual = Schema::new();
        let ty = actual.type_or_insert("A", TypeKind::Vertex);
        ty.extends = vec!["B".into(), "D".into()];
        let actions = diff(&desired, &actual, DropStrategy::Never);
        assert_eq!(actions.len(), 1);
        let sql = actions[0].to_sql();
        assert!(sql.contains("ALTER TYPE A SUPERTYPE +C"), "got {sql}");
        assert!(sql.contains("ALTER TYPE A SUPERTYPE -D"), "got {sql}");
        assert!(
            !sql.contains("+B"),
            "B present on both sides must not alter: {sql}"
        );
        assert!(!actions[0].is_destructive());
    }

    #[test]
    fn create_type_renders_extends_and_clause() {
        let desired =
            schema_from("CREATE VERTEX TYPE Employee EXTENDS Person IF NOT EXISTS BUCKETS 8;");
        let actions = diff(&desired, &Schema::new(), DropStrategy::Never);
        assert_eq!(
            actions[0].to_sql(),
            "CREATE VERTEX TYPE Employee IF NOT EXISTS EXTENDS Person BUCKETS 8"
        );
    }

    #[test]
    fn min_max_quote_wrap_on_render() {
        let action = DiffAction::AlterPropertyConstraint {
            type_name: "t".into(),
            property_name: "x".into(),
            constraint: "regexp",
            value: quote_value("[A-Za-z ]+"),
        };
        assert_eq!(action.to_sql(), "ALTER PROPERTY t.x regexp \"[A-Za-z ]+\"");
    }
}
