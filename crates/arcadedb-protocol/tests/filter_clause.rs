//! `FilterClause` — conditional WHERE-assembly for dynamic queries (the
//! read-side sibling of `SetClause`).

use arcadedb_protocol::{grpc_value_to_json, FilterClause, Params};
use serde_json::Value;

#[test]
fn empty_clause_collapses_where_sql() {
    let f = FilterClause::new();
    assert!(f.is_empty());
    assert_eq!(f.sql(), "");
    assert_eq!(f.where_sql(), "");
}

#[test]
fn eq_and_comparisons_emit_bound_forms() {
    let f = FilterClause::new()
        .eq("item_id", 42i64)
        .ne("status", "draft")
        .lt("score", 100i32)
        .lte("score", 200i32)
        .gt("score", 0i32)
        .gte("release_date", 2020i32);
    assert_eq!(
        f.sql(),
        "item_id = :item_id AND status <> :status__ne AND score < :score__lt \
         AND score <= :score__lte AND score > :score__gt \
         AND release_date >= :release_date__gte"
    );
    let params = f.into_params().0;
    assert_eq!(params.len(), 6);
    assert_eq!(grpc_value_to_json(&params["item_id"]), Value::from(42));
    assert_eq!(
        grpc_value_to_json(&params["status__ne"]),
        Value::String("draft".into())
    );
}

#[test]
fn between_binds_one_fragment_two_params() {
    let f = FilterClause::new().between("score", 15, 35);
    assert_eq!(f.sql(), "score BETWEEN :score__from AND :score__to");
    let params = f.into_params().0;
    assert_eq!(params.len(), 2);
    assert!(params.contains_key("score__from"));
    assert!(params.contains_key("score__to"));
}

#[test]
fn null_checks_emit_paramless_fragments() {
    let f = FilterClause::new().is_null("prefix").is_not_null("title");
    assert_eq!(f.sql(), "prefix IS NULL AND title IS NOT NULL");
    assert!(f.into_params().0.is_empty());
}

#[test]
fn in_list_binds_one_list_parameter() {
    let f = FilterClause::new().in_list("item_id", vec![1i64, 2, 3]);
    assert_eq!(f.sql(), "item_id IN :item_id__in");
    let params = f.into_params().0;
    assert_eq!(params.len(), 1);
    assert_eq!(
        grpc_value_to_json(&params["item_id__in"]),
        serde_json::json!([1, 2, 3]),
        "the whole list rides ONE param — no literal-list length cap"
    );
}

#[test]
fn not_in_emits_complement_form() {
    let f = FilterClause::new().not_in("status", vec!["hidden", "banned"]);
    assert_eq!(f.sql(), "status NOT IN :status__nin");
    assert_eq!(
        grpc_value_to_json(&f.into_params().0["status__nin"]),
        serde_json::json!(["hidden", "banned"])
    );
}

#[test]
fn contains_family_emits_expected_forms() {
    let f = FilterClause::new()
        .contains("region_codes", "JP")
        .contains_all("label_ids", vec![4, 62])
        .contains_any("label_ids", vec![7, 9])
        .not_contains_any("label_ids", vec![19]);
    assert_eq!(
        f.sql(),
        "region_codes CONTAINS :region_codes__has \
         AND label_ids CONTAINSALL :label_ids__all \
         AND label_ids CONTAINSANY :label_ids__any \
         AND NOT (label_ids CONTAINSANY :label_ids__any_2)"
    );
    let params = f.into_params().0;
    assert_eq!(
        grpc_value_to_json(&params["label_ids__any"]),
        serde_json::json!([7, 9])
    );
    assert_eq!(
        grpc_value_to_json(&params["label_ids__any_2"]),
        serde_json::json!([19]),
        "the second ANY on the same column dedups its param name"
    );
}

#[test]
fn like_and_ilike_bind_patterns() {
    let f = FilterClause::new().like("name", "Luk%").ilike("name", "LU%");
    assert_eq!(
        f.sql(),
        "name LIKE :name__like AND name ILIKE :name__ilike"
    );
}

#[test]
fn raw_fragments_and_conditional_filters() {
    let on = FilterClause::new()
        .filter("is_visible = true AND NOT (limited_status IS NULL)")
        .filter_when(true, "score_pct >= 95");
    assert!(!on.is_empty());
    assert!(on.sql().starts_with("is_visible"));

    let off = FilterClause::new().filter_when(false, "never");
    assert!(off.is_empty());
}

#[test]
fn bind_registers_param_without_fragment() {
    // The static-subquery country gate: the fragment lives in the caller's
    // FROM text, the param rides with the filter's params.
    let f = FilterClause::new().eq("is_visible", true).bind("cc", "JP");
    assert_eq!(f.sql(), "is_visible = :is_visible");
    let params = f.into_params().0;
    assert_eq!(params.len(), 2);
    assert_eq!(grpc_value_to_json(&params["cc"]), Value::String("JP".into()));
}

/// The core safety property, same scan as the SetClause suite: every
/// `:placeholder` emitted by [`FilterClause::sql`] has a matching bound
/// param. (The inverse — params without fragments — is legitimate via
/// [`bind`].)
#[test]
fn every_placeholder_has_a_bound_param() {
    let f = FilterClause::new()
        .filter("is_visible = true")
        .eq("item_id", 1001i64)
        .eq("item_id", 1002i64) // same column twice: dedup path
        .between("score_pct", 80, 99)
        .in_list("label_ids", vec![4i32, 62])
        .contains_all("label_ids", vec![3i32])
        .not_contains_any("label_ids", vec![1i32])
        .is_not_null("title")
        .like("title", "Sp%")
        .bind("cc", "JP");

    for frag in f.sql().split(" AND ") {
        let names: Vec<&str> = frag.split(':').skip(1).collect();
        for name in names {
            let name = name.trim_end_matches(')');
            assert!(
                f.clone().into_params().0.contains_key(name),
                "placeholder `{name}` (in `{frag}`) has no bound param"
            );
        }
    }
    // And the deduped second eq produced a distinct param.
    assert!(f.clone().into_params().0.contains_key("item_id_2"));
}

#[test]
fn same_column_operators_get_distinct_readable_names() {
    let f = FilterClause::new()
        .gte("release_date", 2020)
        .lte("release_date", 2023)
        .eq("release_date", 2021);
    assert_eq!(
        f.sql(),
        "release_date >= :release_date__gte AND release_date <= :release_date__lte \
         AND release_date = :release_date"
    );
    assert_eq!(f.into_params().0.len(), 3);
}

#[test]
fn params_compose_with_set_clause_maps() {
    // UPDATE … SET … WHERE …: merge the two namespaces (generated names
    // never collide across the two builders).
    use arcadedb_protocol::SetClause;
    let set = SetClause::new().set("labels", "red");
    let filter = FilterClause::new().eq("item_key", "red");
    let mut params: std::collections::HashMap<_, _> = set.into_params().0;
    params.extend(filter.into_params().0);
    let merged = Params(params);
    assert_eq!(merged.0.len(), 2);
    assert!(merged.0.contains_key("labels"));
    assert!(merged.0.contains_key("item_key"));
}
