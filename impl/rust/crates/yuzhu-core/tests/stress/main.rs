//! 並行ストレス（`m5/10` §6.7。TS-2）。プロセス内の `TestCluster` に対して走らせる。
//!
//! 環境変数: `YUZHU_STRESS_SEED`、`YUZHU_STRESS_THREADS`、`YUZHU_STRESS_SECS`。
//! 再現: `YUZHU_STRESS_SEED=<seed> cargo test -p yuzhu-core --test stress <name>`。

mod s0;
