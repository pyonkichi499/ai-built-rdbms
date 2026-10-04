//! Time zones: the zone database (`TZif` files read through [`ZoneSource`]),
//! session time zone values and the PostgreSQL algorithms that use them.

mod source;
mod state;

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::sync::{Arc, RwLock};

pub use source::{FsZoneSource, MemZoneSource, ZoneSource};
pub(crate) use state::Boundary;
use state::ZoneState;

use crate::calendar::{
    SECS_PER_DAY, SECS_PER_HOUR, SECS_PER_MINUTE, Tm, UNIX_EPOCH_JDATE, date2j, is_valid_julian,
    j2date,
};

/// Maximum length of a time zone name (`TZ_STRLEN_MAX`).
const TZ_STRLEN_MAX: usize = 255;

struct ZoneInner {
    /// Canonical name (what `SHOW timezone` displays).
    name: String,
    state: ZoneState,
}

/// A loaded time zone (`pg_tz`). Cheap to clone.
#[derive(Clone)]
pub struct TimeZone {
    inner: Arc<ZoneInner>,
}

impl fmt::Debug for TimeZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TimeZone").field(&self.inner.name).finish()
    }
}

impl PartialEq for TimeZone {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// Local time information for one instant.
pub(crate) struct LocalTime<'a> {
    pub tm: Tm,
    pub fsec: i32,
    /// Offset in PostgreSQL's convention (seconds *west* of UTC).
    pub tz: i32,
    pub abbrev: &'a str,
}

impl TimeZone {
    fn new(name: String, state: ZoneState) -> Self {
        Self {
            inner: Arc::new(ZoneInner { name, state }),
        }
    }

    /// The built-in UTC zone (used when no tzdata is available).
    pub fn utc() -> Self {
        Self::new(
            "UTC".to_owned(),
            ZoneState::parse_posix(b"UTC", true).expect("UTC"),
        )
    }

