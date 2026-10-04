//! Text input/output of the system types (`"char"`, `oid`, `regproc`, `tid`,
//! `xid`, `cid`, `oidvector`, `pg_node_tree`) (`m2.md` §4.2).
//!
//! 担当 F が実装する。ここにあるのは `types::io` が呼ぶ口と、数値だけを扱う
//! 暫定の実装（F が §4.2 の規則どおりに置き換える）。

use super::{Datum, Oid, SqlType, oid};
use crate::error::{Error, Result};

/// Text output of a system-type value. `None` for NULL and for variants
/// that are not system types.
pub fn output_text(d: &Datum) -> Option<String> {
    Some(match d {
        Datum::Oid(v) | Datum::Xid(v) | Datum::Cid(v) => v.to_string(),
        Datum::Char(c) => char::from(*c).to_string(),
        Datum::Tid(t) => format!("({},{})", t.block, t.offset),
        Datum::OidVector(v) => v.iter().map(u32::to_string).collect::<Vec<_>>().join(" "),
        _ => return None,
    })
}

/// Input function of a system type. `ty.oid` is one of the types handled
/// here.
pub fn input_text(s: &str, ty: SqlType) -> Result<Datum> {
    let _ = s;
    Err(Error::not_supported(format!(
        "input of type with OID {} is not supported yet",
        ty.oid
    )))
}

/// Whether `type_oid` is a type handled by this module.
pub fn handles(type_oid: Oid) -> bool {
    matches!(
        type_oid,
        oid::CHAR
            | oid::OID
            | oid::REGPROC
            | oid::TID
            | oid::XID
            | oid::CID
            | oid::OIDVECTOR
            | oid::PG_NODE_TREE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Tid;

    #[test]
    fn provisional_output() {
        assert_eq!(output_text(&Datum::Oid(26)).as_deref(), Some("26"));
        assert_eq!(
            output_text(&Datum::Tid(Tid {
                block: 0,
                offset: 1
            }))
            .as_deref(),
            Some("(0,1)")
        );
        assert_eq!(
            output_text(&Datum::OidVector(vec![23, 23])).as_deref(),
            Some("23 23")
        );
        assert_eq!(output_text(&Datum::Int4(1)), None);
        assert!(input_text("1", SqlType::of(oid::XID)).is_err());
        assert!(handles(oid::TID));
    }
}
