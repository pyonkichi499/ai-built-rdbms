//! `ExplainNode` の木から `QUERY PLAN` の行を作る（`m4/10-explain-copy-compat.md` §3.1、§3.7〜§3.10）。
//!
//! 字下げの規則（PostgreSQL の `ExplainNode` と同じ）:
//!
//! - 根はタイトルを 0 桁、詳細行（`Output:` と `details`）を 2 桁に置く。
//! - 通常の子は、親の詳細行の桁に `->  ` を置いてタイトルを続ける（根の子は 2 桁）。子の詳細行はその矢印の
//!   桁 + 6。
//! - ラベルつきの子（`InitPlan 1`、`SubPlan 1`、`CTE c`）は、親の詳細行の桁にラベルだけの行を置き、その 2 桁
//!   下に矢印を置く。
//! - 行の順は、`Output:`、`details`（`Rows Removed by ...` は対応する詳細行の直後）、`children`（木の並びのまま）。

use std::fmt::Write as _;
use std::time::Duration;

use crate::analyzer::query::ExplainOptions;
use crate::executor::instrument::{Instrumentation, NodeCounters};
use crate::planner::physical::{ExplainNode, RemovedRows};

/// `Planning Time` と `Execution Time` に出す時間。
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings {
    pub planning: Duration,
    /// `ANALYZE` のときだけ。
    pub execution: Option<Duration>,
}

/// 根のノードの詳細行の桁。
const ROOT_DETAIL_COL: usize = 2;
/// 矢印の桁から、子のタイトルまでの幅（`->  `）。
const ARROW_WIDTH: usize = 4;

/// `root` を行にする。`opts.analyze` のとき `instr` の計測値を使う（`instr` が `None`、またはノードの番号が
/// 範囲外なら `(never executed)`）。`summary` が true なら最後に `Planning Time` と、`ANALYZE` なら
/// `Execution Time` を足す。
pub fn render_plan(
    root: &ExplainNode,
    opts: &ExplainOptions,
    instr: Option<&Instrumentation>,
    times: &Timings,
) -> Vec<String> {
    if let Some(i) = instr {
        i.finish();
    }
    let mut r = Renderer {
        opts,
        instr,
        out: Vec::new(),
    };
    r.node(root, None);
    if opts.summary {
        r.out
            .push(format!("Planning Time: {} ms", millis(times.planning)));
        if opts.analyze
            && let Some(t) = times.execution
        {
            r.out.push(format!("Execution Time: {} ms", millis(t)));
        }
    }
    r.out
}

/// `{:.3}` のミリ秒。
fn millis(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1000.0)
}

struct Renderer<'a> {
    opts: &'a ExplainOptions,
    instr: Option<&'a Instrumentation>,
    out: Vec<String>,
}

impl<'a> Renderer<'a> {
    /// `arrow_col`: 根は `None`、それ以外は `->` の桁。
    fn node(&mut self, n: &ExplainNode, arrow_col: Option<usize>) {
        let mut line = match arrow_col {
            None => n.title.clone(),
            Some(a) => format!("{}->  {}", " ".repeat(a), n.title),
        };
        if self.opts.costs {
            let _ = write!(line, "  (cost=0.00..0.00 rows=0 width={})", n.width);
        }
        let counters = self.counters(n.exec_id);
        if self.opts.analyze {
            line.push(' ');
            line.push_str(&self.actual(counters));
        }
        self.out.push(line);

        let col = arrow_col.map_or(ROOT_DETAIL_COL, |a| a + ARROW_WIDTH + 2);
        let pad = " ".repeat(col);
        if !n.output.is_empty() {
            self.out
                .push(format!("{pad}Output: {}", n.output.join(", ")));
        }
        for d in &n.details {
            self.out.push(format!("{pad}{}: {}", d.label, d.text));
            if self.opts.analyze
                && let Some(r) = &d.removed
                && let Some(c) = counters
                && let Some(v) = self.removed(r, c)
            {
                self.out.push(format!("{pad}{}: {v}", r.label));
            }
        }
        for c in &n.children {
            match &c.label {
                Some(label) => {
                    self.out.push(format!("{pad}{label}"));
                    self.node(&c.node, Some(col + 2));
                }
                None => self.node(&c.node, Some(col)),
            }
        }
    }

