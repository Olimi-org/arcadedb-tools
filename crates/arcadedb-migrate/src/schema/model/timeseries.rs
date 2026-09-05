//! [`TimeseriesSpec`] — the declarative model of a TimeSeries type.
//!
//! ArcadeDB's TimeSeries engine has its own DDL surface
//! (`CREATE TIMESERIES TYPE … TIMESTAMP/TAGS/FIELDS/SHARDS/RETENTION…`) and a
//! fundamentally different structure from document/vertex/edge types: no
//! EXTENDS, no record buckets, no indexes; columns are typed by ROLE.
//!
//! Introspection (`schema:types`, engine kind `"t"`) surfaces
//! `timestampColumn`, `tsColumns[]` (name/dataType/role), `shardCount`,
//! `retentionMs`, `compactionBucketIntervalMs` — but NOT the timestamp
//! `PRECISION` or `BLOCK_SIZE`. NONE of these have ALTER forms (only
//! downsampling policies are alterable, via a separate statement). So:
//!
//! - the spec is parsed **structurally**, rendered canonically on CREATE, and
//!   checksummed — any `.sql` edit forces a re-run;
//! - comparable fields (columns, shards, retention, compaction) are compared
//!   desired-vs-actual to WARN on drift, but never auto-altered: the fix is a
//!   drop+recreate override.

use serde::{Deserialize, Serialize};

/// The role a TimeSeries column plays in the storage engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TsRole {
    Timestamp,
    Tag,
    Field,
}

impl TsRole {
    /// The DDL keyword for the section that declares this role.
    pub fn ddl_keyword(self) -> &'static str {
        match self {
            TsRole::Timestamp => "TIMESTAMP",
            TsRole::Tag => "TAGS",
            TsRole::Field => "FIELDS",
        }
    }

    /// Parse an introspected role string (`TIMESTAMP`/`TAG`/`FIELD`).
    pub fn from_introspect(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "TIMESTAMP" => Some(TsRole::Timestamp),
            "TAG" => Some(TsRole::Tag),
            "FIELD" => Some(TsRole::Field),
            _ => None,
        }
    }
}

/// One TimeSeries column: name + data type + role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeseriesColumn {
    pub name: String,
    pub data_type: String,
    pub role: TsRole,
}

/// A parsed `CREATE TIMESERIES TYPE` declaration.
///
/// Authored text is preserved where the engine can't round-trip it
/// (`precision`, `retention`, `compaction_interval`, `block_size`); columns
/// are structured so ownership/introspection/diff all speak the same shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeseriesSpec {
    /// The required timestamp column's name.
    pub timestamp_column: String,
    /// Authored PRECISION token (`"SECOND"`, `"MILLISECOND"`, …). `None`
    /// when defaulted. Not surfaced by introspection — create-time-only.
    pub precision: Option<String>,
    /// TAG columns, declaration order.
    pub tags: Vec<TimeseriesColumn>,
    /// FIELD columns, declaration order.
    pub fields: Vec<TimeseriesColumn>,
    /// SHARDS count. `None` = engine default (CPU count).
    pub shards: Option<u32>,
    /// Authored RETENTION duration (`"90 DAYS"`). Not alterable post-create.
    pub retention: Option<String>,
    /// Authored COMPACTION_INTERVAL duration (`"1 HOURS"`).
    pub compaction_interval: Option<String>,
    /// BLOCK_SIZE samples-per-block. Not surfaced by introspection.
    pub block_size: Option<u64>,
}

impl TimeseriesSpec {
    /// All declared columns (timestamp + tags + fields) in declaration order —
    /// used to flatten into ordinary [`crate::schema::Property`] entries so
    /// DTO-drift tooling and the checksum see them like any other column.
    pub fn columns(&self) -> Vec<TimeseriesColumn> {
        std::iter::once(TimeseriesColumn {
            name: self.timestamp_column.clone(),
            data_type: "LONG".to_string(),
            role: TsRole::Timestamp,
        })
        .chain(self.tags.iter().cloned())
        .chain(self.fields.iter().cloned())
        .collect()
    }

