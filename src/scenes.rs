//! Scenes: layout profiles that switch themselves. config.json `sceneRules` = [{"program": "chrome.exe", "profile": "Work"},
//! {"from": "22:00", "to": "06:00", "profile": "Night"}] (global, not per monitor; the profile is switched on the monitor the desk is on).
//! Priority: a program rule (the foreground program), then a time rule (local time, the first match of each kind in list order), then the
//! user's own profile. Everything here is pure (the host feeds it the foreground program, the minute of the day and the active profile every
//! ~1 s and does the switching): see `Auto::step` for the rules of the game.
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum When {
    /// lower-case exe file name (`chrome.exe`)
    Program(String),
    /// minutes since midnight; `to` < `from` crosses midnight, `to` == `from` never matches
    Time { from: u16, to: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub when: When,
    pub profile: String,
}

/// "chrome", " C:\Apps\Chrome.EXE " -> "chrome.exe": the file name, lower case, `.exe` added when there is no extension. None = nothing usable.
pub fn norm_program(s: &str) -> Option<String> {
    let name = s.trim().trim_matches('"').rsplit(['\\', '/']).next()?.trim().to_lowercase();
    if name.is_empty() || name.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c)) {
        return None;
    }
    Some(if name.contains('.') { name } else { format!("{name}.exe") })
}

/// "7:05" / "07:05" -> minutes since midnight (0..1440).
pub fn parse_hm(s: &str) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u16, u16) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn fmt_hm(m: u16) -> String {
    format!("{:02}:{:02}", m / 60 % 24, m % 60)
}

/// "22:00-06:00" (also "22:00 - 06:00", an en dash or "to") -> (from, to).
pub fn parse_range(s: &str) -> Option<(u16, u16)> {
    let t = s.replace(['\u{2013}', '\u{2014}'], "-");
    let (a, b) = t.split_once('-')?;
    Some((parse_hm(a)?, parse_hm(b)?))
}

/// `now` (minute of the day) inside [from, to): across midnight when `to` < `from`; an empty range (equal ends) never matches.
pub fn time_in(from: u16, to: u16, now: u16) -> bool {
    match from.cmp(&to) {
        std::cmp::Ordering::Equal => false,
        std::cmp::Ordering::Less => from <= now && now < to,
        std::cmp::Ordering::Greater => now >= from || now < to,
    }
}

/// `sceneRules` of a parsed config.json; entries that are not understood are skipped.
pub fn rules_from_value(root: &Value) -> Vec<Rule> {
    let Some(arr) = root.get("sceneRules").and_then(|a| a.as_array()) else { return Vec::new() };
    arr.iter()
        .filter_map(|e| {
            let profile = e.get("profile")?.as_str()?.trim().to_string();
            let s = |k: &str| e.get(k).and_then(|v| v.as_str());
            let when = match (s("program"), s("from"), s("to")) {
                (Some(p), ..) => When::Program(norm_program(p)?),
                (None, Some(f), Some(t)) => When::Time { from: parse_hm(f)?, to: parse_hm(t)? },
                _ => return None,
            };
            (!profile.is_empty()).then_some(Rule { when, profile })
        })
        .collect()
}

pub fn rules_to_value(rules: &[Rule]) -> Value {
    Value::Array(
        rules
            .iter()
            .map(|r| match &r.when {
                When::Program(p) => json!({ "program": p, "profile": r.profile }),
                When::Time { from, to } => json!({ "from": fmt_hm(*from), "to": fmt_hm(*to), "profile": r.profile }),
            })
            .collect(),
    )
}

/// "chrome.exe -> Work" / "22:00-06:00 -> Night": the line of the tray menu.
pub fn describe(r: &Rule) -> String {
    match &r.when {
        When::Program(p) => format!("{p} \u{2192} {}", r.profile),
        When::Time { from, to } => format!("{}\u{2013}{} \u{2192} {}", fmt_hm(*from), fmt_hm(*to), r.profile),
    }
}

