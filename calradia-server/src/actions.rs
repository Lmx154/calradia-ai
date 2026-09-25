//! Actions a character may propose in a talk (Milestone 5): the validated interface
//! between the model's words and game state.
//!
//! The model may end a reply with one line `ACTION: <kind> <number>`. The server strips the
//! line from the spoken text, and validates the proposal here against fixed bounds, the
//! live context the game sent (who the NPC is, what gold there is) and the history of the
//! same savegame chain (cooldowns). Only a valid proposal reaches the game, in the v2 frame
//! (`K` = `ACT_*`, `N` = amount, `W` = the NPC's troop). The game checks it again and
//! executes it in one script, `script_cai_execute_action`; gold changes hands only if the
//! player accepts. Nothing else the model says can change the game.

use crate::ids::{troop_kind, Kind};
use crate::memory::Turn;
use crate::protocol::{
    TalkV2, ACT_ASK, ACT_GIVE, ACT_MAX_ASK, ACT_MAX_GIVE, ACT_MAX_RELATION, ACT_MIN_GOLD,
    ACT_RELATION, FRAME_V2, ST_PLAYER_PRISONER,
};

/// A validated proposal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Action {
    pub kind: u32,
    pub amount: i32,
}

/// Total relation change one character may cause in one game day.
pub const MAX_RELATION_PER_DAY: i32 = 5;
/// Days between two gold proposals of one character.
pub const GOLD_COOLDOWN_DAYS: u32 = 3;

/// Splits a reply into the spoken text and the proposal of its `ACTION:` line (the last one
/// wins; every such line is removed). The marker may also end the last line.
pub fn split(reply: &str) -> (String, Option<(String, i64)>) {
    let mut spoken = Vec::new();
    let mut proposal = None;
    for line in reply.lines() {
        match line.to_ascii_lowercase().find("action:") {
            Some(i) => {
                let before = line[..i].trim_end_matches(['[', '(', ' ', '*']);
                if !before.trim().is_empty() {
                    spoken.push(before.to_string());
                }
                let mut words = line[i + "action:".len()..]
                    .split(|c: char| c.is_whitespace() || c == ']' || c == ')' || c == '*')
                    .filter(|w| !w.is_empty());
                let kind = words.next().map(str::to_ascii_lowercase);
                let number = words
                    .next()
                    .and_then(|n| n.trim_start_matches('+').parse().ok());
                proposal = kind.zip(number);
            }
            None => spoken.push(line.to_string()),
        }
    }
    (spoken.join("\n").trim().to_string(), proposal)
}

/// Validates a proposal for talk `t`. `history` is the character's turns on the chain,
/// oldest first. The error is a reason for the log.
pub fn validate(proposal: &(String, i64), t: &TalkV2, history: &[Turn]) -> Result<Action, String> {
    if t.frame != FRAME_V2 {
        return Err("the game cannot receive actions (f=1)".into());
    }
    let (word, number) = proposal;
    let kind = match word.as_str() {
        "relation" | "regard" | "attitude" => ACT_RELATION,
        "give" | "offer" | "gift" => ACT_GIVE,
        "ask" | "request" | "demand" => ACT_ASK,
        other => return Err(format!("unknown action {other:?}")),
    };
    let amount = i32::try_from(*number).map_err(|_| "amount out of range".to_string())?;
    let today = t.context.day;
    match kind {
        ACT_RELATION => {
            if amount == 0 || amount.abs() > ACT_MAX_RELATION {
                return Err(format!("relation {amount} outside 1..={ACT_MAX_RELATION}"));
            }
            let used: i32 = history
                .iter()
                .filter(|h| h.day == today && h.action_kind == ACT_RELATION)
                .map(|h| h.action_amount.abs())
                .sum();
            if used + amount.abs() > MAX_RELATION_PER_DAY {
                return Err(format!("relation budget for day {today} spent ({used})"));
            }
        }
        _ => {
            let recent = history.iter().rev().find(|h| {
                (h.action_kind == ACT_GIVE || h.action_kind == ACT_ASK)
                    && h.day + GOLD_COOLDOWN_DAYS > today
            });
            if let Some(h) = recent {
                return Err(format!("a gold proposal was made on day {}", h.day));
            }
            let max = if kind == ACT_GIVE {
                ACT_MAX_GIVE
            } else {
                ACT_MAX_ASK
            };
            if !(ACT_MIN_GOLD..=max).contains(&amount) {
                return Err(format!("{amount} denars outside {ACT_MIN_GOLD}..={max}"));
            }
            if kind == ACT_GIVE {
                let noble = matches!(
                    troop_kind(&t.character),
                    Kind::King | Kind::Lord | Kind::Pretender | Kind::Lady
                );
                if !noble {
                    return Err("only nobles have a purse to give from".into());
                }
                if t.context.status & ST_PLAYER_PRISONER != 0 {
                    return Err("a prisoner cannot give gold".into());
                }
                match t.context.npc_gold {
                    Some(g) if g >= 2 * amount => {}
                    g => return Err(format!("{amount} denars is too much for a purse of {g:?}")),
                }
            } else {
                match t.context.player_gold {
                    Some(g) if g >= amount => {}
                    g => return Err(format!("the player has {g:?} denars, not {amount}")),
                }
            }
        }
    }
    Ok(Action { kind, amount })
}

