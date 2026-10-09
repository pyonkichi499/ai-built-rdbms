//! VACUUM の統計と autovacuum の設定（03 §5.9.1）。VC が `ModStats` ほかを足す。
//! F0b-1 は `ClusterOptions` が持つ `AutovacuumConfig` だけを置く。

use std::time::Duration;

/// 簡易 autovacuum の設定（既定は無効。00 D17）。
#[derive(Clone, Debug, PartialEq)]
pub struct AutovacuumConfig {
    pub enabled: bool,
    pub naptime: Duration,
    pub vacuum_threshold: u64,
    pub vacuum_scale_factor: f64,
    /// XID の数。clog の切り詰めを進めるための条件。
    pub freeze_max_age: u64,
}

impl Default for AutovacuumConfig {
    fn default() -> Self {
        AutovacuumConfig {
            enabled: false,
            naptime: Duration::from_secs(60),
            vacuum_threshold: 50,
            vacuum_scale_factor: 0.2,
            freeze_max_age: 200_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autovacuum_is_off_by_default() {
        let c = AutovacuumConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.naptime, Duration::from_secs(60));
        assert_eq!(c.vacuum_threshold, 50);
        assert_eq!(c.freeze_max_age, 200_000_000);
    }
}
