//! 日時の構文: INTERVAL の型修飾子（フィールド指定）、`SET TIME ZONE INTERVAL ...`。
//! `m5/06-types-datetime.md` §6.6。`gram.y` の `ConstInterval` / `opt_interval` / `interval_second`
//! に従う。`expr.rs` / `misc.rs` からは 1 行の呼び出しで使う（`m5/06` §11-6）。
//!
//! `TypeName.modifiers` は `interval_typmod_in` の `mods` の形（`[range]` または `[range, p]`）にする:
//! `interval(3)` は `[32767, 3]`、`interval year to month` は `[6]`、`interval second(3)` は `[4096, 3]`。

use yuzhu_datetime::{DateTimeEnv, Interval, IntervalStyle, TimeZone, ZoneDb, interval_range};

use super::Parser;
use crate::error::{Result, Span};
use crate::sql::ast::{Expr, Literal, SetArg, SetValue};
use crate::sql::token::TokenKind;
use crate::types::datetime::typmod_in;
use crate::types::oid;

/// INTERVAL のフィールドの名前とビット。
const FIELDS: [(&str, i32); 6] = [
    ("year", interval_range::YEAR),
    ("month", interval_range::MONTH),
    ("day", interval_range::DAY),
    ("hour", interval_range::HOUR),
    ("minute", interval_range::MINUTE),
    ("second", interval_range::SECOND),
];

/// `to` の右に書けるフィールド（`gram.y` の `opt_interval`）。
fn valid_to(from: &str) -> &'static [&'static str] {
    match from {
        "year" => &["month"],
        "day" => &["hour", "minute", "second"],
        "hour" => &["minute", "second"],
        "minute" => &["second"],
        _ => &[],
    }
}

/// `from` から `to` までのビットの和（`YEAR TO MONTH` = YEAR | MONTH、`DAY TO MINUTE` = DAY | HOUR | MINUTE）。
fn range_mask(from: &str, to: &str) -> i32 {
    let pos = |name: &str| FIELDS.iter().position(|(n, _)| *n == name).unwrap_or(0);
    let (lo, hi) = (pos(from), pos(to));
    FIELDS[lo..=hi].iter().fold(0, |m, (_, bit)| m | bit)
}

fn int_modifier(v: i64, span: Span) -> Expr {
    Expr::Literal {
        value: Literal::Integer(v.to_string()),
        span,
    }
}