/// A memory note for a turn's action, e.g. "you offered Ylva 100 denars; Ylva accepted".
pub fn describe(kind: u32, amount: i32, outcome: u32, player: &str) -> Option<String> {
    use crate::protocol::{OUT_ACCEPTED, OUT_DECLINED, OUT_FAILED};
    let what = match kind {
        ACT_RELATION if amount > 0 => format!("you warmed to {player} ({amount:+})"),
        ACT_RELATION => format!("you cooled towards {player} ({amount:+})"),
        ACT_GIVE => format!("you offered {player} {amount} denars"),
        ACT_ASK => format!("you asked {player} for {amount} denars"),
        _ => return None,
    };
    let result = match (kind, outcome) {
        (ACT_RELATION, _) => "",
        (_, OUT_ACCEPTED) => "; accepted",
        (_, OUT_DECLINED) => "; refused",
        (_, OUT_FAILED) => "; it came to nothing",
        _ => "; no answer yet",
    };
    Some(format!("{what}{result}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ids;
    use crate::protocol::GameContext;

    #[test]
    fn splits_action_lines_from_speech() {
        assert_eq!(split("Aye, boss."), ("Aye, boss.".into(), None));
        assert_eq!(
            split("Take this purse.\nACTION: give 100"),
            ("Take this purse.".into(), Some(("give".into(), 100)))
        );
        assert_eq!(
            split("You insult me. [ACTION: relation -2]"),
            ("You insult me.".into(), Some(("relation".into(), -2)))
        );
        assert_eq!(
            split("Hm.\nAction: relation +1\nmore words\nACTION: ask 50"),
            ("Hm.\nmore words".into(), Some(("ask".into(), 50)))
        );
        assert_eq!(split("ACTION: dance").1, None);
        assert_eq!(split("ACTION: relation two").1, None);
    }

    fn talk(character: &str) -> TalkV2 {
        TalkV2 {
            campaign: 1,
            conversation: 1,
            head: 0,
            troop: ids().troops.index(character).unwrap(),
            character: character.into(),
            context: GameContext {
                day: 10,
                faction: 15,
                player_faction: 0,
                faction_relation: 0,
                relation: 0,
                reputation: 0,
                occupation: 2,
                status: 0,
                renown: 0,
                honor: 0,
                location: 0,
                location_distance: 0,
                player_female: false,
                player_name: "P".into(),
                npc_name: "N".into(),
                faction_name: String::new(),
                player_faction_name: String::new(),
                location_name: String::new(),
                wars: None,
                ruler_name: String::new(),
                spouse_name: String::new(),
                father_name: String::new(),
                npc_gold: Some(1000),
                player_gold: Some(300),
            },
            msg: "Hi".into(),
            frame: FRAME_V2,
            world_head: None,
            head_outcome: 0,
        }
    }

    fn turn(day: u32, kind: u32, amount: i32) -> Turn {
        Turn {
            job: 1,
            conversation: 1,
            day,
            player_name: "P".into(),
            player_text: "x".into(),
            npc_text: "y".into(),
            action_kind: kind,
            action_amount: amount,
            outcome: 0,
        }
    }

    fn ok(word: &str, n: i64, t: &TalkV2, h: &[Turn]) -> Result<Action, String> {
        validate(&(word.into(), n), t, h)
    }

    #[test]
    fn validates_bounds_purses_and_cooldowns() {
        let lord = talk("trp_knight_1_1");
        let act = |kind, amount| Ok(Action { kind, amount });
        assert_eq!(ok("relation", -3, &lord, &[]), act(ACT_RELATION, -3));
        assert_eq!(ok("give", 500, &lord, &[]), act(ACT_GIVE, 500));
        assert_eq!(ok("ask", 300, &lord, &[]), act(ACT_ASK, 300));
        for (word, n) in [
            ("relation", 0),
            ("relation", 4),
            ("give", 9),
            ("give", 501),
            ("ask", 301),
            ("fly", 1),
        ] {
            assert!(ok(word, n, &lord, &[]).is_err(), "{word} {n}");
        }
        // Daily relation budget and gold cooldown, from the chain's history.
        let history = [turn(10, ACT_RELATION, -3), turn(8, ACT_GIVE, 50)];
        assert!(ok("relation", -2, &lord, &history[..1]).is_ok());
        assert!(ok("relation", -3, &lord, &history[..1]).is_err());
        assert!(ok("ask", 50, &lord, &history).is_err());
        assert!(ok("ask", 50, &lord, &[turn(7, ACT_GIVE, 50)]).is_ok());
        // Companions have no purse to give from; prisoners give nothing; unknown gold.
        assert!(ok("give", 50, &talk("trp_npc1"), &[]).is_err());
        assert!(ok("ask", 50, &talk("trp_npc1"), &[]).is_ok());
        let mut prisoner = lord.clone();
        prisoner.context.status = ST_PLAYER_PRISONER;
        assert!(ok("give", 50, &prisoner, &[]).is_err());
        let mut old = lord.clone();
        old.context.npc_gold = None;
        assert!(ok("give", 50, &old, &[]).is_err());
        old.frame = 1;
        assert!(ok("relation", 1, &old, &[]).is_err());
    }

    #[test]
    fn describes_actions_for_memory() {
        use crate::protocol::{OUT_ACCEPTED, OUT_DECLINED};
        assert_eq!(
            describe(ACT_GIVE, 100, OUT_ACCEPTED, "Ylva").unwrap(),
            "you offered Ylva 100 denars; accepted"
        );
        assert_eq!(
            describe(ACT_ASK, 20, OUT_DECLINED, "Ylva").unwrap(),
            "you asked Ylva for 20 denars; refused"
        );
        assert_eq!(
            describe(ACT_RELATION, -2, 0, "Ylva").unwrap(),
            "you cooled towards Ylva (-2)"
        );
        assert_eq!(describe(0, 0, 0, "Ylva"), None);
    }
}
