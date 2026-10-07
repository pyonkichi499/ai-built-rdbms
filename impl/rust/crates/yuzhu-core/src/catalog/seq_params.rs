//! シーケンスのパラメータの検証（`init_params`・`parse_seq_int`。`m4/08-sequence-serial.md` §4.7、§6.1、§6.2）。
//!
//! PostgreSQL の `init_params`（`PG:src/backend/commands/sequence.c`）の検査の順序をそのまま写す。
//! 最初に出るエラーを PostgreSQL と一致させるため。パーサ・アナライザ・`ddl` が共有する純粋関数で、
//! 依存は `types` と `error` だけ。

use super::SequenceParams;
use crate::error::{Error, Result, sqlstate};
use crate::storage::SeqState;
use crate::types::{Oid, oid};

/// `CREATE` / `ALTER SEQUENCE` のオプションを文面どおりに保持したもの（検査前）。`None` = 指定なし。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeqOptions {
    /// `int2` / `int4` / `int8` の OID（アナライザが型名を解決した結果。違う型は `init_params` が 22023）。
    pub as_type: Option<Oid>,
    pub increment: Option<i64>,
    /// `Some(None)` = `NO MINVALUE`。
    pub min: Option<Option<i64>>,
    /// `Some(None)` = `NO MAXVALUE`。
    pub max: Option<Option<i64>>,
    pub start: Option<i64>,
    /// `Some(None)` = `RESTART`（`START` の値に戻す）。
    pub restart: Option<Option<i64>>,
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
}

impl SeqOptions {
    pub fn is_empty(&self) -> bool {
        *self == SeqOptions::default()
    }
}

#[derive(Debug)]
pub enum InitMode<'a> {
    Create,
    /// `current` = 変更前の `pg_sequence` の値、`state` = 変更前のシーケンスの状態。
    Alter {
        current: &'a SequenceParams,
        state: SeqState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitOutcome {
    /// 新しい値（`owned_by` は `None` で返す。呼び出し側が現在の値を引き継ぐ）。
    pub params: SequenceParams,
    /// 新しい状態: CREATE は（`start` または `RESTART` の値, 0, false）。ALTER は `RESTART` なら
    /// （値, 0, false）、そうでなければ（現在の `last_value`, 0, 現在の `is_called`）。
    pub state: SeqState,
    /// `RESTART` が指定された。
    pub restarted: bool,
}

fn invalid(msg: String) -> Error {
    Error::new(sqlstate::INVALID_PARAMETER_VALUE, msg)
}

/// `"smallint"` / `"integer"` / `"bigint"`。
pub fn seq_type_name(type_oid: Oid) -> &'static str {
    match type_oid {
        oid::INT2 => "smallint",
        oid::INT4 => "integer",
        _ => "bigint",
    }
}

fn type_max(type_oid: Oid) -> i64 {
    match type_oid {
        oid::INT2 => i64::from(i16::MAX),
        oid::INT4 => i64::from(i32::MAX),
        _ => i64::MAX,
    }
}

fn type_min(type_oid: Oid) -> i64 {
    match type_oid {
        oid::INT2 => i64::from(i16::MIN),
        oid::INT4 => i64::from(i32::MIN),
        _ => i64::MIN,
    }
}

fn in_type_range(type_oid: Oid, v: i64) -> bool {
    v >= type_min(type_oid) && v <= type_max(type_oid)
}

