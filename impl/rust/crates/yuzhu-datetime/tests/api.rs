//! API-level tests that the PostgreSQL corpus cannot cover: the current
//! time (`now`, `today`, ...), zone sources (fault injection, missing
//! tzdata, path safety) and value semantics.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use yuzhu_datetime::{
    Date, DateTimeEnv, FsZoneSource, Interval, IntervalStyle, MemZoneSource, Timestamp,
    TimestampTz, ZoneDb, ZoneSource, parse_timezone_setting, sqlstate,
};

fn zoneinfo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/zoneinfo")
}

fn fixture_db() -> ZoneDb {
    ZoneDb::new(Box::new(FsZoneSource::new(zoneinfo())))
}

#[test]
fn now_and_relative_days_use_transaction_start_in_session_zone() {
    let zones = fixture_db();
    let tokyo = zones.lookup("Asia/Tokyo").unwrap();
    let mut env = DateTimeEnv::new(&tokyo, &zones);
    let utc = zones.lookup("UTC").unwrap();
    let utc_env = DateTimeEnv::new(&utc, &zones);
    // 2024-12-31 20:30:00.25 UTC = 2025-01-01 05:30:00.25 in Tokyo.
    env.now = TimestampTz::parse("2024-12-31 20:30:00.25+00", -1, &utc_env).unwrap();

    let now = TimestampTz::parse("now", -1, &env).unwrap();
    assert_eq!(now, env.now);
    assert_eq!(
        Timestamp::parse("now", -1, &env)
            .unwrap()
            .format(&env)
            .unwrap(),
        "2025-01-01 05:30:00.25"
    );
    assert_eq!(
        Date::parse("today", &env).unwrap().format(&env),
        "2025-01-01"
    );
    assert_eq!(
        Date::parse("tomorrow", &env).unwrap().format(&env),
        "2025-01-02"
    );
    assert_eq!(
        Date::parse("yesterday", &env).unwrap().format(&env),
        "2024-12-31"
    );
    assert_eq!(
        TimestampTz::parse("today", -1, &env)
            .unwrap()
            .format(&env)
            .unwrap(),
        "2025-01-01 00:00:00+09"
    );
    assert_eq!(
        Timestamp::parse("tomorrow 12:00", -1, &env)
            .unwrap()
            .format(&env)
            .unwrap(),
        "2025-01-02 12:00:00"
    );
    assert_eq!(
        Timestamp::parse("now", 0, &env)
            .unwrap()
            .format(&env)
            .unwrap(),
        "2025-01-01 05:30:00"
    );
}

