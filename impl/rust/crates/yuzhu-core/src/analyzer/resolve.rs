//! Operator and function resolution (PostgreSQL's `parse_oper.c` /
//! `parse_func.c`): exact match, unknown-as-other-side, implicit-cast
//! candidates and `func_select_candidate`.

use super::Analyzer;
use super::bound::{BoundExpr, BoundExprKind, SessionValueKind};
use super::coerce::{CoercionContext, Pathway, tname};
use crate::catalog::{BuiltinOperator, builtin};
use crate::error::{Error, Result, Span, sqlstate};
use crate::types::{Oid, SqlType, oid};

/// Keeps the candidates with the highest score.
fn keep_max(cands: Vec<usize>, score: impl Fn(usize) -> usize) -> Vec<usize> {
    let scores: Vec<usize> = cands.iter().map(|&c| score(c)).collect();
    let max = scores.iter().copied().max().unwrap_or(0);
    cands
        .into_iter()
        .zip(scores)
        .filter(|(_, s)| *s == max)
        .map(|(c, _)| c)
        .collect()
}

fn format_args(types: &[Oid]) -> String {
    types
        .iter()
        .map(|t| tname(*t))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Analyzer<'_> {
    /// `func_match_argtypes`: candidates whose arguments the inputs can be
    /// implicitly coerced to.
    fn func_match_argtypes(&self, inputs: &[Oid], cands: &[Vec<Oid>]) -> Vec<usize> {
        (0..cands.len())
            .filter(|&c| {
                cands[c].len() == inputs.len()
                    && inputs
                        .iter()
                        .zip(&cands[c])
                        .all(|(i, a)| self.can_coerce(*i, *a, CoercionContext::Implicit))
            })
            .collect()
    }

    /// PostgreSQL's `func_select_candidate`. Returns `None` when the
    /// choice is ambiguous.
    #[allow(clippy::too_many_lines)]
    fn func_select_candidate(
        &self,
        inputs: &[Oid],
        all: &[Vec<Oid>],
        cands: Vec<usize>,
    ) -> Option<usize> {
        let unknown = |t: Oid| t == oid::UNKNOWN;
        let nunknowns = inputs.iter().filter(|t| unknown(**t)).count();

        // (c) most exact matches on known inputs.
        let cands = keep_max(cands, |c| {
            inputs
                .iter()
                .zip(&all[c])
                .filter(|(i, a)| !unknown(**i) && *i == *a)
                .count()
        });
        if cands.len() == 1 {
            return Some(cands[0]);
        }

        // (d) exact or preferred-in-the-input's-category matches.
        let cats: Vec<char> = inputs.iter().map(|t| self.category(*t)).collect();
        let cands = keep_max(cands, |c| {
            inputs
                .iter()
                .zip(&all[c])
                .enumerate()
                .filter(|(i, (inp, a))| {
                    !unknown(**inp)
                        && (*inp == *a
                            || (self.category(**a) == cats[*i] && self.is_preferred(**a)))
                })
                .count()
        });
        if cands.len() == 1 {
            return Some(cands[0]);
        }
        if nunknowns == 0 {
            return None;
        }

        // (e) guess the category of unknown inputs from the candidates.
        let mut cands = cands;
        let mut slot_cat = vec!['\0'; inputs.len()];
        let mut slot_pref = vec![false; inputs.len()];
        let mut resolved = true;
        for (i, inp) in inputs.iter().enumerate() {
            if !unknown(*inp) {
                continue;
            }
            let mut cat: Option<char> = None;
            let mut has_pref = false;
            let mut conflict = false;
            for &c in &cands {
                let t = all[c][i];
                let tc = self.category(t);
                let tp = self.is_preferred(t);
                match cat {
                    None => {
                        cat = Some(tc);
                        has_pref = tp;
                    }
                    Some(sc) if sc == tc => has_pref |= tp,
                    Some(_) => {
                        if tc == 'S' {
                            cat = Some(tc);
                            has_pref = tp;
                        } else {
                            conflict = true;
                        }
                    }
                }
            }
            if conflict && cat != Some('S') {
                resolved = false;
                break;
            }
            slot_cat[i] = cat.unwrap_or('\0');
            slot_pref[i] = has_pref;
        }
        if resolved {
            let kept: Vec<usize> = cands
                .iter()
                .copied()
                .filter(|&c| {
                    inputs.iter().enumerate().all(|(i, inp)| {
                        if !unknown(*inp) {
                            return true;
                        }
                        let t = all[c][i];
                        self.category(t) == slot_cat[i] && (!slot_pref[i] || self.is_preferred(t))
                    })
                })
                .collect();
            if !kept.is_empty() {
                cands = kept;
            }
            if cands.len() == 1 {
                return Some(cands[0]);
            }
        }

        // (f) if all known inputs have the same type, treat the unknowns as
        // that type too.
        if nunknowns < inputs.len() {
            let mut known: Option<Oid> = None;
            for t in inputs.iter().filter(|t| !unknown(**t)) {
                match known {
                    None => known = Some(*t),
                    Some(k) if k == *t => {}
                    Some(_) => {
                        known = None;
                        break;
                    }
                }
            }
            if let Some(k) = known {
                let same = vec![k; inputs.len()];
                let matching: Vec<usize> = cands
                    .iter()
                    .copied()
                    .filter(|&c| {
                        same.iter()
                            .zip(&all[c])
                            .all(|(i, a)| self.can_coerce(*i, *a, CoercionContext::Implicit))
                    })
                    .collect();
                if matching.len() == 1 {
                    return Some(matching[0]);
                }
            }
        }
        None
    }

    /// PostgreSQL's `oper` / `left_oper`: resolves an operator by name and
    /// input types (`left = None` for a prefix operator).
    pub(super) fn oper(
        &self,
        name: &str,
        left: Option<Oid>,
        right: Oid,
        span: Span,
    ) -> Result<&'static BuiltinOperator> {
        let all: Vec<&'static BuiltinOperator> = self
            .catalog
            .operators_named(name)
            .into_iter()
            .filter(|o| o.left.is_some() == left.is_some())
            .collect();

        // binary_oper_exact: an unknown side takes the other side's type.
        let (mut l, mut r) = (left, right);
        if let Some(lt) = left {
            if lt == oid::UNKNOWN && right != oid::UNKNOWN {
                l = Some(right);
            } else if right == oid::UNKNOWN && lt != oid::UNKNOWN {
                r = lt;
            }
        }
        if let Some(o) = all.iter().find(|o| o.left == l && o.right == r) {
            return Ok(o);
        }

        let inputs: Vec<Oid> = left.into_iter().chain([right]).collect();
        let cand_args: Vec<Vec<Oid>> = all
            .iter()
            .map(|o| o.left.into_iter().chain([o.right]).collect())
            .collect();
        let describe = || match left {
            Some(lt) => format!("{} {name} {}", tname(lt), tname(right)),
            None => format!("{name} {}", tname(right)),
        };
        let matched = self.func_match_argtypes(&inputs, &cand_args);
        let chosen = match matched.len() {
            0 => None,
            1 => Some(matched[0]),
            _ => match self.func_select_candidate(&inputs, &cand_args, matched) {
                Some(c) => Some(c),
                None => {
                    return Err(Error::new(
                        sqlstate::AMBIGUOUS_FUNCTION,
                        format!("operator is not unique: {}", describe()),
                    )
                    .with_hint("Could not choose a best candidate operator. You might need to add explicit type casts.")
                    .with_span(span));
                }
            },
        };
        chosen.map(|c| all[c]).ok_or_else(|| {
            Error::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!("operator does not exist: {}", describe()),
            )
            .with_hint("No operator matches the given name and argument types. You might need to add explicit type casts.")
            .with_span(span)
        })
    }

    /// Coerces an argument to a resolved operator / function argument type.
    /// Polymorphic arguments (`anynonarray` of `anytextcat`/`textanycat`)
    /// are converted to text explicitly, as their SQL definitions do.
    fn coerce_arg(&self, e: BoundExpr, declared: Oid) -> Result<BoundExpr> {
        if builtin::is_polymorphic(declared) {
            let span = e.span;
            return self.coerce_explicit(e, SqlType::TEXT, span);
        }
        if declared == oid::ANYARRAY {
            return Ok(e);
        }
        let (src, span) = (e.ty.oid, e.span);
        self.coerce_type(e, declared, CoercionContext::Implicit)?
            .ok_or_else(|| {
                Error::internal(format!(
                    "no implicit coercion from {} to {} for a resolved argument",
                    tname(src),
                    tname(declared)
                ))
                .with_span(span)
            })
    }

    /// `make_op`: resolves the operator and builds the call with coerced
    /// arguments.
    pub(super) fn make_op(
        &self,
        name: &str,
        left: Option<BoundExpr>,
        right: BoundExpr,
        span: Span,
    ) -> Result<BoundExpr> {
        let op = self.oper(name, left.as_ref().map(|l| l.ty.oid), right.ty.oid, span)?;
        let mut args = Vec::with_capacity(2);
        if let (Some(l), Some(lt)) = (left, op.left) {
            args.push(self.coerce_arg(l, lt)?);
        }
        args.push(self.coerce_arg(right, op.right)?);
        Ok(BoundExpr::new(
            BoundExprKind::Operator { op, args },
            SqlType::of(op.result),
            span,
        ))
    }

    /// `concat` / `concat_ws` (`VARIADIC "any"`): every argument but bool is converted to text.
    fn make_concat_call(&self, name: &str, args: Vec<BoundExpr>, span: Span) -> Result<BoundExpr> {
        let func = self
            .catalog
            .functions_named(name)
            .first()
            .copied()
            .ok_or_else(|| Error::internal(format!("built-in {name} is missing")))?;
        let args = args
            .into_iter()
            .map(|a| {
                if a.ty.oid == oid::BOOL || a.ty.oid == oid::TEXT {
                    Ok(a)
                } else {
                    self.coerce_explicit(a, SqlType::TEXT, span)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(BoundExpr::new(
            BoundExprKind::Function { func, args },
            SqlType::of(func.result),
            span,
        ))
    }

    /// Function call resolution (`func_get_detail`), including the
    /// function-style cast `typename(x)`.
    pub(super) fn make_func_call(
        &self,
        name: &str,
        args: Vec<BoundExpr>,
        span: Span,
    ) -> Result<BoundExpr> {
        // regtype is not supported, so `pg_typeof(x)` folds to the type name
        // as text (its text output equals the regtype output).
        if name == "pg_typeof" && args.len() == 1 {
            return Ok(BoundExpr::new(
                BoundExprKind::Literal(crate::types::Datum::Text(super::coerce::tname(
                    args[0].ty.oid,
                ))),
                SqlType::TEXT,
                span,
            ));
        }
        if (name == "concat" || name == "concat_ws") && !args.is_empty() {
            return self.make_concat_call(name, args, span);
        }
        let inputs: Vec<Oid> = args.iter().map(|a| a.ty.oid).collect();
        let all: Vec<_> = self
            .catalog
            .functions_named(name)
            .into_iter()
            .filter(|f| f.args.len() == inputs.len())
            .collect();
        let exact = all.iter().position(|f| f.args == inputs.as_slice());

        if exact.is_none() && args.len() == 1 {
            // FuncNameAsType: `int4('12')`, `text(1)`.
            if let Some(t) = self.catalog.type_by_name(name)
                && builtin::is_supported_type(t.oid)
                && t.oid != oid::UNKNOWN
            {
                let a = &args[0];
                let is_unknown_literal =
                    a.ty.oid == oid::UNKNOWN && matches!(a.kind, BoundExprKind::Literal(_));
                if is_unknown_literal
                    || matches!(
                        self.find_coercion_pathway(t.oid, a.ty.oid, CoercionContext::Explicit),
                        Pathway::Relabel | Pathway::CoerceViaIo
                    )
                {
                    let a = args.into_iter().next().expect("one argument");
                    return self.coerce_explicit(a, SqlType::of(t.oid), span);
                }
            }
        }

        let chosen = if let Some(i) = exact {
            i
        } else {
            let cand_args: Vec<Vec<Oid>> = all.iter().map(|f| f.args.to_vec()).collect();
            let matched = self.func_match_argtypes(&inputs, &cand_args);
            match matched.len() {
                    0 if all.is_empty() && is_unsupported_pg_function(name) => {
                        return Err(Error::new(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            format!("function {name}() is not supported"),
                        )
                        .with_span(span));
                    }
                    0 => {
                        return Err(Error::new(
                            sqlstate::UNDEFINED_FUNCTION,
                            format!("function {name}({}) does not exist", format_args(&inputs)),
                        )
                        .with_hint("No function matches the given name and argument types. You might need to add explicit type casts.")
                        .with_span(span));
                    }
                    1 => matched[0],
                    _ => self
                        .func_select_candidate(&inputs, &cand_args, matched)
                        .ok_or_else(|| {
                            Error::new(
                                sqlstate::AMBIGUOUS_FUNCTION,
                                format!("function {name}({}) is not unique", format_args(&inputs)),
                            )
                            .with_hint("Could not choose a best candidate function. You might need to add explicit type casts.")
                            .with_span(span)
                        })?,
                }
        };
        let func = all[chosen];

        // Functions that read session state become session values.
        let session = match func.oid {
            861 => Some(SessionValueKind::CurrentCatalog),
            1402 => Some(SessionValueKind::CurrentSchema),
            _ => None,
        };
        if let Some(kind) = session {
            return Ok(BoundExpr::new(
                BoundExprKind::SessionValue(kind),
                SqlType::of(func.result),
                span,
            ));
        }

        let args = args
            .into_iter()
            .zip(func.args)
            .map(|(a, t)| self.coerce_arg(a, *t))
            .collect::<Result<Vec<_>>>()?;
        Ok(BoundExpr::new(
            BoundExprKind::Function { func, args },
            SqlType::of(func.result),
            span,
        ))
    }
}

/// PostgreSQL functions that exist but need types yuzhu does not have yet
/// (timestamps) or XID assignment in read-only transactions. They fail with
/// 0A000 rather than 42883.
fn is_unsupported_pg_function(name: &str) -> bool {
    matches!(
        name,
        "now"
            | "statement_timestamp"
            | "transaction_timestamp"
            | "clock_timestamp"
            | "timeofday"
            | "txid_current"
            | "pg_current_xact_id"
    )
}