impl Parser<'_> {
    /// `a AT TIME ZONE z` は `timezone(z, a)`、`a AT LOCAL` は `timezone(a)`（`gram.y`）。
    pub(super) fn at_time_zone(&mut self, left: Expr, zone: Option<Expr>, op_start: u32) -> Expr {
        let span = self.span_from(op_start);
        let args = match zone {
            Some(z) => vec![z, left],
            None => vec![left],
        };
        Expr::Function {
            name: crate::sql::ast::ObjectName {
                parts: vec![crate::sql::ast::Ident {
                    value: "timezone".to_owned(),
                    quoted: false,
                    span,
                }],
                span,
            },
            args,
            distinct: false,
            star: false,
            filter: None,
            order_by: Vec::new(),
            span,
        }
    }

    /// `Iconst`（int4 に収まる整数）。
    fn iconst(&mut self) -> Result<(i64, Span)> {
        if let TokenKind::Integer(s) = self.peek_kind()
            && let Ok(v) = s.parse::<i32>()
        {
            let span = self.advance().span;
            return Ok((i64::from(v), span));
        }
        Err(self.unexpected())
    }

    /// `INTERVAL` の直後（キーワードは消費済み）の修飾子。`allow_fields` が偽なら（型付きリテラルの
    /// 前置、`ConstInterval '(' Iconst ')' Sconst`）精度だけ。
    ///
    /// * なし → `[]`
    /// * `(p)` → `[FULL, p]`
    /// * フィールド指定（`allow_fields` のとき）→ `[mask]` または `[mask, p]`
    pub(super) fn parse_interval_modifiers(&mut self, allow_fields: bool) -> Result<Vec<Expr>> {
        if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            let (p, span) = self.iconst()?;
            self.expect(&TokenKind::RParen)?;
            return Ok(vec![
                int_modifier(i64::from(interval_range::FULL), span),
                int_modifier(p, span),
            ]);
        }
        if allow_fields {
            self.parse_interval_fields()
        } else {
            Ok(Vec::new())
        }
    }

    /// `opt_interval`（フィールド指定。なければ `[]`）。型付きリテラルの文字列の後ろにも書ける
    /// （`interval '1' day to second(3)`）。
    pub(super) fn parse_interval_fields(&mut self) -> Result<Vec<Expr>> {
        let Some(from) = FIELDS.iter().map(|(n, _)| *n).find(|n| self.is_kw(n)) else {
            return Ok(Vec::new());
        };
        let span = self.advance().span;
        let mut last = from;
        if self.is_kw("to") && !valid_to(from).is_empty() {
            self.advance();
            let to = valid_to(from)
                .iter()
                .copied()
                .find(|n| self.is_kw(n))
                .ok_or_else(|| self.unexpected())?;
            self.advance();
            last = to;
        }
        let mask = range_mask(from, last);
        let mut mods = vec![int_modifier(i64::from(mask), span)];
        // 精度は最後のフィールドが SECOND のときだけ（`interval_second`）。
        if last == "second" && self.peek_kind() == &TokenKind::LParen {
            self.advance();
            let (p, pspan) = self.iconst()?;
            self.expect(&TokenKind::RParen)?;
            mods.push(int_modifier(p, pspan));
        }
        Ok(mods)
    }

    /// `SET TIME ZONE INTERVAL 'text' [fields]`（`INTERVAL` は現在のトークン）。
    ///
    /// 値は postgres 形式の `INTERVAL '09:00:00'` という文字列にして `SetArg::String` で返す
    /// （`parse_timezone_setting` がこの形を受け付ける）。解釈できなければ元のテキストのまま
    /// `INTERVAL '<text>'` にして、SET 時の 22023 に任せる。
    pub(super) fn parse_zone_interval(&mut self) -> Result<SetValue> {
        self.expect_kw("interval")?;
        let text = match self.peek_kind().clone() {
            TokenKind::String(s) => {
                self.advance();
                s
            }
            _ => return Err(self.unexpected()),
        };
        let mods = self.parse_interval_fields()?;
        let rendered = render_zone_interval(&text, &mods);
        Ok(SetValue::Values(vec![SetArg::String(rendered)]))
    }
}

