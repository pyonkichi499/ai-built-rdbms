//! permutation の実行。PostgreSQL 17 の `src/test/isolation/isolationtester.c` の
//! 制御の流れと出力をそのまま写している（関数名も対応させてある）。
//!
//! isolationtester は `pg_isolation_regress` から `2>&1` 付きで起動されるので、標準エラーへの
//! メッセージ（`unused step name: ...` など）も期待ファイルに現れる。ここでは標準出力と
//! 標準エラーを区別せず、すべて 1 本の出力バッファに順番どおり書く。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::conn::{Conn, ConnParams, QueryResult};
use crate::spec::{BlockerSpec, TestSpec};

/// ステップがロック待ちになったかの判定方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum BlockingDetection {
    /// 一定時間（`--block-timeout-ms`）応答がなければ待ちとみなす。
    Timeout,
    /// 制御用の接続で `pg_isolation_test_session_is_blocked()` を問い合わせる（isolationtester と同じ）。
    Pg,
}

/// 実行時の設定。
#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) conn: ConnParams,
    pub(crate) detection: BlockingDetection,
    pub(crate) block_timeout: Duration,
    /// isolationtester の `max_step_wait`。これを過ぎたら CancelRequest、2 倍で打ち切る。
    pub(crate) max_step_wait: Duration,
}

/// isolationtester が `exit(1)` する状況。出力にはメッセージが書き込み済み。
#[derive(Debug)]
pub(crate) struct Fatal;

/// select(2) の間隔。isolationtester と同じ 10ms。
const POLL: Duration = Duration::from_millis(10);

/// `try_complete_step` のフラグ。
#[derive(Debug, Clone, Copy)]
struct Flags {
    nonblock: bool,
    retry: bool,
}

const NONBLOCK: Flags = Flags {
    nonblock: true,
    retry: false,
};
const RETRY: Flags = Flags {
    nonblock: false,
    retry: true,
};
const NONBLOCK_RETRY: Flags = Flags {
    nonblock: true,
    retry: true,
};

/// ステップの識別子（セッション番号, セッション内の番号）。
type StepId = (usize, usize);

#[derive(Debug, Clone)]
enum Blocker {
    Once,
    OtherStep {
        session: usize,
        step: StepId,
    },
    Notices {
        session: usize,
        num: usize,
        target: usize,
    },
}

/// permutation の中の 1 ステップ（`PermutationStep`）。
#[derive(Debug, Clone)]
struct PStep {
    name: String,
    session: usize,
    step: StepId,
    sql: String,
    blockers: Vec<Blocker>,
}

#[derive(Debug)]
struct IsoConn {
    conn: Conn,
    name: String,
    /// 実行中のステップ（現在の permutation の `psteps` の添字）。
    active: Option<usize>,
    total_notices: usize,
}

/// 1 つの spec ファイルを実行する。戻り値は出力全体と、途中で打ち切ったかどうか。
///
/// `test_name` は `application_name` `に使う。pg_isolation_regress` は `PGAPPNAME=isolation/<テスト名>`
/// を設定し、isolationtester がそれに `/<セッション名>` を足すので、同じ値にする。
pub(crate) fn run_spec(spec: &TestSpec, test_name: &str, opts: &Options) -> (String, bool) {
    let mut out = String::new();
    let fatal = run_spec_inner(spec, test_name, opts, &mut out).is_err();
    (out, fatal)
}

