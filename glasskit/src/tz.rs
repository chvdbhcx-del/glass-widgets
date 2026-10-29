//! Civil-calendar arithmetic and a small fixed table of world time zones (no registry, no ICU).
//!
//! Callers: widget.rs (`Now`), widgets/clock.rs, widgets/worldclock.rs. Zones carry a standard offset and a simple DST rule
//! (US, EU, southern-hemisphere AU); good enough for a clock face, not for historical dates.

/// Days since 1970-01-01 of a civil date (proleptic Gregorian). Howard Hinnant's algorithm.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`: (year, month 1..12, day 1..31).
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + if m <= 2 { 1 } else { 0 }) as i32, m, d)
}

/// Weekday of a civil date, 0 = Monday .. 6 = Sunday.
pub fn dow_mon0(y: i32, m: u32, d: u32) -> u32 {
    (days_from_civil(y, m, d) + 3).rem_euclid(7) as u32 // 1970-01-01 was a Thursday (3)
}

pub fn days_in(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// n-th (1-based) given weekday (0 = Monday) of a month, or the last one for n = 0.
fn nth_dow(y: i32, m: u32, dow: u32, n: u32) -> u32 {
    if n == 0 {
        let last = days_in(y, m);
        let back = (dow_mon0(y, m, last) + 7 - dow) % 7;
        last - back
    } else {
        let first = (dow + 7 - dow_mon0(y, m, 1)) % 7 + 1;
        first + 7 * (n - 1)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Dst {
    None,
    /// 2nd Sunday of March 02:00 local .. 1st Sunday of November 02:00 local
    Us,
    /// last Sunday of March 01:00 UTC .. last Sunday of October 01:00 UTC
    Eu,
    /// southern hemisphere (Sydney): 1st Sunday of October 02:00 .. 1st Sunday of April 03:00 local
    Au,
}

#[derive(Clone, Copy, Debug)]
pub struct Zone {
    pub id: &'static str,
    pub ru: &'static str,
    pub en: &'static str,
    /// standard (winter) offset from UTC in minutes
    pub std_min: i32,
    pub dst: Dst,
}

const fn z(id: &'static str, ru: &'static str, en: &'static str, std_min: i32, dst: Dst) -> Zone {
    Zone { id, ru, en, std_min, dst }
}

/// Fixed table, id = lower-case city key used in `cfg` (`{"tz":"tokyo"}`).
pub const ZONES: &[Zone] = &[
    z("utc", "UTC", "UTC", 0, Dst::None),
    z("moscow", "Москва", "Moscow", 180, Dst::None),
    z("kaliningrad", "Калининград", "Kaliningrad", 120, Dst::None),
    z("yekaterinburg", "Екатеринбург", "Yekaterinburg", 300, Dst::None),
    z("novosibirsk", "Новосибирск", "Novosibirsk", 420, Dst::None),
    z("vladivostok", "Владивосток", "Vladivostok", 600, Dst::None),
    z("kyiv", "Киев", "Kyiv", 120, Dst::Eu),
    z("london", "Лондон", "London", 0, Dst::Eu),
    z("paris", "Париж", "Paris", 60, Dst::Eu),
    z("berlin", "Берлин", "Berlin", 60, Dst::Eu),
    z("istanbul", "Стамбул", "Istanbul", 180, Dst::None),
    z("dubai", "Дубай", "Dubai", 240, Dst::None),
    z("delhi", "Дели", "Delhi", 330, Dst::None),
    z("bangkok", "Бангкок", "Bangkok", 420, Dst::None),
    z("shanghai", "Шанхай", "Shanghai", 480, Dst::None),
    z("tokyo", "Токио", "Tokyo", 540, Dst::None),
    z("seoul", "Сеул", "Seoul", 540, Dst::None),
    z("sydney", "Сидней", "Sydney", 600, Dst::Au),
    z("new_york", "Нью-Йорк", "New York", -300, Dst::Us),
    z("chicago", "Чикаго", "Chicago", -360, Dst::Us),
    z("denver", "Денвер", "Denver", -420, Dst::Us),
    z("los_angeles", "Лос-Анджелес", "Los Angeles", -480, Dst::Us),
    z("honolulu", "Гонолулу", "Honolulu", -600, Dst::None),
    z("sao_paulo", "Сан-Паулу", "Sao Paulo", -180, Dst::None),
];

pub fn find(id: &str) -> Option<&'static Zone> {
    ZONES.iter().find(|z| z.id.eq_ignore_ascii_case(id))
}

/// Wall-clock fields of a moment in some zone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Civil {
    pub y: i32,
    pub mo: u32,
    pub d: u32,
    pub h: u32,
    pub mi: u32,
    pub sec: u32,
    pub offset_min: i32,
}

impl Civil {
    pub fn day_number(&self) -> i64 {
        days_from_civil(self.y, self.mo, self.d)
    }
}

pub fn civil_from_unix(secs: i64, offset_min: i32) -> Civil {
    let t = secs + offset_min as i64 * 60;
    let days = t.div_euclid(86400);
    let rem = t.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    Civil { y, mo, d, h: (rem / 3600) as u32, mi: (rem % 3600 / 60) as u32, sec: (rem % 60) as u32, offset_min }
}

impl Zone {
    /// Offset from UTC (minutes) at the UTC instant `utc` (unix seconds).
    pub fn offset_at(&self, utc: i64) -> i32 {
        let std = self.std_min as i64 * 60;
        let year = civil_from_unix(utc, self.std_min).y;
        let at = |m: u32, d: u32, secs: i64| days_from_civil(year, m, d) * 86400 + secs;
        let on = match self.dst {
            Dst::None => false,
            Dst::Us => utc >= at(3, nth_dow(year, 3, 6, 2), 2 * 3600) - std && utc < at(11, nth_dow(year, 11, 6, 1), 2 * 3600) - std - 3600,
            Dst::Eu => utc >= at(3, nth_dow(year, 3, 6, 0), 3600) && utc < at(10, nth_dow(year, 10, 6, 0), 3600),
            Dst::Au => utc >= at(10, nth_dow(year, 10, 6, 1), 2 * 3600) - std || utc < at(4, nth_dow(year, 4, 6, 1), 2 * 3600) - std,
        };
        self.std_min + if on { 60 } else { 0 }
    }
    pub fn at(&self, utc: i64) -> Civil {
        civil_from_unix(utc, self.offset_at(utc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn civil_roundtrip_and_dow() {
        assert_eq!(civil_from_days(days_from_civil(2026, 10, 24)), (2026, 10, 24));
        assert_eq!(dow_mon0(2026, 10, 24), 5); // Saturday
        assert_eq!(dow_mon0(2026, 1, 1), 3); // Thursday
        let t = days_from_civil(2026, 7, 1) * 86400 + 12 * 3600; // 12:00 UTC
        assert_eq!(find("london").unwrap().at(t).h, 13);
        assert_eq!(find("new_york").unwrap().at(t).h, 8);
        assert_eq!(find("moscow").unwrap().at(t).h, 15);
        let w = days_from_civil(2026, 1, 15) * 86400 + 12 * 3600;
        assert_eq!(find("london").unwrap().at(w).h, 12);
        assert_eq!(find("new_york").unwrap().at(w).h, 7);
    }
}