#[test]
fn works_without_tzdata() {
    let zones = ZoneDb::without_tzdata();
    for ok in [
        "UTC",
        "Etc/UTC",
        "GMT",
        "+9",
        "-3.5",
        "JST-9",
        "EST5EDT",
        "interval '+05:30'",
    ] {
        assert!(parse_timezone_setting(&zones, ok).is_ok(), "{ok}");
    }
    let err = parse_timezone_setting(&zones, "Asia/Tokyo").unwrap_err();
    assert_eq!(err.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
    assert_eq!(
        err.message,
        "invalid value for parameter \"TimeZone\": \"Asia/Tokyo\""
    );

    let tz = parse_timezone_setting(&zones, "UTC").unwrap();
    let env = DateTimeEnv::new(&tz, &zones);
    let v = TimestampTz::parse("2024-07-01 12:00 JST", -1, &env).unwrap();
    assert_eq!(v.format(&env).unwrap(), "2024-07-01 03:00:00+00");
    let e = TimestampTz::parse("2024-07-01 12:00 Asia/Tokyo", -1, &env).unwrap_err();
    assert_eq!(e.message, "time zone \"asia/tokyo\" not recognized");
}

#[derive(Debug)]
struct FailingSource;

impl ZoneSource for FailingSource {
    fn read_dir(&self, _: &str) -> io::Result<Vec<String>> {
        Err(io::Error::other("injected I/O failure"))
    }
    fn read_file(&self, _: &str) -> io::Result<Vec<u8>> {
        Err(io::Error::other("injected I/O failure"))
    }
}

#[derive(Debug)]
struct CorruptSource;

impl ZoneSource for CorruptSource {
    fn read_dir(&self, path: &str) -> io::Result<Vec<String>> {
        Ok(match path {
            "" => vec!["Asia".to_owned()],
            _ => vec!["Tokyo".to_owned()],
        })
    }
    fn read_file(&self, _: &str) -> io::Result<Vec<u8>> {
        Ok(b"TZif2 truncated".to_vec())
    }
}

#[test]
fn zone_source_faults_are_reported_as_unknown_zones() {
    for zones in [
        ZoneDb::new(Box::new(FailingSource)),
        ZoneDb::new(Box::new(CorruptSource)),
    ] {
        assert!(zones.lookup("Asia/Tokyo").is_none());
        let err = parse_timezone_setting(&zones, "Asia/Tokyo").unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        // POSIX strings keep working.
        assert!(parse_timezone_setting(&zones, "JST-9").is_ok());
    }
    // An unreadable tree falls back to built-in UTC.
    let zones = ZoneDb::new(Box::new(FailingSource));
    assert_eq!(parse_timezone_setting(&zones, "UTC").unwrap().name(), "UTC");
}

#[test]
fn in_memory_source_and_case_insensitive_lookup() {
    let data = std::fs::read(zoneinfo().join("Asia/Tokyo")).unwrap();
    let mut src = MemZoneSource::new();
    src.insert("Asia/Tokyo", data);
    let zones = ZoneDb::new(Box::new(src));
    let tz = zones.lookup("asia/TOKYO").unwrap();
    assert_eq!(tz.name(), "Asia/Tokyo");
    let env = DateTimeEnv::new(&tz, &zones);
    let v = TimestampTz::parse("1000-01-01 00:00:00+00", -1, &env).unwrap();
    assert_eq!(v.format(&env).unwrap(), "1000-01-01 09:18:59+09:18:59");
}

#[test]
fn zone_names_cannot_escape_the_tree() {
    let zones = fixture_db();
    for bad in [
        "../zoneinfo/UTC",
        "Etc/../UTC",
        "/etc/passwd",
        "./UTC",
        "Asia//Tokyo",
        "Asia/Tokyo/",
        "",
    ] {
        assert!(zones.lookup(bad).is_none(), "{bad}");
    }
    assert!(zones.lookup("etc/utc").is_some());
}

#[test]
fn interval_equality_follows_postgresql() {
    let zones = ZoneDb::without_tzdata();
    let utc = parse_timezone_setting(&zones, "UTC").unwrap();
    let env = DateTimeEnv::new(&utc, &zones);
    let p = |s: &str| Interval::parse(s, -1, &env).unwrap();
    assert_eq!(p("1 day"), p("24 hours"));
    assert_eq!(p("1 mon"), p("30 days"));
    assert!(!p("1 day").fields_eq(&p("24 hours")));
    assert_eq!(p("1 year"), p("360 days"));
    assert!(p("1 year") < p("361 days"));
    let set: HashSet<Interval> = [p("36 hours"), p("1 day 12 hours")].into_iter().collect();
    assert_eq!(set.len(), 1);
    assert_eq!(p("-infinity").format(IntervalStyle::Postgres), "-infinity");
    assert!(Interval::NEG_INFINITY < Interval::ZERO && Interval::ZERO < Interval::INFINITY);
}

#[test]
fn representation_matches_postgresql() {
    let zones = ZoneDb::without_tzdata();
    let utc = parse_timezone_setting(&zones, "UTC").unwrap();
    let env = DateTimeEnv::new(&utc, &zones);
    assert_eq!(Date::parse("2000-01-01", &env).unwrap(), Date(0));
    assert_eq!(Date::parse("1999-12-31", &env).unwrap(), Date(-1));
    assert_eq!(Date::from_ymd(2024, 2, 29).unwrap().to_ymd(), (2024, 2, 29));
    assert!(Date::from_ymd(2023, 2, 29).is_none());
    assert_eq!(
        Timestamp::parse("2000-01-01 00:00:01", -1, &env).unwrap(),
        Timestamp(1_000_000)
    );
    assert_eq!(
        Timestamp::parse("infinity", -1, &env).unwrap(),
        Timestamp(i64::MAX)
    );
    assert_eq!(
        TimestampTz::from_unix_micros(0).format(&env).unwrap(),
        "1970-01-01 00:00:00+00"
    );
    let iv = Interval::parse("1 year 2 mons 3 days 00:00:00.000004", -1, &env).unwrap();
    assert_eq!((iv.months, iv.days, iv.micros), (14, 3, 4));
}
