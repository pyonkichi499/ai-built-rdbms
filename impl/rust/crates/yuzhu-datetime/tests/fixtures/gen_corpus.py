#!/usr/bin/env python3
"""Generate the PostgreSQL 17 differential corpus for yuzhu-datetime.

Usage:
    python3 gen_corpus.py <postgres-17-container-name>

Builds a deterministic list of cases (input parsing, output styles, typmods,
arithmetic, conversions, extract/date_part/date_trunc, AT TIME ZONE) under
several TimeZone / DateStyle / IntervalStyle settings, evaluates every case
in the given PostgreSQL container and writes `pg17_corpus.tsv` next to this
script. Each line is:

    kind <TAB> TimeZone <TAB> DateStyle <TAB> IntervalStyle <TAB> a <TAB> b <TAB> expected

where `expected` is PostgreSQL's text output, `NULL`, or
`ERROR <sqlstate> <message>`.

Inputs only mention zones that are present in `zoneinfo/` (copied from the
same container), so the Rust test can use that tree.
"""

import random
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
rng = random.Random(20261004)

TEMPLATES = {
    "date_in": "SELECT %1$L::date::text",
    "ts_in": "SELECT %1$L::timestamp::text",
    "tstz_in": "SELECT %1$L::timestamptz::text",
    "iv_in": "SELECT %1$L::interval::text",
    "ts_typmod": "SELECT %1$L::timestamp(%2$s)::text",
    "tstz_typmod": "SELECT %1$L::timestamptz(%2$s)::text",
    "iv_typmod": "SELECT %1$L::interval%2$s::text",
    "ts_pl_iv": "SELECT (%1$L::timestamp + %2$L::interval)::text",
    "ts_mi_iv": "SELECT (%1$L::timestamp - %2$L::interval)::text",
    "tstz_pl_iv": "SELECT (%1$L::timestamptz + %2$L::interval)::text",
    "tstz_mi_iv": "SELECT (%1$L::timestamptz - %2$L::interval)::text",
    "ts_mi_ts": "SELECT (%1$L::timestamp - %2$L::timestamp)::text",
    "tstz_mi_tstz": "SELECT (%1$L::timestamptz - %2$L::timestamptz)::text",
    "date_pl_int": "SELECT (%1$L::date + %2$s)::text",
    "date_mi_int": "SELECT (%1$L::date - %2$s)::text",
    "date_mi_date": "SELECT (%1$L::date - %2$L::date)::text",
    "date_pl_iv": "SELECT (%1$L::date + %2$L::interval)::text",
    "date_mi_iv": "SELECT (%1$L::date - %2$L::interval)::text",
    "iv_pl_iv": "SELECT (%1$L::interval + %2$L::interval)::text",
    "iv_mi_iv": "SELECT (%1$L::interval - %2$L::interval)::text",
    "iv_mul": "SELECT (%1$L::interval * %2$s::float8)::text",
    "iv_div": "SELECT (%1$L::interval / %2$s::float8)::text",
    "iv_neg": "SELECT (- %1$L::interval)::text",
    "iv_cmp": "SELECT CASE WHEN %1$L::interval < %2$L::interval THEN -1 "
    "WHEN %1$L::interval = %2$L::interval THEN 0 ELSE 1 END::text",
    "justify_hours": "SELECT justify_hours(%1$L::interval)::text",
    "justify_days": "SELECT justify_days(%1$L::interval)::text",
    "justify_interval": "SELECT justify_interval(%1$L::interval)::text",
    "ts_to_tstz": "SELECT %1$L::timestamp::timestamptz::text",
    "tstz_to_ts": "SELECT %1$L::timestamptz::timestamp::text",
    "date_to_tstz": "SELECT %1$L::date::timestamptz::text",
    "tstz_to_date": "SELECT %1$L::timestamptz::date::text",
    "ts_to_date": "SELECT %1$L::timestamp::date::text",
    "date_to_ts": "SELECT %1$L::date::timestamp::text",
    "extract_ts": "SELECT extract(%2$L from %1$L::timestamp)::text",
    "extract_tstz": "SELECT extract(%2$L from %1$L::timestamptz)::text",
    "extract_iv": "SELECT extract(%2$L from %1$L::interval)::text",
    "extract_date": "SELECT extract(%2$L from %1$L::date)::text",
    "part_ts": "SELECT date_part(%2$L, %1$L::timestamp)::text",
    "part_tstz": "SELECT date_part(%2$L, %1$L::timestamptz)::text",
    "part_iv": "SELECT date_part(%2$L, %1$L::interval)::text",
    "part_date": "SELECT date_part(%2$L, %1$L::date)::text",
    "trunc_ts": "SELECT date_trunc(%2$L, %1$L::timestamp)::text",
    "trunc_tstz": "SELECT date_trunc(%2$L, %1$L::timestamptz)::text",
    "trunc_iv": "SELECT date_trunc(%2$L, %1$L::interval)::text",
    "ts_at_tz": "SELECT (%1$L::timestamp AT TIME ZONE %2$L)::text",
    "tstz_at_tz": "SELECT (%1$L::timestamptz AT TIME ZONE %2$L)::text",
    "set_tz": "SELECT set_config('TimeZone', %1$L, false) || ' ' || "
    "'2024-07-01 12:00:00+00'::timestamptz::text",
    "tstz_roundtrip": "SELECT %1$L::timestamptz::text::timestamptz::text",
    "ts_roundtrip": "SELECT %1$L::timestamp::text::timestamp::text",
    "date_roundtrip": "SELECT %1$L::date::text::date::text",
    "iv_roundtrip": "SELECT %1$L::interval::text::interval::text",
}