    /// Canonical name of the zone, as `SHOW timezone` reports it.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    fn state(&self) -> &ZoneState {
        &self.inner.state
    }

    /// `pg_tzset_offset`: a fixed-offset zone. `gmtoffset` uses the POSIX
    /// sign convention (seconds *west* of Greenwich are positive).
    pub(crate) fn from_posix_offset(gmtoffset: i64) -> Option<Self> {
        let mut absoffset = gmtoffset.abs();
        let mut offsetstr = format!("{:02}", absoffset / SECS_PER_HOUR);
        absoffset %= SECS_PER_HOUR;
        if absoffset != 0 {
            let _ = write!(offsetstr, ":{:02}", absoffset / SECS_PER_MINUTE);
            absoffset %= SECS_PER_MINUTE;
            if absoffset != 0 {
                let _ = write!(offsetstr, ":{absoffset:02}");
            }
        }
        let name = if gmtoffset > 0 {
            format!("<-{offsetstr}>+{offsetstr}")
        } else {
            format!("<+{offsetstr}>-{offsetstr}")
        };
        let state = ZoneState::parse_posix(name.as_bytes(), false)?;
        Some(Self::new(name.to_ascii_uppercase(), state))
    }

    /// Time zone offset (seconds east of UTC) and abbreviation in effect at
    /// a Unix time.
    pub(crate) fn utc_offset_at(&self, unix_secs: i64) -> (i64, bool, &str) {
        match self.state().local_type(unix_secs) {
            Some(tt) => (
                i64::from(tt.utoff),
                tt.isdst,
                self.state().abbrev(tt.desigidx),
            ),
            None => (0, false, ""),
        }
    }

    /// `timestamp2tm` with time zone rotation. `dt` is a timestamptz value.
    pub(crate) fn to_local(&self, dt: i64) -> Option<LocalTime<'_>> {
        let (_, fsec) = crate::calendar::timestamp2tm_utc(dt)?;
        let utime = (dt - i64::from(fsec)) / crate::calendar::USECS_PER_SEC
            + crate::calendar::EPOCH_DIFF_SECS;
        let (gmtoff, isdst, abbrev) = self.utc_offset_at(utime);
        let local = utime + gmtoff;
        let days = local.div_euclid(SECS_PER_DAY);
        let sod = local.rem_euclid(SECS_PER_DAY);
        let jd = days + i64::from(UNIX_EPOCH_JDATE);
        let jd = i32::try_from(jd).ok()?;
        let (year, mon, mday) = j2date(jd);
        #[allow(clippy::cast_possible_truncation)]
        let tm = Tm {
            year,
            mon,
            mday,
            hour: (sod / SECS_PER_HOUR) as i32,
            min: ((sod % SECS_PER_HOUR) / SECS_PER_MINUTE) as i32,
            sec: (sod % SECS_PER_MINUTE) as i32,
            yday: 0,
            isdst: i32::from(isdst),
        };
        #[allow(clippy::cast_possible_truncation)]
        Some(LocalTime {
            tm,
            fsec,
            tz: -(gmtoff as i32),
            abbrev,
        })
    }

    /// `DetermineTimeZoneOffsetInternal`: returns the offset (seconds west
    /// of UTC) and the imputed Unix time; sets `tm.isdst`.
    pub(crate) fn determine_offset_internal(&self, tm: &mut Tm) -> (i32, i64) {
        let overflow = |tm: &mut Tm| {
            tm.isdst = 0;
            (0, 0)
        };
        if !is_valid_julian(tm.year, tm.mon, tm.mday) {
            return overflow(tm);
        }
        let date = i64::from(date2j(tm.year, tm.mon, tm.mday) - UNIX_EPOCH_JDATE);
        let day = date * SECS_PER_DAY;
        let sec =
            i64::from(tm.sec) + (i64::from(tm.min) + i64::from(tm.hour) * 60) * SECS_PER_MINUTE;
        let mytime = day + sec;
        let prevtime = mytime - SECS_PER_DAY;
        #[allow(clippy::cast_possible_truncation)]
        match self.state().next_dst_boundary(prevtime) {
            Boundary::Fail => overflow(tm),
            Boundary::None { gmtoff, isdst } => {
                tm.isdst = i32::from(isdst);
                (-(gmtoff as i32), mytime - gmtoff)
            }
            Boundary::Next {
                before_gmtoff,
                before_isdst,
                boundary,
                after_gmtoff,
                after_isdst,
            } => {
                let beforetime = mytime - before_gmtoff;
                let aftertime = mytime - after_gmtoff;
                if beforetime < boundary && aftertime < boundary {
                    tm.isdst = i32::from(before_isdst);
                    return (-(before_gmtoff as i32), beforetime);
                }
                if beforetime > boundary && aftertime >= boundary {
                    tm.isdst = i32::from(after_isdst);
                    return (-(after_gmtoff as i32), aftertime);
                }
                if beforetime > aftertime {
                    tm.isdst = i32::from(before_isdst);
                    return (-(before_gmtoff as i32), beforetime);
                }
                tm.isdst = i32::from(after_isdst);
                (-(after_gmtoff as i32), aftertime)
            }
        }
    }

    /// `DetermineTimeZoneOffset`.
    pub(crate) fn determine_offset(&self, tm: &mut Tm) -> i32 {
        self.determine_offset_internal(tm).0
    }

    /// `DetermineTimeZoneAbbrevOffset` for a dynamic abbreviation.
    pub(crate) fn determine_abbrev_offset(&self, tm: &mut Tm, abbrev: &[u8]) -> i32 {
        let (zone_offset, t) = self.determine_offset_internal(tm);
        let up = abbrev.to_ascii_uppercase();
        if let Some((gmtoff, isdst)) = self.state().interpret_abbrev(&up, t) {
            tm.isdst = i32::from(isdst);
            #[allow(clippy::cast_possible_truncation)]
            return -(gmtoff as i32);
        }
        zone_offset
    }

    /// `DetermineTimeZoneAbbrevOffsetTS`: offset (west) and DST flag of a
    /// dynamic abbreviation at timestamptz `ts`.
    pub(crate) fn determine_abbrev_offset_ts(&self, ts: i64, abbrev: &[u8]) -> Option<(i32, bool)> {
        let t = crate::timestamp::timestamptz_to_time_t(ts);
        let up = abbrev.to_ascii_uppercase();
        if let Some((gmtoff, isdst)) = self.state().interpret_abbrev(&up, t) {
            #[allow(clippy::cast_possible_truncation)]
            return Some((-(gmtoff as i32), isdst));
        }
        let mut local = self.to_local(ts)?.tm;
        let off = self.determine_offset(&mut local);
        Some((off, local.isdst > 0))
    }

    /// If the zone only ever uses one UTC offset, return it (seconds east).
    pub(crate) fn has_leap_seconds(&self) -> bool {
        self.state().has_leap_seconds()
    }
}