fn run_spec_inner(
    spec: &TestSpec,
    test_name: &str,
    opts: &Options,
    out: &mut String,
) -> Result<(), Fatal> {
    let steps = check_testspec(spec, out)?;
    let _ = writeln!(
        out,
        "Parsed test spec with {} sessions",
        spec.sessions.len()
    );

    let mut conns = Vec::with_capacity(1 + spec.sessions.len());
    for i in 0..=spec.sessions.len() {
        let name = if i == 0 {
            "control connection".to_owned()
        } else {
            spec.sessions[i - 1].name.clone()
        };
        let appname = format!("isolation/{test_name}/{name}");
        match Conn::connect(&opts.conn, &appname) {
            Ok(conn) => conns.push(IsoConn {
                conn,
                name,
                active: None,
                total_notices: 0,
            }),
            Err(e) => {
                let _ = writeln!(out, "Connection {i} failed: {e}");
                return Err(Fatal);
            }
        }
    }
    let pids: Vec<String> = conns[1..].iter().map(|c| c.conn.pid.to_string()).collect();
    let mut r = Runner {
        spec,
        opts,
        conns,
        out,
        any_new_notice: false,
        psteps: Vec::new(),
        all_pids: pids.join(","),
    };
    // 制御用接続の NOTICE は捨てる（blackholeNoticeProcessor）。
    r.conns[0].conn.take_notices();
    if spec.permutations.is_empty() {
        r.run_all_permutations()
    } else {
        for p in &spec.permutations {
            let mut psteps = Vec::with_capacity(p.steps.len());
            for ps in &p.steps {
                let id = steps[&ps.name];
                let mut blockers = Vec::new();
                for b in &ps.blockers {
                    blockers.push(match b {
                        BlockerSpec::Once => Blocker::Once,
                        BlockerSpec::OtherStep(other) => {
                            let s = steps[other];
                            Blocker::OtherStep {
                                session: s.0,
                                step: s,
                            }
                        }
                        BlockerSpec::Notices(other, n) => Blocker::Notices {
                            session: steps[other].0,
                            num: *n,
                            target: 0,
                        },
                    });
                }
                psteps.push(PStep {
                    name: ps.name.clone(),
                    session: id.0,
                    step: id,
                    sql: spec.sessions[id.0].steps[id.1].sql.clone(),
                    blockers,
                });
            }
            r.psteps = psteps;
            r.run_permutation()?;
        }
        Ok(())
    }
}

/// spec の妥当性検査（`check_testspec`）。ステップ名から識別子への表を返す。
fn check_testspec(spec: &TestSpec, out: &mut String) -> Result<HashMap<String, StepId>, Fatal> {
    let mut all: Vec<(&str, StepId)> = Vec::new();
    for (si, s) in spec.sessions.iter().enumerate() {
        for (ti, t) in s.steps.iter().enumerate() {
            all.push((t.name.as_str(), (si, ti)));
        }
    }
    // strcmp と同じくバイト列で比較して並べる。
    all.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    for w in all.windows(2) {
        if w[0].0 == w[1].0 {
            let _ = writeln!(out, "duplicate step name: {}", w[1].0);
            return Err(Fatal);
        }
    }
    let map: HashMap<String, StepId> = all.iter().map(|(n, id)| ((*n).to_owned(), *id)).collect();
    let mut used = vec![false; all.len()];
    for p in &spec.permutations {
        for ps in &p.steps {
            let Some(pos) = all.iter().position(|(n, _)| *n == ps.name) else {
                let _ = writeln!(
                    out,
                    "undefined step \"{}\" specified in permutation",
                    ps.name
                );
                return Err(Fatal);
            };
            used[pos] = true;
        }
        for ps in &p.steps {
            let me = map[&ps.name];
            for b in &ps.blockers {
                let other = match b {
                    BlockerSpec::Once => continue,
                    BlockerSpec::OtherStep(o) | BlockerSpec::Notices(o, _) => o,
                };
                if !p.steps.iter().any(|s| &s.name == other) {
                    let _ = writeln!(
                        out,
                        "undefined blocking step \"{other}\" referenced in permutation step \"{}\"",
                        ps.name
                    );
                    return Err(Fatal);
                }
                if map[other].0 == me.0 {
                    let _ = writeln!(
                        out,
                        "permutation step \"{}\" cannot block on its own session",
                        ps.name
                    );
                    return Err(Fatal);
                }
            }
        }
    }
    if !spec.permutations.is_empty() {
        for (i, (name, _)) in all.iter().enumerate() {
            if !used[i] {
                let _ = writeln!(out, "unused step name: {name}");
            }
        }
    }
    Ok(map)
}

struct Runner<'a> {
    spec: &'a TestSpec,
    opts: &'a Options,
    /// `conns[0]` は制御用、`conns[1 + i]` が `spec.sessions[i]`。
    conns: Vec<IsoConn>,
    out: &'a mut String,
    any_new_notice: bool,
    psteps: Vec<PStep>,
    all_pids: String,
}