/// PostgreSQL の `init_params`（§6.2）。`for_identity` は IDENTITY 列のシーケンス（エラー文言が違う）。
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn init_params(
    opts: &SeqOptions,
    for_identity: bool,
    mode: InitMode<'_>,
) -> Result<InitOutcome> {
    let is_init = matches!(mode, InitMode::Create);
    let (mut f, mut data) = match &mode {
        InitMode::Create => (
            SequenceParams {
                type_oid: oid::INT8,
                start: 0,
                increment: 0,
                min: 0,
                max: 0,
                cache: 0,
                cycle: false,
                owned_by: None,
            },
            SeqState {
                last_value: 0,
                log_cnt: 0,
                is_called: false,
            },
        ),
        InitMode::Alter { current, state } => (**current, *state),
    };
    f.owned_by = None;
    let mut reset_max = false;
    let mut reset_min = false;

    // 1. AS type
    if let Some(t) = opts.as_type {
        if !matches!(t, oid::INT2 | oid::INT4 | oid::INT8) {
            return Err(invalid(
                if for_identity {
                    "identity column type must be smallint, integer, or bigint"
                } else {
                    "sequence type must be smallint, integer, or bigint"
                }
                .into(),
            ));
        }
        if !is_init {
            reset_max = f.max == type_max(f.type_oid);
            reset_min = f.min == type_min(f.type_oid);
        }
        f.type_oid = t;
    } else if is_init {
        f.type_oid = oid::INT8;
    }

    // 2. INCREMENT
    if let Some(v) = opts.increment {
        if v == 0 {
            return Err(invalid("INCREMENT must not be zero".into()));
        }
        f.increment = v;
    } else if is_init {
        f.increment = 1;
    }

    // 3. CYCLE
    if let Some(b) = opts.cycle {
        f.cycle = b;
    } else if is_init {
        f.cycle = false;
    }

    // 4. MAXVALUE
    if let Some(Some(v)) = opts.max {
        f.max = v;
    } else if is_init || opts.max.is_some() || reset_max {
        f.max = if f.increment > 0 || reset_max {
            type_max(f.type_oid)
        } else {
            -1
        };
    }
    if !in_type_range(f.type_oid, f.max) {
        return Err(invalid(format!(
            "MAXVALUE ({}) is out of range for sequence data type {}",
            f.max,
            seq_type_name(f.type_oid)
        )));
    }

    // 5. MINVALUE
    if let Some(Some(v)) = opts.min {
        f.min = v;
    } else if is_init || opts.min.is_some() || reset_min {
        f.min = if f.increment < 0 || reset_min {
            type_min(f.type_oid)
        } else {
            1
        };
    }
    if !in_type_range(f.type_oid, f.min) {
        return Err(invalid(format!(
            "MINVALUE ({}) is out of range for sequence data type {}",
            f.min,
            seq_type_name(f.type_oid)
        )));
    }

    // 6. MINVALUE < MAXVALUE
    if f.min >= f.max {
        return Err(invalid(format!(
            "MINVALUE ({}) must be less than MAXVALUE ({})",
            f.min, f.max
        )));
    }

    // 7. START
    if let Some(v) = opts.start {
        f.start = v;
    } else if is_init {
        f.start = if f.increment > 0 { f.min } else { f.max };
    }
    if f.start < f.min {
        return Err(invalid(format!(
            "START value ({}) cannot be less than MINVALUE ({})",
            f.start, f.min
        )));
    }
    if f.start > f.max {
        return Err(invalid(format!(
            "START value ({}) cannot be greater than MAXVALUE ({})",
            f.start, f.max
        )));
    }

    // 8. RESTART（なくても、現在の last_value を同じ式で検査する）
    match opts.restart {
        Some(r) => {
            data.last_value = r.unwrap_or(f.start);
            data.is_called = false;
            data.log_cnt = 0;
        }
        None if is_init => {
            data.last_value = f.start;
            data.is_called = false;
        }
        None => {}
    }
    if data.last_value < f.min {
        return Err(invalid(format!(
            "RESTART value ({}) cannot be less than MINVALUE ({})",
            data.last_value, f.min
        )));
    }
    if data.last_value > f.max {
        return Err(invalid(format!(
            "RESTART value ({}) cannot be greater than MAXVALUE ({})",
            data.last_value, f.max
        )));
    }

    // 9. CACHE
    if let Some(v) = opts.cache {
        if v <= 0 {
            return Err(invalid(format!("CACHE ({v}) must be greater than zero")));
        }
        f.cache = v;
    } else if is_init {
        f.cache = 1;
    }

    // yuzhu は ALTER でも常に log_cnt = 0（D8-9）。
    data.log_cnt = 0;
    Ok(InitOutcome {
        params: f,
        state: data,
        restarted: opts.restart.is_some(),
    })
}

