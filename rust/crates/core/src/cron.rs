//! Cron expressions for the cleanup schedule: the five fields of crontab(5) (minute, hour, day of
//! month, month, day of week) in the computer's local time, read as Vixie cron reads them: `*`,
//! numbers, ranges `a-b`, steps `*/n` and `a-b/n`, lists, month and day names (jan, mon), 0 or 7 for
//! Sunday, and when both day fields are restricted a day matching either one. Also @hourly, @daily
//! (@midnight), @weekly, @monthly and @yearly (@annually).
//!
//! The schedule is a catch-up one: a cleanup is due when a scheduled time has passed since the last
//! one finished, so a computer that was asleep at 03:00 runs it when the curator is next idle.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone};

/// How far `prev` and `next` look: a 29 February schedule fires at least once in 8 years.
const HORIZON_DAYS: i64 = 366 * 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    text: String,
    minutes: u64,
    hours: u32,
    /// Bits 1..=31.
    days: u32,
    /// Bits 1..=12.
    months: u16,
    /// Bits 0..=6, Sunday first.
    weekdays: u8,
    /// A day field written as `*...`: with both restricted, a day matching either one fires.
    any_day: bool,
    any_weekday: bool,
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// One field's values as a bit set, or why it cannot be read.
fn field(text: &str, name: &str, lo: u32, hi: u32, names: &[&str]) -> Result<u64, String> {
    let value = |s: &str| -> Result<u32, String> {
        let l = s.to_ascii_lowercase();
        if let Some(i) = names.iter().position(|n| *n == l) {
            // Month names start at 1 (jan); weekday names at 0 (sun).
            return Ok(i as u32 + lo);
        }
        let n: u32 = s.parse().map_err(|_| format!("{name}: \"{s}\" is not a number"))?;
        if n < lo || n > hi {
            return Err(format!("{name}: {n} is outside {lo}-{hi}"));
        }
        Ok(n)
    };
    let mut bits = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((r, s)) => (r, Some(s.parse::<u32>().ok().filter(|&n| n > 0).ok_or_else(|| format!("{name}: \"{s}\" is not a step"))?)),
            None => (item, None),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let a = value(range)?;
            // "5/15" means from 5 to the end, every 15.
            (a, if step.is_some() { hi } else { a })
        };
        if a > b {
            return Err(format!("{name}: the range {range} runs backwards"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step.unwrap_or(1);
        }
    }
    Ok(bits)
}