ZONES = [
    "UTC",
    "Asia/Tokyo",
    "America/New_York",
    "Australia/Sydney",
    "Europe/London",
    "+05:30",
    "-3.5",
    "EST5EDT",
]
MAIN_ZONES = ["UTC", "Asia/Tokyo", "America/New_York"]
DATESTYLES = ["ISO, MDY", "ISO, DMY", "ISO, YMD", "Postgres, MDY", "Postgres, DMY",
              "SQL, MDY", "SQL, DMY", "German, DMY"]
ISTYLES = ["postgres", "postgres_verbose", "sql_standard", "iso_8601"]

cases = []


def add(kind, a, b="", tz="UTC", ds="ISO, MDY", ist="postgres"):
    for s in (a, b):
        assert "\t" not in s and "\n" not in s and "\\" not in s, s
    cases.append((kind, tz, ds, ist, a, b))


# ---------------------------------------------------------------------------
# Random value generators
# ---------------------------------------------------------------------------

MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct",
          "Nov", "Dec"]
LONG_MONTHS = ["January", "February", "March", "April", "May", "June", "July",
               "August", "September", "October", "November", "December"]
DOW = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]


def dim(y, m):
    leap = y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)
    return [31, 29 if leap else 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][m - 1]


def rand_year():
    r = rng.random()
    if r < 0.5:
        return rng.randint(1900, 2100)
    if r < 0.75:
        return rng.randint(1, 9999)
    if r < 0.85:
        return rng.randint(1583, 1899)
    if r < 0.95:
        return rng.randint(2100, 3000)
    return rng.randint(10000, 294276)


def rand_date():
    y = rand_year()
    m = rng.randint(1, 12)
    d = rng.randint(1, dim(y, m))
    return y, m, d


def rand_frac():
    r = rng.random()
    if r < 0.4:
        return ""
    n = rng.randint(1, 9)
    return "." + "".join(rng.choice("0123456789") for _ in range(n))


def rand_time(sec=True):
    h = rng.randint(0, 23)
    mi = rng.randint(0, 59)
    if not sec:
        return f"{h:02d}:{mi:02d}"
    s = rng.randint(0, 59)
    return f"{h:02d}:{mi:02d}:{s:02d}{rand_frac()}"


TZ_SUFFIXES = ["", "", "", "Z", "+09", "-05", "+05:30", "-03:30", "+0530", "-0800",
               "+12:45", "+05:30:15", " UTC", " GMT", " PST", " PDT", " EST", " EDT",
               " JST", " CET", " CEST", " MSK", " BST", " America/New_York",
               " Asia/Tokyo", " Europe/London", " Australia/Sydney", " Etc/UTC",
               " Europe/Moscow", "+14", "-15:59", " ART", " NZDT", " IST"]


def iso_ts(y, m, d, t, sep=" "):
    return f"{y:04d}-{m:02d}-{d:02d}{sep}{t}"


def rand_timestamp_text(with_tz=True):
    y, m, d = rand_date()
    t = rand_time(sec=rng.random() < 0.8)
    style = rng.randint(0, 12)
    if style <= 5:
        s = iso_ts(y, m, d, t, sep=rng.choice([" ", " ", "T"]))
    elif style == 6:
        s = f"{m}/{d}/{y} {t}"
    elif style == 7:
        s = f"{d}-{MONTHS[m - 1]}-{y} {t}"
    elif style == 8:
        s = f"{LONG_MONTHS[m - 1]} {d}, {y} {t}"
    elif style == 9:
        s = f"{y:04d}{m:02d}{d:02d} {t.replace(':', '')}"
    elif style == 10:
        s = f"{y}-{MONTHS[m - 1].lower()}-{d:02d} {t}"
    elif style == 11:
        s = f"{y:04d}-{m:02d}-{d:02d}"
    else:
        s = f"{MONTHS[m - 1]} {d:02d} {t} {y}"
    if rng.random() < 0.05:
        s += " BC"
    if with_tz:
        s += rng.choice(TZ_SUFFIXES)
    return s


def rand_iso_timestamp():
    y, m, d = rand_date()
    if rng.random() < 0.03:
        return f"{y:04d}-{m:02d}-{d:02d} {rand_time()} BC"
    return iso_ts(y, m, d, rand_time())


def rand_modern_timestamp():
    y = rng.randint(1880, 2100)
    m = rng.randint(1, 12)
    d = rng.randint(1, dim(y, m))
    return iso_ts(y, m, d, rand_time())


def rand_iso_date():
    y, m, d = rand_date()
    if rng.random() < 0.03:
        return f"{y:04d}-{m:02d}-{d:02d} BC"
    return f"{y:04d}-{m:02d}-{d:02d}"


UNITS_WORDS = ["year", "years", "mon", "mons", "month", "months", "day", "days", "hour",
               "hours", "min", "mins", "minute", "minutes", "sec", "secs", "second",
               "seconds", "week", "weeks", "ms", "msec", "milliseconds", "us",
               "microseconds", "decade", "century", "millennium", "y", "h", "m", "s", "d",
               "w", "yr", "hrs", "qtr"]


