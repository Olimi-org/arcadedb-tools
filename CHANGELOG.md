# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.1] — 2026-09-11

### arcadedb-protocol

#### Added

- `SetClause::apply_if` / `FilterClause::apply_if` — conditional composition
  for clause steps that need more than one builder call (a `bind` + `filter`
  pair, or an operator chosen at runtime). `None` passes the clause through
  untouched, so a ladder of `apply_if`s reads as a declarative
  optional-predicate list:
  ```rust
  let filter = FilterClause::new()
      .apply_if(band.min_reviews, |f, min| {
          f.bind("band_min_reviews", min)
              .filter("reviews >= :band_min_reviews")
      })
      .apply_if(band.max_reviews, |f, max| f.lte("reviews", max));
  ```
  Complements the single-call conditionals that already existed
  (`set_opt`, `set_when`, `filter_when`).

## [0.3.0] — 2026-09-08

### arcadedb-protocol

#### Added

- `sql!` macro — write the statement as a literal, get a compile-time syntax
  check for free (clause order, unbalanced delimiters, reserved-word
  placeholders, `DELETE` without `WHERE`, …). Expands to the `&str` unchanged.
- `SetClause::set_expr` — set a column to a server-side expression
  (`count = count + 1`, `checked_at = sysdate()`).
- `create_and_return`-equivalent form for `upsert_returning`.

[0.3.1]: https://crates.io/crates/arcadedb-protocol/0.3.1
[0.3.0]: https://crates.io/crates/arcadedb-protocol/0.3.0
