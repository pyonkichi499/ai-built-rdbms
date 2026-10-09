//! テスト用の同期点（`m5/02` §7.5）。
//!
//! `sync_point!("btree.leaf.latched")` を実行経路に置くと、テストが「この点に着いたら止まる」を指定できる。
//! 有効なのは Cargo の feature `sync-points` のとき、またはこのクレートの単体テスト（`cfg(test)`）のとき
//! だけで、通常のビルドでは何もしない空の関数になる（`unsafe` なし）。
//!
//! テストは [`Scenario`] を作り、関係するスレッドで [`Scenario::attach`] する。`attach` していないスレッド
//! （並行して走る他のテスト）は同期点を素通りするので、テスト同士が干渉しない。
//!
//! ```ignore
//! let sc = Scenario::new();
//! let gate = sc.block_at("btree.leaf.latched");
//! std::thread::scope(|s| {
//!     s.spawn(|| { let _a = sc.attach(); /* 同期点で止まる処理 */ });
//!     gate.wait_arrived();   // A が点に着いた
//!     /* 別のスレッドの処理（attach しない） */
//!     gate.release();        // A を進める
//! });
//! ```

/// 同期点。名前は `"<モジュール>.<場所>.<状態>"` の形。
#[macro_export]
macro_rules! sync_point {
    ($name:expr) => {
        $crate::util::sync_point::hit($name)
    };
}