/// オプションの数値のテキスト（符号つき。`+5`、`-1`、`1.5`、`9223372036854775808`）を `i64` に。
/// `int8` の入力関数と同じ: 範囲外は 22003、数字でなければ 22P02。
pub fn parse_seq_int(text: &str) -> Result<i64> {
    let bad = || {
        Error::new(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type bigint: \"{text}\""),
        )
    };
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r');
    let s = text.trim_matches(is_space);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    // 桁あふれは飽和させる（範囲外の判定にだけ使う）。
    let mut v: i128 = 0;
    for b in digits.bytes() {
        v = (v * 10 + i128::from(b - b'0')).min(i128::from(u64::MAX));
    }
    let v = if neg { -v } else { v };
    i64::try_from(v).map_err(|_| {
        Error::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{text}\" is out of range for type bigint"),
        )
    })
}

#[cfg(test)]
#[allow(clippy::many_single_char_names, clippy::too_many_lines)]
mod tests {
    use super::*;

    fn create(o: &SeqOptions) -> Result<InitOutcome> {
        init_params(o, false, InitMode::Create)
    }

    fn err(r: Result<InitOutcome>) -> (String, String) {
        let e = r.unwrap_err();
        (e.sqlstate.0.to_string(), e.message)
    }

    fn st(last_value: i64, log_cnt: i64, is_called: bool) -> SeqState {
        SeqState {
            last_value,
            log_cnt,
            is_called,
        }
    }