def rand_interval_text():
    style = rng.randint(0, 9)
    if style <= 3:
        parts = []
        for _ in range(rng.randint(1, 4)):
            v = rng.choice([rng.randint(-100, 100), rng.randint(-10000, 10000),
                            round(rng.uniform(-50, 50), rng.randint(1, 4))])
            parts.append(f"{v} {rng.choice(UNITS_WORDS)}")
        s = " ".join(parts)
        if rng.random() < 0.15:
            s = "@ " + s
        if rng.random() < 0.1:
            s += " ago"
        return s
    if style <= 5:
        days = rng.randint(-400, 400)
        h = rng.randint(0, 99)
        sign = rng.choice(["", "-", "+"])
        return f"{days} days {sign}{h:02d}:{rng.randint(0, 59):02d}:{rng.randint(0, 59):02d}{rand_frac()}"
    if style == 6:
        y = rng.randint(-20, 20)
        mo = rng.randint(-11, 11)
        d = rng.randint(-40, 40)
        return f"{y} years {mo} mons {d} days {rand_time()}"
    if style == 7:
        def comp(v, u):
            return f"{v}{u}" if v else ""
        s = "P" + comp(rng.randint(0, 5), "Y") + comp(rng.randint(0, 13), "M") + \
            comp(rng.randint(0, 40), "D")
        t = comp(rng.randint(0, 30), "H") + comp(rng.randint(0, 70), "M") + \
            comp(round(rng.uniform(0, 70), rng.randint(0, 3)), "S")
        if t:
            s += "T" + t
        return s if s != "P" else "PT0S"
    if style == 8:
        return f"{rng.randint(-5, 5)}-{rng.randint(0, 11)}"
    return f"{rng.choice(['', '-'])}{rng.randint(0, 200)}:{rng.randint(0, 59):02d}:{rng.randint(0, 59):02d}{rand_frac()}"


def rand_arith_interval():
    r = rng.random()
    mo = rng.choice([0, 0, rng.randint(-30, 30), rng.randint(-2000, 2000)])
    d = rng.choice([0, 0, rng.randint(-40, 40), rng.randint(-5000, 5000)])
    h = rng.choice([0, rng.randint(-48, 48)])
    mi = rng.randint(0, 59)
    s = rng.randint(0, 59)
    if r < 0.1:
        return rng.choice(["1 month", "-1 month", "1 day", "24 hours", "1 year",
                           "1 mon -1 day", "-1 day +24:00", "1 day -00:00:01",
                           "0.5 months", "infinity", "-infinity", "1 week"])
    return f"{mo} mons {d} days {h}:{mi:02d}:{s:02d}{rand_frac()[:7]}"


# ---------------------------------------------------------------------------
# Fixed interesting inputs
# ---------------------------------------------------------------------------

