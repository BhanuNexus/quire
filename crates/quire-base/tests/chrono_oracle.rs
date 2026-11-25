// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The oracle of `quire_base::dates`: chrono 0.4, which Quire used until
//! 0.14 (`docs/design/0.14-jiff.md` §3). People's templates, periodic-note
//! names and date settings hold strftime patterns, so every pattern must
//! give the text chrono gave, refuse what chrono refused, and read back
//! what chrono read. chrono is a dev-dependency for this test only.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "an oracle test: it compares with chrono, which nothing else may use"
)]

use std::fmt::Write as _;

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use quire_base::dates::{self, FormatError, Moment};

fn jdate(d: NaiveDate) -> dates::Date {
    dates::Date::new(i16::try_from(d.year()).unwrap(), i8::try_from(d.month()).unwrap(), i8::try_from(d.day()).unwrap()).unwrap()
}

fn jtime(t: NaiveTime) -> dates::Time {
    dates::Time::new(
        i8::try_from(t.hour()).unwrap(),
        i8::try_from(t.minute()).unwrap(),
        i8::try_from(t.second()).unwrap(),
        i32::try_from(t.nanosecond()).unwrap(),
    )
    .unwrap()
}

/// chrono's answer: text, a bad pattern, or a value it can't write.
fn chrono_text(pattern: &str, f: impl Fn(&mut String, &[Item<'_>]) -> std::fmt::Result) -> Result<String, FormatError> {
    let items: Vec<Item<'_>> = StrftimeItems::new(pattern).collect();
    if items.contains(&Item::Error) {
        return Err(FormatError::BadPattern);
    }
    let mut out = String::new();
    f(&mut out, &items).map_err(|_| FormatError::Unwritable)?;
    Ok(out)
}

fn patterns() -> Vec<String> {
    let mut out = Vec::new();
    let specs: Vec<String> = (0x20u8..0x7f)
        .map(|c| char::from(c).to_string())
        .chain([":z", "::z", ":::z", "::::z", ":x", ".f", ".3f", ".6f", ".9f", ".4f", ".x", "3f", "6f", "9f", "4f", "3x"].map(String::from))
        .collect();
    for flag in ["", "-", "0", "_", "#", "^"] {
        for s in &specs {
            out.push(format!("%{flag}{s}"));
        }
    }
    out.extend(
        [
            "",
            "%",
            "%-",
            "%#",
            "%.",
            "%:",
            "%.3",
            "plain text",
            "  two  spaces ",
            "\ttab\nline",
            "100%% sure",
            "Ünïcödé %A — %B ✓",
            "%Y-%m-%d",
            "%Y-%m-%d %H:%M",
            "%Y-%m-%dT%H:%M:%S%:z",
            "%Y-%m-%dT%H:%M:%SZ",
            "%Y%m%d-%H%M%S",
            "%Y%m%d %H%M%S",
            "%Y-%m-%d %H%M%S",
            "%Y-%m-%d %H.%M",
            "%Y-%m",
            "%Y%m%d",
            "%Y%m%dT%H%M%SZ",
            "%a, %d %b %Y %H:%M:%S GMT",
            "%A %-d %B",
            "%A %-d %B %Y",
            "%a %-d %b",
            "%a %-d %b %Y",
            "%-d %b %Y",
            "%-d %B %Y",
            "%B %Y",
            "%b %-d, %H:%M",
            "%d %b",
            "%a %H:%M",
            "%a %Y-%m-%d",
            "%G-W%V",
            "%Y-W%W",
            "%Y-W%U",
            "%Y-Q%q",
            "%Y-%j",
            "%d/%m/%Y",
            "%m/%d/%Y",
            "%d.%m.%Y",
            "%e %B %Y",
            "%A, %B %e, %Y",
            "%I:%M %p",
            "%l:%M%P",
            "%H:%M:%S%.f",
            "%H:%M:%S%.3f %z",
            "%s",
            "%+",
            "%c",
            "%x %X",
            "%r",
            "%v",
            "%D %T",
            "%#z",
            "%Z %z %:z %::z %:::z",
            "%-Y %_Y %0Y %-C %-y %-G %-g %-j %_j %0e %-e %_d %-H %_H %-k %0k %-l %-I %-M %-S %-f %_f %-s %-q %-u %-w %-U %-W %-V",
            "%-a",
            "%-A",
            "%-%",
            "%_.3f",
        ]
        .map(String::from),
    );
    out
}

fn naive_dates() -> Vec<NaiveDate> {
    let mut v: Vec<NaiveDate> = [
        (2026, 11, 24),
        (2026, 1, 1),
        (2026, 12, 31),
        (2027, 1, 1),
        (2027, 1, 3),
        (2027, 1, 4),
        (2025, 12, 29),
        (2024, 2, 29),
        (2024, 12, 30),
        (2020, 12, 31),
        (2021, 1, 3),
        (2015, 12, 28),
        (2016, 1, 3),
        (1970, 1, 1),
        (1969, 12, 31),
        (2000, 3, 5),
        (2099, 7, 4),
        (1, 1, 1),
        (999, 9, 9),
        (1000, 10, 10),
        (9999, 12, 31),
        (0, 6, 15),
        (-1, 1, 1),
    ]
    .iter()
    .map(|&(y, m, d)| NaiveDate::from_ymd_opt(y, m, d).unwrap())
    .collect();
    // Every day of 2026 for week numbers and weekday names.
    let mut d = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    while d.year() == 2026 {
        v.push(d);
        d = d.succ_opt().unwrap();
    }
    v
}

fn naive_times() -> Vec<NaiveTime> {
    [
        (0, 0, 0, 0),
        (0, 5, 9, 1),
        (1, 2, 3, 26_490_000),
        (9, 30, 0, 120_000),
        (11, 59, 59, 999_999_999),
        (12, 0, 0, 0),
        (13, 7, 8, 500_000_000),
        (23, 59, 59, 0),
    ]
    .iter()
    .map(|&(h, m, s, n)| NaiveTime::from_hms_nano_opt(h, m, s, n).unwrap())
    .collect()
}

const OFFSETS: [i32; 9] = [0, 3600, -14_400, 19_800, 50_400, -43_200, 1845, -3_661, 20_700];

#[test]
fn formatting_matches_chrono() {
    let pats = patterns();
    let ds = naive_dates();
    let ts = naive_times();
    let mut checked = 0usize;
    let mut fails = Vec::new();
    let mut check = |p: &str, ours: Result<String, FormatError>, theirs: Result<String, FormatError>, what: &str| {
        checked += 1;
        if ours != theirs && fails.len() < 40 {
            fails.push(format!("{p:?} on {what}: chrono {theirs:?}, ours {ours:?}"));
        }
    };
    for p in &pats {
        // A sample of dates for every pattern; the whole year for date patterns.
        let few: Vec<NaiveDate> = ds.iter().copied().take(23).chain(ds.iter().copied().skip(23).step_by(11)).collect();
        for &d in &few {
            check(
                p,
                dates::format(p, &Moment::date(jdate(d))),
                chrono_text(p, |o, i| write!(o, "{}", d.format_with_items(i.iter()))),
                &format!("{d}"),
            );
            for &t in &ts {
                let n = d.and_time(t);
                let jn = jdate(d).to_datetime(jtime(t));
                check(
                    p,
                    dates::format(p, &Moment::naive(jn)),
                    chrono_text(p, |o, i| write!(o, "{}", n.format_with_items(i.iter()))),
                    &format!("{n}"),
                );
                for off in OFFSETS {
                    let fixed_off = FixedOffset::east_opt(off).unwrap();
                    let z: DateTime<FixedOffset> = fixed_off.from_local_datetime(&n).unwrap();
                    check(
                        p,
                        dates::format(p, &Moment::at_offset(jn, off)),
                        chrono_text(p, |o, i| write!(o, "{}", z.format_with_items(i.iter()))),
                        &format!("{z}"),
                    );
                }
                let u: DateTime<Utc> = Utc.from_utc_datetime(&n);
                check(
                    p,
                    dates::format(p, &Moment::utc(jn)),
                    chrono_text(p, |o, i| write!(o, "{}", u.format_with_items(i.iter()))),
                    &format!("{u}"),
                );
            }
        }
        for &t in &ts {
            check(
                p,
                dates::format(p, &Moment::time(jtime(t))),
                chrono_text(p, |o, i| write!(o, "{}", t.format_with_items(i.iter()))),
                &format!("{t}"),
            );
        }
        assert_eq!(dates::is_valid(p), !StrftimeItems::new(p).any(|i| i == Item::Error), "{p:?}");
    }
    assert!(fails.is_empty(), "{} of {checked} differ:\n{}", fails.len(), fails.join("\n"));
    assert!(checked > 1_000_000, "{checked}");
}

#[test]
fn rfc2822_matches_chrono() {
    for d in naive_dates() {
        for t in naive_times() {
            for off in OFFSETS {
                let z = FixedOffset::east_opt(off).unwrap().from_local_datetime(&d.and_time(t)).unwrap();
                let theirs = (0..=9999).contains(&d.year()).then(|| z.to_rfc2822());
                assert_eq!(dates::rfc2822(jdate(d).to_datetime(jtime(t)), off), theirs);
            }
        }
    }
}

/// A chrono date and time as ours: a leap second (`:60`) is the last
/// nanosecond of second 59 here.
fn as_ours(n: NaiveDateTime) -> dates::DateTime {
    let nano = n.nanosecond();
    let (sec, nano) = if nano >= 1_000_000_000 { (n.second(), 999_999_999) } else { (n.second(), nano) };
    jdate(n.date()).to_datetime(
        dates::Time::new(
            i8::try_from(n.hour()).unwrap(),
            i8::try_from(n.minute()).unwrap(),
            i8::try_from(sec).unwrap(),
            i32::try_from(nano).unwrap(),
        )
        .unwrap(),
    )
}

/// Inputs to read: what the patterns write, and variants people type.
fn inputs(p: &str) -> Vec<String> {
    let mut v = Vec::new();
    for d in naive_dates().into_iter().step_by(17) {
        for t in naive_times() {
            let z = FixedOffset::east_opt(3600).unwrap().from_local_datetime(&d.and_time(t)).unwrap();
            if let Ok(s) = chrono_text(p, |o, i| write!(o, "{}", z.format_with_items(i.iter()))) {
                v.push(s.to_lowercase());
                v.push(format!(" {s}"));
                v.push(s.replace(" 0", " ").replace("-0", "-"));
                v.push(format!("{s}x"));
                v.push(s);
            }
        }
    }
    v.extend(
        [
            "2026-11-20",
            "2026-1-5",
            " 2026-11-2",
            "2026-11-20T10:00",
            "2026-02-30",
            "+2026-11-20",
            "-0001-01-01",
            "20261120",
            "2026-11-20 10:30",
            "2026-11-20 7:5",
            "2026-11-20T10:30+01:00",
            "2026-11-20T10:30:00+0100",
            "2026-11-20T10:30:00Z",
            "2026-11-20T10:30:59.5",
            "2026-11-20T23:59:60",
            "10:30",
            "7:5",
            "24:00",
            "2026-W47",
            "2026-W47 1",
            "2026-W53 1",
            "2026-11 1",
            "2026 1 1",
            "Tuesday 24 November",
            "tue 24 nov 2026",
            "Tuesday, November 24, 2026",
            "1793577600",
            "",
        ]
        .map(String::from),
    );
    v
}

#[test]
fn parsing_matches_chrono() {
    let mut pats: Vec<String> = patterns().into_iter().filter(|p| dates::is_valid(p) && !p.contains("%+")).collect();
    pats.extend(["%Y-W%V %u", "%G-W%V %u", "%Y-%m %d", "%Y %m %d", "%Y %j", "%Y-W%W %u", "%Y-W%U %w"].map(String::from));
    let mut fails = Vec::new();
    let (mut checked, mut read) = (0usize, 0usize);
    for p in &pats {
        for s in inputs(p) {
            checked += 4;
            let theirs = NaiveDate::parse_from_str(&s, p).ok().map(jdate);
            let ours = dates::parse_date(&s, p).ok();
            read += usize::from(ours.is_some());
            if theirs != ours && fails.len() < 40 {
                fails.push(format!("date {p:?} {s:?}: chrono {theirs:?}, ours {ours:?}"));
            }
            let theirs = NaiveDateTime::parse_from_str(&s, p).ok().map(as_ours);
            let ours = dates::parse_datetime(&s, p).ok();
            if theirs != ours && fails.len() < 40 {
                fails.push(format!("datetime {p:?} {s:?}: chrono {theirs:?}, ours {ours:?}"));
            }
            let theirs = NaiveTime::parse_from_str(&s, p).ok().map(|t| jtime(t.with_nanosecond(t.nanosecond().min(999_999_999)).unwrap()));
            let ours = dates::parse_time_of_day(&s, p).ok();
            if theirs != ours && fails.len() < 40 {
                fails.push(format!("time {p:?} {s:?}: chrono {theirs:?}, ours {ours:?}"));
            }
            let theirs = DateTime::parse_from_str(&s, p).ok().map(|z| (as_ours(z.naive_local()), z.offset().local_minus_utc()));
            let ours = dates::parse_with_offset(&s, p).ok();
            if theirs != ours && fails.len() < 40 {
                fails.push(format!("offset {p:?} {s:?}: chrono {theirs:?}, ours {ours:?}"));
            }
        }
    }
    assert!(fails.is_empty(), "{} of {checked} differ:\n{}", fails.len(), fails.join("\n"));
    assert!(read > 5_000, "only {read} inputs read as dates");
    eprintln!("{checked} parses compared, {read} read as dates");
}

#[test]
fn rfc3339_and_http_dates_read_as_chrono_read_them() {
    for s in [
        "2026-11-20T10:30:00Z",
        "2026-11-20t10:30:00z",
        "2026-11-20 10:30:00+01:00",
        "2026-11-20T10:30:00.123456789-05:30",
        "2026-11-20T10:30:00.5+14:00",
        "2026-11-20T10:30:00+0100",
        "2026-11-20T10:30:00+01",
        "2026-11-20T10:30:00",
        "2026-11-20T10:30Z",
        "2026-11-20T23:59:60Z",
        "2026-02-30T10:30:00Z",
        "2026-11-20T10:30:00+01:60",
        "2026-11-20T10:30:00\u{2212}01:00",
        "2026-11-20T10:30:00+99:00",
        "2026-11-20T10:30:00Z ",
        "1793577600000",
    ] {
        let theirs = DateTime::parse_from_rfc3339(s).ok().map(|z| (as_ours(z.naive_local()), z.offset().local_minus_utc()));
        assert_eq!(dates::parse_rfc3339(s).ok(), theirs, "{s:?}");
    }
    for s in [
        "Tue, 24 Nov 2026 10:52:37 GMT",
        "Tue, 24 Nov 2026 10:52:37 +0200",
        "24 Nov 2026 10:52:37 -0500",
        "Tue, 1 Dec 2026 10:52 GMT",
        "Wed, 24 Nov 2026 10:52:37 GMT",
        "Tue, 24 Nov 26 10:52:37 GMT",
        "Tue, 24 Nov 2026 10:52:37 EST",
        "Tue, 24 Nov 2026 10:52:37 XYZ",
        "  Tue, 24 Nov 2026 10:52:37 GMT  ",
        "Tue, 24 Nov 2026 10:52:37",
        "garbage",
    ] {
        let theirs = DateTime::parse_from_rfc2822(s).ok().map(|z| as_ours(z.naive_utc()));
        assert_eq!(dates::parse_rfc2822(s).ok(), theirs, "{s:?}");
    }
}

/// What Quire writes to files that persist or sync, as each 0.13 call site
/// wrote it with chrono (the call, the file and line at b0db287), against
/// what the 0.14 call site writes with `quire_base::dates`, over instants
/// before and after 1970, with fractions, at several offsets; and read back.
#[test]
fn persisted_formats_match_the_0_13_call_sites() {
    let instants_ms: [i64; 7] = [0, -1_500, 1_793_577_600_000, 1_793_577_600_999, 1_767_225_599_250, 951_782_400_000, 4_102_444_799_999];
    let offsets = [0, 3600, -14_400, 19_800, 50_400];
    for ms in instants_ms {
        let utc = DateTime::from_timestamp_millis(ms).unwrap();
        // agent/src/ledger.rs:104 (and proposal.rs:555, shared.rs:117): month folders in UTC.
        assert_eq!(dates::fmt_utc_ms(ms, "%Y-%m").unwrap(), utc.format("%Y-%m").to_string(), "{ms}");
        // mcp/src/tools/common.rs:86 and import/src/convert.rs:11 (seconds): ISO in UTC.
        assert_eq!(dates::fmt_utc_ms(ms, "%Y-%m-%dT%H:%M:%SZ").unwrap(), utc.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        // sync/src/remote/s3.rs:149: x-amz-date; remote/mock.rs:227: an HTTP Date header.
        assert_eq!(dates::fmt_utc_ms(ms, "%Y%m%dT%H%M%SZ").unwrap(), utc.format("%Y%m%dT%H%M%SZ").to_string());
        assert_eq!(dates::fmt_utc_ms(ms, "%a, %d %b %Y %H:%M:%S GMT").unwrap(), utc.format("%a, %d %b %Y %H:%M:%S GMT").to_string());
        for off in offsets {
            let z = utc.with_timezone(&FixedOffset::east_opt(off).unwrap());
            let ours =
                dates::Timestamp::from_millisecond(ms).unwrap().to_zoned(dates::TimeZone::fixed(dates::Offset::from_seconds(off).unwrap()));
            // plan/src/time.rs:74: time entries.
            let entry = z.format("%Y-%m-%dT%H:%M:%S%:z").to_string();
            assert_eq!(dates::fmt_zoned(&ours, "%Y-%m-%dT%H:%M:%S%:z"), entry);
            assert_eq!(dates::zoned_rfc3339(&entry).map(|b| b.timestamp().as_second()), Some(z.timestamp()), "{entry}");
            // mcp/src/capture.rs:123: `captured:` (RFC 3339, seconds, `+00:00` style).
            assert_eq!(dates::fmt_zoned(&ours, "%Y-%m-%dT%H:%M:%S%:z"), z.to_rfc3339_opts(chrono::SecondsFormat::Secs, false));
            // vault/src/storage.rs:352: trash stamps, local.
            assert_eq!(dates::fmt_zoned(&ours, "%Y%m%d-%H%M%S"), z.format("%Y%m%d-%H%M%S").to_string());
            // sync/src/remote/net.rs:274 (the skew test): RFC 2822.
            assert_eq!(dates::rfc2822(ours.datetime(), off), Some(z.to_rfc2822()));
            // plan/src/reminders.rs:29 and app/src/remind_bg.rs:99: reminder keys and the fired log.
            let naive = z.naive_local();
            let key = naive.format("%Y-%m-%d %H:%M").to_string();
            assert_eq!(dates::fmt_naive(ours.datetime(), "%Y-%m-%d %H:%M"), key);
            assert_eq!(dates::parse_datetime(&key, "%Y-%m-%d %H:%M").map(dates::DateTime::date), Ok(jdate(naive.date())));
            // plan/src/recur.rs:391: UNTIL; the task date fields (plan/src/tasks.rs:46).
            for pattern in ["%Y%m%d", "%Y-%m-%d"] {
                let text = naive.date().format(pattern).to_string();
                assert_eq!(dates::fmt_date(ours.date(), pattern), text);
                assert_eq!(dates::parse_date(&text, pattern), Ok(jdate(naive.date())));
            }
        }
    }
}
