//! Autonomous characters (Milestone 6): goals, plans and initiatives.
//!
//! Each game day the game sends a tick. The server then (a) delivers at most one initiative
//! that a character decided on earlier, if it is still valid for this savegame's chain, and
//! (b) queues one planning task in the background, behind any conversation. A planning task
//! picks the important character whose plan is stalest, shows the model what that character
//! knows (profile, realm, world events, dealings with the player, the previous plan), and
//! asks for a JSON answer: a goal, a plan, and optionally one act. Acts are few and bounded:
//! a letter to the player, a change of attitude towards the player (with a letter), or a
//! change of relation with another named lord. They are validated here, stored, delivered by
//! a later tick, checked again by the game and executed by `script_cai_execute_action`.
//! Plans also go into the character's conversation prompt, so what they say and do agree.

use crate::characters::Registry;
use crate::ids::{self, troop_kind, Kind};
use crate::memory::{Initiative, Plan};
use crate::names::names;
use crate::protocol::{INIT_ATTITUDE, INIT_LETTER, INIT_MAX_RELATION, INIT_RIVALRY};
use crate::sanitize::sanitize_text;
use serde::Deserialize;
use std::collections::HashMap;

/// A character plans again after this many days.
pub const PLAN_EVERY_DAYS: u32 = 7;
/// An initiative not delivered within this many days is dropped.
pub const INITIATIVE_LIFETIME_DAYS: u32 = 10;
/// Days between two initiatives of one character, and between any two.
pub const CHARACTER_COOLDOWN_DAYS: u32 = 5;
pub const GLOBAL_COOLDOWN_DAYS: u32 = 1;
/// Caps on the texts of a plan.
pub const MAX_GOAL: usize = 300;
pub const MAX_LETTER: usize = 480;

/// A planning task, queued by a tick.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanTask {
    pub campaign: u32,
    /// The tick's node: the world head the plan is made on.
    pub base: u32,
    /// The save's conversation head.
    pub head: u32,
    pub day: u32,
    pub player_name: String,
}

/// Who may plan: characters with a profile who are rulers or claimants, or whom the player
/// has spoken with in this save.
pub fn important(registry: &Registry, spoken_with: &[String]) -> Vec<String> {
    registry
        .ids()
        .filter(|id| {
            matches!(troop_kind(id), Kind::King | Kind::Pretender)
                || spoken_with.iter().any(|s| s == id)
        })
        .map(str::to_string)
        .collect()
}

/// The character to plan for on `day`: one without a plan first, then the stalest whose
/// plan is at least `PLAN_EVERY_DAYS` old. Deterministic (ties by id).
pub fn pick(candidates: &[String], plan_days: &HashMap<String, u32>, day: u32) -> Option<String> {
    candidates
        .iter()
        .filter_map(|c| match plan_days.get(c) {
            None => Some((0, false, c)),
            Some(&d) if d + PLAN_EVERY_DAYS <= day => Some((d, true, c)),
            Some(_) => None,
        })
        .min_by(|a, b| (a.1, a.0, a.2).cmp(&(b.1, b.0, b.2)))
        .map(|(_, _, c)| c.clone())
}

/// The initiative to deliver with a tick on `day`: the oldest `open` one (decided on this
/// chain) that was not delivered, is still fresh, and respects the cooldowns.
pub fn deliverable(open: &[Initiative], delivered: &[Initiative], day: u32) -> Option<Initiative> {
    if delivered.iter().any(|d| d.day + GLOBAL_COOLDOWN_DAYS > day) {
        return None;
    }
    open.iter()
        .filter(|i| !delivered.iter().any(|d| d.id == i.id))
        .filter(|i| i.day + INITIATIVE_LIFETIME_DAYS >= day && i.day <= day)
        .find(|i| {
            !delivered
                .iter()
                .any(|d| d.character == i.character && d.day + CHARACTER_COOLDOWN_DAYS > day)
        })
        .cloned()
}

#[derive(Debug, Deserialize)]
struct RawPlan {
    goal: Option<String>,
    plan: Option<String>,
    act: Option<RawAct>,
}

#[derive(Debug, Deserialize)]
struct RawAct {
    kind: Option<String>,
    text: Option<String>,
    change: Option<i64>,
    target: Option<String>,
}