/// The rule in force: the first program rule whose exe is in front, else the first time rule that holds now; rules whose profile is gone
/// (`exists` false) are ignored. `fg` None = no program in front.
pub fn winner<'a>(rules: &'a [Rule], fg: Option<&str>, minute: u16, exists: &dyn Fn(&str) -> bool) -> Option<&'a Rule> {
    let live = |r: &&Rule| exists(&r.profile);
    let prog = fg.and_then(|f| rules.iter().filter(live).find(|r| matches!(&r.when, When::Program(p) if p.eq_ignore_ascii_case(f))));
    prog.or_else(|| rules.iter().filter(live).find(|r| matches!(&r.when, When::Time { from, to } if time_in(*from, *to, minute))))
}

/// Evaluations a new winner (or its absence) must hold before it is acted on: alt-tabbing through a window or a click on the taskbar
/// must not rebuild the desk back and forth. At the ~1 s cadence of the host that is about 2 s.
const STABLE: u8 = 2;

#[derive(Clone, Debug, PartialEq)]
enum St {
    /// on the user's own profile (or one a rule happens to share)
    Idle,
    /// a rule's profile is on screen; `base` is what to go back to
    Active { base: String, applied: String },
    /// the user switched by hand while a rule held: auto switching waits until the winner is another one (or none)
    Paused { at: Option<Rule> },
}

#[derive(Debug)]
pub struct Auto {
    st: St,
    /// the winner seen at the last evaluations and for how many in a row; `eff` is the last one that held `STABLE` times
    seen: Option<Rule>,
    n: u8,
    eff: Option<Rule>,
}

impl Default for Auto {
    fn default() -> Self {
        Auto { st: St::Idle, seen: None, n: STABLE, eff: None }
    }
}

impl Auto {
    /// Nothing applied, nothing paused, no candidate: with no rules there is nothing left to evaluate.
    pub fn idle(&self) -> bool {
        self.st == St::Idle && self.eff.is_none()
    }