FIXED_TS = [
    "2024-01-02 03:04:05", "2024-01-02T03:04:05", "2024-01-02 03:04:05.123456789",
    "2024-01-02 03:04:05.5", "2024-01-02 03:04:05.0000005", "2024-01-02 03:04:05.9999995",
    "1999-12-31 23:59:59.999999", "1999-12-31 24:00:00", "2024-02-29 23:59:60",
    "2023-02-29", "2024-13-01", "2024-02-30", "2024-00-10", "2024-01-32",
    "January 8, 1999", "1999-Jan-08", "Jan-08-1999", "08-Jan-1999", "99-Jan-08",
    "1/8/1999", "1/18/1999", "18/1/1999", "01/02/03", "1999.008", "2024.366",
    "19990108", "990108", "19990108 040506", "19990108T040506", "20011225T040506.789-07",
    "J2451187", "J2451187.5", "julian 2451187", "1999-01-08 04:05:06 PST",
    "1999-01-08 04:05:06 -8:00", "1999-01-08 04:05:06 America/New_York",
    "Fri Feb 07 15:23:27 1997", "Feb 07 15:23:27 1997 PST", "Feb-7-1997 15:23:27",
    "2-7-1997 15:23:27", "1997-2-7 15:23:27", "1997.038 15:23:27",
    "970207 152327", "97038 152327", "epoch", "infinity", "-infinity", "+infinity",
    "Infinity", "EPOCH", "allballs", "2024-01-01 allballs", "2024-01-01 12:00 AM",
    "2024-01-01 12:00 PM", "2024-01-01 11:59:59 pm", "2024-01-01 13:00 PM",
    "2024-01-01 00:30 am", "0044-03-15 BC", "0001-01-01 BC", "0001-12-31 BC",
    "4713-01-01 BC", "4714-11-24 BC", "4714-11-23 BC", "4714-11-24 00:00:00+00 BC",
    "294276-12-31 23:59:59.999999", "294277-01-01", "294276-12-31 23:59:59.9999999",
    "5874897-12-31", "5874898-01-01", "10000-01-01", "0999-01-01", "0001-01-01",
    "0000-01-01", "99-01-08", "69-01-08", "70-01-08", "0-01-01", "00-01-01",
    "2024-01-01 +16", "2024-01-01 +15:59:59", "2024-01-01 -15:59:59", "2024-01-01 +15:60",
    "2024-01-01 Foo/Bar", "2024-01-01 foo", "garbage", "", "   ", "2024-01-01 25:00",
    "2024-01-01 24:00:01", "2024-01-01 23:60", "2024-01-01 12:00:00 xyz",
    "2024-01-01 12:00 MET DST", "2024-01-01 12:00 CET DST", "2024-01-01 12:00 DST",
    "2024-01-01 12:00 America/New_York DST", "2024-01-01 12:00 ZULU", "2024-01-01 12:00 z",
    "2024-01-01 12:00:00+09:00:30", "2024-01-01 12:00+0930", "2024-01-01 12:00 +9",
    "2024-01-01 12:00 - 05", "2024-01-01 12:00 UTC+3", "2024-01-01 12:00 XYZ+3",
    "2024-01-01 12:00 <+05>-05", "2024-01-01T12:00:00Z", "2024-01-01 T12:00:00",
    "2024-01-01 t 120000", "2024-01-01T1200", "20240101T12", "2024-01-01 on Monday",
    "Monday, 2024-01-01 at 12:00", "2024-01-01 12:00 ad", "12:00 2024-01-01",
    "2024-01-01 12", "2024 01 01", "2024/01/01", "2024.01.01", "01.02.2024",
    "1.2.3", "2024-1-1 1:2:3", "2024-01-01 1:2", "2024-01-01 12:34.5", "2024-01-01 12:",
    "2024-01-01 12::30", "2024-01-01 12:30:15.", "2024-01-01 .5",
    "Jan 1 2024", "1 Jan 2024", "2024 Jan 1", "Jan 2024 1", "january 2024",
    "2024-01-01 12:00 JST", "2024-07-01 12:00 BST", "2024-07-01 12:00 MSK",
    "2010-07-01 12:00 MSK", "1990-07-01 12:00 MSK", "2024-07-01 12:00 ART",
    "1950-07-01 12:00 ART", "2024-07-01 12:00 europe/moscow", "2024-07-01 12:00 ASIA/TOKYO",
    "2024-07-01 12:00 Asia/Tokyo/x", "2024-07-01 12:00 Etc/../UTC", "2024-07-01 12:00 utc",
    "1883-11-18 12:03:57 America/New_York", "1883-11-18 12:03:58 America/New_York",
    "1918-03-31 02:30 America/New_York", "1945-08-14 19:00 America/New_York",
    "1948-05-02 00:30 Asia/Tokyo", "1951-09-08 23:30 Asia/Tokyo",
    "2024-03-10 02:30 America/New_York", "2024-11-03 01:30 America/New_York",
    "2100-03-14 02:30 America/New_York", "2100-11-07 01:30 America/New_York",
    "2500-06-01 12:00 America/New_York", "30000-06-01 12:00 America/New_York",
    "2024-10-06 02:30 Australia/Sydney", "2024-04-07 02:30 Australia/Sydney",
    "2024-03-31 01:30 Europe/London", "2024-10-27 01:30 Europe/London",
    "1000-01-01 00:00 Asia/Tokyo", "1800-01-01 00:00 Europe/London",
    "2024-06-01 12:00 Europe/Dublin", "2024-01-01 12:00 Europe/Dublin",
    "2024-06-01 12:00 Pacific/Chatham", "2011-12-30 12:00 Pacific/Apia",
    "2011-12-29 12:00 Pacific/Apia", "2024-04-01 12:00 Africa/Casablanca",
    "2024-06-01 12:00 Antarctica/Troll", "2024-06-01 12:00 America/St_Johns",
    "2024-06-01 12:00 Asia/Kathmandu", "1985-01-01 12:00 Asia/Kathmandu",
    "2024-06-01 12:00 Australia/Lord_Howe", "2024-12-01 12:00 Australia/Lord_Howe",
    "2024-01-01 12:00:00.123456789123456789", "2024-01-01 12:00:00.9999999999",
    "2024-01-01 12:00:00 +01:00 +02:00", "2024-01-01 2024-01-02", "2024-01-01 12:00 12:00",
    "2024-01-01 BC BC", "Jan Feb 2024", "2024-01-01 yesterday", "123456789012345678901234567890",
    "2024-01-01 12:00 -00:00", "2024-01-01 12:00 +00", "2147483648-01-01",
    "2024-01-01 4294967296:00", "99999999999-01-01", "2024-01-99999999999",
    "J0", "J-1", "J2147483647", "J5373484", "J5373483", "julian -1", "J1.5 12:00",
    "2024-01-01 12:00 j", "2024-01-01 at", "on", "2024y01m01d", "y2024m01d01",
    "2024-01-01 12:00:00 ago", "2024-01-01 12h", "2024-01-01 h12", "doy 2024",
    "2024-01-01 isodow", "2024-366", "2023.366", "2024.000", "2024.367",
    "1-2-3", "11-12-13", "100-12-13", "13-12-11", "31-12-99", "12-31-99", "2024-12-31",
    "12/31/2024", "31/12/2024", "2024/12/31", "12.31.2024", "31.12.2024",
    "8 January 99", "January 8 99", "99 January 8", "8-January-1999", "jan-8-99",
    "99-jan-8", "8-jan-99", "Jan 8 99 BC", "sept 3 2024", "septe 3 2024",
    "september3 2024", "3september2024", "2024-09-03 tuesday", "thurs 2024-09-05",
]