fn clean(s: &str, max: usize) -> Option<String> {
    let mut t = sanitize_text(s).ok()?;
    if t.len() > max {
        t.truncate(t[..max].rfind(' ').unwrap_or(max));
        t.push_str("...");
    }
    Some(t)
}

/// The troop id of a lord, lady, ruler or claimant called `name` (case-insensitive).
pub fn lord_named(name: &str) -> Option<String> {
    let wanted = name.trim().trim_start_matches("the ").to_ascii_lowercase();
    let t = &ids::ids().troops;
    (0..t.len() as u32)
        .filter_map(|i| t.name(i))
        .filter(|id| {
            matches!(
                troop_kind(id),
                Kind::King | Kind::Lord | Kind::Pretender | Kind::Lady
            )
        })
        .find(|id| {
            names()
                .troop(id)
                .is_some_and(|n| n.to_ascii_lowercase() == wanted)
        })
        .map(str::to_string)
}

/// Parses and validates the model's answer for `character` on `day`. A missing or invalid
/// act only drops the act; a missing goal or plan rejects the answer.
pub fn parse(
    answer: &str,
    character: &str,
    day: u32,
) -> Result<(Plan, Option<Initiative>), String> {
    let start = answer.find('{').ok_or("no JSON object")?;
    let end = answer.rfind('}').ok_or("no JSON object")?;
    let raw: RawPlan = serde_json::from_str(&answer[start..=end.max(start)])
        .map_err(|e| format!("bad JSON: {e}"))?;
    let goal = raw
        .goal
        .as_deref()
        .and_then(|g| clean(g, MAX_GOAL))
        .ok_or("no goal")?;
    let plan = raw
        .plan
        .as_deref()
        .and_then(|p| clean(p, MAX_GOAL))
        .ok_or("no plan")?;
    let plan = Plan { day, goal, plan };
    let act = raw.act.and_then(|a| {
        let text = a.text.as_deref().and_then(|t| clean(t, MAX_LETTER));
        let change = a
            .change
            .and_then(|c| i32::try_from(c).ok())
            .filter(|c| *c != 0 && c.abs() <= INIT_MAX_RELATION);
        let initiative = |kind, amount, target: String, text: String| Initiative {
            id: 0,
            character: character.to_string(),
            day,
            kind,
            amount,
            target,
            text,
        };
        match a.kind.as_deref()?.to_ascii_lowercase().as_str() {
            "letter" => Some(initiative(INIT_LETTER, 0, String::new(), text?)),
            "attitude" => Some(initiative(INIT_ATTITUDE, change?, String::new(), text?)),
            "rivalry" => {
                let target = lord_named(a.target.as_deref()?).filter(|t| t != character)?;
                let change = change?;
                let text = text.unwrap_or_else(|| {
                    let (me, them) = (
                        names().troop(character).unwrap_or(character),
                        names().troop(&target).unwrap_or(&target),
                    );
                    if change < 0 {
                        format!("Word is that {me} and {them} have fallen out.")
                    } else {
                        format!("Word is that {me} and {them} have grown closer.")
                    }
                });
                Some(initiative(INIT_RIVALRY, change, target, text))
            }
            _ => None,
        }
    });
    Ok((plan, act))
}

