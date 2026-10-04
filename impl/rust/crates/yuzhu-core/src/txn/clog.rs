//! Commit log `pg_xact/` (`m2.md` §3.8, §6.7.2).
//!
//! 担当 E が実装する。

#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]
#![allow(clippy::unimplemented)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::Xid;
use crate::error::{Error, Result};
use crate::storage::vfs::Vfs;

fn pending() -> Error {
    Error::not_supported("commit log is not implemented yet")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XidStatus {
    InProgress = 0,
    Committed = 1,
    Aborted = 2,
}

#[derive(Debug)]
struct ClogPage {
    #[allow(dead_code)]
    data: Vec<u8>,
    #[allow(dead_code)]
    dirty: bool,
}

#[derive(Debug)]
pub struct Clog {
    #[allow(dead_code)]
    vfs: Arc<dyn Vfs>,
    #[allow(dead_code)]
    pages: Mutex<HashMap<u64, ClogPage>>,
}

impl Clog {
    /// Loads the page containing `next_xid`; pages of XIDs assigned later
    /// are new, so they need no I/O.
    #[allow(clippy::needless_pass_by_value)]
    pub fn open(vfs: Arc<dyn Vfs>, _next_xid: Xid) -> Result<Clog> {
        let _ = vfs;
        Err(pending())
    }

    pub fn status(&self, _xid: Xid) -> Result<XidStatus> {
        Err(pending())
    }

    /// Called by `TxnManager` inside the `proc` mutex when assigning an
    /// XID (`ExtendCLOG`). Creates a zero page if absent; no I/O.
    pub fn ensure_page_for(&self, _xid: Xid) {
        unimplemented!("担当 E が実装")
    }

    /// Only `InProgress -> Committed | Aborted` is allowed. In memory only.
    pub fn set_status(&self, _xid: Xid, _s: XidStatus) -> Result<()> {
        Err(pending())
    }

    /// Writes dirty pages and `sync_data`s them (checkpoint and shutdown).
    pub fn flush(&self) -> Result<()> {
        Err(pending())
    }
}