FIXED_IV = [
    "1 day", "1 year 2 mons 3 days 04:05:06.7", "-1 day", "1.5 years", "1.5 months",
    "0.5 days", "1 week", "100000 hours", "@ 1 minute ago", "@ 1 day 2 hours ago",
    "1 2:03:04", "1-2", "-1-2", "-1-2 +3 -4:05:06", "P1Y2M3DT4H5M6S", "P0002-06-07T01:30:00",
    "P00020607T013000", "PT1.5S", "P1W", "PT36H", "P1.5Y", "P1.5M", "P1.5W", "P-1D",
    "P1Y-2M", "PT-1.5H", "P1D T", "PT", "P", "P1", "P1-2-3", "P1-2-3T4:5:6", "PT4:5:6",
    "PT040506", "P1Y2M3DT", "p1d", "P1DT1H1M1.5S", "P1e2D", "P-inf", "P1e400D",
    "1 millennium", "2 centuries", "3 decades", "5 microseconds", "7 ms", "infinity",
    "-infinity", "+infinity", "infinity ago", "1 day infinity", "1e2 days", "1 day 1 day",
    "10:00:00 1 hour", "+5 hours", "-00:00:01.5", "999999999 years", "2147483647 days",
    "2147483648 days", "178956970 years", "178956971 years", "-178956971 years", "1:2",
    "1:2:3.4567891", "12:34.5", "-12:34.5", "1 +02:03", "1 -02:03:04", "1 day -02:03",
    "2562047788:00:54.775807", "2562047788:00:54.775808", "-2562047788:00:54.775808",
    "9223372036854775807 microseconds", "9223372036854775808 microseconds",
    "153722867280 minutes", "0", "0 days", "00:00:00", "", "garbage", "1 fortnight",
    "1 day ago 2 hours", "1 day 2 hours ago", "ago", "@", "@ 1", "1", "1.5", "-1.5",
    "1 2", "1 2 3", "1 day 2", "2 1 day", "1 hour 30", "1:30 2", "1.5 weeks",
    "0.1 centuries", "0.01 millennium", "1.75 decades", "1.333333333 years",
    "0.000001 seconds", "0.0000005 seconds", "0.0000004 seconds", "1.0000005 seconds",
    "2.5 min", "0.1 hours", "1.5 days 1.5 hours", "-0.5 months", "-1.5 days",
    "1 mon 1.5 days", "1 y 2 mon 3 d 4 h 5 m 6 s", "1 yr 2 mons", "3 hrs 4 mins",
    "5 secs 6 msecs 7 usecs", "1 qtr", "1 timezone", "1 dow", "1 day at", "@ 2 days on",
    "1 Year", "1 YEARS", "1 DAY 01:00", "1-2 3 4:05:06", "1-2 3", "-1-2 -3 -4:05:06",
    "+1-2 +3 +4:05:06", "1 day 25:00:00", "-1 days +25:00:00", "1 mon -1 days",
    "01:02:03.000000001", "1-13", "1-12", "1--2", "10-11-12", "12:60", "12:59:61",
    "12:59:60", "-12:00:00.5", "100:00:00", "1 day 10:00:00 10:00:00", "1 microsecond 1 us",
]


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    container = sys.argv[1]

    ts_inputs = FIXED_TS + [rand_timestamp_text() for _ in range(700)]
    for s in ts_inputs:
        for tz, ds in [("UTC", "ISO, MDY"), ("America/New_York", "ISO, MDY"),
                       ("Asia/Tokyo", "ISO, DMY"), ("Australia/Sydney", "ISO, YMD")]:
            add("ts_in", s, tz=tz, ds=ds)
            add("date_in", s, tz=tz, ds=ds)
        for tz, ds in [("UTC", "ISO, MDY"), ("America/New_York", "ISO, MDY"),
                       ("Asia/Tokyo", "Postgres, DMY"), ("Australia/Sydney", "SQL, YMD"),
                       ("Europe/London", "German, DMY"), ("+05:30", "Postgres, MDY"),
                       ("-3.5", "SQL, DMY"), ("EST5EDT", "ISO, MDY")]:
            add("tstz_in", s, tz=tz, ds=ds)

    # Output styles over a wide range of plain values.
    for _ in range(600):
        s = rand_iso_timestamp()
        ds = rng.choice(DATESTYLES)
        tz = rng.choice(ZONES)
        add("ts_in", s, tz=tz, ds=ds)
        add("tstz_in", s, tz=tz, ds=ds)
        add("date_in", s.split(" ")[0] + (" BC" if s.endswith("BC") else ""), tz=tz, ds=ds)
    for _ in range(600):
        s = rand_modern_timestamp()
        for tz in ZONES:
            add("tstz_in", s, tz=tz, ds=rng.choice(["ISO, MDY", "Postgres, MDY", "SQL, DMY"]))

    # DST boundaries, minute by minute around transitions.
    for zone, days in [
        ("America/New_York", ["2024-03-10", "2024-11-03", "1974-01-06", "2007-03-11",
                              "2006-04-02", "2050-03-13", "2050-11-06"]),
        ("Australia/Sydney", ["2024-04-07", "2024-10-06"]),
        ("Europe/London", ["2024-03-31", "2024-10-27", "1968-10-27", "1971-10-31"]),
        ("Asia/Tokyo", ["1948-05-01", "1949-09-10"]),
    ]:
        for day in days:
            for hh in range(0, 5):
                for mm in (0, 15, 30, 45, 59):
                    s = f"{day} {hh:02d}:{mm:02d}:00"
                    add("tstz_in", s, tz=zone)
                    add("ts_to_tstz", s, tz=zone)
                    add("tstz_in", s + "+00", tz=zone)

    # Intervals.
    iv_inputs = FIXED_IV + [rand_interval_text() for _ in range(900)]
    for s in iv_inputs:
        for ist in ISTYLES:
            add("iv_in", s, ist=ist)

    # Typmods.
    for _ in range(250):
        s = rand_iso_timestamp()
        p = rng.randint(0, 6)
        add("ts_typmod", s, str(p))
        add("tstz_typmod", s, str(p), tz=rng.choice(MAIN_ZONES))
    for s in ["2024-01-01 00:00:00.5", "2024-01-01 00:00:00.6", "2024-01-01 00:00:00.4999995",
              "1999-12-31 23:59:59.95", "0001-01-01 00:00:00.5 BC", "infinity",
              "294276-12-31 23:59:59.999999"]:
        for p in range(0, 7):
            add("ts_typmod", s, str(p))
            add("tstz_typmod", s, str(p))
    ranges = ["(0)", "(3)", "(6)", " year", " month", " day", " hour", " minute",
              " second", " year to month", " day to hour", " day to minute",
              " day to second", " hour to minute", " hour to second", " minute to second",
              " second(2)", " day to second(1)", " minute to second(0)"]
    for s in FIXED_IV[:60] + [rand_interval_text() for _ in range(150)] + \
            ["10", "1 2", "1 2:03", "1:30", "1:30.5", "12.5", "2-3", "1 2:03:04.56789"]:
        for r in rng.sample(ranges, 5):
            add("iv_typmod", s, r)
    for s in ["10", "1 2", "1 2:03", "1:30", "1:30.5", "12.5", "2-3", "1 2:03:04.56789"]:
        for r in ranges:
            add("iv_typmod", s, r)

    # Arithmetic.
    month_end = ["2024-01-31 10:00", "2023-01-31 10:00", "2024-02-29 00:00", "2024-03-31 23:59",
                 "2023-12-31 12:00", "0001-01-31 00:00 BC", "2000-02-29 12:00"]
    for _ in range(900):
        a = rng.choice(month_end) if rng.random() < 0.15 else rand_iso_timestamp()
        b = rand_arith_interval()
        add("ts_pl_iv", a, b)
        add("ts_mi_iv", a, b)
        tz = rng.choice(ZONES)
        add("tstz_pl_iv", a, b, tz=tz)
        add("tstz_mi_iv", a, b, tz=tz)
    for _ in range(300):
        a = rand_modern_timestamp()
        b = rng.choice(["1 day", "-1 day", "24 hours", "1 mon", "1 day 1 hour", "12 hours",
                        "6 mons", "-1 mon", "7 days", "1 year"])
        tz = rng.choice(["America/New_York", "Australia/Sydney", "Europe/London",
                         "EST5EDT"])
        add("tstz_pl_iv", a, b, tz=tz)
    for a in ["2024-03-09 12:00", "2024-03-10 01:30", "2024-11-02 01:30", "2024-11-03 00:30",
              "2024-03-09 02:30"]:
        for b in ["1 day", "24 hours", "1 mon", "-1 day", "1 day 1 hour", "23 hours"]:
            add("tstz_pl_iv", a, b, tz="America/New_York")
            add("tstz_mi_iv", a, b, tz="America/New_York")
    for a in ["infinity", "-infinity"]:
        for b in ["infinity", "-infinity", "1 day"]:
            add("ts_pl_iv", a, b)
            add("ts_mi_iv", a, b)
            add("tstz_pl_iv", a, b)
    add("ts_pl_iv", "294276-12-31 00:00", "1 day")
    add("ts_pl_iv", "294276-12-31 00:00", "1 mon")
    add("ts_mi_iv", "4714-11-24 00:00 BC", "1 microsecond")
    add("ts_pl_iv", "2024-01-01", "2147483647 mons")
    add("ts_pl_iv", "2024-01-01", "2147483647 days")

    for _ in range(600):
        a = rand_iso_timestamp()
        b = rand_iso_timestamp()
        add("ts_mi_ts", a, b)
        add("tstz_mi_tstz", a, b, tz=rng.choice(ZONES))
    for a, b in [("infinity", "infinity"), ("infinity", "2024-01-01"), ("-infinity", "2024-01-01"),
                 ("2024-01-01", "infinity"), ("2024-01-01", "-infinity"),
                 ("-infinity", "-infinity"), ("294276-12-31", "4713-01-01 BC"),
                 ("2024-03-11", "2024-03-10")]:
        add("ts_mi_ts", a, b)
        add("tstz_mi_tstz", a, b, tz="America/New_York")

    for _ in range(400):
        d = rand_iso_date()
        n = str(rng.choice([rng.randint(-1000, 1000), rng.randint(-3000000, 3000000)]))
        add("date_pl_int", d, n)
        add("date_mi_int", d, n)
        add("date_mi_date", d, rand_iso_date())
        iv = rand_arith_interval()
        add("date_pl_iv", d, iv)
        add("date_mi_iv", d, iv)
    for d in ["infinity", "-infinity", "5874897-12-31", "4714-11-24 BC"]:
        add("date_pl_int", d, "1")
        add("date_mi_int", d, "1")
        add("date_mi_date", d, "2024-01-01")
        add("date_pl_iv", d, "1 day")
    add("date_pl_int", "2024-01-01", "2147483647")
    add("date_mi_int", "2024-01-01", "-2147483648")

    iv_pool = FIXED_IV[:120] + [rand_interval_text() for _ in range(300)] + \
        [rand_arith_interval() for _ in range(200)]
    for _ in range(700):
        a = rng.choice(iv_pool)
        b = rng.choice(iv_pool)
        add("iv_pl_iv", a, b)
        add("iv_mi_iv", a, b)
        add("iv_cmp", a, b)
        f = rng.choice(["0", "1", "-1", "2", "0.5", "1.5", "-2.5", "3.14159", "1e-7",
                        "1e10", "'Infinity'", "'-Infinity'", "'NaN'", "0.333333333",
                        str(round(rng.uniform(-100, 100), rng.randint(0, 6)))])
        add("iv_mul", a, f)
        add("iv_div", a, f)
        add("iv_neg", a)
        add("justify_hours", a)
        add("justify_days", a)
        add("justify_interval", a)
    for a, b in [("1 day", "24 hours"), ("1 mon", "30 days"), ("36 hours", "1 day 12 hours"),
                 ("1 year", "360 days"), ("infinity", "infinity"), ("-infinity", "1 day"),
                 ("1 day", "infinity")]:
        add("iv_cmp", a, b)

    # Conversions.
    for _ in range(400):
        a = rand_iso_timestamp() if rng.random() < 0.5 else rand_modern_timestamp()
        tz = rng.choice(ZONES)
        add("ts_to_tstz", a, tz=tz)
        add("tstz_to_ts", a, tz=tz)
        add("tstz_to_date", a, tz=tz)
        add("ts_to_date", a, tz=tz)
        d = rand_iso_date()
        add("date_to_tstz", d, tz=tz)
        add("date_to_ts", d, tz=tz)
    for a in ["infinity", "-infinity", "294276-12-31", "4714-11-24 BC", "5874897-12-31",
              "294276-12-31 23:00-03"]:
        for tz in ["UTC", "Asia/Tokyo", "America/New_York"]:
            for k in ["ts_to_tstz", "tstz_to_ts", "tstz_to_date", "date_to_tstz", "date_to_ts",
                      "ts_to_date"]:
                add(k, a, tz=tz)

    # extract / date_part / date_trunc.
    ts_fields = ["microseconds", "milliseconds", "second", "minute", "hour", "day", "month",
                 "quarter", "week", "year", "decade", "century", "millennium", "julian",
                 "isoyear", "dow", "isodow", "doy", "epoch", "timezone", "timezone_hour",
                 "timezone_minute", "Year", "SECONDS", "ms", "us", "y", "h", "j", "jd", "dec",
                 "foo", "jan", "now", "infinity", "msec", "usec", "mil", "c", "qtr", "w",
                 "microsecondsxyz", "timezone_h", "timezone_m"]
    for _ in range(500):
        a = rand_iso_timestamp()
        f = rng.choice(ts_fields)
        add("extract_ts", a, f)
        add("part_ts", a, f)
        tz = rng.choice(ZONES)
        add("extract_tstz", a, f, tz=tz)
        add("part_tstz", a, f, tz=tz)
        d = rand_iso_date()
        add("extract_date", d, f)
        add("part_date", d, f)
    for a in ["infinity", "-infinity", "2024-01-01 12:34:56.789012", "0001-01-01 BC",
              "0010-12-31 BC", "0011-01-01 BC", "2000-12-31", "2001-01-01", "2008-12-29",
              "2010-01-03", "2005-01-01", "1999-12-31 23:59:59.999999"]:
        for f in ts_fields:
            add("extract_ts", a, f)
            add("part_ts", a, f)
            add("extract_tstz", a, f, tz="America/New_York")
            add("part_tstz", a, f, tz="Asia/Tokyo")
            add("extract_date", a.split(" ")[0] + (" BC" if a.endswith("BC") else ""), f)
    iv_fields = ["microseconds", "milliseconds", "second", "minute", "hour", "day", "month",
                 "quarter", "year", "decade", "century", "millennium", "epoch", "week", "dow",
                 "foo", "timezone"]
    for a in iv_pool[:250]:
        f = rng.choice(iv_fields)
        add("extract_iv", a, f)
        add("part_iv", a, f)
    for a in ["infinity", "-infinity", "-1 year -2 mons -3 days -04:05:06.789",
              "1 year 2 mons 3 days 04:05:06.789", "1234 years 5 mons"]:
        for f in iv_fields:
            add("extract_iv", a, f)
            add("part_iv", a, f)
    trunc_fields = ["microseconds", "milliseconds", "second", "minute", "hour", "day", "week",
                    "month", "quarter", "year", "decade", "century", "millennium", "dow",
                    "epoch", "foo", "timezone", "Hour", "mons"]
    for _ in range(400):
        a = rand_iso_timestamp()
        f = rng.choice(trunc_fields)
        add("trunc_ts", a, f)
        add("trunc_tstz", a, f, tz=rng.choice(ZONES))
        add("trunc_iv", rng.choice(iv_pool), f)
    for a in ["2024-03-10 12:34:56.789", "2024-11-03 12:34:56.789", "0001-06-15 BC",
              "0011-06-15 BC", "0101-06-15 BC", "1001-06-15 BC", "infinity",
              "2021-01-01 01:00", "2027-01-01 01:00", "2026-12-31 12:00"]:
        for f in trunc_fields:
            add("trunc_ts", a, f)
            add("trunc_tstz", a, f, tz="America/New_York")
            add("trunc_tstz", a, f, tz="Australia/Sydney")

    # AT TIME ZONE.
    at_zones = ["UTC", "utc", "JST", "PST", "PDT", "MSK", "Asia/Tokyo", "America/New_York",
                "europe/london", "Foo/Bar", "+05", "-03:30", "UTC+3", "EST5EDT", "ART", "GMT"]
    for _ in range(300):
        a = rand_modern_timestamp()
        z = rng.choice(at_zones)
        add("ts_at_tz", a, z, tz=rng.choice(MAIN_ZONES))
        add("tstz_at_tz", a, z, tz=rng.choice(MAIN_ZONES))
    for z in at_zones:
        add("ts_at_tz", "2024-03-10 02:30", z, tz="America/New_York")
        add("tstz_at_tz", "2024-11-03 01:30-04", z)
        add("ts_at_tz", "infinity", z)

    # TimeZone settings.
    for z in ["UTC", "utc", "Asia/Tokyo", "asia/tokyo", "America/New_York", "+9", "-3.5", "9.25",
              "+05:30", "UTC+3", "<+03>-03", "EST5EDT", "PST8PDT", "GMT", "Etc/UTC", "Z",
              "Foo/Bar", "interval '+05:30'", "INTERVAL '-08:00'", "interval '1 day'",
              "1000", "JST-9", "XYZ", "Europe/Dublin", "CET-1CEST,M3.5.0,M10.5.0/3",
              "<-0330>+0330", "EST5EDT,M4.1.0,M10.5.0", "Australia/Lord_Howe", "UTC0"]:
        add("set_tz", z)

    # Output -> input round trips in every style.
    for _ in range(500):
        a = rand_iso_timestamp() if rng.random() < 0.5 else rand_modern_timestamp()
        for ds in rng.sample(DATESTYLES, 3):
            tz = rng.choice(ZONES + ["Australia/Lord_Howe", "Asia/Kathmandu",
                                     "America/St_Johns", "Europe/Dublin"])
            add("tstz_roundtrip", a, tz=tz, ds=ds)
            add("ts_roundtrip", a, tz=tz, ds=ds)
            add("date_roundtrip", a.split(" ")[0] + (" BC" if a.endswith("BC") else ""),
                tz=tz, ds=ds)
    for a in iv_pool[:400]:
        for ist in ISTYLES:
            add("iv_roundtrip", a, ist=ist)

    # Fuzz: random token soup to exercise error paths.
    words = ["jan", "february", "bc", "ad", "pm", "am", "utc", "z", "j", "t", "dst", "est",
             "pdt", "mon", "day", "days", "hour", "ago", "@", "infinity", "-infinity",
             "epoch", "allballs", "at", "on", "y", "m", "d", "h", "s", "mm", "julian",
             "doy", "week", "years", "msk", "jst", "Asia/Tokyo", "america/new_york",
             "foo", "isodow", "sat", "september", "P", "PT", "W", "Y"]
    chars = "0123456789-/.:+ T,_"
    fuzz = []
    for _ in range(2500):
        parts = []
        for _ in range(rng.randint(1, 6)):
            r = rng.random()
            if r < 0.45:
                parts.append(str(rng.choice([rng.randint(0, 99), rng.randint(0, 9999),
                                             rng.randint(0, 99999999)])))
            elif r < 0.7:
                parts.append(rng.choice(words))
            else:
                parts.append("".join(rng.choice(chars) for _ in range(rng.randint(1, 4))))
        sep = rng.choice(["", " ", "-", ":", "/", "."])
        fuzz.append(sep.join(parts).strip())
    for s in fuzz:
        add("ts_in", s, ds=rng.choice(["ISO, MDY", "ISO, DMY", "ISO, YMD"]))
        add("tstz_in", s, tz=rng.choice(MAIN_ZONES))
        add("date_in", s, ds=rng.choice(["ISO, MDY", "ISO, DMY", "ISO, YMD"]))
        add("iv_in", s, ist=rng.choice(ISTYLES))

    write_and_run(container)