impl Cron {
    pub fn parse(text: &str) -> Result<Cron, String> {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let expanded = match text.to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            s if s.starts_with('@') => return Err(format!("{text} is not a cron shortcut (@hourly, @daily, @weekly, @monthly, @yearly)")),
            _ => &text,
        };
        let f: Vec<&str> = expanded.split(' ').collect();
        if f.len() != 5 {
            return Err(format!("a cron schedule has five fields (minute hour day month weekday), not {}", f.len()));
        }
        let weekdays = field(f[4], "weekday", 0, 7, &WEEKDAYS)?;
        Ok(Cron {
            minutes: field(f[0], "minute", 0, 59, &[])?,
            hours: field(f[1], "hour", 0, 23, &[])? as u32,
            days: field(f[2], "day", 1, 31, &[])? as u32,
            months: field(f[3], "month", 1, 12, &MONTHS)? as u16,
            // 7 is Sunday too.
            weekdays: ((weekdays | weekdays >> 7) & 0x7f) as u8,
            any_day: f[2].starts_with('*'),
            any_weekday: f[4].starts_with('*'),
            text,
        })
    }

    /// The expression as written (whitespace collapsed).
    pub fn as_str(&self) -> &str {
        &self.text
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        let day = self.days & (1 << d.day()) != 0;
        let weekday = self.weekdays & (1 << d.weekday().num_days_from_sunday()) != 0;
        self.months & (1 << d.month()) != 0
            && match (self.any_day, self.any_weekday) {
                (true, true) => true,
                (true, false) => weekday,
                (false, true) => day,
                (false, false) => day || weekday,
            }
    }

    /// The day's scheduled times, earliest first.
    fn times(&self) -> impl DoubleEndedIterator<Item = NaiveTime> + '_ {
        (0..24u32).filter(|h| self.hours & (1 << h) != 0).flat_map(move |h| (0..60u32).filter(|m| self.minutes & (1 << m) != 0).filter_map(move |m| NaiveTime::from_hms_opt(h, m, 0)))
    }

    /// A local time in `tz`; one that a clock change skips runs at the same time an hour later (as
    /// cron does), and one that happens twice runs the first time.
    fn resolve<Tz: TimeZone>(tz: &Tz, t: NaiveDateTime) -> Option<DateTime<Tz>> {
        tz.from_local_datetime(&t).earliest().or_else(|| tz.from_local_datetime(&(t + TimeDelta::hours(1))).earliest())
    }

    /// The latest scheduled time at or before `at`.
    pub fn prev<Tz: TimeZone>(&self, at: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = at.timezone();
        let mut day = at.naive_local().date();
        for _ in 0..HORIZON_DAYS {
            if self.day_matches(day) {
                for t in self.times().rev() {
                    if let Some(dt) = Self::resolve(&tz, day.and_time(t)).filter(|dt| dt <= at) {
                        return Some(dt);
                    }
                }
            }
            day = day.pred_opt()?;
        }
        None
    }

    /// The first scheduled time after `at`.
    pub fn next<Tz: TimeZone>(&self, at: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = at.timezone();
        let mut day = at.naive_local().date();
        for _ in 0..HORIZON_DAYS {
            if self.day_matches(day) {
                for t in self.times() {
                    if let Some(dt) = Self::resolve(&tz, day.and_time(t)).filter(|dt| dt > at) {
                        return Some(dt);
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }

    /// In words, for the window: "Daily at 03:00", "Mondays at 03:00", "Every hour at :15", or the
    /// expression itself when it is not one of the common shapes.
    pub fn describe(&self) -> String {
        let one = |bits: u64| (bits.count_ones() == 1).then(|| bits.trailing_zeros());
        let all_days = self.any_day && self.any_weekday && self.months == 0x1ffe;
        let at = match (one(self.minutes), one(u64::from(self.hours))) {
            (Some(m), Some(h)) => Some(format!("{h:02}:{m:02}")),
            _ => None,
        };
        if let (Some(m), true, true) = (one(self.minutes), self.hours == 0xff_ffff, all_days) {
            return format!("Every hour at :{m:02}");
        }
        let Some(at) = at else { return format!("Custom: {}", self.text) };
        if all_days {
            return format!("Daily at {at}");
        }
        if self.any_day && self.months == 0x1ffe {
            const NAMES: [&str; 7] = ["Sundays", "Mondays", "Tuesdays", "Wednesdays", "Thursdays", "Fridays", "Saturdays"];
            if self.weekdays == 0b0011_1110 {
                return format!("Weekdays at {at}");
            }
            let days: Vec<&str> = (0..7).filter(|d| self.weekdays & (1 << d) != 0).map(|d| NAMES[d]).collect();
            let list = match days.as_slice() {
                [d] => d.to_string(),
                [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
                [] => return format!("Custom: {}", self.text),
            };
            return format!("{list} at {at}");
        }
        if self.any_weekday
            && self.months == 0x1ffe
            && let Some(d) = one(u64::from(self.days))
        {
            return format!("Monthly on day {d} at {at}");
        }
        format!("Custom: {}", self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn at(s: &str) -> DateTime<Utc> {
        Utc.from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()).unwrap()
    }
    fn fmt(d: Option<DateTime<Utc>>) -> String {
        d.map(|d| d.format("%Y-%m-%d %H:%M %a").to_string()).unwrap_or_default()
    }

    #[test]
    fn previous_and_next_times() {
        let c = Cron::parse("0 3 * * *").unwrap();
        // 2026-10-06 is a Tuesday.
        assert_eq!(fmt(c.prev(&at("2026-10-06 02:59"))), "2026-10-05 03:00 Mon");
        assert_eq!(fmt(c.prev(&at("2026-10-06 03:00"))), "2026-10-06 03:00 Tue");
        assert_eq!(fmt(c.next(&at("2026-10-06 03:00"))), "2026-10-07 03:00 Wed");
        let c = Cron::parse("30 9 * * mon-fri").unwrap();
        assert_eq!(fmt(c.prev(&at("2026-10-05 08:00"))), "2026-10-02 09:30 Fri");
        assert_eq!(fmt(c.next(&at("2026-10-09 10:00"))), "2026-10-12 09:30 Mon");
        let c = Cron::parse("*/15 8-17/3 * * *").unwrap();
        assert_eq!(fmt(c.next(&at("2026-10-06 17:50"))), "2026-10-07 08:00 Wed");
        assert_eq!(fmt(c.prev(&at("2026-10-06 12:00"))), "2026-10-06 11:45 Tue");
        // Both day fields restricted: either one (the 1st, or any Friday).
        let c = Cron::parse("0 0 1 * fri").unwrap();
        assert_eq!(fmt(c.next(&at("2026-10-02 01:00"))), "2026-10-09 00:00 Fri");
        assert_eq!(fmt(c.next(&at("2026-10-30 01:00"))), "2026-11-01 00:00 Sun");
        assert_eq!(fmt(Cron::parse("0 0 29 feb *").unwrap().next(&at("2026-10-06 00:00"))), "2028-02-29 00:00 Tue");
        assert_eq!(Cron::parse("0 0 31 2 *").unwrap().next(&at("2026-10-06 00:00")), None);
        assert_eq!(fmt(Cron::parse("@weekly").unwrap().prev(&at("2026-10-06 00:00"))), "2026-10-04 00:00 Sun");
        assert_eq!(fmt(Cron::parse("0 0 * * 7").unwrap().prev(&at("2026-10-06 00:00"))), "2026-10-04 00:00 Sun");
    }

    #[test]
    fn mistakes_are_named() {
        for (bad, why) in [
            ("0 3 * *", "five fields"),
            ("60 * * * *", "minute: 60 is outside 0-59"),
            ("0 24 * * *", "hour: 24"),
            ("0 0 0 * *", "day: 0"),
            ("0 0 * 13 *", "month: 13"),
            ("0 0 * * 8", "weekday: 8"),
            ("0 0 * * fri-mon", "runs backwards"),
            ("*/0 * * * *", "not a step"),
            ("x * * * *", "not a number"),
            ("@often", "not a cron shortcut"),
        ] {
            let e = Cron::parse(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn described_in_words() {
        for (expr, words) in [
            ("0 3 * * *", "Daily at 03:00"),
            ("@daily", "Daily at 00:00"),
            ("15 * * * *", "Every hour at :15"),
            ("0 3 * * 1", "Mondays at 03:00"),
            ("0 3 * * MON,wed,5", "Mondays, Wednesdays and Fridays at 03:00"),
            ("0 18 * * 1-5", "Weekdays at 18:00"),
            ("0 3 1 * *", "Monthly on day 1 at 03:00"),
            ("0 */6 * * *", "Custom: 0 */6 * * *"),
            ("0  3 * jan *", "Custom: 0 3 * jan *"),
        ] {
            assert_eq!(Cron::parse(expr).unwrap().describe(), words, "{expr}");
        }
    }
}