    /// Canonical clause suffix for `CREATE TIMESERIES TYPE <name> …` —
    /// everything after the name, in docs order.
    pub fn render_suffix(&self) -> String {
        let mut out = format!("TIMESTAMP {}", self.timestamp_column);
        if let Some(p) = &self.precision {
            out.push_str(&format!(" PRECISION {p}"));
        }
        if !self.tags.is_empty() {
            out.push_str(&format!(" TAGS ({})", render_columns(self.tags.iter())));
        }
        if !self.fields.is_empty() {
            out.push_str(&format!(" FIELDS ({})", render_columns(self.fields.iter())));
        }
        if let Some(n) = self.shards {
            out.push_str(&format!(" SHARDS {n}"));
        }
        if let Some(r) = &self.retention {
            out.push_str(&format!(" RETENTION {r}"));
        }
        if let Some(c) = &self.compaction_interval {
            out.push_str(&format!(" COMPACTION_INTERVAL {c}"));
        }
        if let Some(b) = self.block_size {
            out.push_str(&format!(" BLOCK_SIZE {b}"));
        }
        out
    }

    /// Normalize the authored RETENTION duration to milliseconds, for
    /// comparison against the introspected `retentionMs`.
    pub fn retention_ms(&self) -> Option<i64> {
        self.retention.as_deref().and_then(duration_ms)
    }

    /// Normalize the authored COMPACTION_INTERVAL to milliseconds.
    pub fn compaction_ms(&self) -> Option<i64> {
        self.compaction_interval.as_deref().and_then(duration_ms)
    }
}

fn render_columns<'a, I: Iterator<Item = &'a TimeseriesColumn>>(cols: I) -> String {
    cols.map(|c| format!("{} {}", c.name, c.data_type))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse `<n> <UNIT>` durations (`"90 DAYS"`, `"1 HOURS"`, `"30 MINUTES"`,
/// `"5 SECOND"`, singular/plural, case-insensitive) to milliseconds.
pub fn duration_ms(text: &str) -> Option<i64> {
    let mut parts = text.split_whitespace();
    let value: i64 = parts.next()?.parse().ok()?;
    let unit = parts.next()?.trim_end_matches('S').to_ascii_uppercase();
    let ms = match unit.as_str() {
        "DAY" => 86_400_000,
        "HOUR" => 3_600_000,
        "MINUTE" => 60_000,
        "SECOND" => 1_000,
        _ => return None,
    };
    value.checked_mul(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_suffix_is_canonical_and_complete() {
        let spec = TimeseriesSpec {
            timestamp_column: "ts".into(),
            precision: Some("SECOND".into()),
            tags: vec![TimeseriesColumn {
                name: "sensor_id".into(),
                data_type: "LONG".into(),
                role: TsRole::Tag,
            }],
            fields: vec![
                TimeseriesColumn {
                    name: "temperature".into(),
                    data_type: "INTEGER".into(),
                    role: TsRole::Field,
                },
                TimeseriesColumn {
                    name: "humidity".into(),
                    data_type: "INTEGER".into(),
                    role: TsRole::Field,
                },
            ],
            shards: Some(2),
            retention: Some("90 DAYS".into()),
            compaction_interval: Some("1 HOURS".into()),
            block_size: Some(65536),
        };
        assert_eq!(
            spec.render_suffix(),
            "TIMESTAMP ts PRECISION SECOND \
             TAGS (sensor_id LONG) \
             FIELDS (temperature INTEGER, humidity INTEGER) \
             SHARDS 2 RETENTION 90 DAYS COMPACTION_INTERVAL 1 HOURS BLOCK_SIZE 65536"
        );
    }

    #[test]
    fn minimal_spec_renders_only_what_was_declared() {
        let spec = TimeseriesSpec {
            timestamp_column: "ts".into(),
            fields: vec![TimeseriesColumn {
                name: "temperature".into(),
                data_type: "DOUBLE".into(),
                role: TsRole::Field,
            }],
            ..Default::default()
        };
        assert_eq!(
            spec.render_suffix(),
            "TIMESTAMP ts FIELDS (temperature DOUBLE)"
        );
    }

    #[test]
    fn duration_normalization_covers_documented_units() {
        assert_eq!(duration_ms("90 DAYS"), Some(7_776_000_000));
        assert_eq!(duration_ms("21 day"), Some(1_814_400_000));
        assert_eq!(duration_ms("1 HOURS"), Some(3_600_000));
        assert_eq!(duration_ms("30 MINUTES"), Some(1_800_000));
        assert_eq!(duration_ms("5 SECOND"), Some(5_000));
        assert_eq!(duration_ms("90 FORTNIGHTS"), None);
        assert_eq!(duration_ms("DAYS"), None);
    }

    #[test]
    fn columns_includes_timestamp_head() {
        let spec = TimeseriesSpec {
            timestamp_column: "ts".into(),
            tags: vec![TimeseriesColumn {
                name: "t1".into(),
                data_type: "STRING".into(),
                role: TsRole::Tag,
            }],
            ..Default::default()
        };
        let names: Vec<String> = spec.columns().iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, vec!["ts", "t1"]);
    }
}
