//! `LOCK TABLE` の実行（M5 LK-4b。01 §6.4、LK-D17）。
//!
//! ロックは session が文の前に取り終えている（§5.9）。ここは 25P01 の検査と、取れていることの
//! `debug_assert`、コマンドタグだけ。

use super::DdlCtx;
use crate::analyzer::ddl_ext::lock_table::BoundLockTable;
use crate::error::{Error, Result, sqlstate};
use crate::txn::lock::LockTag;
use crate::types::Oid;

/// ブロックの外の `LOCK TABLE` は 25P01（ロックより前、存在検査より前）。
pub fn require_transaction_block(outside_block: bool) -> Result<()> {
    if outside_block {
        return Err(Error::new(
            sqlstate::NO_ACTIVE_SQL_TRANSACTION,
            "LOCK TABLE can only be used in transaction blocks",
        ));
    }
    Ok(())
}

pub fn execute(ctx: &mut DdlCtx<'_>, b: &BoundLockTable) -> Result<String> {
    require_transaction_block(ctx.outside_block)?;
    for t in b.targets.iter().filter(|t| !t.virtual_rel) {
        let db: Oid = if t.shared { 0 } else { ctx.db.oid };
        debug_assert!(
            ctx.cluster
                .locks()
                .holds(ctx.backend, LockTag::Relation { db, rel: t.rel }, b.mode),
            "LOCK TABLE: the lock must have been acquired before execution"
        );
    }
    Ok("LOCK TABLE".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_block_is_25p01() {
        let e = require_transaction_block(true).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NO_ACTIVE_SQL_TRANSACTION);
        assert_eq!(
            e.message,
            "LOCK TABLE can only be used in transaction blocks"
        );
        assert!(require_transaction_block(false).is_ok());
    }
}