def write_and_run(container):
    def esc(s):
        return s.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")

    lines = []
    for i, (kind, tz, ds, ist, a, b) in enumerate(cases):
        lines.append("\t".join([str(i), kind, TEMPLATES[kind], tz, ds, ist, esc(a), esc(b)]))
    script = r"""
\set ON_ERROR_STOP 1
SET client_min_messages = warning;
CREATE OR REPLACE FUNCTION pg_temp.yz_eval(tz text, ds text, ist text, tmpl text, a text, b text)
RETURNS text LANGUAGE plpgsql AS $f$
DECLARE r text;
BEGIN
  PERFORM set_config('TimeZone', tz, false);
  PERFORM set_config('DateStyle', ds, false);
  PERFORM set_config('IntervalStyle', ist, false);
  EXECUTE format(tmpl, a, b) INTO r;
  RETURN coalesce(r, 'NULL');
EXCEPTION WHEN others THEN
  RETURN 'ERROR ' || SQLSTATE || ' ' || SQLERRM;
END
$f$;
CREATE TEMP TABLE cases(id int, kind text, tmpl text, tz text, ds text, ist text, a text, b text);
COPY cases FROM STDIN;
""" + "\n".join(lines) + "\n\\.\n" + r"""
COPY (SELECT id, pg_temp.yz_eval(tz, ds, ist, tmpl, a, b) FROM cases ORDER BY id) TO STDOUT;
"""
    out = subprocess.run(
        ["docker", "exec", "-i", container, "psql", "-U", "postgres", "-X", "-q", "-At"],
        input=script.encode(), capture_output=True, check=True,
    ).stdout.decode()
    results = {}
    for line in out.splitlines():
        if not line:
            continue
        idx, res = line.split("\t", 1)
        results[int(idx)] = res
    assert len(results) == len(cases), (len(results), len(cases))
    with open(HERE / "pg17_corpus.tsv", "w", encoding="utf-8") as f:
        f.write("# Generated by gen_corpus.py against PostgreSQL 17 (tzdata in zoneinfo/).\n")
        for i, (kind, tz, ds, ist, a, b) in enumerate(cases):
            f.write("\t".join([kind, tz, ds, ist, a, b, results[i]]) + "\n")
    print(f"wrote {len(cases)} cases")


if __name__ == "__main__":
    main()
