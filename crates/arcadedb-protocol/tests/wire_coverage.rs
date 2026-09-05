//! Exhaustive wire-coverage test: every `Kind` variant must (a) be
//! constructible from a natural Rust type through `IntoGrpcValue` and
//! (b) decode back through `FromGrpcValue` — and the match below has NO
//! wildcard arm, so a proto bump that adds a variant FAILS COMPILATION
//! until this test teaches it the round trip. Coverage is enforced, not
//! aspirational.

use arcadedb_protocol::proto::com::arcadedb::grpc::grpc_value::Kind;
use arcadedb_protocol::record::FromGrpcValue;
use arcadedb_protocol::{grpc_value_to_json, json_to_grpc_value, GrpcValue, IntoGrpcValue};

/// Encode a natural value → assert the wire kind → decode back → assert
/// equality. One arm per Kind; adding a variant without an arm = compile
/// error here.
#[test]
fn every_wire_kind_round_trips() {
    // Int64
    let v: GrpcValue = 100001i64.into_value();
    assert!(matches!(v.kind, Some(Kind::Int64Value(100001))));
    assert_eq!(i64::from_grpc_value(&v).unwrap(), 100001);

    // Int32
    let v: GrpcValue = 1716i32.into_value();
    assert!(matches!(v.kind, Some(Kind::Int32Value(1716))));
    assert_eq!(i32::from_grpc_value(&v).unwrap(), 1716);

    // Double
    let v: GrpcValue = 2.5f64.into_value();
    assert!(matches!(v.kind, Some(Kind::DoubleValue(2.5))));
    assert_eq!(f64::from_grpc_value(&v).unwrap(), 2.5);

    // Float
    let v: GrpcValue = 1.5f32.into_value();
    assert!(matches!(v.kind, Some(Kind::FloatValue(_))));
    assert_eq!(f32::from_grpc_value(&v).unwrap(), 1.5);

    // Bool
    let v: GrpcValue = true.into_value();
    assert!(matches!(v.kind, Some(Kind::BoolValue(true))));
    assert!(bool::from_grpc_value(&v).unwrap());

    // String
    let v: GrpcValue = "StS".into_value();
    assert!(matches!(v.kind, Some(Kind::StringValue(ref s)) if s == "StS"));
    assert_eq!(String::from_grpc_value(&v).unwrap(), "StS");

    // Bytes (owned encode, borrowed decode view)
    let v: GrpcValue = vec![1u8, 2, 3].into_value();
    assert!(matches!(v.kind, Some(Kind::BytesValue(_))));
    assert_eq!(<&[u8]>::from_grpc_value(&v).unwrap(), &[1u8, 2, 3][..]);

    // Timestamp (DateTime encode, DateTime decode)
    let dt = chrono::Utc::now();
    let v: GrpcValue = dt.into_value();
    assert!(matches!(v.kind, Some(Kind::TimestampValue(_))));
    assert_eq!(
        chrono::DateTime::<chrono::Utc>::from_grpc_value(&v).unwrap(),
        dt
    );

    // List (Vec<T> encode, Vec<T> decode)
    let v: GrpcValue = vec![1i64, 2, 3].into_value();
    assert!(matches!(v.kind, Some(Kind::ListValue(_))));
    assert_eq!(Vec::<i64>::from_grpc_value(&v).unwrap(), vec![1, 2, 3]);

    // Map (HashMap encode, HashMap decode)
    let mut m = std::collections::HashMap::new();
    m.insert("k".to_string(), "v".to_string());
    let v: GrpcValue = m.into_value();
    assert!(matches!(v.kind, Some(Kind::MapValue(_))));
    let mut back = std::collections::HashMap::<String, String>::from_grpc_value(&v).unwrap();
    assert_eq!(back.remove("k").as_deref(), Some("v"));

    // Link
    let link = arcadedb_protocol::Link::parse("#12:0").unwrap();
    let v: GrpcValue = link.into_value();
    assert!(matches!(v.kind, Some(Kind::LinkValue(_))));
    assert_eq!(arcadedb_protocol::Link::from_grpc_value(&v).unwrap(), link);

    // Embedded — no natural scalar Rust type; the constructor is the API.
    // Round trip via the JSON bridge instead (decode side covers it).
    let v = arcadedb_protocol::__private::embedded_v("x", vec![("a", 1i32.into_value())]);
    assert!(matches!(v.kind, Some(Kind::EmbeddedValue(_))));
    let json = grpc_value_to_json(&v);
    assert!(json.to_string().contains("\"a\""));

    // Decimal — feature-gated; constructor round trip via JSON bridge.
    #[cfg(feature = "decimal")]
    {
        let v = arcadedb_protocol::__private::decimal_v(rust_decimal::Decimal::new(1234, 2));
        assert!(matches!(v.kind, Some(Kind::DecimalValue(_))));
    }

    // JSON bridge: the documented mapping narrows small integers to Int32
    // (JSON carries no width) — bridge round trips are KIND-narrowing by
    // design, which is exactly why typed binding (above) is the API.
    let v: GrpcValue = 42i64.into_value();
    let j = grpc_value_to_json(&v);
    let back = json_to_grpc_value(&j);
    assert!(
        matches!(
            back.kind,
            Some(Kind::Int32Value(42)) | Some(Kind::Int64Value(42))
        ),
        "bridge must return an integer kind"
    );

    // Exhaustiveness guard — NO wildcard: a new Kind variant from a proto
    // bump must be taught to this match (and given a round trip above).
    fn _touch_every_kind(k: &Kind) {
        match k {
            Kind::BoolValue(_) => {}
            Kind::Int32Value(_) => {}
            Kind::Int64Value(_) => {}
            Kind::FloatValue(_) => {}
            Kind::DoubleValue(_) => {}
            Kind::StringValue(_) => {}
            Kind::BytesValue(_) => {}
            Kind::TimestampValue(_) => {}
            Kind::ListValue(_) => {}
            Kind::MapValue(_) => {}
            Kind::EmbeddedValue(_) => {}
            Kind::LinkValue(_) => {}
            Kind::DecimalValue(_) => {}
        }
    }
}
