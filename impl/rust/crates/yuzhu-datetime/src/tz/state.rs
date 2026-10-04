//! Time zone state: `TZif` loading (`tzloadbody`), POSIX TZ strings
//! (`tzparse`) and the lookups PostgreSQL performs on them (`localsub`,
//! `pg_next_dst_boundary`, `pg_interpret_timezone_abbrev`).
//!
//! This is a hand-written port of PostgreSQL's `src/timezone/localtime.c`
//! (itself derived from the IANA reference code), so that results match
//! PostgreSQL bit for bit, including its handling of times beyond the last
//! explicit transition.

const TZ_MAX_TIMES: usize = 2000;
const TZ_MAX_TYPES: usize = 256;
const TZ_MAX_CHARS: usize = 50;
const TZ_MAX_LEAPS: usize = 50;
/// `sizeof(state.chars)` in PostgreSQL.
const CHARS_CAPACITY: usize = 2 * (255 + 1);
const YEARSPERREPEAT: i64 = 400;
const AVGSECSPERYEAR: i64 = 31_556_952;
const SECSPERREPEAT: i64 = YEARSPERREPEAT * AVGSECSPERYEAR;
const SECSPERDAY: i32 = 86_400;
const SECSPERHOUR: i32 = 3_600;
const SECSPERMIN: i32 = 60;
const EPOCH_YEAR: i32 = 1970;
const TZDEFRULESTRING: &[u8] = b",M3.2.0,M11.1.0";
const TZHEAD_SIZE: usize = 44;

const MON_LENGTHS: [[i32; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];
const YEAR_LENGTHS: [i32; 2] = [365, 366];

fn isleap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// One local time type (`struct ttinfo`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TtInfo {
    /// UT offset in seconds (east positive).
    pub utoff: i32,
    pub isdst: bool,
    pub desigidx: usize,
    pub ttisstd: bool,
    pub ttisut: bool,
}

#[derive(Debug, Clone, Copy)]
struct LsInfo {
    trans: i64,
    corr: i64,
}

/// `struct state`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ZoneState {
    ats: Vec<i64>,
    types: Vec<u8>,
    ttis: Vec<TtInfo>,
    /// Abbreviation characters, NUL separated (`charcnt` bytes).
    chars: Vec<u8>,
    lsis: Vec<LsInfo>,
    goback: bool,
    goahead: bool,
    defaulttype: usize,
}

/// Result of [`ZoneState::next_dst_boundary`].
pub(crate) enum Boundary {
    /// No transition after the probe time; the state prevailing there.
    None { gmtoff: i64, isdst: bool },
    /// The next transition after the probe time.
    Next {
        before_gmtoff: i64,
        before_isdst: bool,
        boundary: i64,
        after_gmtoff: i64,
        after_isdst: bool,
    },
    /// Failure (not expected in practice).
    Fail,
}

fn cstr_at(buf: &[u8], idx: usize) -> &[u8] {
    let rest = buf.get(idx..).unwrap_or(&[]);
    let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
    &rest[..end]
}

fn detzcode(p: &[u8]) -> i32 {
    i32::from_be_bytes([p[0], p[1], p[2], p[3]])
}

fn detzcode64(p: &[u8]) -> i64 {
    i64::from_be_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]])
}

fn increment_overflow_time(tp: &mut i64, j: i64) -> bool {
    match tp.checked_add(j) {
        Some(v) => {
            *tp = v;
            false
        }
        None => true,
    }
}

impl ZoneState {
    /// Abbreviation at a `desigidx`.
    pub(crate) fn abbrev(&self, idx: usize) -> &str {
        std::str::from_utf8(cstr_at(&self.chars, idx)).unwrap_or("")
    }

    fn typesequiv(&self, a: usize, b: usize) -> bool {
        if a >= self.ttis.len() || b >= self.ttis.len() {
            return false;
        }
        let ap = &self.ttis[a];
        let bp = &self.ttis[b];
        ap.utoff == bp.utoff
            && ap.isdst == bp.isdst
            && ap.ttisstd == bp.ttisstd
            && ap.ttisut == bp.ttisut
            && cstr_at(&self.chars, ap.desigidx) == cstr_at(&self.chars, bp.desigidx)
    }

