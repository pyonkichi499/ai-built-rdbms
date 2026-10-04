//! Keyword tables of the date/time parser (`datetktbl`, `deltatktbl`) and
//! the `Default` time zone abbreviation set (`timezone_abbreviations`).

// Field type codes (`datetime.h`).
pub(crate) const RESERV: i32 = 0;
pub(crate) const MONTH: i32 = 1;
pub(crate) const YEAR: i32 = 2;
pub(crate) const DAY: i32 = 3;
pub(crate) const TZ: i32 = 5;
pub(crate) const DTZ: i32 = 6;
pub(crate) const DYNTZ: i32 = 7;
pub(crate) const IGNORE_DTF: i32 = 8;
pub(crate) const AMPM: i32 = 9;
pub(crate) const HOUR: i32 = 10;
pub(crate) const MINUTE: i32 = 11;
pub(crate) const SECOND: i32 = 12;
pub(crate) const MILLISECOND: i32 = 13;
pub(crate) const MICROSECOND: i32 = 14;
pub(crate) const DOY: i32 = 15;
pub(crate) const DOW: i32 = 16;
pub(crate) const UNITS: i32 = 17;
pub(crate) const ADBC: i32 = 18;
pub(crate) const AGO: i32 = 19;
pub(crate) const ISOTIME: i32 = 23;
pub(crate) const WEEK: i32 = 24;
pub(crate) const DECADE: i32 = 25;
pub(crate) const CENTURY: i32 = 26;
pub(crate) const MILLENNIUM: i32 = 27;
pub(crate) const DTZMOD: i32 = 28;
pub(crate) const UNKNOWN_FIELD: i32 = 31;

// Token values (`DTK_*`).
pub(crate) const DTK_NUMBER: i32 = 0;
pub(crate) const DTK_STRING: i32 = 1;
pub(crate) const DTK_DATE: i32 = 2;
pub(crate) const DTK_TIME: i32 = 3;
pub(crate) const DTK_TZ: i32 = 4;
pub(crate) const DTK_SPECIAL: i32 = 6;
pub(crate) const DTK_EARLY: i32 = 9;
pub(crate) const DTK_LATE: i32 = 10;
pub(crate) const DTK_EPOCH: i32 = 11;
pub(crate) const DTK_NOW: i32 = 12;
pub(crate) const DTK_YESTERDAY: i32 = 13;
pub(crate) const DTK_TODAY: i32 = 14;
pub(crate) const DTK_TOMORROW: i32 = 15;
pub(crate) const DTK_ZULU: i32 = 16;
pub(crate) const DTK_DELTA: i32 = 17;
pub(crate) const DTK_SECOND: i32 = 18;
pub(crate) const DTK_MINUTE: i32 = 19;
pub(crate) const DTK_HOUR: i32 = 20;
pub(crate) const DTK_DAY: i32 = 21;
pub(crate) const DTK_WEEK: i32 = 22;
pub(crate) const DTK_MONTH: i32 = 23;
pub(crate) const DTK_QUARTER: i32 = 24;
pub(crate) const DTK_YEAR: i32 = 25;
pub(crate) const DTK_DECADE: i32 = 26;
pub(crate) const DTK_CENTURY: i32 = 27;
pub(crate) const DTK_MILLENNIUM: i32 = 28;
pub(crate) const DTK_MILLISEC: i32 = 29;
pub(crate) const DTK_MICROSEC: i32 = 30;
pub(crate) const DTK_JULIAN: i32 = 31;
pub(crate) const DTK_DOW: i32 = 32;
pub(crate) const DTK_DOY: i32 = 33;
pub(crate) const DTK_TZ_HOUR: i32 = 34;
pub(crate) const DTK_TZ_MINUTE: i32 = 35;
pub(crate) const DTK_ISOYEAR: i32 = 36;
pub(crate) const DTK_ISODOW: i32 = 37;

pub(crate) const AM: i32 = 0;
pub(crate) const PM: i32 = 1;
pub(crate) const HR24: i32 = 2;
pub(crate) const BC: i32 = 1;