    fn counters(&self, id: usize) -> Option<&'a NodeCounters> {
        self.instr.and_then(|i| i.get(id))
    }

    /// `(actual time=0.010..0.020 rows=2 loops=3)` / `(actual rows=2 loops=3)` / `(never executed)`。
    fn actual(&self, c: Option<&NodeCounters>) -> String {
        let Some(c) = c.filter(|c| c.loops.get() > 0) else {
            return "(never executed)".to_owned();
        };
        let loops = c.loops.get();
        #[allow(clippy::cast_precision_loss)]
        let div = loops as f64;
        #[allow(clippy::cast_precision_loss)]
        let rows = c.rows.get() as f64 / div;
        if self.opts.timing {
            let startup = c.startup.get().as_secs_f64() * 1000.0 / div;
            let total = c.total.get().as_secs_f64() * 1000.0 / div;
            format!("(actual time={startup:.3}..{total:.3} rows={rows:.0} loops={loops})")
        } else {
            format!("(actual rows={rows:.0} loops={loops})")
        }
    }

    /// `Rows Removed by ...` の値（1 ループ平均を丸めたもの）。合計が 0 なら `None`（出さない）。
    fn removed(&self, r: &RemovedRows, c: &NodeCounters) -> Option<String> {
        let loops = c.loops.get();
        if loops == 0 {
            return None;
        }
        let total: u64 = r
            .sources
            .iter()
            .filter_map(|(id, counter)| self.counters(*id).map(|c| c.removed(*counter)))
            .sum();
        if total == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let avg = total as f64 / loops as f64;
        Some(format!("{avg:.0}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::physical::{ExplainChild, ExplainDetail, FilterCounter};

    // ----- 木の組み立て ------------------------------------------------------

    fn node(title: &str, exec_id: usize) -> ExplainNode {
        ExplainNode {
            title: title.to_owned(),
            details: vec![],
            output: vec![],
            children: vec![],
            exec_id,
            width: 4,
        }
    }

    impl ExplainNode {
        fn detail(mut self, label: &'static str, text: &str) -> Self {
            self.details.push(ExplainDetail {
                label,
                text: text.to_owned(),
                removed: None,
            });
            self
        }

        fn filter(mut self, label: &'static str, text: &str, sources: &[usize]) -> Self {
            self.details.push(ExplainDetail {
                label,
                text: text.to_owned(),
                removed: Some(RemovedRows {
                    label: if label == "Join Filter" {
                        "Rows Removed by Join Filter"
                    } else {
                        "Rows Removed by Filter"
                    },
                    sources: sources
                        .iter()
                        .map(|id| {
                            (
                                *id,
                                if label == "Join Filter" {
                                    FilterCounter::JoinFilter
                                } else {
                                    FilterCounter::Filter
                                },
                            )
                        })
                        .collect(),
                }),
            });
            self
        }

        fn output(mut self, cols: &[&str]) -> Self {
            self.output = cols.iter().map(|s| (*s).to_owned()).collect();
            self
        }

        fn child(mut self, n: ExplainNode) -> Self {
            self.children.push(ExplainChild {
                label: None,
                node: n,
            });
            self
        }

        fn labeled(mut self, label: &str, n: ExplainNode) -> Self {
            self.children.push(ExplainChild {
                label: Some(label.to_owned()),
                node: n,
            });
            self
        }
    }

    #[allow(clippy::fn_params_excessive_bools)]
    fn opts(analyze: bool, costs: bool, timing: bool, summary: bool) -> ExplainOptions {
        ExplainOptions {
            analyze,
            verbose: false,
            costs,
            timing,
            summary,
        }
    }

    /// `COSTS OFF`（ANALYZE なし）。
    fn plain() -> ExplainOptions {
        opts(false, false, false, false)
    }

    fn lines(root: &ExplainNode, o: ExplainOptions) -> String {
        render_plan(root, &o, None, &Timings::default()).join("\n")
    }

    // ----- §3.11 の出力例（COSTS OFF） ------------------------------------------

    #[test]
    fn single_node_with_one_time_filter() {
        let n = node("Result", 0).detail("One-Time Filter", "false");
        assert_eq!(lines(&n, plain()), "Result\n  One-Time Filter: false");
    }

    #[test]
    fn verbose_output_comes_before_details() {
        let n = node("Seq Scan on public.t", 0)
            .output(&["a", "b", "c"])
            .detail("Filter", "(t.b > 5)");
        assert_eq!(
            lines(&n, plain()),
            "Seq Scan on public.t\n  Output: a, b, c\n  Filter: (t.b > 5)"
        );
    }

    #[test]
    fn hash_join_tree_with_nested_children() {
        let n = node("Sort", 0).detail("Sort Key", "t.a").child(
            node("Hash Join", 1)
                .detail("Hash Cond", "(u.a = t.a)")
                .child(node("Seq Scan on u", 2))
                .child(node("Hash", 3).child(node("Seq Scan on t", 4).detail("Filter", "(b > 5)"))),
        );
        let expect = "\
Sort
  Sort Key: t.a
  ->  Hash Join
        Hash Cond: (u.a = t.a)
        ->  Seq Scan on u
        ->  Hash
              ->  Seq Scan on t
                    Filter: (b > 5)";
        assert_eq!(lines(&n, plain()), expect);
    }

    #[test]
    fn verbose_join_output_lines() {
        let n = node("Sort", 0)
            .output(&["t.a", "t.b"])
            .detail("Sort Key", "t.a")
            .child(
                node("Hash Join", 1)
                    .output(&["t.a", "t.b"])
                    .detail("Hash Cond", "(u.a = t.a)")
                    .child(node("Seq Scan on public.u", 2).output(&["u.a"]))
                    .child(
                        node("Hash", 3)
                            .output(&["t.a", "t.b"])
                            .child(node("Seq Scan on public.t", 4).output(&["t.a", "t.b"])),
                    ),
            );
        let expect = "\
Sort
  Output: t.a, t.b
  Sort Key: t.a
  ->  Hash Join
        Output: t.a, t.b
        Hash Cond: (u.a = t.a)
        ->  Seq Scan on public.u
              Output: u.a
        ->  Hash
              Output: t.a, t.b
              ->  Seq Scan on public.t
                    Output: t.a, t.b";
        assert_eq!(lines(&n, plain()), expect);
    }

    #[test]
    fn init_plan_is_a_labeled_child_before_plain_children() {
        // InitPlan は根の子で、通常の子より前（C-21）。
        let n = node("Sort", 0)
            .detail("Sort Key", "t.a")
            .labeled("InitPlan 1", node("Result", 2))
            .child(node("Seq Scan on t", 1).detail("Filter", "(b = (InitPlan 1).col1)"));
        let expect = "\
Sort
  Sort Key: t.a
  InitPlan 1
    ->  Result
  ->  Seq Scan on t
        Filter: (b = (InitPlan 1).col1)";
        assert_eq!(lines(&n, plain()), expect);
    }

    #[test]
    fn labeled_subtree_indents_its_own_children() {
        let n = node("Seq Scan on t", 0)
            .detail("Filter", "(b = (InitPlan 1).col1)")
            .labeled(
                "InitPlan 1",
                node("Aggregate", 1).child(node("Seq Scan on u2", 2)),
            );
        let expect = "\
Seq Scan on t
  Filter: (b = (InitPlan 1).col1)
  InitPlan 1
    ->  Aggregate
          ->  Seq Scan on u2";
        assert_eq!(lines(&n, plain()), expect);
    }

    #[test]
    fn sub_plan_follows_plain_children() {
        let n = node("Hash Join", 0)
            .detail("Hash Cond", "(u2.b = t.b)")
            .child(node("Seq Scan on u2", 1))
            .child(node("Hash", 2).child(node("Seq Scan on t", 3)))
            .labeled(
                "SubPlan 1",
                node("Aggregate", 4).child(node("Seq Scan on u", 5).detail("Filter", "(a = t.a)")),
            );
        let expect = "\
Hash Join
  Hash Cond: (u2.b = t.b)
  ->  Seq Scan on u2
  ->  Hash
        ->  Seq Scan on t
  SubPlan 1
    ->  Aggregate
          ->  Seq Scan on u
                Filter: (a = t.a)";
        assert_eq!(lines(&n, plain()), expect);
    }

    #[test]
    fn cte_label_precedes_plain_children() {
        let n = node("Hash Join", 0)
            .detail("Hash Cond", "(c1.a = c2.a)")
            .labeled("CTE c", node("Seq Scan on t", 3))
            .child(node("CTE Scan on c c1", 1))
            .child(node("Hash", 2).child(node("CTE Scan on c c2", 2)));
        let expect = "\
Hash Join
  Hash Cond: (c1.a = c2.a)
  CTE c
    ->  Seq Scan on t
  ->  CTE Scan on c c1
  ->  Hash
        ->  CTE Scan on c c2";
        assert_eq!(lines(&n, plain()), expect);
    }

    #[test]
    fn nested_labeled_child_inside_a_labeled_child() {
        let n = node("Seq Scan on t", 0).labeled(
            "SubPlan 2",
            node("Seq Scan on u", 1)
                .detail("Filter", "(x = (InitPlan 1).col1)")
                .labeled("InitPlan 1", node("Result", 2)),
        );
        let expect = "\
Seq Scan on t
  SubPlan 2
    ->  Seq Scan on u
          Filter: (x = (InitPlan 1).col1)
          InitPlan 1
            ->  Result";
        assert_eq!(lines(&n, plain()), expect);
    }

    #[test]
    fn dml_node_with_child() {
        let n = node("Update on t", 0)
            .labeled("InitPlan 1", node("Result", 2))
            .child(node("Index Scan using t_pkey on t", 1).detail("Index Cond", "(a = 1)"));
        let expect = "\
Update on t
  InitPlan 1
    ->  Result
  ->  Index Scan using t_pkey on t
        Index Cond: (a = 1)";
        assert_eq!(lines(&n, plain()), expect);
    }

    // ----- コスト欄 ---------------------------------------------------------------

    #[test]
    fn cost_column_has_two_spaces_and_covers_every_node() {
        let mut inner = node("Seq Scan on t", 1);
        inner.width = 8;
        let n = node("Result", 0)
            .labeled("InitPlan 1", node("Result", 2))
            .child(inner);
        let o = opts(false, true, false, false);
        let expect = "\
Result  (cost=0.00..0.00 rows=0 width=4)
  InitPlan 1
    ->  Result  (cost=0.00..0.00 rows=0 width=4)
  ->  Seq Scan on t  (cost=0.00..0.00 rows=0 width=8)";
        assert_eq!(lines(&n, o), expect);
    }

    // ----- ANALYZE ----------------------------------------------------------------

    /// 番号 `id` のノードに、`loops` 回の実行で合計 `rows` 行、`removed_filter` / `removed_join` を設定する。
    fn set(i: &Instrumentation, id: usize, loops: u64, rows: u64, rf: u64, rj: u64) {
        let c = i.node(id);
        c.loops.set(loops);
        c.rows.set(rows);
        c.removed_filter.set(rf);
        c.removed_join_filter.set(rj);
    }

    fn render(root: &ExplainNode, o: ExplainOptions, i: &Instrumentation) -> String {
        render_plan(root, &o, Some(i), &Timings::default()).join("\n")
    }

    #[test]
    fn analyze_without_timing_and_rows_removed() {
        let n = node("Seq Scan on t", 0).filter("Filter", "(b > 5)", &[0]);
        let i = Instrumentation::new(1);
        set(&i, 0, 1, 400, 600, 0);
        let o = opts(true, false, false, false);
        assert_eq!(
            render(&n, o, &i),
            "Seq Scan on t (actual rows=400 loops=1)\n  Filter: (b > 5)\n  Rows Removed by Filter: 600"
        );
    }

    #[test]
    fn analyze_with_costs_puts_one_space_before_actual() {
        let n = node("Result", 0);
        let i = Instrumentation::new(1);
        set(&i, 0, 1, 1, 0, 0);
        let o = opts(true, true, false, false);
        assert_eq!(
            render(&n, o, &i),
            "Result  (cost=0.00..0.00 rows=0 width=4) (actual rows=1 loops=1)"
        );
    }

    #[test]
    fn analyze_with_timing_averages_by_loops() {
        let n = node("Index Scan using u_a_idx on u", 0).detail("Index Cond", "(a = t.a)");
        let i = Instrumentation::new(1);
        set(&i, 0, 100, 200, 0, 0);
        i.node(0).startup.set(Duration::from_micros(500));
        i.node(0).total.set(Duration::from_millis(250));
        let o = opts(true, false, true, false);
        // startup 0.5ms / 100 = 0.005、total 250ms / 100 = 2.500。rows 200 / 100 = 2。
        assert_eq!(
            render(&n, o, &i),
            "Index Scan using u_a_idx on u (actual time=0.005..2.500 rows=2 loops=100)\n  Index Cond: (a = t.a)"
        );
    }

    #[test]
    fn rows_are_rounded_to_integers() {
        let n = node("Seq Scan on u", 0);
        let i = Instrumentation::new(1);
        // 5 / 3 = 1.67 → 2、7 / 3 = 2.33 → 2。
        set(&i, 0, 3, 5, 0, 0);
        let o = opts(true, false, false, false);
        assert_eq!(render(&n, o, &i), "Seq Scan on u (actual rows=2 loops=3)");
        set(&i, 0, 3, 7, 0, 0);
        assert_eq!(render(&n, o, &i), "Seq Scan on u (actual rows=2 loops=3)");
        set(&i, 0, 4, 2, 0, 0);
        // 0.5 は偶数への丸め（printf と同じ）。
        assert_eq!(render(&n, o, &i), "Seq Scan on u (actual rows=0 loops=4)");
    }

    #[test]
    fn never_executed_with_and_without_costs_and_timing() {
        let n = node("Result", 0).filter("Filter", "(a > 1)", &[0]);
        let i = Instrumentation::new(1);
        // 計測値があっても loops = 0 なら出さない。
        i.node(0).removed_filter.set(5);
        for (o, expect) in [
            (
                opts(true, false, false, false),
                "Result (never executed)\n  Filter: (a > 1)",
            ),
            (
                opts(true, false, true, false),
                "Result (never executed)\n  Filter: (a > 1)",
            ),
            (
                opts(true, true, true, false),
                "Result  (cost=0.00..0.00 rows=0 width=4) (never executed)\n  Filter: (a > 1)",
            ),
        ] {
            assert_eq!(render(&n, o, &i), expect);
        }
    }

    #[test]
    fn missing_instrumentation_is_never_executed() {
        let n = node("Result", 3);
        let o = opts(true, false, false, false);
        assert_eq!(lines(&n, o), "Result (never executed)");
        let i = Instrumentation::new(1);
        assert_eq!(render(&n, o, &i), "Result (never executed)");
    }

    #[test]
    fn rows_removed_zero_is_not_shown_and_average_is_per_loop() {
        let n = node("Seq Scan on u", 0)
            .filter("Filter", "(x = t.c)", &[0])
            .filter("Filter", "(y = 1)", &[]);
        let i = Instrumentation::new(1);
        set(&i, 0, 1000, 0, 2_000_000, 0);
        let o = opts(true, false, false, false);
        // 空の sources は 0 なので出さない。
        assert_eq!(
            render(&n, o, &i),
            "Seq Scan on u (actual rows=0 loops=1000)\n  Filter: (x = t.c)\n  Rows Removed by Filter: 2000\n  Filter: (y = 1)"
        );
        set(&i, 0, 1, 10, 0, 0);
        assert_eq!(
            render(&n, o, &i),
            "Seq Scan on u (actual rows=10 loops=1)\n  Filter: (x = t.c)\n  Filter: (y = 1)"
        );
    }

    #[test]
    fn rows_removed_sums_sources_and_distinguishes_join_filter() {
        // 結合の Join Filter は結合ノード自身のカウンタ（JoinFilter）、Filter の群は別ノード（Filter）。
        let n = node("Nested Loop", 0)
            .filter("Join Filter", "(t.a < u.a)", &[0])
            .child(node("Seq Scan on t", 1).filter("Filter", "(b = 1)", &[1, 2]));
        let i = Instrumentation::new(3);
        set(&i, 0, 1, 20, 7, 5980);
        set(&i, 1, 1, 100, 400, 0);
        i.node(2).removed_filter.set(500);
        let o = opts(true, false, false, false);
        let expect = "\
Nested Loop (actual rows=20 loops=1)
  Join Filter: (t.a < u.a)
  Rows Removed by Join Filter: 5980
  ->  Seq Scan on t (actual rows=100 loops=1)
        Filter: (b = 1)
        Rows Removed by Filter: 900";
        assert_eq!(render(&n, o, &i), expect);
    }

    #[test]
    fn analyze_full_example_with_materialize_and_loops() {
        let n = node("Nested Loop", 0)
            .filter("Join Filter", "(t.a < u.a)", &[0])
            .child(node("Seq Scan on t", 1).filter("Filter", "(b = 1)", &[1]))
            .child(
                node("Materialize", 2).child(node("Seq Scan on u", 3).filter(
                    "Filter",
                    "(a < 3)",
                    &[3],
                )),
            );
        let i = Instrumentation::new(4);
        set(&i, 0, 1, 20, 0, 5980);
        set(&i, 1, 1, 100, 900, 0);
        set(&i, 2, 100, 6000, 0, 0);
        set(&i, 3, 1, 60, 1940, 0);
        let o = opts(true, false, false, false);
        let expect = "\
Nested Loop (actual rows=20 loops=1)
  Join Filter: (t.a < u.a)
  Rows Removed by Join Filter: 5980
  ->  Seq Scan on t (actual rows=100 loops=1)
        Filter: (b = 1)
        Rows Removed by Filter: 900
  ->  Materialize (actual rows=60 loops=100)
        ->  Seq Scan on u (actual rows=60 loops=1)
              Filter: (a < 3)
              Rows Removed by Filter: 1940";
        assert_eq!(render(&n, o, &i), expect);
    }

    #[test]
    fn analyze_sub_plan_and_never_executed_init_plan() {
        let n = node("Seq Scan on t", 0)
            .filter(
                "Filter",
                "((ANY (a = (SubPlan 1).col1)) OR (b = 99999))",
                &[0],
            )
            .labeled(
                "SubPlan 1",
                node("Seq Scan on u", 1).filter("Filter", "(x = t.c)", &[1]),
            )
            .labeled("InitPlan 2", node("Result", 2));
        let i = Instrumentation::new(3);
        set(&i, 0, 1, 0, 1000, 0);
        set(&i, 1, 1000, 0, 2_000_000, 0);
        let o = opts(true, false, false, false);
        let expect = "\
Seq Scan on t (actual rows=0 loops=1)
  Filter: ((ANY (a = (SubPlan 1).col1)) OR (b = 99999))
  Rows Removed by Filter: 1000
  SubPlan 1
    ->  Seq Scan on u (actual rows=0 loops=1000)
          Filter: (x = t.c)
          Rows Removed by Filter: 2000
  InitPlan 2
    ->  Result (never executed)";
        assert_eq!(render(&n, o, &i), expect);
    }

    #[test]
    fn dml_actual_rows_come_from_the_counters() {
        let n = node("Insert on u", 0).child(node("Result", 1));
        let i = Instrumentation::new(2);
        set(&i, 0, 1, 0, 0, 0);
        set(&i, 1, 1, 1, 0, 0);
        let o = opts(true, false, false, false);
        assert_eq!(
            render(&n, o, &i),
            "Insert on u (actual rows=0 loops=1)\n  ->  Result (actual rows=1 loops=1)"
        );
    }

    #[test]
    fn render_calls_finish_for_running_nodes() {
        let n = node("Result", 0);
        let i = Instrumentation::new(1);
        let c = i.node(0);
        c.running.set(true);
        c.cycle_rows.set(3);
        let o = opts(true, false, false, false);
        assert_eq!(render(&n, o, &i), "Result (actual rows=3 loops=1)");
    }

    // ----- Planning Time / Execution Time -----------------------------------------

    fn times() -> Timings {
        Timings {
            planning: Duration::from_micros(8),
            execution: Some(Duration::from_micros(2_345)),
        }
    }

    #[test]
    fn summary_without_analyze_prints_planning_time_only() {
        let n = node("Result", 0);
        let o = opts(false, false, false, true);
        assert_eq!(
            render_plan(&n, &o, None, &times()),
            vec!["Result".to_owned(), "Planning Time: 0.008 ms".to_owned()]
        );
    }

    #[test]
    fn summary_with_analyze_prints_both() {
        let n = node("Result", 0);
        let i = Instrumentation::new(1);
        set(&i, 0, 1, 1, 0, 0);
        let o = opts(true, false, false, true);
        assert_eq!(
            render_plan(&n, &o, Some(&i), &times()),
            vec![
                "Result (actual rows=1 loops=1)".to_owned(),
                "Planning Time: 0.008 ms".to_owned(),
                "Execution Time: 2.345 ms".to_owned(),
            ]
        );
    }

    #[test]
    fn summary_off_prints_no_times() {
        let n = node("Result", 0);
        let o = opts(false, false, false, false);
        assert_eq!(render_plan(&n, &o, None, &times()), vec!["Result"]);
    }

    #[test]
    fn analyze_timing_on_costs_on_matches_postgres_layout() {
        // `EXPLAIN ANALYZE VERBOSE SELECT 1`（§3.1）。
        let n = node("Result", 0).output(&["1"]);
        let i = Instrumentation::new(1);
        set(&i, 0, 1, 1, 0, 0);
        let o = ExplainOptions {
            analyze: true,
            verbose: true,
            costs: true,
            timing: true,
            summary: true,
        };
        let out = render_plan(
            &n,
            &o,
            Some(&i),
            &Timings {
                planning: Duration::from_micros(1),
                execution: Some(Duration::from_micros(1)),
            },
        );
        assert_eq!(
            out,
            vec![
                "Result  (cost=0.00..0.00 rows=0 width=4) (actual time=0.000..0.000 rows=1 loops=1)",
                "  Output: 1",
                "Planning Time: 0.001 ms",
                "Execution Time: 0.001 ms",
            ]
        );
    }
}