/// The time zone database: a [`ZoneSource`] plus a process-wide cache of
/// loaded zones (`pg_tzset`).
pub struct ZoneDb {
    source: Option<Box<dyn ZoneSource>>,
    cache: RwLock<HashMap<String, TimeZone>>,
}

impl fmt::Debug for ZoneDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZoneDb")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl ZoneDb {
    /// A database reading `TZif` files from `source`.
    pub fn new(source: Box<dyn ZoneSource>) -> Self {
        Self {
            source: Some(source),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// The system database at `/usr/share/zoneinfo`.
    pub fn system() -> Self {
        Self::new(Box::new(FsZoneSource::new("/usr/share/zoneinfo")))
    }

    /// A database without tzdata: only `UTC`/`GMT`, POSIX-style zone
    /// specifications and fixed offsets work.
    pub fn without_tzdata() -> Self {
        Self {
            source: None,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// `pg_tzset`: look up a zone by name (case-insensitive), falling back
    /// to interpreting it as a POSIX TZ specification.
    pub fn lookup(&self, name: &str) -> Option<TimeZone> {
        if name.len() > TZ_STRLEN_MAX {
            return None;
        }
        let upper = name.to_ascii_uppercase();
        if let Ok(cache) = self.cache.read()
            && let Some(hit) = cache.get(&upper)
        {
            return Some(hit.clone());
        }
        // Like PostgreSQL, only successful lookups are cached.
        let result = self.load_uncached(&upper)?;
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(upper, result.clone());
        }
        Some(result)
    }

    fn load_uncached(&self, upper: &str) -> Option<TimeZone> {
        if upper == "GMT" {
            let state = ZoneState::parse_posix(upper.as_bytes(), true)?;
            return Some(TimeZone::new(upper.to_owned(), state));
        }
        if let Some((canon, state)) = self.tzload(upper) {
            return Some(TimeZone::new(canon, state));
        }
        if !upper.starts_with(':')
            && let Some(state) = ZoneState::parse_posix(upper.as_bytes(), false)
        {
            return Some(TimeZone::new(upper.to_owned(), state));
        }
        // Without tzdata, still understand the common UTC spellings.
        if matches!(
            upper,
            "UTC" | "ETC/UTC" | "UCT" | "ETC/UCT" | "ZULU" | "ETC/ZULU"
        ) && self.source.as_ref().is_none_or(|s| s.read_dir("").is_err())
        {
            let state = ZoneState::parse_posix(b"UTC", true)?;
            return Some(TimeZone::new("UTC".to_owned(), state));
        }
        None
    }

    /// `tzload` + `pg_open_tzfile`: case-insensitive path resolution.
    fn tzload(&self, name: &str) -> Option<(String, ZoneState)> {
        let source = self.source.as_ref()?;
        let name = name.strip_prefix(':').unwrap_or(name);
        let mut canon = String::new();
        for comp in name.split('/') {
            let entries = source.read_dir(&canon).ok()?;
            let found = entries.into_iter().find(|e| {
                !e.starts_with('.') && e.len() == comp.len() && e.eq_ignore_ascii_case(comp)
            })?;
            if !canon.is_empty() {
                canon.push('/');
            }
            canon.push_str(&found);
        }
        let data = source.read_file(&canon).ok()?;
        let state = ZoneState::load(&data, true)?;
        Some((canon, state))
    }
}