/// フィールド指定つきで `text` を解釈し、`INTERVAL '<postgres 形式>'` にする。
fn render_zone_interval(text: &str, mods: &[Expr]) -> String {
    let raw = |t: &str| format!("INTERVAL '{}'", t.replace('\'', "''"));
    let ints: Vec<i64> = mods
        .iter()
        .filter_map(|e| match e {
            Expr::Literal {
                value: Literal::Integer(s),
                ..
            } => s.parse().ok(),
            _ => None,
        })
        .collect();
    let mut notices = Vec::new();
    let Ok(typmod) = typmod_in(oid::INTERVAL, &ints, &mut notices) else {
        return raw(text);
    };
    let tz = TimeZone::utc();
    let zones = ZoneDb::without_tzdata();
    let mut env = DateTimeEnv::new(&tz, &zones);
    env.interval_style = IntervalStyle::Postgres;
    match Interval::parse(text, typmod, &env) {
        Ok(iv) => format!("INTERVAL '{}'", iv.format(IntervalStyle::Postgres)),
        Err(_) => raw(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods_of(sql: &str, allow_fields: bool) -> Vec<i64> {
        let mut p = Parser::new(sql);
        let mods = p.parse_interval_modifiers(allow_fields).unwrap();
        assert!(p.at_eof(), "unconsumed input in {sql:?}");
        mods.iter()
            .map(|e| match e {
                Expr::Literal {
                    value: Literal::Integer(s),
                    ..
                } => s.parse().unwrap(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn fails(sql: &str, allow_fields: bool) -> bool {
        let mut p = Parser::new(sql);
        match p.parse_interval_modifiers(allow_fields) {
            Ok(_) => !p.at_eof(),
            Err(e) => {
                assert_eq!(e.sqlstate.code(), "42601", "{sql}");
                true
            }
        }
    }

    #[test]
    fn precision_and_single_fields() {
        assert_eq!(mods_of("", true), Vec::<i64>::new());
        assert_eq!(mods_of("(3)", true), [32767, 3]);
        assert_eq!(mods_of("(0)", false), [32767, 0]);
        assert_eq!(mods_of("year", true), [4]);
        assert_eq!(mods_of("MONTH", true), [2]);
        assert_eq!(mods_of("day", true), [8]);
        assert_eq!(mods_of("hour", true), [1024]);
        assert_eq!(mods_of("minute", true), [2048]);
        assert_eq!(mods_of("second", true), [4096]);
        assert_eq!(mods_of("second(3)", true), [4096, 3]);
    }

    #[test]
    fn ranges() {
        assert_eq!(mods_of("year to month", true), [6]);
        assert_eq!(mods_of("day to hour", true), [8 | 1024]);
        assert_eq!(mods_of("day to minute", true), [8 | 1024 | 2048]);
        assert_eq!(mods_of("day to second", true), [8 | 1024 | 2048 | 4096]);
        assert_eq!(
            mods_of("day to second(2)", true),
            [8 | 1024 | 2048 | 4096, 2]
        );
        assert_eq!(mods_of("hour to minute", true), [1024 | 2048]);
        assert_eq!(mods_of("hour to second(6)", true), [1024 | 2048 | 4096, 6]);
        assert_eq!(mods_of("minute to second", true), [2048 | 4096]);
    }

    #[test]
    fn invalid_combinations_are_syntax_errors() {
        assert!(fails("year to day", true));
        assert!(fails("day to day", true));
        assert!(fails("second to minute", true));
        assert!(fails("month to year", true));
        assert!(fails("hour to hour", true));
        assert!(fails("day to", true));
        // 精度は最後のフィールドが SECOND のときだけ。残った `(` は呼び出し側が構文エラーにする
        assert!(fails("minute(3)", true));
        assert!(fails("year to month(2)", true));
        assert!(fails("second(3) to minute", true));
        assert!(fails("(", true));
        assert!(fails("(x)", true));
        // 前置の位置ではフィールドを取らない
        assert!(fails("year", false));
        assert!(fails("second(3)", false));
    }

    #[test]
    fn zone_interval_renders_postgres_form() {
        let mut p = Parser::new("interval '+09:00' hour to minute");
        let v = p.parse_zone_interval().unwrap();
        assert!(p.at_eof());
        assert_eq!(
            v,
            SetValue::Values(vec![SetArg::String("INTERVAL '09:00:00'".to_owned())])
        );
        let mut p = Parser::new("interval '-5:30'");
        assert_eq!(
            p.parse_zone_interval().unwrap(),
            SetValue::Values(vec![SetArg::String("INTERVAL '-05:30:00'".to_owned())])
        );
        let mut p = Parser::new("interval '1 day'");
        assert_eq!(
            p.parse_zone_interval().unwrap(),
            SetValue::Values(vec![SetArg::String("INTERVAL '1 day'".to_owned())])
        );
        // 解釈できない文字列は元のまま（SET 時の 22023 に任せる）
        let mut p = Parser::new("interval 'it''s'");
        assert_eq!(
            p.parse_zone_interval().unwrap(),
            SetValue::Values(vec![SetArg::String("INTERVAL 'it''s'".to_owned())])
        );
        let mut p = Parser::new("interval 9");
        assert_eq!(
            p.parse_zone_interval().unwrap_err().sqlstate.code(),
            "42601"
        );
    }
}