    fn leapcorr(&self, t: i64) -> i64 {
        for ls in self.lsis.iter().rev() {
            if t >= ls.trans {
                return ls.corr;
            }
        }
        0
    }

    /// `tzloadbody` on the raw bytes of a `TZif` file.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn load(data: &[u8], doextend: bool) -> Option<ZoneState> {
        let mut buf: Vec<u8> = data.to_vec();
        if buf.len() < TZHEAD_SIZE || &buf[..4] != b"TZif" {
            return None;
        }
        let mut sp = ZoneState::default();
        for stored in [4usize, 8] {
            if buf.len() < TZHEAD_SIZE {
                return None;
            }
            let ttisutcnt = detzcode(&buf[20..24]);
            let ttisstdcnt = detzcode(&buf[24..28]);
            let leapcnt = detzcode(&buf[28..32]);
            let timecnt = detzcode(&buf[32..36]);
            let typecnt = detzcode(&buf[36..40]);
            let charcnt = detzcode(&buf[40..44]);
            let ok = (0..TZ_MAX_LEAPS as i32).contains(&leapcnt)
                && (0..TZ_MAX_TYPES as i32).contains(&typecnt)
                && (0..TZ_MAX_TIMES as i32).contains(&timecnt)
                && (0..TZ_MAX_CHARS as i32).contains(&charcnt)
                && (ttisstdcnt == typecnt || ttisstdcnt == 0)
                && (ttisutcnt == typecnt || ttisutcnt == 0);
            if !ok {
                return None;
            }
            #[allow(clippy::cast_sign_loss)]
            let (leapcnt, timecnt, typecnt, charcnt, ttisstdcnt, ttisutcnt) = (
                leapcnt as usize,
                timecnt as usize,
                typecnt as usize,
                charcnt as usize,
                ttisstdcnt as usize,
                ttisutcnt as usize,
            );
            let need = TZHEAD_SIZE
                + timecnt * stored
                + timecnt
                + typecnt * 6
                + charcnt
                + leapcnt * (stored + 4)
                + ttisstdcnt
                + ttisutcnt;
            if buf.len() < need {
                return None;
            }
            let mut p = TZHEAD_SIZE;
            // Transition times; an equal-time duplicate replaces the
            // previous transition.
            let mut ats: Vec<i64> = Vec::with_capacity(timecnt);
            let mut keep: Vec<bool> = vec![true; timecnt];
            for i in 0..timecnt {
                let at = if stored == 4 {
                    i64::from(detzcode(&buf[p..]))
                } else {
                    detzcode64(&buf[p..])
                };
                if let Some(&last) = ats.last()
                    && at <= last
                {
                    if at < last {
                        return None;
                    }
                    keep[i - 1] = false;
                    ats.pop();
                }
                ats.push(at);
                p += stored;
            }
            let mut types: Vec<u8> = Vec::with_capacity(ats.len());
            for &k in &keep {
                let typ = buf[p];
                p += 1;
                if usize::from(typ) >= typecnt {
                    return None;
                }
                if k {
                    types.push(typ);
                }
            }
            let mut ttis: Vec<TtInfo> = Vec::with_capacity(typecnt);
            for _ in 0..typecnt {
                let utoff = detzcode(&buf[p..]);
                p += 4;
                let isdst = buf[p];
                p += 1;
                if isdst >= 2 {
                    return None;
                }
                let desigidx = usize::from(buf[p]);
                p += 1;
                if desigidx >= charcnt {
                    return None;
                }
                ttis.push(TtInfo {
                    utoff,
                    isdst: isdst == 1,
                    desigidx,
                    ttisstd: false,
                    ttisut: false,
                });
            }
            let chars: Vec<u8> = buf[p..p + charcnt].to_vec();
            p += charcnt;
            let mut lsis = Vec::new();
            let mut prevtr: i64 = 0;
            let mut prevcorr: i64 = 0;
            for _ in 0..leapcnt {
                let tr = if stored == 4 {
                    i64::from(detzcode(&buf[p..]))
                } else {
                    detzcode64(&buf[p..])
                };
                let corr = i64::from(detzcode(&buf[p + stored..]));
                p += stored + 4;
                if tr < 0 {
                    return None;
                }
                if tr - prevtr < i64::from(28 * SECSPERDAY - 1)
                    || (corr != prevcorr - 1 && corr != prevcorr + 1)
                {
                    return None;
                }
                prevtr = tr;
                prevcorr = corr;
                lsis.push(LsInfo { trans: tr, corr });
            }
            for tti in &mut ttis {
                if ttisstdcnt != 0 {
                    let v = buf[p];
                    if v > 1 {
                        return None;
                    }
                    tti.ttisstd = v == 1;
                    p += 1;
                }
            }
            for tti in &mut ttis {
                if ttisutcnt != 0 {
                    let v = buf[p];
                    if v > 1 {
                        return None;
                    }
                    tti.ttisut = v == 1;
                    p += 1;
                }
            }
            sp.ats = ats;
            sp.types = types;
            sp.ttis = ttis;
            sp.chars = chars;
            sp.lsis = lsis;
            if buf[4] == 0 {
                break;
            }
            buf.drain(..p);
        }

