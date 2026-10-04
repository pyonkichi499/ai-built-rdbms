//! Shutdown request flag shared by `ExecCtx`, `Session` and the server's
//! connection registry (`m2.md` §4.1).

use std::sync::atomic::{AtomicBool, Ordering};

/// A request to terminate. Shared through `Arc`.
#[derive(Debug, Default)]
pub struct InterruptFlag {
    terminate: AtomicBool,
}

impl InterruptFlag {
    pub fn request_terminate(&self) {
        self.terminate.store(true, Ordering::SeqCst);
    }

    pub fn is_terminate_requested(&self) -> bool {
        self.terminate.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn flag_is_shared_and_sticky() {
        let f = Arc::new(InterruptFlag::default());
        assert!(!f.is_terminate_requested());
        let g = Arc::clone(&f);
        std::thread::spawn(move || g.request_terminate())
            .join()
            .unwrap();
        assert!(f.is_terminate_requested());
    }
}