/// 同期点に着いたことを知らせる。テストがその点を止めているなら、解放されるまで待つ。
#[cfg(not(any(test, feature = "sync-points")))]
#[inline(always)]
pub fn hit(_name: &'static str) {}

#[cfg(any(test, feature = "sync-points"))]
pub use imp::{Attached, Gate, Scenario, hit};

#[cfg(any(test, feature = "sync-points"))]
mod imp {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
    use std::time::{Duration, Instant};

    /// 止められたスレッドが待つ上限（テストが固まったら失敗にする）。
    const STUCK_LIMIT: Duration = Duration::from_secs(20);

    type Registry = HashMap<(u64, &'static str), Arc<GateInner>>;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

    thread_local! {
        /// このスレッドが `attach` しているシナリオの ID（0 = なし）。
        static SCOPE: Cell<u64> = const { Cell::new(0) };
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[derive(Debug)]
    struct GateState {
        /// この点に着いた延べ回数（止めたかどうかによらない）。
        hits: usize,
        /// `skip` 回までは止めずに通す。
        skip: usize,
        arrived: usize,
        open: bool,
    }

    #[derive(Debug)]
    struct GateInner {
        state: Mutex<GateState>,
        cv: Condvar,
    }

    pub fn hit(name: &'static str) {
        let id = SCOPE.with(Cell::get);
        if id == 0 {
            return;
        }
        let gate = {
            let reg = lock(&REGISTRY);
            reg.as_ref().and_then(|r| r.get(&(id, name)).cloned())
        };
        let Some(gate) = gate else { return };
        let mut st = lock(&gate.state);
        st.hits += 1;
        if st.hits <= st.skip {
            return;
        }
        st.arrived += 1;
        gate.cv.notify_all();
        let deadline = Instant::now() + STUCK_LIMIT;
        while !st.open {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "sync point \"{name}\" was never released");
            st = gate
                .cv
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// テスト 1 本ぶんの同期点の集まり。落とすと、止めていた点をすべて解放して登録を消す。
    #[derive(Debug)]
    pub struct Scenario {
        id: u64,
    }

    impl Default for Scenario {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Scenario {
        #[must_use]
        pub fn new() -> Scenario {
            Scenario {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            }
        }

        /// 呼んだスレッドをこのシナリオに入れる。返したガードを落とすと外れる。
        #[must_use]
        pub fn attach(&self) -> Attached {
            let prev = SCOPE.with(|s| s.replace(self.id));
            Attached { prev }
        }

        /// 点 `name` を止める。`attach` 済みのスレッドが着くと、[`Gate::release`] まで待つ。
        #[must_use]
        pub fn block_at(&self, name: &'static str) -> Gate {
            self.block_at_nth(name, 1)
        }

        /// 点 `name` の `nth` 回目（1 始まり）に着いたときから止める。それまでは素通りする。
        #[must_use]
        pub fn block_at_nth(&self, name: &'static str, nth: usize) -> Gate {
            let inner = Arc::new(GateInner {
                state: Mutex::new(GateState {
                    hits: 0,
                    skip: nth.saturating_sub(1),
                    arrived: 0,
                    open: false,
                }),
                cv: Condvar::new(),
            });
            lock(&REGISTRY)
                .get_or_insert_with(HashMap::new)
                .insert((self.id, name), Arc::clone(&inner));
            Gate { inner, name }
        }
    }

    impl Drop for Scenario {
        fn drop(&mut self) {
            let mut reg = lock(&REGISTRY);
            if let Some(r) = reg.as_mut() {
                r.retain(|(id, _), gate| {
                    if *id == self.id {
                        lock(&gate.state).open = true;
                        gate.cv.notify_all();
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    /// `Scenario::attach` の結果。
    #[derive(Debug)]
    pub struct Attached {
        prev: u64,
    }

    impl Drop for Attached {
        fn drop(&mut self) {
            SCOPE.with(|s| s.set(self.prev));
        }
    }

    /// 止めている同期点の操作。
    #[derive(Debug)]
    pub struct Gate {
        inner: Arc<GateInner>,
        name: &'static str,
    }

    impl Gate {
        /// 点に着いて待っているスレッドの延べ数。
        #[must_use]
        pub fn arrived(&self) -> usize {
            lock(&self.inner.state).arrived
        }

        /// 少なくとも 1 つのスレッドが点に着くまで待つ（20 秒で失敗）。
        pub fn wait_arrived(&self) {
            self.wait_arrived_n(1);
        }

        /// 延べ `n` 回、点に着くまで待つ。
        pub fn wait_arrived_n(&self, n: usize) {
            let mut st = lock(&self.inner.state);
            let deadline = Instant::now() + STUCK_LIMIT;
            while st.arrived < n {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "no thread reached sync point \"{}\"",
                    self.name
                );
                st = self
                    .inner
                    .cv
                    .wait_timeout(st, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }

        /// 止めていたスレッドを進める。以降この点では止まらない。
        pub fn release(&self) {
            lock(&self.inner.state).open = true;
            self.inner.cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_blocked_thread_waits_until_release() {
        let sc = Scenario::new();
        let gate = sc.block_at("test.point");
        let passed = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let _a = sc.attach();
                crate::sync_point!("test.point");
                passed.store(true, Ordering::SeqCst);
            });
            gate.wait_arrived();
            assert!(!passed.load(Ordering::SeqCst));
            gate.release();
        });
        assert!(passed.load(Ordering::SeqCst));
        assert_eq!(gate.arrived(), 1);
    }

    #[test]
    fn threads_not_attached_pass_through() {
        let sc = Scenario::new();
        let gate = sc.block_at("test.other");
        crate::sync_point!("test.other"); // attach していないので止まらない
        assert_eq!(gate.arrived(), 0);
        let other = Scenario::new();
        let _a = other.attach();
        crate::sync_point!("test.other"); // 別のシナリオの点も止まらない
        assert_eq!(gate.arrived(), 0);
    }

    #[test]
    fn dropping_the_scenario_releases_waiters() {
        let sc = Scenario::new();
        let gate = sc.block_at("test.drop");
        std::thread::scope(|s| {
            let id_holder = &sc;
            s.spawn(|| {
                let _a = id_holder.attach();
                crate::sync_point!("test.drop");
            });
            gate.wait_arrived();
            gate.release();
        });
        drop(sc);
    }
}