        let nread = buf.len();
        if doextend
            && nread > 2
            && buf[0] == b'\n'
            && buf[nread - 1] == b'\n'
            && sp.ttis.len() + 2 <= TZ_MAX_TYPES
            && let Some(mut ts) = ZoneState::parse_posix(&buf[1..nread - 1], false)
        {
            sp.merge_footer(&mut ts);
        }
        if sp.ttis.is_empty() {
            return None;
        }
        let timecnt = sp.ats.len();
        if timecnt > 1 {
            for i in 1..timecnt {
                if sp.typesequiv(usize::from(sp.types[i]), usize::from(sp.types[0]))
                    && sp.ats[i] - sp.ats[0] == SECSPERREPEAT
                {
                    sp.goback = true;
                    break;
                }
            }
            for i in (0..timecnt - 1).rev() {
                if sp.typesequiv(usize::from(sp.types[timecnt - 1]), usize::from(sp.types[i]))
                    && sp.ats[timecnt - 1] - sp.ats[i] == SECSPERREPEAT
                {
                    sp.goahead = true;
                    break;
                }
            }
        }
        sp.defaulttype = sp.infer_defaulttype();
        Some(sp)
    }

    fn infer_defaulttype(&self) -> usize {
        let unused0 = !self.types.contains(&0);
        let mut i: isize = if unused0 { 0 } else { -1 };
        if i < 0 && !self.ats.is_empty() && self.ttis[usize::from(self.types[0])].isdst {
            i = isize::from(self.types[0]);
            loop {
                i -= 1;
                #[allow(clippy::cast_sign_loss)]
                if i < 0 || !self.ttis[i as usize].isdst {
                    break;
                }
            }
        }
        if i < 0 {
            let mut k = 0usize;
            while self.ttis[k].isdst {
                k += 1;
                if k >= self.ttis.len() {
                    k = 0;
                    break;
                }
            }
            return k;
        }
        #[allow(clippy::cast_sign_loss)]
        let r = i as usize;
        r
    }

    /// Merge the transitions generated from the TZ string footer
    /// (the `doextend` part of `tzloadbody`).
    fn merge_footer(&mut self, ts: &mut ZoneState) {
        let mut gotabbr = 0usize;
        let mut charcnt = self.chars.len();
        // Work on a scratch copy of the abbreviation buffer, which may grow.
        let mut chars = self.chars.clone();
        for i in 0..ts.ttis.len() {
            let tsabbr = cstr_at(&ts.chars, ts.ttis[i].desigidx).to_vec();
            let mut found = None;
            for j in 0..charcnt {
                if cstr_at(&chars, j) == tsabbr.as_slice() {
                    found = Some(j);
                    break;
                }
            }
            if let Some(j) = found {
                ts.ttis[i].desigidx = j;
                gotabbr += 1;
            } else {
                let j = charcnt;
                let tsabbrlen = tsabbr.len();
                if j + tsabbrlen < TZ_MAX_CHARS {
                    chars.truncate(j);
                    chars.extend_from_slice(&tsabbr);
                    chars.push(0);
                    charcnt = j + tsabbrlen + 1;
                    ts.ttis[i].desigidx = j;
                    gotabbr += 1;
                }
            }
        }
        if gotabbr != ts.ttis.len() {
            return;
        }
        chars.truncate(charcnt);
        self.chars = chars;
        while self.ats.len() > 1
            && self.types[self.types.len() - 1] == self.types[self.types.len() - 2]
        {
            self.ats.pop();
            self.types.pop();
        }
        let mut i = 0;
        while i < ts.ats.len() {
            if self.ats.is_empty()
                || self.ats[self.ats.len() - 1] < ts.ats[i] + self.leapcorr(ts.ats[i])
            {
                break;
            }
            i += 1;
        }
        let base = self.ttis.len();
        while i < ts.ats.len() && self.ats.len() < TZ_MAX_TIMES {
            let at = ts.ats[i] + self.leapcorr(ts.ats[i]);
            self.ats.push(at);
            #[allow(clippy::cast_possible_truncation)]
            self.types.push((base + usize::from(ts.types[i])) as u8);
            i += 1;
        }
        self.ttis.extend_from_slice(&ts.ttis);
    }

    /// `tzparse`: parse a POSIX TZ string.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn parse_posix(name: &[u8], lastditch: bool) -> Option<ZoneState> {
        let mut pos = 0usize;
        let at = |p: usize| -> u8 { name.get(p).copied().unwrap_or(0) };
        let stdname_start;
        let stdlen;
        let stdoffset: i32;
        if lastditch {
            stdname_start = 0;
            stdlen = name.len();
            pos = name.len();
            stdoffset = 0;
        } else {
            if at(pos) == b'<' {
                pos += 1;
                stdname_start = pos;
                pos = getqzname(name, pos, b'>');
                if at(pos) != b'>' {
                    return None;
                }
                stdlen = pos - stdname_start;
                pos += 1;
            } else {
                stdname_start = pos;
                pos = getzname(name, pos);
                stdlen = pos - stdname_start;
            }
            if at(pos) == 0 {
                return None;
            }
            let (np, off) = getoffset(name, pos)?;
            pos = np;
            stdoffset = off;
        }
        let mut charcnt = stdlen + 1;
        if CHARS_CAPACITY < charcnt {
            return None;
        }
        let mut sp = ZoneState::default();
        let stdname = &name[stdname_start..stdname_start + stdlen];
        let mut dstname: &[u8] = &[];
        if at(pos) == 0 {
            sp.ttis = vec![TtInfo {
                utoff: -stdoffset,
                ..TtInfo::default()
            }];
        } else {
            let dstname_start;
            let dstlen;
            if at(pos) == b'<' {
                pos += 1;
                dstname_start = pos;
                pos = getqzname(name, pos, b'>');
                if at(pos) != b'>' {
                    return None;
                }
                dstlen = pos - dstname_start;
                pos += 1;
            } else {
                dstname_start = pos;
                pos = getzname(name, pos);
                dstlen = pos - dstname_start;
            }
            if dstlen == 0 {
                return None;
            }
            dstname = &name[dstname_start..dstname_start + dstlen];
            charcnt += dstlen + 1;
            if CHARS_CAPACITY < charcnt {
                return None;
            }
            let dstoffset: i32;
            if at(pos) != 0 && at(pos) != b',' && at(pos) != b';' {
                let (np, off) = getoffset(name, pos)?;
                pos = np;
                dstoffset = off;
            } else {
                dstoffset = stdoffset - SECSPERHOUR;
            }
            let mut rest: &[u8] = &name[pos..];
            if rest.is_empty() {
                rest = TZDEFRULESTRING;
            }
            if rest[0] != b',' && rest[0] != b';' {
                return None;
            }
            let mut rp = 1usize;
            let (np, start) = getrule(rest, rp)?;
            rp = np;
            if rest.get(rp).copied() != Some(b',') {
                return None;
            }
            rp += 1;
            let (np, end) = getrule(rest, rp)?;
            rp = np;
            if rp != rest.len() {
                return None;
            }
            sp.ttis = vec![
                TtInfo {
                    utoff: -stdoffset,
                    isdst: false,
                    desigidx: 0,
                    ..TtInfo::default()
                },
                TtInfo {
                    utoff: -dstoffset,
                    isdst: true,
                    desigidx: stdlen + 1,
                    ..TtInfo::default()
                },
            ];
            let mut janfirst: i64 = 0;
            let mut janoffset: i64 = 0;
            let mut yearbeg = EPOCH_YEAR;
            loop {
                let yearsecs =
                    i64::from(YEAR_LENGTHS[usize::from(isleap(yearbeg - 1))] * SECSPERDAY);
                yearbeg -= 1;
                if increment_overflow_time(&mut janfirst, -yearsecs) {
                    janoffset = -yearsecs;
                    break;
                }
                #[allow(clippy::cast_possible_truncation)]
                if EPOCH_YEAR - (YEARSPERREPEAT as i32) / 2 >= yearbeg {
                    break;
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            let repeat = YEARSPERREPEAT as i32;
            let mut yearlim = yearbeg + repeat + 1;
            let mut year = yearbeg;
            while year < yearlim {
                let mut starttime = transtime(year, &start, stdoffset);
                let mut endtime = transtime(year, &end, dstoffset);
                let yearsecs = YEAR_LENGTHS[usize::from(isleap(year))] * SECSPERDAY;
                let reversed = endtime < starttime;
                if reversed {
                    std::mem::swap(&mut starttime, &mut endtime);
                }
                if reversed
                    || (starttime < endtime
                        && (endtime - starttime < (yearsecs + (stdoffset - dstoffset))))
                {
                    if TZ_MAX_TIMES - 2 < sp.ats.len() {
                        break;
                    }
                    let mut a = janfirst;
                    if !increment_overflow_time(&mut a, janoffset + i64::from(starttime)) {
                        sp.ats.push(a);
                        sp.types.push(u8::from(!reversed));
                    }
                    let mut b = janfirst;
                    if !increment_overflow_time(&mut b, janoffset + i64::from(endtime)) {
                        sp.ats.push(b);
                        sp.types.push(u8::from(reversed));
                        yearlim = year + repeat + 1;
                    }
                }
                if increment_overflow_time(&mut janfirst, janoffset + i64::from(yearsecs)) {
                    break;
                }
                janoffset = 0;
                year += 1;
            }
            if sp.ats.is_empty() {
                sp.ttis = vec![sp.ttis[1]];
            } else if i64::from(year - yearbeg) > YEARSPERREPEAT {
                sp.goback = true;
                sp.goahead = true;
            }
        }
        let mut chars = Vec::with_capacity(charcnt);
        chars.extend_from_slice(stdname);
        chars.push(0);
        if !dstname.is_empty() {
            chars.extend_from_slice(dstname);
            chars.push(0);
        }
        sp.chars = chars;
        sp.defaulttype = 0;
        Some(sp)
    }

    /// The time type in effect at UTC time `t` (`localsub`).
    pub(crate) fn local_type(&self, t: i64) -> Option<&TtInfo> {
        let timecnt = self.ats.len();
        if (self.goback && t < self.ats[0]) || (self.goahead && t > self.ats[timecnt - 1]) {
            let mut seconds = if t < self.ats[0] {
                self.ats[0] - t
            } else {
                t - self.ats[timecnt - 1]
            };
            seconds -= 1;
            let years = (seconds / SECSPERREPEAT + 1) * YEARSPERREPEAT;
            let seconds = years * AVGSECSPERYEAR;
            let newt = if t < self.ats[0] {
                t + seconds
            } else {
                t - seconds
            };
            if newt < self.ats[0] || newt > self.ats[timecnt - 1] {
                return None;
            }
            return self.local_type(newt);
        }
        let i = if timecnt == 0 || t < self.ats[0] {
            self.defaulttype
        } else {
            let mut lo = 1usize;
            let mut hi = timecnt;
            while lo < hi {
                let mid = usize::midpoint(lo, hi);
                if t < self.ats[mid] {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            usize::from(self.types[lo - 1])
        };
        self.ttis.get(i)
    }

    fn lowest_standard_type(&self) -> &TtInfo {
        let mut i = 0usize;
        while self.ttis[i].isdst {
            i += 1;
            if i >= self.ttis.len() {
                i = 0;
                break;
            }
        }
        &self.ttis[i]
    }

    /// `pg_next_dst_boundary`.
    pub(crate) fn next_dst_boundary(&self, t: i64) -> Boundary {
        let timecnt = self.ats.len();
        if timecnt == 0 {
            let tt = self.lowest_standard_type();
            return Boundary::None {
                gmtoff: i64::from(tt.utoff),
                isdst: tt.isdst,
            };
        }
        if (self.goback && t < self.ats[0]) || (self.goahead && t > self.ats[timecnt - 1]) {
            let mut seconds = if t < self.ats[0] {
                self.ats[0] - t
            } else {
                t - self.ats[timecnt - 1]
            };
            seconds -= 1;
            let tcycles = seconds / YEARSPERREPEAT / AVGSECSPERYEAR + 1;
            let seconds = tcycles * YEARSPERREPEAT * AVGSECSPERYEAR;
            let newt = if t < self.ats[0] {
                t + seconds
            } else {
                t - seconds
            };
            if newt < self.ats[0] || newt > self.ats[timecnt - 1] {
                return Boundary::Fail;
            }
            return match self.next_dst_boundary(newt) {
                Boundary::Next {
                    before_gmtoff,
                    before_isdst,
                    boundary,
                    after_gmtoff,
                    after_isdst,
                } => Boundary::Next {
                    before_gmtoff,
                    before_isdst,
                    boundary: if t < self.ats[0] {
                        boundary - seconds
                    } else {
                        boundary + seconds
                    },
                    after_gmtoff,
                    after_isdst,
                },
                other => other,
            };
        }
        if t >= self.ats[timecnt - 1] {
            let tt = &self.ttis[usize::from(self.types[timecnt - 1])];
            return Boundary::None {
                gmtoff: i64::from(tt.utoff),
                isdst: tt.isdst,
            };
        }
        if t < self.ats[0] {
            let before = self.lowest_standard_type();
            let after = &self.ttis[usize::from(self.types[0])];
            return Boundary::Next {
                before_gmtoff: i64::from(before.utoff),
                before_isdst: before.isdst,
                boundary: self.ats[0],
                after_gmtoff: i64::from(after.utoff),
                after_isdst: after.isdst,
            };
        }
        let mut lo = 1usize;
        let mut hi = timecnt - 1;
        while lo < hi {
            let mid = usize::midpoint(lo, hi);
            if t < self.ats[mid] {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let i = lo;
        let before = &self.ttis[usize::from(self.types[i - 1])];
        let after = &self.ttis[usize::from(self.types[i])];
        Boundary::Next {
            before_gmtoff: i64::from(before.utoff),
            before_isdst: before.isdst,
            boundary: self.ats[i],
            after_gmtoff: i64::from(after.utoff),
            after_isdst: after.isdst,
        }
    }

    /// `pg_interpret_timezone_abbrev`: meaning of an (upper-case)
    /// abbreviation at or around UTC time `t`.
    pub(crate) fn interpret_abbrev(&self, abbrev: &[u8], t: i64) -> Option<(i64, bool)> {
        let charcnt = self.chars.len();
        let mut abbrind = 0usize;
        while abbrind < charcnt {
            if cstr_at(&self.chars, abbrind) == abbrev {
                break;
            }
            while abbrind < charcnt && self.chars[abbrind] != 0 {
                abbrind += 1;
            }
            abbrind += 1;
        }
        if abbrind >= charcnt {
            return None;
        }
        let timecnt = self.ats.len();
        let mut lo = 0usize;
        let mut hi = timecnt;
        while lo < hi {
            let mid = usize::midpoint(lo, hi);
            if t < self.ats[mid] {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let cutoff = lo;
        for i in (0..cutoff).rev() {
            let tt = &self.ttis[usize::from(self.types[i])];
            if tt.desigidx == abbrind {
                return Some((i64::from(tt.utoff), tt.isdst));
            }
        }
        for i in cutoff..timecnt {
            let tt = &self.ttis[usize::from(self.types[i])];
            if tt.desigidx == abbrind {
                return Some((i64::from(tt.utoff), tt.isdst));
            }
        }
        None
    }

    /// Whether the zone uses leap seconds (PostgreSQL rejects such zones
    /// as the session time zone).
    pub(crate) fn has_leap_seconds(&self) -> bool {
        !self.lsis.is_empty()
    }
}

fn getzname(s: &[u8], mut p: usize) -> usize {
    while p < s.len() {
        let c = s[p];
        if c.is_ascii_digit() || c == b',' || c == b'-' || c == b'+' {
            break;
        }
        p += 1;
    }
    p
}

fn getqzname(s: &[u8], mut p: usize, delim: u8) -> usize {
    while p < s.len() && s[p] != delim {
        p += 1;
    }
    p
}

fn getnum(s: &[u8], p: usize, min: i32, max: i32) -> Option<(usize, i32)> {
    let mut p = p;
    if !s.get(p).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let mut num: i32 = 0;
    while let Some(&c) = s.get(p) {
        if !c.is_ascii_digit() {
            break;
        }
        num = num * 10 + i32::from(c - b'0');
        if num > max {
            return None;
        }
        p += 1;
    }
    if num < min {
        return None;
    }
    Some((p, num))
}

fn getsecs(s: &[u8], p: usize) -> Option<(usize, i32)> {
    let (mut p, num) = getnum(s, p, 0, 24 * 7 - 1)?;
    let mut secs = num * SECSPERHOUR;
    if s.get(p) == Some(&b':') {
        let (np, num) = getnum(s, p + 1, 0, 59)?;
        p = np;
        secs += num * SECSPERMIN;
        if s.get(p) == Some(&b':') {
            let (np, num) = getnum(s, p + 1, 0, 60)?;
            p = np;
            secs += num;
        }
    }
    Some((p, secs))
}

fn getoffset(s: &[u8], p: usize) -> Option<(usize, i32)> {
    let mut p = p;
    let mut neg = false;
    match s.get(p) {
        Some(b'-') => {
            neg = true;
            p += 1;
        }
        Some(b'+') => p += 1,
        _ => {}
    }
    let (p, secs) = getsecs(s, p)?;
    Some((p, if neg { -secs } else { secs }))
}

#[derive(Debug, Clone, Copy, Default)]
struct Rule {
    kind: u8, // b'J', b'D' (day of year), b'M'
    day: i32,
    week: i32,
    mon: i32,
    time: i32,
}

fn getrule(s: &[u8], p: usize) -> Option<(usize, Rule)> {
    let mut r = Rule::default();
    let mut p = p;
    match s.get(p) {
        Some(b'J') => {
            r.kind = b'J';
            let (np, d) = getnum(s, p + 1, 1, 365)?;
            p = np;
            r.day = d;
        }
        Some(b'M') => {
            r.kind = b'M';
            let (np, m) = getnum(s, p + 1, 1, 12)?;
            r.mon = m;
            if s.get(np) != Some(&b'.') {
                return None;
            }
            let (np, w) = getnum(s, np + 1, 1, 5)?;
            r.week = w;
            if s.get(np) != Some(&b'.') {
                return None;
            }
            let (np, d) = getnum(s, np + 1, 0, 6)?;
            r.day = d;
            p = np;
        }
        Some(c) if c.is_ascii_digit() => {
            r.kind = b'D';
            let (np, d) = getnum(s, p, 0, 365)?;
            p = np;
            r.day = d;
        }
        _ => return None,
    }
    if s.get(p) == Some(&b'/') {
        let (np, t) = getoffset(s, p + 1)?;
        p = np;
        r.time = t;
    } else {
        r.time = 2 * SECSPERHOUR;
    }
    Some((p, r))
}

fn transtime(year: i32, rule: &Rule, offset: i32) -> i32 {
    let leap = usize::from(isleap(year));
    let value = match rule.kind {
        b'J' => {
            let mut v = (rule.day - 1) * SECSPERDAY;
            if leap == 1 && rule.day >= 60 {
                v += SECSPERDAY;
            }
            v
        }
        b'D' => rule.day * SECSPERDAY,
        _ => {
            let m1 = (rule.mon + 9) % 12 + 1;
            let yy0 = if rule.mon <= 2 { year - 1 } else { year };
            let yy1 = yy0 / 100;
            let yy2 = yy0 % 100;
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            let mut d = rule.day - dow;
            if d < 0 {
                d += 7;
            }
            for _ in 1..rule.week {
                if d + 7 >= MON_LENGTHS[leap][(rule.mon - 1) as usize] {
                    break;
                }
                d += 7;
            }
            let mut v = d * SECSPERDAY;
            for len in &MON_LENGTHS[leap][..(rule.mon - 1) as usize] {
                v += len * SECSPERDAY;
            }
            v
        }
    };
    value + rule.time + offset
}
