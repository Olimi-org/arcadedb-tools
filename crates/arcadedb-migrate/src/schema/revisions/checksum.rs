use sha2::{Digest, Sha256};

use super::super::model::{Schema, TsRole};

/// Compute a stable checksum over a desired schema. Used to detect no-op syncs
/// (if the schema hasn't changed since last run, the diff step is skippable).
///
/// The checksum is over a deterministic serialization of the schema (sorted
/// types/properties, indexes in declaration order) — not over the raw .sql
/// text, so cosmetic edits (whitespace, comment changes, clause reordering)
/// don't trigger runs. It covers the structured fields *plus* the verbatim
/// pockets (defaults, create-time-only type clauses, TimeSeries specs, index
/// metadata), so any .sql edit — even one the diff won't reconcile — forces a
/// re-run.
pub fn checksum(schema: &Schema) -> String {
    let mut hasher = Sha256::new();
    for (tname, ty) in &schema.types {
        hasher.update(tname.as_bytes());
        hasher.update(b"|");
        hasher.update(ty.kind.ddl_keyword().as_bytes());
        // extends — structural, order matters (it mirrors parentTypes ordering).
        for e in &ty.extends {
            hasher.update(b"|extends:");
            hasher.update(e.as_bytes());
        }
        if let Some(clause) = ty.clause() {
            hasher.update(b"|");
            hasher.update(clause.as_bytes());
        }
        if let Some(ts) = &ty.timeseries {
            // The whole declaration is create-time-only: every authored piece
            // participates, so any edit forces a re-run.
            hasher.update(b"|ts:");
            hasher.update(ts.timestamp_column.as_bytes());
            if let Some(p) = &ts.precision {
                hasher.update(b"|precision=");
                hasher.update(p.as_bytes());
            }
            for c in ts.columns() {
                hasher.update(b"|col:");
                hasher.update(c.name.as_bytes());
                hasher.update(b":");
                hasher.update(c.data_type.as_bytes());
                hasher.update(b":");
                hasher.update(match c.role {
                    TsRole::Timestamp => &b"ts"[..],
                    TsRole::Tag => &b"tag"[..],
                    TsRole::Field => &b"fld"[..],
                });
            }
            if let Some(s) = ts.shards {
                hasher.update(format!("|shards={s}").as_bytes());
            }
            if let Some(r) = &ts.retention {
                hasher.update(b"|retention=");
                hasher.update(r.as_bytes());
            }
            if let Some(c) = &ts.compaction_interval {
                hasher.update(b"|compaction=");
                hasher.update(c.as_bytes());
            }
            if let Some(b) = ts.block_size {
                hasher.update(format!("|block_size={b}").as_bytes());
            }
        }
        hasher.update(b"\n");
        for (pname, prop) in &ty.properties {
            hasher.update(b"  P ");
            hasher.update(pname.as_bytes());
            hasher.update(b":");
            hasher.update(prop.type_name.as_bytes());
            let c = &prop.constraints;
            hasher.update(b"\n  C ");
            for (attr, v) in c.boolean_keywords() {
                hasher.update(b"|");
                hasher.update(attr.as_bytes());
                hasher.update(b"=");
                hasher.update(if v { b"1" } else { b"0" });
            }
            for (attr, v) in [("min", &c.min), ("max", &c.max), ("regexp", &c.regexp)] {
                hasher.update(b"|");
                hasher.update(attr.as_bytes());
                hasher.update(b"=");
                if let Some(v) = v {
                    hasher.update(v.as_bytes());
                }
            }
            if let Some(d) = &c.default {
                hasher.update(b"|default=");
                hasher.update(d.text().as_bytes());
            }
            hasher.update(b"\n");
        }
        for idx in &ty.indexes {
            hasher.update(b"  I ");
            if let Some(n) = &idx.name {
                hasher.update(n.as_bytes());
            }
            hasher.update(b"[");
            hasher.update(idx.columns.join(",").as_bytes());
            hasher.update(b"]:");
            hasher.update(idx.kind.ddl_keyword().as_bytes());
            hasher.update(b"|unique=");
            hasher.update(if idx.unique { b"1" } else { b"0" });
            if let Some(m) = idx.metadata() {
                hasher.update(b"|");
                hasher.update(m.as_bytes());
            }
            hasher.update(b"\n");
        }
    }
    hex::encode(hasher.finalize())
}