    #[test]
    fn defaults() {
        let o = create(&SeqOptions::default()).unwrap();
        assert_eq!(
            o.params,
            SequenceParams {
                type_oid: oid::INT8,
                start: 1,
                increment: 1,
                min: 1,
                max: i64::MAX,
                cache: 1,
                cycle: false,
                owned_by: None,
            }
        );
        assert_eq!(o.state, st(1, 0, false));
        assert!(!o.restarted);
        assert!(SeqOptions::default().is_empty());
        assert!(
            !SeqOptions {
                cycle: Some(false),
                ..SeqOptions::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn descending_with_no_minvalue_and_max_5() {
        let o = create(&SeqOptions {
            min: Some(None),
            max: Some(Some(5)),
            increment: Some(-1),
            ..SeqOptions::default()
        })
        .unwrap();
        assert_eq!(o.params.min, i64::MIN);
        assert_eq!(o.params.max, 5);
        assert_eq!(o.params.start, 5);
    }

    #[test]
    fn descending_default_max_is_minus_one() {
        let e = err(create(&SeqOptions {
            increment: Some(-1),
            start: Some(5),
            ..SeqOptions::default()
        }));
        assert_eq!(e.0, "22023");
        assert_eq!(e.1, "START value (5) cannot be greater than MAXVALUE (-1)");
    }

    #[test]
    fn smallint_start_out_of_range() {
        let e = err(create(&SeqOptions {
            as_type: Some(oid::INT2),
            start: Some(40000),
            ..SeqOptions::default()
        }));
        assert_eq!(
            e.1,
            "START value (40000) cannot be greater than MAXVALUE (32767)"
        );
    }

    #[test]
    fn integer_descending() {
        let o = create(&SeqOptions {
            as_type: Some(oid::INT4),
            increment: Some(-1),
            ..SeqOptions::default()
        })
        .unwrap();
        assert_eq!(
            (o.params.min, o.params.max, o.params.start),
            (-2_147_483_648, -1, -1)
        );
    }

    #[test]
    fn maxvalue_zero_minvalue_minus_five() {
        let o = create(&SeqOptions {
            max: Some(Some(0)),
            min: Some(Some(-5)),
            ..SeqOptions::default()
        })
        .unwrap();
        assert_eq!(o.params.start, -5);
    }

    #[test]
    fn start_and_restart() {
        let o = create(&SeqOptions {
            start: Some(3),
            restart: Some(Some(4)),
            ..SeqOptions::default()
        })
        .unwrap();
        assert_eq!(o.params.start, 3);
        assert_eq!(o.state, st(4, 0, false));
        assert!(o.restarted);
        let o = create(&SeqOptions {
            start: Some(7),
            restart: Some(None),
            ..SeqOptions::default()
        })
        .unwrap();
        assert_eq!(o.state, st(7, 0, false));
    }

    #[test]
    fn error_table() {
        let case = |o: SeqOptions, ident: bool| {
            let r = init_params(&o, ident, InitMode::Create);
            let e = r.unwrap_err();
            (e.sqlstate.0.to_string(), e.message)
        };
        let d = SeqOptions::default;
        assert_eq!(
            case(
                SeqOptions {
                    as_type: Some(oid::TEXT),
                    ..d()
                },
                false
            ),
            (
                "22023".into(),
                "sequence type must be smallint, integer, or bigint".into()
            )
        );
        assert_eq!(
            case(
                SeqOptions {
                    as_type: Some(oid::TEXT),
                    ..d()
                },
                true
            )
            .1,
            "identity column type must be smallint, integer, or bigint"
        );
        assert_eq!(
            case(
                SeqOptions {
                    increment: Some(0),
                    ..d()
                },
                false
            ),
            ("22023".into(), "INCREMENT must not be zero".into())
        );
        assert_eq!(
            case(
                SeqOptions {
                    as_type: Some(oid::INT2),
                    max: Some(Some(40000)),
                    ..d()
                },
                false
            )
            .1,
            "MAXVALUE (40000) is out of range for sequence data type smallint"
        );
        assert_eq!(
            case(
                SeqOptions {
                    as_type: Some(oid::INT4),
                    min: Some(Some(-3_000_000_000)),
                    ..d()
                },
                false
            )
            .1,
            "MINVALUE (-3000000000) is out of range for sequence data type integer"
        );
        assert_eq!(
            case(
                SeqOptions {
                    min: Some(Some(5)),
                    max: Some(Some(1)),
                    ..d()
                },
                false
            )
            .1,
            "MINVALUE (5) must be less than MAXVALUE (1)"
        );
        assert_eq!(
            case(
                SeqOptions {
                    min: Some(Some(1)),
                    start: Some(0),
                    ..d()
                },
                false
            )
            .1,
            "START value (0) cannot be less than MINVALUE (1)"
        );
        assert_eq!(
            case(
                SeqOptions {
                    max: Some(Some(10)),
                    start: Some(11),
                    ..d()
                },
                false
            )
            .1,
            "START value (11) cannot be greater than MAXVALUE (10)"
        );
        assert_eq!(
            case(
                SeqOptions {
                    max: Some(Some(100)),
                    restart: Some(Some(500)),
                    ..d()
                },
                false
            )
            .1,
            "RESTART value (500) cannot be greater than MAXVALUE (100)"
        );
        assert_eq!(
            case(
                SeqOptions {
                    min: Some(Some(10)),
                    restart: Some(Some(5)),
                    start: Some(10),
                    ..d()
                },
                false
            )
            .1,
            "RESTART value (5) cannot be less than MINVALUE (10)"
        );
        assert_eq!(
            case(
                SeqOptions {
                    cache: Some(0),
                    ..d()
                },
                false
            ),
            ("22023".into(), "CACHE (0) must be greater than zero".into())
        );
    }

    fn base(type_oid: Oid, min: i64, max: i64, increment: i64) -> SequenceParams {
        SequenceParams {
            type_oid,
            start: if increment > 0 { min } else { max },
            increment,
            min,
            max,
            cache: 1,
            cycle: false,
            owned_by: Some((16384, 1)),
        }
    }

    fn alter(cur: &SequenceParams, state: SeqState, o: &SeqOptions) -> Result<InitOutcome> {
        init_params(
            o,
            false,
            InitMode::Alter {
                current: cur,
                state,
            },
        )
    }

    #[test]
    fn alter_as_smallint_keeps_explicit_max() {
        let cur = base(oid::INT8, 1, 100, 1);
        let o = alter(
            &cur,
            st(1, 5, false),
            &SeqOptions {
                as_type: Some(oid::INT2),
                ..SeqOptions::default()
            },
        )
        .unwrap();
        assert_eq!(o.params.type_oid, oid::INT2);
        assert_eq!(o.params.max, 100);
        // owned_by は呼び出し側が引き継ぐ。log_cnt は 0 に戻る。
        assert_eq!(o.params.owned_by, None);
        assert_eq!(o.state, st(1, 0, false));
        assert!(!o.restarted);
    }

    #[test]
    fn alter_as_resets_type_limits() {
        let cur = base(oid::INT4, i64::from(i32::MIN), -1, -1);
        let o = alter(
            &cur,
            st(-1, 0, false),
            &SeqOptions {
                as_type: Some(oid::INT2),
                ..SeqOptions::default()
            },
        )
        .unwrap();
        assert_eq!((o.params.min, o.params.max), (-32768, -1));
        // 昇順の型の最大が既定のときは最大が型に追従する。
        let cur = base(oid::INT8, 1, i64::MAX, 1);
        let o = alter(
            &cur,
            st(1, 0, false),
            &SeqOptions {
                as_type: Some(oid::INT4),
                ..SeqOptions::default()
            },
        )
        .unwrap();
        assert_eq!(o.params.max, i64::from(i32::MAX));
    }

    #[test]
    fn alter_checks_the_current_value() {
        let cur = base(oid::INT8, 1, 1000, 1);
        let e = err(alter(
            &cur,
            st(50, 0, false),
            &SeqOptions {
                max: Some(Some(40)),
                ..SeqOptions::default()
            },
        ));
        assert_eq!(e.0, "22023");
        assert_eq!(
            e.1,
            "RESTART value (50) cannot be greater than MAXVALUE (40)"
        );
        let mut cur = base(oid::INT8, 1, 1000, 1);
        cur.start = 20;
        let e = err(alter(
            &cur,
            st(50, 0, true),
            &SeqOptions {
                min: Some(Some(30)),
                ..SeqOptions::default()
            },
        ));
        assert_eq!(e.1, "START value (20) cannot be less than MINVALUE (30)");
    }

    #[test]
    fn alter_restart_and_unchanged_state() {
        let cur = base(oid::INT8, 1, 1000, 1);
        let o = alter(
            &cur,
            st(50, 3, true),
            &SeqOptions {
                restart: Some(Some(7)),
                ..SeqOptions::default()
            },
        )
        .unwrap();
        assert_eq!(o.state, st(7, 0, false));
        assert!(o.restarted);
        let o = alter(
            &cur,
            st(50, 3, true),
            &SeqOptions {
                cache: Some(10),
                ..SeqOptions::default()
            },
        )
        .unwrap();
        assert_eq!(o.state, st(50, 0, true));
        assert_eq!(o.params.cache, 10);
        let o = alter(
            &cur,
            st(50, 3, true),
            &SeqOptions {
                restart: Some(None),
                ..SeqOptions::default()
            },
        )
        .unwrap();
        assert_eq!(o.state, st(cur.start, 0, false));
    }

    #[test]
    fn alter_no_minvalue_resets_to_default() {
        let mut cur = base(oid::INT8, 5, 1000, 1);
        cur.start = 5;
        let o = alter(
            &cur,
            st(5, 0, false),
            &SeqOptions {
                min: Some(None),
                ..SeqOptions::default()
            },
        )
        .unwrap();
        assert_eq!(o.params.min, 1);
    }

    #[test]
    fn parse_ints() {
        assert_eq!(parse_seq_int("+5").unwrap(), 5);
        assert_eq!(parse_seq_int("-1").unwrap(), -1);
        assert_eq!(parse_seq_int("  42  ").unwrap(), 42);
        assert_eq!(parse_seq_int("-9223372036854775808").unwrap(), i64::MIN);
        assert_eq!(parse_seq_int("9223372036854775807").unwrap(), i64::MAX);
        let e = parse_seq_int("9223372036854775808").unwrap_err();
        assert_eq!(e.sqlstate.0, "22003");
        assert_eq!(
            e.message,
            "value \"9223372036854775808\" is out of range for type bigint"
        );
        let e = parse_seq_int("99999999999999999999999999999999999999999").unwrap_err();
        assert_eq!(e.sqlstate.0, "22003");
        for s in ["1.5", "3.0", "", "-", "abc", "1 2", "--1"] {
            let e = parse_seq_int(s).unwrap_err();
            assert_eq!(e.sqlstate.0, "22P02", "{s}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type bigint: \"{s}\"")
            );
        }
    }

    #[test]
    fn type_names() {
        assert_eq!(seq_type_name(oid::INT2), "smallint");
        assert_eq!(seq_type_name(oid::INT4), "integer");
        assert_eq!(seq_type_name(oid::INT8), "bigint");
    }
}
