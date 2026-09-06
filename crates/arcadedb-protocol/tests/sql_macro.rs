//! `sql!` — compile-time syntax-checked statement literals. Positive
//! cases (these compile); each trap the macro rejects is covered by unit
//! tests in `arcadedb-record-macros/src/sql.rs` — negative cases cannot
//! live here without a compile-fail harness.

use arcadedb_protocol::{params, sql};

#[test]
fn checked_form_validates_and_expands_to_str() {
    let q: &str = sql!("UPDATE users SET status = :status UPSERT WHERE user_id = :user_id");
    assert_eq!(
        q,
        "UPDATE users SET status = :status UPSERT WHERE user_id = :user_id"
    );
    let returning: &str =
        sql!("UPDATE users SET status = :status UPSERT RETURN AFTER WHERE user_id = :user_id");
    assert!(returning.contains("UPSERT RETURN AFTER WHERE"));
    let plain: &str = sql!("UPDATE users SET status = :status WHERE user_id = :user_id");
    assert!(!plain.contains("UPSERT"));
    let select: &str = sql!("SELECT * FROM orders WHERE total > 0 ORDER BY total DESC");
    assert!(select.contains("ORDER BY"));

    // Whole-statement guards accept their valid shapes.
    let del: &str = sql!("DELETE FROM users WHERE user_id = :user_id");
    assert!(del.contains("DELETE"));
    let ins: &str = sql!("INSERT INTO users SET name = :name");
    assert!(ins.contains("INSERT INTO"));
    let edge: &str = sql!(
        "CREATE EDGE LIKES FROM (SELECT FROM users WHERE user_id = :a) TO (SELECT FROM posts WHERE post_id = :b)"
    );
    assert!(edge.contains(" TO "));

    // Composes with params! at the call site — binding stays params!'s job.
    let user_id = 7i64;
    let (q, p) = (
        sql!("SELECT FROM orders WHERE user_id = :user_id"),
        params! { user_id },
    );
    assert!(q.contains(":user_id"));
    assert!(p.0.contains_key("user_id"));
}