pub(crate) const fn dtk_m(t: i32) -> i32 {
    1 << t
}
pub(crate) const DTK_ALL_SECS_M: i32 = dtk_m(SECOND) | dtk_m(MILLISECOND) | dtk_m(MICROSECOND);
pub(crate) const DTK_DATE_M: i32 = dtk_m(YEAR) | dtk_m(MONTH) | dtk_m(DAY);
pub(crate) const DTK_TIME_M: i32 = dtk_m(HOUR) | dtk_m(MINUTE) | DTK_ALL_SECS_M;

const TOKMAXLEN: usize = 10;

/// One keyword table entry (`datetkn`).
pub(crate) struct Tkn {
    pub token: &'static str,
    pub ty: i32,
    pub value: i32,
}

const fn t(token: &'static str, ty: i32, value: i32) -> Tkn {
    Tkn { token, ty, value }
}

/// `strncmp(key, token, TOKMAXLEN) == 0`.
pub(crate) fn tok_eq(key: &[u8], token: &str) -> bool {
    let tb = token.as_bytes();
    let n = key.len().min(TOKMAXLEN);
    if key.len() >= TOKMAXLEN {
        tb.len() == TOKMAXLEN && &key[..TOKMAXLEN] == tb
    } else {
        &key[..n] == tb
    }
}