    /// One evaluation. `win` = `winner(..)` now, `cur` = the active profile of the monitor the desk is on right now (None = no active profile).
    /// Returns the profile to switch to, if any; the host switches and the NEXT call sees it in `cur`.
    /// * a rule takes effect after it held `STABLE` evaluations; the profile that was on screen is remembered as the user's own
    /// * moving from one rule to another keeps that first remembered profile
    /// * when no rule holds any more the remembered profile is restored (if it still exists)
    /// * if `cur` is not what we switched to the user (or a failed switch) changed it: auto switching pauses until the winner changes
    /// * with no active profile there is nothing to restore later, so nothing is started
    pub fn step(&mut self, win: Option<&Rule>, cur: Option<&str>, exists: &dyn Fn(&str) -> bool) -> Option<String> {
        if self.seen.as_ref() == win {
            self.n = self.n.saturating_add(1);
        } else {
            self.seen = win.cloned();
            self.n = 1;
        }
        if self.n >= STABLE {
            self.eff = self.seen.clone();
        }
        if let St::Active { applied, .. } = &self.st {
            if cur != Some(applied.as_str()) {
                self.st = St::Paused { at: self.eff.clone() };
            }
        }
        if let St::Paused { at } = &self.st {
            if *at != self.eff {
                self.st = St::Idle;
            }
        }
        let eff = self.eff.clone();
        match (&mut self.st, eff) {
            (St::Idle, Some(r)) => {
                let cur = cur?;
                if r.profile == cur || !exists(&r.profile) {
                    return None;
                }
                self.st = St::Active { base: cur.to_string(), applied: r.profile.clone() };
                Some(r.profile)
            }
            (St::Active { applied, .. }, Some(r)) if r.profile != *applied && exists(&r.profile) => {
                *applied = r.profile.clone();
                Some(r.profile)
            }
            (St::Active { base, .. }, None) => {
                let base = base.clone();
                self.st = St::Idle;
                (exists(&base) && Some(base.as_str()) != cur).then_some(base)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &dyn Fn(&str) -> bool = &|_| true;
    fn prog(p: &str, profile: &str) -> Rule {
        Rule { when: When::Program(p.into()), profile: profile.into() }
    }
    fn time(f: &str, t: &str, profile: &str) -> Rule {
        Rule { when: When::Time { from: parse_hm(f).unwrap(), to: parse_hm(t).unwrap() }, profile: profile.into() }
    }

    #[test]
    fn program_names_and_times_parse() {
        assert_eq!(norm_program(" C:\\Apps\\Chrome.EXE ").as_deref(), Some("chrome.exe"));
        assert_eq!(norm_program("\"D:/g/Game\"").as_deref(), Some("game.exe"));
        assert_eq!(norm_program("code").as_deref(), Some("code.exe"));
        assert_eq!(norm_program("a.b.exe").as_deref(), Some("a.b.exe"));
        assert_eq!((norm_program(""), norm_program("  "), norm_program("C:\\dir\\"), norm_program("a|b")), (None, None, None, None));
        assert_eq!((parse_hm("7:05"), parse_hm("07:05"), parse_hm("23:59"), parse_hm("00:00")), (Some(425), Some(425), Some(1439), Some(0)));
        for bad in ["24:00", "12:60", "12", "1:2:3", "", "ab:cd", "-1:00", "12:5x"] {
            assert_eq!(parse_hm(bad), None, "{bad}");
        }
        assert_eq!(parse_range("22:00-06:00"), Some((1320, 360)));
        assert_eq!(parse_range(" 9:00 \u{2013} 17:30 "), Some((540, 1050)));
        assert_eq!(parse_range("22:00"), None);
        assert_eq!(parse_range("22:00-"), None);
        assert_eq!(fmt_hm(425), "07:05");
    }

    #[test]
    fn time_ranges_cross_midnight() {
        let (f, t) = (parse_hm("22:00").unwrap(), parse_hm("06:00").unwrap());
        for (hm, want) in [("21:59", false), ("22:00", true), ("23:59", true), ("00:00", true), ("05:59", true), ("06:00", false), ("12:00", false)] {
            assert_eq!(time_in(f, t, parse_hm(hm).unwrap()), want, "{hm}");
        }
        let (f, t) = (parse_hm("09:00").unwrap(), parse_hm("17:00").unwrap());
        assert!(!time_in(f, t, 539) && time_in(f, t, 540) && time_in(f, t, 1019) && !time_in(f, t, 1020));
        assert!(!time_in(100, 100, 100) && !time_in(0, 0, 0)); // empty range
        assert!(time_in(0, 1439, 0) && !time_in(0, 1439, 1439));
        assert!(time_in(1, 0, 1439) && time_in(1, 0, 1) && !time_in(1, 0, 0)); // 00:01-00:00 = nearly all day
    }

    #[test]
    fn rules_round_trip_and_garbage_is_skipped() {
        let v = json!({"sceneRules": [
            {"program": "Chrome", "profile": "Work"},
            {"from": "22:00", "to": "06:00", "profile": "Night"},
            {"program": "x.exe"}, {"profile": "P"}, {"from": "25:00", "to": "1:00", "profile": "P"}, {"from": "1:00", "profile": "P"},
            {"program": "|", "profile": "P"}, {"program": "a.exe", "profile": "  "}, 5, "x"
        ]});
        let rules = rules_from_value(&v);
        assert_eq!(rules, vec![prog("chrome.exe", "Work"), time("22:00", "06:00", "Night")]);
        let back = json!({"sceneRules": rules_to_value(&rules)});
        assert_eq!(rules_from_value(&back), rules);
        assert_eq!(back["sceneRules"][1], json!({"from": "22:00", "to": "06:00", "profile": "Night"}));
        assert!(rules_from_value(&json!({})).is_empty() && rules_from_value(&json!({"sceneRules": 3})).is_empty());
        assert_eq!(describe(&rules[0]), "chrome.exe \u{2192} Work");
        assert_eq!(describe(&rules[1]), "22:00\u{2013}06:00 \u{2192} Night");
    }

    #[test]
    fn program_beats_time_beats_nothing() {
        let rules = vec![time("22:00", "06:00", "Night"), prog("game.exe", "Game"), prog("code.exe", "Dev"), time("00:00", "23:00", "Day")];
        let w = |fg: Option<&str>, m: u16| winner(&rules, fg, m, ALL).map(|r| r.profile.as_str());
        assert_eq!(w(Some("game.exe"), 23 * 60), Some("Game")); // program over the night rule
        assert_eq!(w(Some("GAME.EXE"), 12 * 60), Some("Game")); // case
        assert_eq!(w(Some("notepad.exe"), 23 * 60), Some("Night")); // first time rule in list order wins over the later "Day"
        assert_eq!(w(None, 12 * 60), Some("Day"));
        assert_eq!(w(Some("code.exe"), 3 * 60), Some("Dev"));
        assert_eq!(w(Some("notepad.exe"), 23 * 60 + 30), Some("Night"));
        assert_eq!(w(Some("notepad.exe"), 23 * 60 + 30), w(None, 23 * 60 + 30));
        // a rule whose profile was deleted is skipped, the next one applies
        let no_game = |p: &str| p != "Game";
        assert_eq!(winner(&rules, Some("game.exe"), 12 * 60, &no_game).map(|r| r.profile.as_str()), Some("Day"));
        assert_eq!(winner(&[], Some("a.exe"), 0, ALL), None);
    }

    /// Feed `step` the same winner and the active profile it should see; the debounce needs STABLE evaluations.
    fn run(a: &mut Auto, win: Option<&Rule>, cur: &mut Option<String>) -> Option<String> {
        let mut out = None;
        for _ in 0..STABLE {
            if let Some(p) = a.step(win, cur.as_deref(), ALL) {
                *cur = Some(p.clone()); // the host switched
                out = Some(p);
            }
        }
        out
    }

    #[test]
    fn rule_applies_then_the_users_profile_comes_back() {
        let (mut a, mut cur) = (Auto::default(), Some("Home".to_string()));
        let game = prog("game.exe", "Game");
        assert_eq!(a.step(Some(&game), cur.as_deref(), ALL), None); // first sighting: not yet
        assert_eq!(a.step(Some(&game), cur.as_deref(), ALL).as_deref(), Some("Game"));
        cur = Some("Game".into());
        assert_eq!(a.step(Some(&game), cur.as_deref(), ALL), None); // stays quiet while it holds
        assert_eq!(a.step(None, cur.as_deref(), ALL), None); // gone for one evaluation only: still Game
        assert_eq!(a.step(Some(&game), cur.as_deref(), ALL), None); // and back: nothing happens
        assert_eq!(run(&mut a, None, &mut cur).as_deref(), Some("Home"));
        assert_eq!(run(&mut a, None, &mut cur), None); // nothing more to do
    }

    #[test]
    fn a_flicker_of_one_evaluation_does_nothing() {
        let (mut a, cur) = (Auto::default(), Some("Home".to_string()));
        let game = prog("game.exe", "Game");
        for _ in 0..5 {
            assert_eq!(a.step(Some(&game), cur.as_deref(), ALL), None);
            assert_eq!(a.step(None, cur.as_deref(), ALL), None);
        }
    }

    #[test]
    fn rule_to_rule_keeps_the_first_base() {
        let (mut a, mut cur) = (Auto::default(), Some("Home".to_string()));
        let (g, d, night) = (prog("game.exe", "Game"), prog("code.exe", "Dev"), time("22:00", "06:00", "Night"));
        assert_eq!(run(&mut a, Some(&night), &mut cur).as_deref(), Some("Night"));
        assert_eq!(run(&mut a, Some(&g), &mut cur).as_deref(), Some("Game"));
        assert_eq!(run(&mut a, Some(&d), &mut cur).as_deref(), Some("Dev"));
        assert_eq!(run(&mut a, Some(&night), &mut cur).as_deref(), Some("Night")); // the program left, the night rule is next
        assert_eq!(run(&mut a, None, &mut cur).as_deref(), Some("Home"));
    }

    #[test]
    fn a_manual_switch_pauses_until_the_condition_changes() {
        let (mut a, mut cur) = (Auto::default(), Some("Home".to_string()));
        let (game, code) = (prog("game.exe", "Game"), prog("code.exe", "Dev"));
        assert_eq!(run(&mut a, Some(&game), &mut cur).as_deref(), Some("Game"));
        cur = Some("Work".into()); // the user picks Work by hand
        assert_eq!(run(&mut a, Some(&game), &mut cur), None);
        assert_eq!(run(&mut a, Some(&game), &mut cur), None); // still the same game in front: auto is paused, the user wins
        cur = Some("Game".into()); // even switching back to the rule's own profile does not unpause it
        assert_eq!(run(&mut a, Some(&game), &mut cur), None);
        // the game closes: the user's choice stays, nothing is restored over it
        cur = Some("Work".into());
        assert_eq!(run(&mut a, None, &mut cur), None);
        assert_eq!(cur.as_deref(), Some("Work"));
        // a later rule works again, and its base is the profile the user picked
        assert_eq!(run(&mut a, Some(&code), &mut cur).as_deref(), Some("Dev"));
        assert_eq!(run(&mut a, None, &mut cur).as_deref(), Some("Work"));
    }

    #[test]
    fn pause_ends_when_the_winner_changes_to_another_rule() {
        let (mut a, mut cur) = (Auto::default(), Some("Home".to_string()));
        let (game, code) = (prog("game.exe", "Game"), prog("code.exe", "Dev"));
        run(&mut a, Some(&game), &mut cur);
        cur = Some("Work".into()); // manual
        assert_eq!(run(&mut a, Some(&game), &mut cur), None);
        assert_eq!(run(&mut a, Some(&code), &mut cur).as_deref(), Some("Dev")); // new condition: auto resumes
        assert_eq!(run(&mut a, None, &mut cur).as_deref(), Some("Work")); // base = the manual pick
    }

    #[test]
    fn a_manual_choice_while_no_rule_holds_is_just_the_new_base() {
        let (mut a, mut cur) = (Auto::default(), Some("Home".to_string()));
        let game = prog("game.exe", "Game");
        assert_eq!(run(&mut a, None, &mut cur), None);
        cur = Some("Work".into());
        assert_eq!(run(&mut a, Some(&game), &mut cur).as_deref(), Some("Game"));
        assert_eq!(run(&mut a, None, &mut cur).as_deref(), Some("Work"));
    }

    #[test]
    fn edge_cases() {
        let game = prog("game.exe", "Game");
        // already on the rule's profile: nothing to switch, nothing to restore later
        let (mut a, mut cur) = (Auto::default(), Some("Game".to_string()));
        assert_eq!(run(&mut a, Some(&game), &mut cur), None);
        assert_eq!(run(&mut a, None, &mut cur), None);
        // no active profile: nothing is started (there would be nothing to restore)
        let (mut a, mut cur) = (Auto::default(), None);
        assert_eq!(run(&mut a, Some(&game), &mut cur), None);
        // the base profile was deleted meanwhile: no restore, no error
        let (mut a, mut cur) = (Auto::default(), Some("Home".to_string()));
        run(&mut a, Some(&game), &mut cur);
        let gone = |p: &str| p != "Home";
        let mut out = None;
        for _ in 0..STABLE {
            out = a.step(None, cur.as_deref(), &gone);
        }
        assert_eq!(out, None);
        // a failed switch (cur never became the target) is read as a manual choice: no retry loop
        let (mut a, cur) = (Auto::default(), Some("Home".to_string()));
        let mut asked = 0;
        for _ in 0..20 {
            asked += a.step(Some(&game), cur.as_deref(), ALL).is_some() as u32;
        }
        assert_eq!(asked, 1);
        // the winner starts out stable if the program was already in front at start (two evaluations, not more)
        let (mut a, cur) = (Auto::default(), Some("Home".to_string()));
        assert_eq!(a.step(Some(&game), cur.as_deref(), ALL), None);
        assert_eq!(a.step(Some(&game), cur.as_deref(), ALL).as_deref(), Some("Game"));
    }
}