impl Runner<'_> {
    fn print(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// 接続 `ci` で受け取った NOTICE を出力する（isotesterNoticeProcessor）。
    fn flush_notices(&mut self, ci: usize) {
        let notices = self.conns[ci].conn.take_notices();
        if ci == 0 {
            return;
        }
        for n in notices {
            let _ = write!(self.out, "{}: {}", self.conns[ci].name, n);
            self.conns[ci].total_notices += 1;
            self.any_new_notice = true;
        }
    }

    fn exec_and_print(&mut self, ci: usize, sql: &str) -> Result<(), String> {
        let res = self.conns[ci].conn.exec(sql);
        self.flush_notices(ci);
        match res {
            QueryResult::Tuples { fields, rows } => {
                print_result_set(self.out, &fields, &rows);
                Ok(())
            }
            QueryResult::Command | QueryResult::Empty => Ok(()),
            QueryResult::Error(e) => Err(e.libpq_message(true)),
        }
    }

    fn run_all_permutations(&mut self) -> Result<(), Fatal> {
        let mut piles = vec![0usize; self.spec.sessions.len()];
        let mut chosen = Vec::new();
        self.run_all_permutations_recurse(&mut piles, &mut chosen)
    }

    fn run_all_permutations_recurse(
        &mut self,
        piles: &mut [usize],
        chosen: &mut Vec<StepId>,
    ) -> Result<(), Fatal> {
        let mut found = false;
        for i in 0..self.spec.sessions.len() {
            if piles[i] < self.spec.sessions[i].steps.len() {
                chosen.push((i, piles[i]));
                piles[i] += 1;
                self.run_all_permutations_recurse(piles, chosen)?;
                piles[i] -= 1;
                chosen.pop();
                found = true;
            }
        }
        if !found {
            self.psteps = chosen
                .iter()
                .map(|&(s, t)| {
                    let st = &self.spec.sessions[s].steps[t];
                    PStep {
                        name: st.name.clone(),
                        session: s,
                        step: (s, t),
                        sql: st.sql.clone(),
                        blockers: Vec::new(),
                    }
                })
                .collect();
            self.run_permutation()?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // isolationtester.c の関数をそのまま写しているため
    fn run_permutation(&mut self) -> Result<(), Fatal> {
        let mut line = String::from("\nstarting permutation:");
        for p in &self.psteps {
            line.push(' ');
            line.push_str(&p.name);
        }
        line.push('\n');
        self.print(&line);

        let spec = self.spec;
        for sql in &spec.setup_sqls {
            if let Err(msg) = self.exec_and_print(0, sql) {
                let _ = write!(self.out, "setup failed: {msg}");
                return Err(Fatal);
            }
        }
        for (i, s) in spec.sessions.iter().enumerate() {
            if let Some(sql) = &s.setup_sql
                && let Err(msg) = self.exec_and_print(i + 1, sql)
            {
                let _ = write!(self.out, "setup of session {} failed: {msg}", s.name);
                return Err(Fatal);
            }
        }

        let mut waiting: Vec<usize> = Vec::new();
        for i in 0..self.psteps.len() {
            let ci = 1 + self.psteps[i].session;
            // 前のステップでまだ待っているセッションなら、終わるまで待つ。
            if self.conns[ci].active.is_some() {
                let start = Instant::now();
                while let Some(old) = self.conns[ci].active {
                    if !self.try_complete_step(old, RETRY)? {
                        let w = waiting
                            .iter()
                            .position(|&x| x == old)
                            .expect("completed step must be in waiting list");
                        waiting.remove(w);
                    }
                    waiting = self.try_complete_steps(waiting, NONBLOCK_RETRY)?;
                    if self.conns[ci].active.is_some() {
                        let td = start.elapsed();
                        if td > 2 * self.opts.max_step_wait {
                            let name = self.psteps[self.conns[ci].active.unwrap_or(old)]
                                .name
                                .clone();
                            let _ = writeln!(
                                self.out,
                                "step {name} timed out after {} seconds",
                                td.as_secs()
                            );
                            let mut s = String::from("active steps are:");
                            for c in &self.conns[1..] {
                                if let Some(a) = c.active {
                                    s.push(' ');
                                    s.push_str(&self.psteps[a].name);
                                }
                            }
                            let _ = writeln!(self.out, "{s}");
                            return Err(Fatal);
                        }
                    }
                }
            }

            let sql = self.psteps[i].sql.clone();
            if let Some(target) = cancel_directive(&sql) {
                // 拡張: `-- @cancel <セッション名>` のステップは SQL を送らず、
                // 対象セッションの実行中の問い合わせに CancelRequest を送る。
                self.run_cancel_step(i, target, &mut waiting)?;
                continue;
            }
            if let Err(e) = self.conns[ci].conn.send_query(&sql) {
                let _ = writeln!(
                    self.out,
                    "failed to send query for step {}: {e}",
                    self.psteps[i].name
                );
                return Err(Fatal);
            }
            self.conns[ci].active = Some(i);
            let totals: Vec<usize> = self.conns.iter().map(|c| c.total_notices).collect();
            for b in &mut self.psteps[i].blockers {
                if let Blocker::Notices {
                    session,
                    num,
                    target,
                } = b
                {
                    *target = *num + totals[*session + 1];
                }
            }
            let mustwait = self.try_complete_step(i, NONBLOCK)?;
            waiting = self.try_complete_steps(waiting, NONBLOCK_RETRY)?;
            if mustwait {
                waiting.push(i);
            }
        }

        let waiting = self.try_complete_steps(waiting, RETRY)?;
        if !waiting.is_empty() {
            self.print("failed to complete permutation due to mutually-blocking steps\n");
            return Err(Fatal);
        }

        for (i, s) in spec.sessions.iter().enumerate() {
            if let Some(sql) = &s.teardown_sql
                && let Err(msg) = self.exec_and_print(i + 1, sql)
            {
                let _ = write!(self.out, "teardown of session {} failed: {msg}", s.name);
            }
        }
        if let Some(sql) = &spec.teardown_sql
            && let Err(msg) = self.exec_and_print(0, sql)
        {
            let _ = write!(self.out, "teardown failed: {msg}");
        }
        Ok(())
    }

    /// `-- @cancel <セッション名>` ステップの実行。対象のステップが完了するまで待って結果も出す
    /// （キャンセルの到達が非同期でも、出力の順序が変わらないようにする）。
    fn run_cancel_step(
        &mut self,
        i: usize,
        target: &str,
        waiting: &mut Vec<usize>,
    ) -> Result<(), Fatal> {
        let _ = writeln!(
            self.out,
            "step {}: {}",
            self.psteps[i].name, self.psteps[i].sql
        );
        let Some(ti) = self.spec.sessions.iter().position(|s| s.name == target) else {
            let _ = writeln!(self.out, "cancel target session \"{target}\" not found");
            return Err(Fatal);
        };
        let tci = 1 + ti;
        let Some(active) = self.conns[tci].active else {
            let _ = writeln!(
                self.out,
                "cancel target session \"{target}\" is not running a step"
            );
            return Err(Fatal);
        };
        if let Err(e) = self.conns[tci].conn.cancel() {
            let _ = writeln!(self.out, "PQcancel failed: {e}");
            return Err(Fatal);
        }
        let r = self.try_complete_step(active, RETRY)?;
        if !r {
            waiting.retain(|&x| x != active);
        }
        Ok(())
    }

    fn try_complete_steps(
        &mut self,
        mut waiting: Vec<usize>,
        flags: Flags,
    ) -> Result<Vec<usize>, Fatal> {
        loop {
            self.any_new_notice = false;
            let old = waiting.len();
            let mut have_blocker = false;
            let mut w = 0;
            while w < waiting.len() {
                if self.try_complete_step(waiting[w], flags)? {
                    if !self.psteps[waiting[w]].blockers.is_empty() {
                        have_blocker = true;
                    }
                    w += 1;
                } else {
                    waiting.remove(w);
                }
            }
            if !(have_blocker && (waiting.len() < old || self.any_new_notice)) {
                return Ok(waiting);
            }
        }
    }

    /// 制御用接続でロック待ちかを問い合わせる。
    fn is_blocked_pg(&mut self, ci: usize) -> Result<bool, Fatal> {
        let sql = format!(
            "SELECT pg_catalog.pg_isolation_test_session_is_blocked({}, '{{{}}}')",
            self.conns[ci].conn.pid, self.all_pids
        );
        let res = self.conns[0].conn.exec(&sql);
        self.conns[0].conn.take_notices();
        match res {
            QueryResult::Tuples { rows, .. } if rows.len() == 1 && !rows[0].is_empty() => {
                Ok(rows[0][0].as_deref().is_some_and(|v| v.starts_with('t')))
            }
            QueryResult::Error(e) => {
                let _ = write!(
                    self.out,
                    "lock wait query failed: {}",
                    e.libpq_message(true)
                );
                Err(Fatal)
            }
            _ => {
                self.print("lock wait query failed: unexpected result\n");
                Err(Fatal)
            }
        }
    }

    fn print_waiting(&mut self, p: usize) {
        let _ = writeln!(
            self.out,
            "step {}: {} <waiting ...>",
            self.psteps[p].name, self.psteps[p].sql
        );
    }

    /// 送信済みのステップの完了を待つ。完了していなければ `true`（`try_complete_step`）。
    #[allow(clippy::too_many_lines)] // isolationtester.c の関数をそのまま写しているため
    fn try_complete_step(&mut self, p: usize, flags: Flags) -> Result<bool, Fatal> {
        let ci = 1 + self.psteps[p].session;
        if !flags.retry
            && self.psteps[p]
                .blockers
                .iter()
                .any(|b| matches!(b, Blocker::Once))
        {
            self.print_waiting(p);
            return Ok(true);
        }

        let start = Instant::now();
        let mut canceled = false;
        loop {
            let busy = self.conns[ci].conn.is_busy();
            self.flush_notices(ci);
            if !busy {
                break;
            }
            let got = self.conns[ci].conn.wait_input(POLL);
            self.flush_notices(ci);
            if got {
                continue;
            }
            if flags.nonblock {
                let waiting = match self.opts.detection {
                    BlockingDetection::Pg => self.is_blocked_pg(ci)?,
                    BlockingDetection::Timeout => start.elapsed() >= self.opts.block_timeout,
                };
                if waiting {
                    self.conns[ci].conn.consume_input();
                    let busy = self.conns[ci].conn.is_busy();
                    self.flush_notices(ci);
                    if !busy {
                        break;
                    }
                    if !flags.retry {
                        self.print_waiting(p);
                    }
                    return Ok(true);
                }
            }
            let td = start.elapsed();
            if td > self.opts.max_step_wait && !canceled {
                match self.conns[ci].conn.cancel() {
                    Ok(()) => {
                        let _ = writeln!(
                            self.out,
                            "isolationtester: canceling step {} after {} seconds",
                            self.psteps[p].name,
                            td.as_secs()
                        );
                        canceled = true;
                    }
                    Err(e) => {
                        let _ = writeln!(self.out, "PQcancel failed: {e}");
                    }
                }
            }
            if td > 2 * self.opts.max_step_wait {
                let _ = writeln!(
                    self.out,
                    "step {} timed out after {} seconds",
                    self.psteps[p].name,
                    td.as_secs()
                );
                return Err(Fatal);
            }
        }

        if self.step_has_blocker(p) {
            if !flags.retry {
                self.print_waiting(p);
            }
            return Ok(true);
        }

        if flags.retry {
            let _ = writeln!(self.out, "step {}: <... completed>", self.psteps[p].name);
        } else {
            let _ = writeln!(
                self.out,
                "step {}: {}",
                self.psteps[p].name, self.psteps[p].sql
            );
        }
        loop {
            let res = self.conns[ci].conn.get_result();
            self.flush_notices(ci);
            let Some(res) = res else { break };
            match res {
                QueryResult::Command | QueryResult::Empty => {}
                QueryResult::Tuples { fields, rows } => print_result_set(self.out, &fields, &rows),
                QueryResult::Error(e) => match (e.field(b'S'), e.field(b'M')) {
                    (Some(sev), Some(msg)) => {
                        let _ = writeln!(self.out, "{sev}:  {msg}");
                    }
                    _ => {
                        let _ = writeln!(self.out, "{}", e.libpq_message(true));
                    }
                },
            }
        }

        self.conns[ci].conn.consume_input();
        self.flush_notices(ci);
        while let Some(n) = self.conns[ci].conn.next_notify() {
            let sender = self.conns[1..]
                .iter()
                .find(|c| c.conn.pid == n.pid)
                .map_or_else(|| format!("PID {}", n.pid), |c| c.name.clone());
            let _ = writeln!(
                self.out,
                "{}: NOTIFY \"{}\" with payload \"{}\" from {}",
                self.conns[ci].name, n.channel, n.payload, sender
            );
            self.conns[ci].conn.consume_input();
            self.flush_notices(ci);
        }
        self.conns[ci].active = None;
        Ok(false)
    }

    /// 完了報告を遅らせる条件がまだ満たされていないか（`step_has_blocker`）。
    fn step_has_blocker(&self, p: usize) -> bool {
        self.psteps[p].blockers.iter().any(|b| match b {
            Blocker::Once => false,
            Blocker::OtherStep { session, step } => self.conns[1 + session]
                .active
                .is_some_and(|a| self.psteps[a].step == *step),
            Blocker::Notices {
                session, target, ..
            } => self.conns[1 + session].total_notices < *target,
        })
    }
}

/// `-- @cancel <セッション名>` だけから成る SQL なら、セッション名を返す。
fn cancel_directive(sql: &str) -> Option<&str> {
    let rest = sql.trim().strip_prefix("-- @cancel")?;
    let name = rest.trim();
    (!name.is_empty() && !name.contains(char::is_whitespace)).then_some(name)
}

/// `width` バイトになるまで空白を足す（printf の `%-*s` / `%*s` はバイト数で数える）。
fn pad(out: &mut String, s: &str, width: usize, left: bool) {
    let fill = width.saturating_sub(s.len());
    if left {
        out.push_str(s);
    }
    out.extend(std::iter::repeat_n(' ', fill));
    if !left {
        out.push_str(s);
    }
}

/// libpq の `PQprint`（header・align・fieldSep="|"）と同じ形式で結果表を出す。
pub(crate) fn print_result_set(out: &mut String, fields: &[String], rows: &[Vec<Option<String>>]) {
    let n = fields.len();
    if n == 0 {
        return;
    }
    let mut max: Vec<usize> = fields.iter().map(String::len).collect();
    let mut not_num = vec![false; n];
    for row in rows {
        for (j, v) in row.iter().enumerate().take(n) {
            let Some(v) = v.as_deref().filter(|v| !v.is_empty()) else {
                continue;
            };
            if !not_num[j] {
                let mut last = '0';
                for c in v.chars() {
                    last = c;
                    if !(c.is_ascii_digit() || matches!(c, '.' | 'E' | 'e' | ' ' | '-')) {
                        not_num[j] = true;
                        break;
                    }
                }
                if v.starts_with(['E', 'e']) || !last.is_ascii_digit() {
                    not_num[j] = true;
                }
            }
            max[j] = max[j].max(v.len());
        }
    }
    for (j, name) in fields.iter().enumerate() {
        pad(out, name, max[j], not_num[j]);
        if j + 1 < n {
            out.push('|');
        }
    }
    out.push('\n');
    for (j, m) in max.iter().enumerate() {
        out.extend(std::iter::repeat_n('-', *m));
        if j + 1 < n {
            out.push('+');
        }
    }
    out.push('\n');
    for row in rows {
        for j in 0..n {
            let v = row.get(j).and_then(Option::as_deref).unwrap_or("");
            pad(out, v, max[j], not_num[j]);
            if j + 1 < n {
                out.push('|');
            }
        }
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "({} row{})\n",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unnecessary_wraps)]
    fn s(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    #[test]
    fn prints_like_pqprint() {
        let mut out = String::new();
        print_result_set(
            &mut out,
            &["id".into(), "name".into(), "n".into()],
            &[vec![s("1"), s("alice"), None], vec![s("10"), s("b"), None]],
        );
        assert_eq!(
            out,
            "id|name |n\n--+-----+-\n 1|alice| \n10|b    | \n(2 rows)\n\n"
        );
    }

    #[test]
    fn numeric_detection() {
        let mut out = String::new();
        print_result_set(
            &mut out,
            &["a".into(), "b".into(), "c".into()],
            &[vec![s("-1.5e3"), s("1."), s("e1")]],
        );
        assert_eq!(out, "     a|b |c \n------+--+--\n-1.5e3|1.|e1\n(1 row)\n\n");
    }

    #[test]
    fn zero_rows_and_no_columns() {
        let mut out = String::new();
        print_result_set(&mut out, &["x".into()], &[]);
        assert_eq!(out, "x\n-\n(0 rows)\n\n");
        let mut out = String::new();
        print_result_set(&mut out, &[], &[vec![]]);
        assert_eq!(out, "");
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::cancel_directive;

    #[test]
    fn parses_cancel_directive() {
        assert_eq!(cancel_directive("-- @cancel s1"), Some("s1"));
        assert_eq!(cancel_directive("  -- @cancel  b \n"), Some("b"));
        assert_eq!(cancel_directive("-- @cancel"), None);
        assert_eq!(cancel_directive("-- @cancel a b"), None);
        assert_eq!(cancel_directive("SELECT 1"), None);
    }
}