pub(crate) fn search(key: &[u8], table: &'static [Tkn]) -> Option<&'static Tkn> {
    table.iter().find(|e| tok_eq(key, e.token))
}

pub(crate) static DATETKTBL: &[Tkn] = &[
    t("+infinity", RESERV, DTK_LATE),
    t("-infinity", RESERV, DTK_EARLY),
    t("ad", ADBC, 0),
    t("allballs", RESERV, DTK_ZULU),
    t("am", AMPM, AM),
    t("apr", MONTH, 4),
    t("april", MONTH, 4),
    t("at", IGNORE_DTF, 0),
    t("aug", MONTH, 8),
    t("august", MONTH, 8),
    t("bc", ADBC, BC),
    t("d", UNITS, DTK_DAY),
    t("dec", MONTH, 12),
    t("december", MONTH, 12),
    t("dow", UNITS, DTK_DOW),
    t("doy", UNITS, DTK_DOY),
    t("dst", DTZMOD, 3600),
    t("epoch", RESERV, DTK_EPOCH),
    t("feb", MONTH, 2),
    t("february", MONTH, 2),
    t("fri", DOW, 5),
    t("friday", DOW, 5),
    t("h", UNITS, DTK_HOUR),
    t("infinity", RESERV, DTK_LATE),
    t("isodow", UNITS, DTK_ISODOW),
    t("isoyear", UNITS, DTK_ISOYEAR),
    t("j", UNITS, DTK_JULIAN),
    t("jan", MONTH, 1),
    t("january", MONTH, 1),
    t("jd", UNITS, DTK_JULIAN),
    t("jul", MONTH, 7),
    t("julian", UNITS, DTK_JULIAN),
    t("july", MONTH, 7),
    t("jun", MONTH, 6),
    t("june", MONTH, 6),
    t("m", UNITS, DTK_MONTH),
    t("mar", MONTH, 3),
    t("march", MONTH, 3),
    t("may", MONTH, 5),
    t("mm", UNITS, DTK_MINUTE),
    t("mon", DOW, 1),
    t("monday", DOW, 1),
    t("nov", MONTH, 11),
    t("november", MONTH, 11),
    t("now", RESERV, DTK_NOW),
    t("oct", MONTH, 10),
    t("october", MONTH, 10),
    t("on", IGNORE_DTF, 0),
    t("pm", AMPM, PM),
    t("s", UNITS, DTK_SECOND),
    t("sat", DOW, 6),
    t("saturday", DOW, 6),
    t("sep", MONTH, 9),
    t("sept", MONTH, 9),
    t("september", MONTH, 9),
    t("sun", DOW, 0),
    t("sunday", DOW, 0),
    t("t", ISOTIME, DTK_TIME),
    t("thu", DOW, 4),
    t("thur", DOW, 4),
    t("thurs", DOW, 4),
    t("thursday", DOW, 4),
    t("today", RESERV, DTK_TODAY),
    t("tomorrow", RESERV, DTK_TOMORROW),
    t("tue", DOW, 2),
    t("tues", DOW, 2),
    t("tuesday", DOW, 2),
    t("wed", DOW, 3),
    t("wednesday", DOW, 3),
    t("weds", DOW, 3),
    t("y", UNITS, DTK_YEAR),
    t("yesterday", RESERV, DTK_YESTERDAY),
];

pub(crate) static DELTATKTBL: &[Tkn] = &[
    t("@", IGNORE_DTF, 0),
    t("ago", AGO, 0),
    t("c", UNITS, DTK_CENTURY),
    t("cent", UNITS, DTK_CENTURY),
    t("centuries", UNITS, DTK_CENTURY),
    t("century", UNITS, DTK_CENTURY),
    t("d", UNITS, DTK_DAY),
    t("day", UNITS, DTK_DAY),
    t("days", UNITS, DTK_DAY),
    t("dec", UNITS, DTK_DECADE),
    t("decade", UNITS, DTK_DECADE),
    t("decades", UNITS, DTK_DECADE),
    t("decs", UNITS, DTK_DECADE),
    t("h", UNITS, DTK_HOUR),
    t("hour", UNITS, DTK_HOUR),
    t("hours", UNITS, DTK_HOUR),
    t("hr", UNITS, DTK_HOUR),
    t("hrs", UNITS, DTK_HOUR),
    t("m", UNITS, DTK_MINUTE),
    t("microsecon", UNITS, DTK_MICROSEC),
    t("mil", UNITS, DTK_MILLENNIUM),
    t("millennia", UNITS, DTK_MILLENNIUM),
    t("millennium", UNITS, DTK_MILLENNIUM),
    t("millisecon", UNITS, DTK_MILLISEC),
    t("mils", UNITS, DTK_MILLENNIUM),
    t("min", UNITS, DTK_MINUTE),
    t("mins", UNITS, DTK_MINUTE),
    t("minute", UNITS, DTK_MINUTE),
    t("minutes", UNITS, DTK_MINUTE),
    t("mon", UNITS, DTK_MONTH),
    t("mons", UNITS, DTK_MONTH),
    t("month", UNITS, DTK_MONTH),
    t("months", UNITS, DTK_MONTH),
    t("ms", UNITS, DTK_MILLISEC),
    t("msec", UNITS, DTK_MILLISEC),
    t("msecond", UNITS, DTK_MILLISEC),
    t("mseconds", UNITS, DTK_MILLISEC),
    t("msecs", UNITS, DTK_MILLISEC),
    t("qtr", UNITS, DTK_QUARTER),
    t("quarter", UNITS, DTK_QUARTER),
    t("s", UNITS, DTK_SECOND),
    t("sec", UNITS, DTK_SECOND),
    t("second", UNITS, DTK_SECOND),
    t("seconds", UNITS, DTK_SECOND),
    t("secs", UNITS, DTK_SECOND),
    t("timezone", UNITS, DTK_TZ),
    t("timezone_h", UNITS, DTK_TZ_HOUR),
    t("timezone_m", UNITS, DTK_TZ_MINUTE),
    t("us", UNITS, DTK_MICROSEC),
    t("usec", UNITS, DTK_MICROSEC),
    t("usecond", UNITS, DTK_MICROSEC),
    t("useconds", UNITS, DTK_MICROSEC),
    t("usecs", UNITS, DTK_MICROSEC),
    t("w", UNITS, DTK_WEEK),
    t("week", UNITS, DTK_WEEK),
    t("weeks", UNITS, DTK_WEEK),
    t("y", UNITS, DTK_YEAR),
    t("year", UNITS, DTK_YEAR),
    t("years", UNITS, DTK_YEAR),
    t("yr", UNITS, DTK_YEAR),
    t("yrs", UNITS, DTK_YEAR),
];

/// A time zone abbreviation (offsets are seconds east of UTC).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Abbrev {
    /// Fixed-offset standard-time abbreviation.
    Tz(i32),
    /// Fixed-offset daylight-time abbreviation.
    Dtz(i32),
    /// Dynamic abbreviation whose meaning comes from the named zone.
    Dyn(&'static str),
}

pub(crate) fn search_abbrev(key: &[u8]) -> Option<(&'static str, Abbrev)> {
    ZONE_ABBREVS
        .iter()
        .find(|(name, _)| tok_eq(key, name))
        .map(|&(n, a)| (n, a))
}

/// The `Default` abbreviation set shipped with PostgreSQL 17
/// (`src/timezone/tznames/Default`).
pub(crate) static ZONE_ABBREVS: &[(&str, Abbrev)] = &[
    ("acdt", Abbrev::Dtz(37800)),
    ("acsst", Abbrev::Dtz(37800)),
    ("acst", Abbrev::Tz(34200)),
    ("act", Abbrev::Tz(-18000)),
    ("acwst", Abbrev::Tz(31500)),
    ("adt", Abbrev::Dtz(-10800)),
    ("aedt", Abbrev::Dtz(39600)),
    ("aesst", Abbrev::Dtz(39600)),
    ("aest", Abbrev::Tz(36000)),
    ("aft", Abbrev::Tz(16200)),
    ("akdt", Abbrev::Dtz(-28800)),
    ("akst", Abbrev::Tz(-32400)),
    ("almst", Abbrev::Dtz(25200)),
    ("almt", Abbrev::Tz(21600)),
    ("amst", Abbrev::Dyn("Asia/Yerevan")),
    ("amt", Abbrev::Tz(-14400)),
    ("anast", Abbrev::Dyn("Asia/Anadyr")),
    ("anat", Abbrev::Dyn("Asia/Anadyr")),
    ("arst", Abbrev::Dyn("America/Argentina/Buenos_Aires")),
    ("art", Abbrev::Dyn("America/Argentina/Buenos_Aires")),
    ("ast", Abbrev::Tz(-14400)),
    ("awsst", Abbrev::Dtz(32400)),
    ("awst", Abbrev::Tz(28800)),
    ("azost", Abbrev::Dtz(0)),
    ("azot", Abbrev::Tz(-3600)),
    ("azst", Abbrev::Dyn("Asia/Baku")),
    ("azt", Abbrev::Dyn("Asia/Baku")),
    ("bdst", Abbrev::Dtz(7200)),
    ("bdt", Abbrev::Tz(21600)),
    ("bnt", Abbrev::Tz(28800)),
    ("bort", Abbrev::Tz(28800)),
    ("bot", Abbrev::Tz(-14400)),
    ("bra", Abbrev::Tz(-10800)),
    ("brst", Abbrev::Dtz(-7200)),
    ("brt", Abbrev::Tz(-10800)),
    ("bst", Abbrev::Dtz(3600)),
    ("btt", Abbrev::Tz(21600)),
    ("cadt", Abbrev::Dtz(37800)),
    ("cast", Abbrev::Tz(34200)),
    ("cct", Abbrev::Tz(28800)),
    ("cdt", Abbrev::Dtz(-18000)),
    ("cest", Abbrev::Dtz(7200)),
    ("cet", Abbrev::Tz(3600)),
    ("cetdst", Abbrev::Dtz(7200)),
    ("chadt", Abbrev::Dtz(49500)),
    ("chast", Abbrev::Tz(45900)),
    ("chut", Abbrev::Tz(36000)),
    ("ckt", Abbrev::Dyn("Pacific/Rarotonga")),
    ("clst", Abbrev::Dtz(-10800)),
    ("clt", Abbrev::Dyn("America/Santiago")),
    ("cot", Abbrev::Tz(-18000)),
    ("cst", Abbrev::Tz(-21600)),
    ("cxt", Abbrev::Tz(25200)),
    ("davt", Abbrev::Dyn("Antarctica/Davis")),
    ("ddut", Abbrev::Tz(36000)),
    ("easst", Abbrev::Dyn("Pacific/Easter")),
    ("east", Abbrev::Dyn("Pacific/Easter")),
    ("eat", Abbrev::Tz(10800)),
    ("edt", Abbrev::Dtz(-14400)),
    ("eest", Abbrev::Dtz(10800)),
    ("eet", Abbrev::Tz(7200)),
    ("eetdst", Abbrev::Dtz(10800)),
    ("egst", Abbrev::Dtz(0)),
    ("egt", Abbrev::Tz(-3600)),
    ("est", Abbrev::Tz(-18000)),
    ("fet", Abbrev::Tz(10800)),
    ("fjst", Abbrev::Dtz(46800)),
    ("fjt", Abbrev::Tz(43200)),
    ("fkst", Abbrev::Dyn("Atlantic/Stanley")),
    ("fkt", Abbrev::Dyn("Atlantic/Stanley")),
    ("fnst", Abbrev::Dtz(-3600)),
    ("fnt", Abbrev::Tz(-7200)),
    ("galt", Abbrev::Tz(-21600)),
    ("gamt", Abbrev::Tz(-32400)),
    ("gest", Abbrev::Dyn("Asia/Tbilisi")),
    ("get", Abbrev::Dyn("Asia/Tbilisi")),
    ("gft", Abbrev::Tz(-10800)),
    ("gilt", Abbrev::Tz(43200)),
    ("gmt", Abbrev::Tz(0)),
    ("gyt", Abbrev::Dyn("America/Guyana")),
    ("hkt", Abbrev::Tz(28800)),
    ("hst", Abbrev::Tz(-36000)),
    ("ict", Abbrev::Tz(25200)),
    ("idt", Abbrev::Dtz(10800)),
    ("iot", Abbrev::Dyn("Indian/Chagos")),
    ("irkst", Abbrev::Dyn("Asia/Irkutsk")),
    ("irkt", Abbrev::Dyn("Asia/Irkutsk")),
    ("irt", Abbrev::Tz(12600)),
    ("ist", Abbrev::Tz(7200)),
    ("jayt", Abbrev::Tz(32400)),
    ("jst", Abbrev::Tz(32400)),
    ("kdt", Abbrev::Dtz(36000)),
    ("kgst", Abbrev::Dtz(21600)),
    ("kgt", Abbrev::Dyn("Asia/Bishkek")),
    ("kost", Abbrev::Dyn("Pacific/Kosrae")),
    ("krast", Abbrev::Dyn("Asia/Krasnoyarsk")),
    ("krat", Abbrev::Dyn("Asia/Krasnoyarsk")),
    ("kst", Abbrev::Tz(32400)),
    ("lhdt", Abbrev::Dyn("Australia/Lord_Howe")),
    ("lhst", Abbrev::Tz(37800)),
    ("ligt", Abbrev::Tz(36000)),
    ("lint", Abbrev::Dyn("Pacific/Kiritimati")),
    ("lkt", Abbrev::Dyn("Asia/Colombo")),
    ("magst", Abbrev::Dyn("Asia/Magadan")),
    ("magt", Abbrev::Dyn("Asia/Magadan")),
    ("mart", Abbrev::Tz(-34200)),
    ("mawt", Abbrev::Dyn("Antarctica/Mawson")),
    ("mdt", Abbrev::Dtz(-21600)),
    ("mest", Abbrev::Dtz(7200)),
    ("mesz", Abbrev::Dtz(7200)),
    ("met", Abbrev::Tz(3600)),
    ("metdst", Abbrev::Dtz(7200)),
    ("mez", Abbrev::Tz(3600)),
    ("mht", Abbrev::Tz(43200)),
    ("mmt", Abbrev::Tz(23400)),
    ("mpt", Abbrev::Tz(36000)),
    ("msd", Abbrev::Dtz(14400)),
    ("msk", Abbrev::Dyn("Europe/Moscow")),
    ("mst", Abbrev::Tz(-25200)),
    ("must", Abbrev::Dtz(18000)),
    ("mut", Abbrev::Tz(14400)),
    ("mvt", Abbrev::Tz(18000)),
    ("myt", Abbrev::Tz(28800)),
    ("ndt", Abbrev::Dtz(-9000)),
    ("nft", Abbrev::Tz(-12600)),
    ("novst", Abbrev::Dyn("Asia/Novosibirsk")),
    ("novt", Abbrev::Dyn("Asia/Novosibirsk")),
    ("npt", Abbrev::Tz(20700)),
    ("nst", Abbrev::Tz(-12600)),
    ("nut", Abbrev::Dyn("Pacific/Niue")),
    ("nzdt", Abbrev::Dtz(46800)),
    ("nzst", Abbrev::Tz(43200)),
    ("nzt", Abbrev::Tz(43200)),
    ("omsst", Abbrev::Dyn("Asia/Omsk")),
    ("omst", Abbrev::Dyn("Asia/Omsk")),
    ("pdt", Abbrev::Dtz(-25200)),
    ("pet", Abbrev::Tz(-18000)),
    ("petst", Abbrev::Dyn("Asia/Kamchatka")),
    ("pett", Abbrev::Dyn("Asia/Kamchatka")),
    ("pgt", Abbrev::Tz(36000)),
    ("pht", Abbrev::Tz(28800)),
    ("pkst", Abbrev::Dtz(21600)),
    ("pkt", Abbrev::Tz(18000)),
    ("pmdt", Abbrev::Dtz(-7200)),
    ("pmst", Abbrev::Tz(-10800)),
    ("pont", Abbrev::Tz(39600)),
    ("pst", Abbrev::Tz(-28800)),
    ("pwt", Abbrev::Tz(32400)),
    ("pyst", Abbrev::Dtz(-10800)),
    ("pyt", Abbrev::Dyn("America/Asuncion")),
    ("ret", Abbrev::Tz(14400)),
    ("sadt", Abbrev::Dtz(37800)),
    ("sast", Abbrev::Tz(7200)),
    ("sct", Abbrev::Tz(14400)),
    ("sgt", Abbrev::Dyn("Asia/Singapore")),
    ("taht", Abbrev::Tz(-36000)),
    ("tft", Abbrev::Tz(18000)),
    ("tjt", Abbrev::Tz(18000)),
    ("tkt", Abbrev::Dyn("Pacific/Fakaofo")),
    ("tmt", Abbrev::Dyn("Asia/Ashgabat")),
    ("tot", Abbrev::Tz(46800)),
    ("trut", Abbrev::Tz(36000)),
    ("tvt", Abbrev::Tz(43200)),
    ("uct", Abbrev::Tz(0)),
    ("ulast", Abbrev::Dtz(32400)),
    ("ulat", Abbrev::Dyn("Asia/Ulaanbaatar")),
    ("ut", Abbrev::Tz(0)),
    ("utc", Abbrev::Tz(0)),
    ("uyst", Abbrev::Dtz(-7200)),
    ("uyt", Abbrev::Tz(-10800)),
    ("uzst", Abbrev::Dtz(21600)),
    ("uzt", Abbrev::Tz(18000)),
    ("vet", Abbrev::Dyn("America/Caracas")),
    ("vlast", Abbrev::Dyn("Asia/Vladivostok")),
    ("vlat", Abbrev::Dyn("Asia/Vladivostok")),
    ("volt", Abbrev::Dyn("Europe/Volgograd")),
    ("vut", Abbrev::Tz(39600)),
    ("wadt", Abbrev::Dtz(28800)),
    ("wakt", Abbrev::Tz(43200)),
    ("wast", Abbrev::Tz(25200)),
    ("wat", Abbrev::Tz(3600)),
    ("wdt", Abbrev::Dtz(32400)),
    ("wet", Abbrev::Tz(0)),
    ("wetdst", Abbrev::Dtz(3600)),
    ("wft", Abbrev::Tz(43200)),
    ("wgst", Abbrev::Dtz(-7200)),
    ("wgt", Abbrev::Tz(-10800)),
    ("xjt", Abbrev::Tz(21600)),
    ("yakst", Abbrev::Dyn("Asia/Yakutsk")),
    ("yakt", Abbrev::Dyn("Asia/Yakutsk")),
    ("yapt", Abbrev::Tz(36000)),
    ("yekst", Abbrev::Dtz(21600)),
    ("yekt", Abbrev::Dyn("Asia/Yekaterinburg")),
    ("z", Abbrev::Tz(0)),
    ("zulu", Abbrev::Tz(0)),
];
