//! initdb: bootstrap of template1, copy to template0 / postgres
//! (`m2.md` §5.8).
//!
//! 担当 G が実装する。

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::storage::vfs::Vfs;

#[derive(Debug, Clone)]
pub struct InitdbOptions {
    pub superuser: String,
    pub no_sync: bool,
    pub rel_seg_blocks: u32,
}

pub fn initdb(_vfs: Arc<dyn Vfs>, _opts: &InitdbOptions) -> Result<()> {
    Err(Error::not_supported("initdb is not implemented yet"))
}
