//! [`FilterClause`] — WHERE assembly for dynamic queries, the
//! read-side sibling of [`SetClause`](crate::SetClause).
//!
//! Every operator method emits its fragment AND binds its parameter in the
//! same call — the fragment can never reference a param that wasn't bound.
//! Values go through [`IntoGrpcValue`]; the only caller-owned text is the
//! column name (same contract as `SetClause`: treat it like your schema).
//!
//! The operator surface follows ArcadeDB's SQL reference
//! (`sql-where.adoc`):
//!
//! - Comparisons `= <> < <= > >=`, `BETWEEN :a AND :b`, `IS [NOT] NULL`.
//! - `IN :p` — bound lists carry no SQL-text length cap; `NOT IN` is
//!   documented NOT to use the index (complement scan) — see
//!   [`FilterClause::not_in`].
//! - `CONTAINS :scalar` — collection holds one equal element (**bind
//!   scalars only**: a collection on the right of `CONTAINS` is searched
//!   for as a single *nested* element, not as membership).
//! - `CONTAINSALL` / `CONTAINSANY :list` — membership over a bound
//!   collection.
//! - `LIKE` / `ILIKE :pattern` — non-indexed.
//! - `LIMIT :limit` / `SKIP :skip` accept bound placeholders — call sites
//!   can stop interpolating `{limit}`.
//!
//! Two traps worth knowing before naming things:
//!
//! - **Param names share the lexer with SQL keywords**: `:after` fails to
//!   *parse* (`AFTER` is reserved by `RETURN BEFORE|AFTER`). Generated
//!   names are identifier-shaped (`{col}`, `{col}__lt`, …) and safe unless
//!   a column is exactly a keyword.
//! - **`@rid` in WHERE is documented as slow** — prefer the rid as target
//!   (`SELECT FROM #12:3`) or
//!   [`lookup_by_rid`](crate::ArcadeDbClient::lookup_by_rid). The record
//!   intrinsics (`@rid`, `@type`, `@out`, `@in`) ARE valid filter columns.

use std::collections::HashMap;

use crate::encode::{IntoGrpcValue, Params};
use crate::GrpcValue;

/// Accumulates AND-joined WHERE fragments and their bound values.
///
/// ```
/// # use arcadedb_protocol::FilterClause;
/// let filter = FilterClause::new()
///     .filter("is_visible = true")
///     .gte("release_date", chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap());
/// assert!(filter.sql().contains("release_date >= :release_date__gte"));
/// assert_eq!(FilterClause::new().where_sql(), "");
/// ```
///
/// Compose into a query with [`Params`](crate::Params); merge with
/// [`SetClause`](crate::SetClause) for `UPDATE … SET … WHERE …`.
#[derive(Debug, Clone, Default)]
pub struct FilterClause {
    frags: Vec<String>,
    params: HashMap<String, GrpcValue>,
}

impl FilterClause {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when nothing was pushed.
    pub fn is_empty(&self) -> bool {
        self.frags.is_empty()
    }

    /// The WHERE body: `a = :a AND b >= :b__gte`. Empty when nothing
    /// was pushed.
    pub fn sql(&self) -> String {
        self.frags.join(" AND ")
    }