/// The instructions for a planning answer.
pub fn instructions(name: &str, player: &str, day: u32) -> String {
    format!(
        "It is day {day}. You are the private mind of {name}, deciding in secret what you want \
         and what to do next, from what is written above only. Answer with one JSON object \
         and nothing else:\n\
         {{\"goal\": \"what you want most now, at most 25 words\", \
         \"plan\": \"how you mean to get it, at most 40 words\", \
         \"act\": {{\"kind\": \"none\"}}}}\n\
         For act, choose \"none\" on most days. Otherwise one of: \
         {{\"kind\": \"letter\", \"text\": \"a letter to {player}, in your own voice, at most 60 words\"}}; \
         {{\"kind\": \"attitude\", \"change\": -2 to 2, \"text\": \"a letter to {player} saying why your regard for them changed\"}}; \
         {{\"kind\": \"rivalry\", \"target\": \"the exact name of another lord or lady\", \"change\": -2 to 2, \
         \"text\": \"one line of rumour about it\"}}. \
         Act only on facts written above; stay true to your character and station."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(id: i64, character: &str, day: u32) -> Initiative {
        Initiative {
            id,
            character: character.into(),
            day,
            kind: INIT_LETTER,
            amount: 0,
            target: String::new(),
            text: "t".into(),
        }
    }

    #[test]
    fn picks_unplanned_then_stalest() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let days: HashMap<String, u32> = [("a".to_string(), 3), ("b".to_string(), 1)].into();
        assert_eq!(pick(&c(&["a", "b", "z"]), &days, 5), Some("z".into()));
        assert_eq!(pick(&c(&["a", "b"]), &days, 7), None);
        assert_eq!(pick(&c(&["a", "b"]), &days, 8), Some("b".into()));
        assert_eq!(pick(&c(&["a", "b"]), &days, 10), Some("b".into()));
        assert_eq!(pick(&[], &days, 10), None);
    }

    #[test]
    fn delivers_fresh_undelivered_initiatives_within_cooldowns() {
        let open = [init(1, "a", 2), init(2, "a", 5), init(3, "b", 6)];
        assert_eq!(deliverable(&open, &[], 6).map(|i| i.id), Some(1));
        // Delivered on day 6: nothing more that day; "a" waits five days.
        let done = [init(1, "a", 6)];
        assert_eq!(deliverable(&open, &done, 6), None);
        assert_eq!(deliverable(&open, &done, 7).map(|i| i.id), Some(3));
        assert_eq!(
            deliverable(&open, &[init(1, "a", 6), init(3, "b", 7)], 11).map(|i| i.id),
            Some(2)
        );
        // Stale after ten days; never from the future.
        assert_eq!(deliverable(&[init(4, "c", 1)], &[], 12), None);
        assert_eq!(deliverable(&[init(4, "c", 9)], &[], 8), None);
    }

    #[test]
    fn parses_and_validates_answers() {
        let (plan, act) = parse(
            "Here: {\"goal\": \"Keep Swadia whole.\", \"plan\": \"Watch Isolla.\", \
             \"act\": {\"kind\": \"rivalry\", \"target\": \"count klargus\", \"change\": -2}}",
            "trp_kingdom_1_lord",
            9,
        )
        .unwrap();
        assert_eq!(
            (plan.goal.as_str(), plan.plan.as_str(), plan.day),
            ("Keep Swadia whole.", "Watch Isolla.", 9)
        );
        let act = act.unwrap();
        assert_eq!(
            (act.kind, act.amount, act.target.as_str()),
            (INIT_RIVALRY, -2, "trp_knight_1_1")
        );
        assert_eq!(
            act.text,
            "Word is that King Harlaus and Count Klargus have fallen out."
        );
        let letter = |a: &str| {
            parse(
                &format!("{{\"goal\":\"g\",\"plan\":\"p\",\"act\":{a}}}"),
                "trp_npc1",
                1,
            )
            .unwrap()
            .1
        };
        assert_eq!(
            letter("{\"kind\":\"letter\",\"text\":\"Boss, come to Tulga.\"}")
                .unwrap()
                .text,
            "Boss, come to Tulga."
        );
        for bad in [
            "{\"kind\":\"none\"}",
            "{\"kind\":\"letter\"}",
            "{\"kind\":\"attitude\",\"change\":3,\"text\":\"x\"}",
            "{\"kind\":\"attitude\",\"change\":0,\"text\":\"x\"}",
            "{\"kind\":\"rivalry\",\"target\":\"Nobody\",\"change\":1}",
            "{\"kind\":\"rivalry\",\"target\":\"Borcha\",\"change\":1}",
            "{\"kind\":\"invade\"}",
        ] {
            assert_eq!(letter(bad), None, "{bad}");
        }
        assert_eq!(
            letter("{\"kind\":\"attitude\",\"change\":-1,\"text\":\"You shamed me.\"}")
                .unwrap()
                .amount,
            -1
        );
        assert!(parse("no json", "trp_npc1", 1).is_err());
        assert!(parse("{\"plan\": \"p\"}", "trp_npc1", 1).is_err());
        let long = format!(
            "{{\"goal\":\"g\",\"plan\":\"p\",\"act\":{{\"kind\":\"letter\",\"text\":\"{}\"}}}}",
            "word ".repeat(200)
        );
        assert!(parse(&long, "trp_npc1", 1).unwrap().1.unwrap().text.len() <= MAX_LETTER + 3);
    }
}
