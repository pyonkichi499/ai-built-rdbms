//! 層 1 のクラッシュ試験（`m3.md` §7.5）。担当 T が実装する。
//!
//! `SimVfs` の上で、I/O の N 番目でクラッシュさせ、リカバリ後に不変条件 I1〜I12 を検査する。

mod invariants;
mod model;
mod mutation;
mod workload;

/// A が置いた足場。T が `workload` / `model` / `invariants` / `mutation` を実装して置き換える。
#[test]
fn scaffold_modules_are_linked() {
    assert!(workload::WORKLOAD_NAMES.is_empty());
}
