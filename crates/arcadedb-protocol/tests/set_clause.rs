//! `SetClause` — conditional SET-clause assembly for dynamic
//! `INSERT INTO … SET` / `UPDATE … SET` statements.

use arcadedb_protocol::__private::{i32_v, str_v};
use arcadedb_protocol::{grpc_value_to_json, IntoGrpcValue, Params, SetClause};
use serde_json::Value;

#[test]
fn empty_clause() {
    let c = SetClause::new();
    assert!(c.is_empty());
    assert_eq!(c.sql(), "");
}

#[test]
fn single_and_multiple_sets_keep_push_order() {
    let c = SetClause::new()
        .set("completed_at", chrono_now())
        .set("score", 100i64);
    assert!(!c.is_empty());
    // Order follows push order (deterministic SQL for tests/logs).
    assert_eq!(c.sql(), "completed_at = :completed_at, score = :score");
    let params = c.into_params().0;
    assert_eq!(params.len(), 2);
    assert!(params.contains_key("completed_at"));
    assert!(params.contains_key("score"));
}

#[test]
fn set_opt_includes_some_omits_none() {
    let some = SetClause::new().set_opt("score", Some(50i64));
    assert_eq!(some.sql(), "score = :score");

    let none = SetClause::new().set_opt::<i64>("score", None);
    assert!(none.is_empty());
    assert_eq!(none.sql(), "");
}

#[test]
fn set_when_respects_condition() {
    let on = SetClause::new().set_when(true, "audio_model", "tts-x");
    let off = SetClause::new().set_when(false, "audio_model", "tts-x");
    assert_eq!(on.sql(), "audio_model = :audio_model");
    assert!(off.is_empty());
}

#[test]
fn bind_registers_param_without_fragment() {
    let c = SetClause::new()
        .bind("rid", "#12:5")
        .set("status", "active");
    assert_eq!(c.sql(), "status = :status"); // rid referenced from WHERE, not SET
    let params = c.into_params().0;
    assert_eq!(params.len(), 2);
    assert_eq!(
        grpc_value_to_json(&params["rid"]),
        Value::String("#12:5".into())
    );
}

/// The core safety property: every placeholder emitted by [`SetClause::sql`]
/// has a matching bound param. Scans the fragment text the way a reviewer —
/// or a future client-side validator — would.
#[test]
fn every_placeholder_has_a_bound_param() {
    let c = SetClause::new()
        .set("a", 1i64)
        .set_opt("b", Some("x"))
        .set_opt::<f64>("c", None)
        .set_when(false, "d", true)
        .set_raw("payload = :payload", "payload", str_v("{}"))
        .bind("e", 2i64);

    for frag in c.sql().split(", ") {
        if frag.is_empty() {
            continue;
        }
        let name = frag.split(":").nth(1).expect("fragment carries :name");
        // set_raw fragments may reference names registered via set_raw/bind;
        // plain set-fragments use their own column name.
        assert!(
            c.clone().into_params().0.contains_key(name),
            "placeholder `{name}` has no bound param"
        );
    }
}

#[test]
fn values_round_trip_through_the_decode_bridge() {
    let params: Params = SetClause::new()
        .set("n", 7i64)
        .set("s", "hello")
        .set_opt("opt", Some(i32_v(3)))
        .into_params();
    let map = params.0;
    assert_eq!(grpc_value_to_json(&map["n"]), Value::from(7));
    assert_eq!(grpc_value_to_json(&map["s"]), Value::String("hello".into()));
    assert_eq!(
        grpc_value_to_json(&map["opt"]),
        Value::from(3),
        "Int32 wire kind survives into the JSON bridge"
    );
}

#[test]
fn raw_fragment_for_embedded_literal_rebuild() {
    // The literal-rebuild pattern: a value embedded inside an array/map
    // literal, where `col = :col` does not apply but the param must still
    // be bound.
    let c = SetClause::new()
        .set_raw(
            "entries = [{author: entries[0].author, media_ref: :media_ref}]",
            "media_ref",
            "media/clip.mp3",
        )
        .bind("nkey", "note:cafe");
    assert!(c.sql().starts_with("entries = ["));
    let params = c.into_params().0;
    assert_eq!(params.len(), 2);
    assert_eq!(
        grpc_value_to_json(&params["media_ref"]),
        Value::String("media/clip.mp3".into())
    );
    assert!(params.contains_key("nkey"));
}

#[test]
fn re_set_last_value_wins_in_params() {
    let c = SetClause::new().set("x", 1i64).set("x", 2i64);
    assert_eq!(c.sql(), "x = :x, x = :x"); // both fragments emitted…
    let params = c.into_params().0; // …but only one param entry (last wins)
    assert_eq!(params.len(), 1);
    assert_eq!(grpc_value_to_json(&params["x"]), Value::from(2));
}

// -- server-side list append (`||` form) -------------------------------------

/// The append form: one fragment, one single-element list
/// param — never a SQL literal (injection rule), never `+` (accepted but
/// silently ignored by the server) or `concat` (nulls the column).
#[test]
fn append_emits_single_fragment_single_param() {
    let c = SetClause::new().append("labels", "red");
    assert_eq!(c.sql(), "labels = labels || :labels__append");
    let params = c.into_params().0;
    assert_eq!(params.len(), 1, "exactly one bound element");
    assert_eq!(
        grpc_value_to_json(&params["labels__append"]),
        serde_json::json!(["red"]),
        "the element binds as a one-element LIST"
    );
}

/// Chained appends on one column coalesce into the SINGLE fragment + one
/// merged list param.
#[test]
fn chained_appends_coalesce_into_one_fragment() {
    let c = SetClause::new().append("tags", "a").append("tags", "b");
    assert_eq!(c.sql(), "tags = tags || :tags__append");
    let params = c.into_params().0;
    assert_eq!(
        grpc_value_to_json(&params["tags__append"]),
        serde_json::json!(["a", "b"])
    );
}

/// Appends mix with plain sets on other columns; each column keeps its own
/// fragment and param.
#[test]
fn append_composes_with_sets_on_other_columns() {
    let c = SetClause::new()
        .set("name", "oak")
        .append("labels", "red");
    assert_eq!(
        c.sql(),
        "name = :name, labels = labels || :labels__append"
    );
    let params = c.into_params().0;
    assert_eq!(params.len(), 2);
    assert_eq!(
        grpc_value_to_json(&params["name"]),
        Value::String("oak".into())
    );
    assert_eq!(
        grpc_value_to_json(&params["labels__append"]),
        serde_json::json!(["red"])
    );
}

/// The `every_placeholder_has_a_bound_param` scan also holds for append
/// fragments — the `||` fragment's `:name` placeholder resolves.
#[test]
fn append_placeholders_resolve_in_the_safety_scan() {
    let c = SetClause::new()
        .append("links", "https://example.com/x")
        .set("n", 1i64);
    let sql = c.sql();
    let params = c.clone().into_params().0;
    for frag in sql.split(", ") {
        let name = frag.split(":").nth(1).expect("fragment carries :name");
        assert!(
            params.contains_key(name),
            "placeholder `{name}` has no bound param"
        );
    }
}

/// Element kind fidelity: integers append as integers.
#[test]
fn append_binds_integers_as_integers() {
    let c = SetClause::new().append("nums", 42i32);
    let params = c.into_params().0;
    assert_eq!(
        grpc_value_to_json(&params["nums__append"]),
        serde_json::json!([42])
    );
}

// -- helpers ----------------------------------------------------------------

fn chrono_now() -> impl IntoGrpcValue {
    arcadedb_protocol::__private::timestamp_v(chrono::Utc::now())
}
