//! Human labels for an instant, and the one rule that makes them honest: a
//! timestamp says which day it belongs to.
//!
//! The transcript used to render every entry with
//! `format("Today %-I:%M %p")`, so a conversation from last week labelled its
//! rows "Today". That is not a cosmetic bug: the label is a claim about *when*
//! something happened, and a reader who believes it mis-reads the whole
//! transcript. [`moment_label`] therefore compares calendar days in the viewer's
//! own zone and only uses a bare time for the day it can actually vouch for.
//!
//! `now` is a parameter rather than a `Utc::now()` read, so the boundaries
//! (midnight, yesterday) are assertable in a test without waiting for one. The
//! zone is `FixedOffset` for the same reason: a test that pinned a wall-clock
//! time but inherited the machine's time zone would pass in Jakarta and fail in
//! UTC, which is the kind of test that teaches nothing.

use chrono::{DateTime, Datelike, FixedOffset, Local, Utc};

/// `Today 5:34 PM`, `Yesterday 9:02 AM`, `Nov 3, 5:34 PM`, or
/// `Nov 3, 2025, 5:34 PM` for another year.
///
/// "Today" and "Yesterday" are calendar days in `now`'s zone, not 24-hour
/// windows: 23:59 and 00:01 two minutes apart are still two different days, and
/// saying "Today" for the first one while the clock has moved on would be wrong
/// in exactly the case a reader cares about.
pub fn moment_label(now: DateTime<FixedOffset>, at: DateTime<Utc>) -> String {
    let at = at.with_timezone(&now.timezone());
    let same_year = at.year() == now.year();
    let day_delta = now.date_naive().num_days_from_ce() - at.date_naive().num_days_from_ce();
    match day_delta {
        0 => format!("Today {}", at.format("%-I:%M %p")),
        1 => format!("Yesterday {}", at.format("%-I:%M %p")),
        _ if same_year => at.format("%b %-d, %-I:%M %p").to_string(),
        _ => at.format("%b %-d, %Y, %-I:%M %p").to_string(),
    }
}

/// [`moment_label`] against this machine's clock, for the render path.
///
/// `Local::now()` is read here and nowhere else, so the view layer has one place
/// that touches the wall clock and the tested function above stays pure.
pub fn moment_label_here(at: DateTime<Utc>) -> String {
    moment_label(Local::now().fixed_offset(), at)
}

/// Just the day, for a row whose time of day is noise (`Nov 3`,
/// `Nov 3, 2025`).
pub fn day_label(now: DateTime<FixedOffset>, at: DateTime<Utc>) -> String {
    let at = at.with_timezone(&now.timezone());
    let same_year = at.year() == now.year();
    let day_delta = now.date_naive().num_days_from_ce() - at.date_naive().num_days_from_ce();
    match day_delta {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        _ if same_year => at.format("%b %-d").to_string(),
        _ => at.format("%b %-d, %Y").to_string(),
    }
}

/// [`day_label`] against this machine's clock.
pub fn day_label_here(at: DateTime<Utc>) -> String {
    day_label(Local::now().fixed_offset(), at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A fixed +07:00 zone, the one this project's owner works in, written out
    /// so the assertions do not depend on the machine's `TZ`.
    fn zone() -> FixedOffset {
        FixedOffset::east_opt(7 * 3600).expect("+07:00 is a valid offset")
    }

    fn now(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<FixedOffset> {
        zone()
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("a real local time")
    }

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("a real UTC time")
    }

    /// The bug this module exists for: yesterday is not "Today".
    #[test]
    fn a_moment_from_yesterday_says_yesterday() {
        // 2026-11-14 12:00 +07:00; the entry is one calendar day earlier.
        let now = now(2026, 11, 14, 12, 0);
        assert_eq!(
            moment_label(now, utc(2026, 11, 13, 3, 0)),
            "Yesterday 10:00 AM"
        );
        // …and a week-old one names its date instead of a weekday-less time.
        assert_eq!(moment_label(now, utc(2026, 11, 7, 3, 0)), "Nov 7, 10:00 AM");
    }

    /// A moment on the same local day keeps the bare time — that is the one case
    /// in which "Today" is true.
    #[test]
    fn a_moment_from_today_keeps_the_time_of_day() {
        let now = now(2026, 11, 14, 12, 0);
        assert_eq!(moment_label(now, utc(2026, 11, 14, 2, 30)), "Today 9:30 AM");
        // Late last night is still today in local time.
        assert_eq!(
            moment_label(now, utc(2026, 11, 13, 17, 30)),
            "Today 12:30 AM"
        );
    }

    /// Two minutes apart across midnight are two different days. A 24-hour
    /// window would call the earlier one "Today" and be wrong.
    #[test]
    fn midnight_is_a_day_boundary_not_a_24_hour_window() {
        let now = now(2026, 11, 14, 0, 1);
        assert_eq!(
            moment_label(now, utc(2026, 11, 13, 16, 59)),
            "Yesterday 11:59 PM"
        );
    }

    /// A different year is named, because "Nov 3" alone is ambiguous once an
    /// app has been open across a new year.
    #[test]
    fn another_year_carries_its_year() {
        let now = now(2026, 1, 2, 9, 0);
        assert_eq!(
            moment_label(now, utc(2025, 11, 3, 3, 0)),
            "Nov 3, 2025, 10:00 AM"
        );
        // A day this year drops the year, so the row stays readable.
        assert_eq!(
            moment_label(now, utc(2026, 1, 1, 3, 0)),
            "Yesterday 10:00 AM"
        );
    }

    #[test]
    fn the_day_label_has_no_clock_in_it() {
        let now = now(2026, 11, 14, 12, 0);
        assert_eq!(day_label(now, utc(2026, 11, 14, 1, 0)), "Today");
        assert_eq!(day_label(now, utc(2026, 11, 13, 1, 0)), "Yesterday");
        assert_eq!(day_label(now, utc(2026, 3, 2, 1, 0)), "Mar 2");
        assert_eq!(day_label(now, utc(2024, 3, 2, 1, 0)), "Mar 2, 2024");
    }
}