    /// [`sql`](Self::sql) with the `WHERE` keyword, or `""` when empty
    /// (no dangling WHERE).
    pub fn where_sql(&self) -> String {
        if self.frags.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.sql())
        }
    }

    /// Consume into `Params` for `query_with_values` / `execute_with_values`.
    pub fn into_params(self) -> Params {
        Params(self.params)
    }

    /// Include a raw fragment as-is. The fragment owns any placeholders it
    /// references — bind them via [`bind`](Self::bind).
    pub fn filter(mut self, fragment: impl Into<String>) -> Self {
        self.frags.push(fragment.into());
        self
    }

    /// [`filter`](Self::filter) only when `cond` holds.
    pub fn filter_when(self, cond: bool, fragment: impl Into<String>) -> Self {
        if cond {
            self.filter(fragment)
        } else {
            self
        }
    }

    /// Register a param WITHOUT emitting a fragment — for placeholders
    /// referenced elsewhere in the statement.
    pub fn bind(mut self, name: &str, value: impl IntoGrpcValue) -> Self {
        self.params.insert(name.to_string(), value.into_value());
        self
    }

    /// `col = :col`.
    pub fn eq(mut self, col: &str, value: impl IntoGrpcValue) -> Self {
        let c = col.to_string();
        let name = self.param_name(&c);
        self.frags.push(format!("{c} = :{name}"));
        self.params.insert(name, value.into_value());
        self
    }

    /// `col <> :col__ne`.
    pub fn ne(self, col: &str, value: impl IntoGrpcValue) -> Self {
        self.tagged(col, "ne", "<>", value)
    }

    /// `col < :col__lt`.
    pub fn lt(self, col: &str, value: impl IntoGrpcValue) -> Self {
        self.tagged(col, "lt", "<", value)
    }

    /// `col <= :col__lte`.
    pub fn lte(self, col: &str, value: impl IntoGrpcValue) -> Self {
        self.tagged(col, "lte", "<=", value)
    }

    /// `col > :col__gt`.
    pub fn gt(self, col: &str, value: impl IntoGrpcValue) -> Self {
        self.tagged(col, "gt", ">", value)
    }

    /// `col >= :col__gte`.
    pub fn gte(self, col: &str, value: impl IntoGrpcValue) -> Self {
        self.tagged(col, "gte", ">=", value)
    }

    /// `col BETWEEN :col__from AND :col__to`.
    pub fn between(
        mut self,
        col: &str,
        from: impl IntoGrpcValue,
        to: impl IntoGrpcValue,
    ) -> Self {
        let c = col.to_string();
        let name = self.param_name(&format!("{c}__from"));
        let to_name = self.param_name(&format!("{c}__to"));
        self.frags
            .push(format!("{c} BETWEEN :{name} AND :{to_name}"));
        self.params.insert(name, from.into_value());
        self.params.insert(to_name, to.into_value());
        self
    }

    /// `col IS NULL` (no param).
    pub fn is_null(mut self, col: &str) -> Self {
        self.frags.push(format!("{col} IS NULL"));
        self
    }

    /// `col IS NOT NULL` (no param).
    pub fn is_not_null(mut self, col: &str) -> Self {
        self.frags.push(format!("{col} IS NOT NULL"));
        self
    }

    /// `col IN :col__in` with the values bound as ONE list parameter.
    pub fn in_list<I>(mut self, col: &str, values: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoGrpcValue,
    {
        let c = col.to_string();
        let name = self.param_name(&format!("{c}__in"));
        self.frags.push(format!("{c} IN :{name}"));
        self.params
            .insert(name, crate::encode::list_v(values.into_iter().map(IntoGrpcValue::into_value)));
        self
    }

    /// `col NOT IN :col__nin`. Documented NOT to use the index — pair
    /// with an indexed condition on large types.
    ///
    /// History: on an *indexed* property, releases 26.7.1–26.9.0 served the
    /// IN result set here (ArcadeData/arcadedb#6796). Fixed in 26.9.1.
    pub fn not_in<I>(mut self, col: &str, values: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoGrpcValue,
    {
        let c = col.to_string();
        let name = self.param_name(&format!("{c}__nin"));
        self.frags.push(format!("{c} NOT IN :{name}"));
        self.params
            .insert(name, crate::encode::list_v(values.into_iter().map(IntoGrpcValue::into_value)));
        self
    }

    /// `col CONTAINS :col__has` — true when the collection holds the
    /// bound SCALAR value. For multi-element membership use
    /// [`contains_all`](Self::contains_all) /
    /// [`contains_any`](Self::contains_any).
    pub fn contains(self, col: &str, value: impl IntoGrpcValue) -> Self {
        self.tagged(col, "has", "CONTAINS", value)
    }

    /// `col CONTAINSALL :col__all` — every element of the bound collection
    /// is present in `col`.
    pub fn contains_all<I>(mut self, col: &str, values: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoGrpcValue,
    {
        let c = col.to_string();
        let name = self.param_name(&format!("{c}__all"));
        self.frags.push(format!("{c} CONTAINSALL :{name}"));
        self.params
            .insert(name, crate::encode::list_v(values.into_iter().map(IntoGrpcValue::into_value)));
        self
    }

    /// `col CONTAINSANY :col__any` — at least one element of the bound
    /// collection is present in `col`.
    pub fn contains_any<I>(mut self, col: &str, values: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoGrpcValue,
    {
        let c = col.to_string();
        let name = self.param_name(&format!("{c}__any"));
        self.frags.push(format!("{c} CONTAINSANY :{name}"));
        self.params
            .insert(name, crate::encode::list_v(values.into_iter().map(IntoGrpcValue::into_value)));
        self
    }

    /// `NOT (col CONTAINSANY :col__any)` — none of the bound elements
    /// may be present.
    pub fn not_contains_any<I>(self, col: &str, values: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoGrpcValue,
    {
        // Emit through the same path so the param name stays identical to
        // contains_any's, then wrap the just-pushed fragment.
        let mut this = self.contains_any(col, values);
        let last = this.frags.last_mut().expect("contains_any pushed a fragment");
        *last = format!("NOT ({last})");
        this
    }

    /// `col LIKE :col__like` (case-sensitive, `%`/`?` wildcards).
    pub fn like(self, col: &str, pattern: impl IntoGrpcValue) -> Self {
        self.tagged(col, "like", "LIKE", pattern)
    }

    /// `col ILIKE :col__ilike` (case-insensitive `LIKE`).
    pub fn ilike(self, col: &str, pattern: impl IntoGrpcValue) -> Self {
        self.tagged(col, "ilike", "ILIKE", pattern)
    }

    /// The one-flag-per-operator emitter: `{col} {OP} :{col}__{tag}`.
    fn tagged(
        mut self,
        col: &str,
        tag: &str,
        op: &str,
        value: impl IntoGrpcValue,
    ) -> Self {
        let c = col.to_string();
        let name = self.param_name(&format!("{c}__{tag}"));
        self.frags.push(format!("{c} {op} :{name}"));
        self.params.insert(name, value.into_value());
        self
    }

    /// Deterministic param name: the base if free, else
    /// `{base}_2`, `{base}_3`, …
    fn param_name(&mut self, base: &str) -> String {
        if !self.params.contains_key(base) {
            return base.to_string();
        }
        for n in 2.. {
            let candidate = format!("{base}_{n}");
            if !self.params.contains_key(&candidate) {
                return candidate;
            }
        }
        unreachable!("u32 sequence exhausts memory first")
    }
}
