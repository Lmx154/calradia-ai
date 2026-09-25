//! Builds the chat messages for one talk.
//!
//! v1 (Hrodvar): a compact system prompt from the NPC's registry entry plus the shared
//! setting and rules; the user message is the player's text.
//!
//! v2 (a real Warband character): the system prompt is assembled from separate parts --
//! the character profile, the live game state, the recalled memories and the rules -- and
//! the conversation in progress follows as alternating user/assistant messages. The whole
//! prompt is kept under `MAX_PROMPT_CHARS` by dropping the least important memories first.

use crate::characters::Profile;
use crate::ids::{self, place_kind, troop_kind, Kind};
use crate::memory::{Recall, Turn};
use crate::npc::Npc;
use crate::protocol::{
    TalkV2, ST_BETROTHED, ST_FACTION_LEADER, ST_IN_PARTY, ST_MARSHAL, ST_OTHER_PRISONER,
    ST_PLAYER_PRISONER, ST_PLAYER_RULER, ST_PLAYER_VASSAL, ST_SPOUSE,
};
use std::fmt::Write as _;

const SETTING: &str = "Calradia, the world of Mount&Blade: Warband. You know its six \
    factions: the Kingdom of Swadia, the Vaegirs, the Khergit Khanate, the Nords, the \
    Rhodoks and the Sarranid Sultanate, and its well-known towns such as Praven, Suno, \
    Reyvadin, Tulga, Sargoth, Jelkala and Shariz.";

const RULES: &str = "Stay in character. Reply in 1 to 3 sentences of plain text. No \
    emojis, markdown, asterisks or stage directions. Never mention being an AI or anything \
    modern.";

/// The messages of one chat completion, and what `--fake-llm` answers instead.
#[derive(Debug, PartialEq)]
pub struct Chat {
    /// (role, content) pairs: "system", then "user"/"assistant" turns, ending with "user".
    pub messages: Vec<(&'static str, String)>,
    /// A deterministic summary of what the prompt contains (v2 only; empty for v1).
    pub fake_reply: String,
}

impl Chat {
    pub fn len(&self) -> usize {
        self.messages.iter().map(|(_, c)| c.len()).sum()
    }
}

/// The system prompt for `npc` talking to the player `pname` (may be empty) on `day`.
pub fn system_prompt(npc: &Npc, pname: &str, day: u32) -> String {
    let captain = if pname.is_empty() {
        "your captain".to_string()
    } else {
        format!("your captain, {pname}")
    };
    format!(
        "You are {name}, {identity}.\nPersonality: {personality}.\nSpeaking style: {style}.\n\
         Setting: {SETTING}\nContext: you are talking to {captain}. It is day {day} of the \
         campaign.\nRules: {RULES}",
        name = npc.name,
        identity = npc.identity,
        personality = npc.personality,
        style = npc.style,
    )
}

pub fn v1_chat(npc: &Npc, pname: &str, day: u32, msg: &str) -> Chat {
    Chat {
        messages: vec![
            ("system", system_prompt(npc, pname, day)),
            ("user", msg.to_string()),
        ],
        fake_reply: String::new(),
    }
}

// ---------------------------------------------------------------- v2

/// Upper bound on the characters of all v2 messages together (about 3000 tokens).
pub const MAX_PROMPT_CHARS: usize = 12000;

/// The shape of the answer, and what the character may and may not claim.
fn rules(name: &str, player: &str) -> String {
    format!(
        "Rules: Speak only as {name}, in the first person, answering what {player} just said. \
         Reply in 1 to 3 sentences of plain speech: no narration, stage directions, asterisks, \
         emojis or markdown, and never speak or act for {player}. You know only what is \
         written above and what someone of your station in Calradia would know. The \
         situation and memories above are true; do not present any other event between you \
         and {player} as having happened, and do not state rumours as fact. You have your \
         own goals and values: you need not agree with, like, trust or help {player}, and \
         you may be proud, rude, evasive or deceitful when it suits you. Words are all you \
         can give here: you cannot hand over money, goods, troops or land in this talk, so \
         do not claim to. Never mention being an AI, a game or anything modern."
    )
}

/// The Native module's own descriptions of `slot_lord_reputation_type` (module_constants.py).
pub fn reputation(rep: u32) -> Option<&'static str> {
    Some(match rep {
        1 => "martial: chivalrous and proud, values courage and prowess, not introspective",
        2 => "quarrelsome: spiteful, cynical, a bit paranoid and hot-headed",
        3 => "self-righteous: cold-blooded and moralizing, often cruel",
        4 => "cunning: cold-blooded, pragmatic and amoral",
        5 => "debauched: spiteful, amoral and sadistic",
        6 => "good-natured: chivalrous and benevolent, perhaps too decent for a warlord",
        7 => "upstanding: moralizing, benevolent and pragmatic",
        8 => "roguish: means to live life to the full",
        9 => "a benefactor: wants to improve the lot of common folk",
        10 => "a custodian: careful with money, wants what they hold to prosper",
        21 => "conventional: well-bred and proper",
        22 => "adventurous: loves travel and the hunt, and yearns for wider adventures",
        23 => "otherworldly: romantic and prone to mysticism",
        24 => "ambitious: scheming, and hungry for power",
        25 => "a moralist: takes duty and morality very seriously",
        _ => return None,
    })
}

fn relation_word(rel: i32) -> &'static str {
    match rel {
        i32::MIN..=-30 => "you hate them",
        -29..=-10 => "you dislike them",
        -9..=-3 => "you are cool towards them",
        -2..=2 => "you are indifferent to them",
        3..=9 => "you are on good terms",
        10..=29 => "you are friends",
        _ => "you are close friends",
    }
}

fn renown_word(renown: i32) -> &'static str {
    match renown {
        i32::MIN..=49 => "hardly known",
        50..=199 => "somewhat known",
        200..=499 => "known across the realm",
        500..=999 => "famous",
        _ => "one of the most famous people in Calradia",
    }
}

fn honor_word(honor: i32) -> &'static str {
    match honor {
        i32::MIN..=-20 => "known to be dishonourable",
        -19..=-1 => "thought a little untrustworthy",
        0..=9 => "of unremarkable honour",
        10..=49 => "known to be honourable",
        _ => "famous for their honour",
    }
}

/// The live game state as prose. Only facts the game sent are stated.
fn situation(t: &TalkV2, player: &str) -> String {
    let c = &t.context;
    let ids = ids::ids();
    let has = |bit: u32| c.status & bit != 0;
    let mut out = format!("Current situation (reported by the game; treat it as fact):\n- It is day {} of the campaign.\n", c.day);
    let mut line = |s: String| {
        out += "- ";
        out += &s;
        out += "\n";
    };
    if c.location > 0 && !c.location_name.is_empty() {
        let kind = place_kind(ids.parties.name(c.location).unwrap_or(""));
        let name = &c.location_name;
        line(match c.location_distance {
            0..=1 => format!("You and {player} are at {name}, a {kind}."),
            2..=10 => format!("You and {player} are near {name}, a {kind}."),
            _ => format!(
                "You and {player} are in open country; the nearest settlement is {name}, a {kind}."
            ),
        });
    }
    let faction_id = ids.factions.name(c.faction).unwrap_or("");
    let is_realm =
        faction_id.starts_with("fac_kingdom_") || faction_id == "fac_player_supporters_faction";
    if is_realm && !c.faction_name.is_empty() {
        let role = if has(ST_FACTION_LEADER) {
            ", and you are its ruler"
        } else if has(ST_MARSHAL) {
            ", and you are its marshal"
        } else {
            ""
        };
        line(format!("You belong to {}{role}.", c.faction_name));
    }
    if has(ST_IN_PARTY) {
        line(format!("You ride in {player}'s company."));
    }
    if has(ST_PLAYER_PRISONER) {
        line(format!("You are {player}'s prisoner."));
    } else if has(ST_OTHER_PRISONER) {
        line("You are being held prisoner.".into());
    }
    if c.occupation == 9 {
        line("You are a claimant in exile, without lands or an army of your own.".into());
    }
    if has(ST_SPOUSE) {
        line(format!("You are married to {player}."));
    } else if has(ST_BETROTHED) {
        line(format!("You are betrothed to {player}."));
    }
    let who = if c.player_female { "a woman" } else { "a man" };
    line(format!(
        "{player} is {who}. Renown {} ({}), honour {} ({}).",
        c.renown,
        renown_word(c.renown),
        c.honor,
        honor_word(c.honor)
    ));
    let pfac = ids.factions.name(c.player_faction).unwrap_or("");
    let pname = if c.player_faction_name.is_empty() {
        "a kingdom"
    } else {
        &c.player_faction_name
    };
    line(
        if c.player_faction == 0 || pfac.is_empty() || pfac == "fac_no_faction" {
            format!("{player} has sworn allegiance to no kingdom.")
        } else if has(ST_PLAYER_RULER) {
            format!("{player} rules their own realm, {pname}.")
        } else if has(ST_PLAYER_VASSAL) {
            format!("{player} is a sworn vassal of {pname}.")
        } else {
            format!("{player} fights for {pname} as a mercenary.")
        },
    );
    line(format!(
        "Your personal relation with {player} is {} on a scale from -100 to 100: {}.",
        c.relation,
        relation_word(c.relation)
    ));
    if is_realm {
        line(if c.faction_relation < 0 {
            format!(
                "Your realm is hostile to {player} and their side (relation {}).",
                c.faction_relation
            )
        } else {
            format!(
                "Your realm is not at war with {player}'s side (relation {}).",
                c.faction_relation
            )
        });
    }
    out
}

/// A character without a profile: who they are, from live data only.
fn generic_profile(t: &TalkV2) -> String {
    let c = &t.context;
    let kind = match troop_kind(&t.character) {
        Kind::King => "a ruler",
        Kind::Lord => "a lord",
        Kind::Pretender => "a claimant to a throne",
        Kind::Lady => "a noblewoman",
        Kind::Companion => "a wandering adventurer",
        Kind::Other => "a person",
    };
    let mut out = format!("You are {}, {kind} of Calradia.\n", display_name(t));
    if let Some(rep) = reputation(c.reputation) {
        let _ = writeln!(out, "Personality: {rep}.");
    }
    out += "No personal history has been written for you: do not invent a detailed past or \
            family. Keep to your station and temperament.\nSpeaking style: as befits your \
            station; brief and direct.";
    out
}

fn display_name(t: &TalkV2) -> &str {
    if t.context.npc_name.is_empty() {
        &t.character
    } else {
        &t.context.npc_name
    }
}

fn quote(turn: &Turn, player: &str) -> String {
    format!(
        "Day {}: {player} said \"{}\" and you answered \"{}\"",
        turn.day, turn.player_text, turn.npc_text
    )
}

fn memories(r: &Recall, player: &str) -> String {
    let earlier = r.total_turns - r.current.len();
    if earlier == 0 {
        return format!(
            "Memories: you remember no earlier conversation with {player}. Do not pretend to \
             remember one."
        );
    }
    let mut out = format!(
        "Memories of your earlier conversations with {player} (these happened):\n- You \
         first spoke with {player} on day {}; you have talked {} times before this \
         conversation.\n",
        r.first_day.unwrap_or(0),
        earlier
    );
    if !r.relevant.is_empty() {
        out += "Older words that bear on what is being said now:\n";
        for t in &r.relevant {
            let _ = writeln!(out, "- {}", quote(t, player));
        }
    }
    if !r.recent.is_empty() {
        out += "Your most recent earlier conversations:\n";
        for t in &r.recent {
            let _ = writeln!(out, "- {}", quote(t, player));
        }
    }
    out.trim_end().to_string()
}

/// The chat for a v2 talk, within `MAX_PROMPT_CHARS`. `profile` is None when the character
/// has no file; `recall` has already been bounded by memory::recall.
pub fn v2_chat(t: &TalkV2, profile: Option<&Profile>, recall: &Recall) -> Chat {
    let player = if t.context.player_name.is_empty() {
        "the player"
    } else {
        &t.context.player_name
    };
    let name = display_name(t);
    let who = match profile {
        Some(p) => format!("You are {name}.\n{}", p.render()),
        None => generic_profile(t),
    };
    let fixed = format!("{who}\n\nSetting: {SETTING}\n\n{}", situation(t, player));
    let mut r = Recall {
        current: recall.current.clone(),
        recent: recall.recent.clone(),
        relevant: recall.relevant.clone(),
        ..*recall
    };
    loop {
        let chat = assemble(&fixed, &r, t, player, name, profile.is_some());
        let over = chat.len() > MAX_PROMPT_CHARS;
        // Drop the least important memory first: relevant, then recent, then the oldest
        // turns of the conversation in progress.
        if over && !r.relevant.is_empty() {
            r.relevant.remove(0);
        } else if over && !r.recent.is_empty() {
            r.recent.remove(0);
        } else if over && !r.current.is_empty() {
            r.current.remove(0);
        } else {
            return chat;
        }
    }
}

fn assemble(
    fixed: &str,
    r: &Recall,
    t: &TalkV2,
    player: &str,
    name: &str,
    has_profile: bool,
) -> Chat {
    let system = format!(
        "{fixed}\n{}\n\n{}",
        memories(r, player),
        rules(name, player)
    );
    let mut messages = vec![("system", system)];
    for turn in &r.current {
        messages.push(("user", turn.player_text.clone()));
        messages.push(("assistant", turn.npc_text.clone()));
    }
    messages.push(("user", t.msg.clone()));
    let mut fake = format!(
        "[fake] I am {name}{} of {}, day {}, relation {}. I remember {} earlier talks",
        if has_profile { "" } else { " (no profile)" },
        if t.context.faction_name.is_empty() {
            "no faction"
        } else {
            &t.context.faction_name
        },
        t.context.day,
        t.context.relation,
        r.total_turns - r.current.len(),
    );
    if let Some(last) = r.recent.last() {
        let _ = write!(fake, "; you last said: {}", last.player_text);
    }
    if let Some(first) = r.relevant.first() {
        let _ = write!(fake, "; you once said: {}", first.player_text);
    }
    if !r.current.is_empty() {
        let _ = write!(fake, "; this talk has {} earlier lines", r.current.len());
    }
    fake += ".";
    Chat {
        messages,
        fake_reply: fake,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::GameContext;

    fn talk(character: &str) -> TalkV2 {
        let ids = ids::ids();
        TalkV2 {
            campaign: 1,
            conversation: 2,
            head: 0,
            troop: ids.troops.index(character).unwrap(),
            character: character.into(),
            context: GameContext {
                day: 30,
                faction: ids.factions.index("fac_kingdom_1").unwrap(),
                player_faction: 0,
                faction_relation: -40,
                relation: -12,
                reputation: 2,
                occupation: 2,
                status: ST_FACTION_LEADER,
                renown: 120,
                honor: 3,
                location: ids.parties.index("p_town_6").unwrap(),
                location_distance: 0,
                player_female: true,
                player_name: "Ylva".into(),
                npc_name: "Count Klargus".into(),
                faction_name: "Kingdom of Swadia".into(),
                player_faction_name: String::new(),
                location_name: "Praven".into(),
            },
            msg: "Hail.".into(),
        }
    }

    #[test]
    fn situation_states_only_what_the_game_sent() {
        let t = talk("trp_knight_1_1");
        let s = situation(&t, "Ylva");
        for part in [
            "day 30",
            "You and Ylva are at Praven, a town.",
            "You belong to Kingdom of Swadia, and you are its ruler.",
            "Ylva is a woman. Renown 120 (somewhat known), honour 3",
            "Ylva has sworn allegiance to no kingdom.",
            "relation with Ylva is -12 on a scale from -100 to 100: you dislike them.",
            "Your realm is hostile to Ylva",
        ] {
            assert!(s.contains(part), "{part:?} missing from {s}");
        }
        for absent in ["prisoner", "married", "company", "exile"] {
            assert!(!s.contains(absent), "{absent} in {s}");
        }
    }

    #[test]
    fn generic_profile_uses_the_campaigns_reputation() {
        let chat = v2_chat(&talk("trp_knight_1_1"), None, &Recall::default());
        let system = &chat.messages[0].1;
        assert!(
            system.starts_with(
                "You are Count Klargus, a lord of Calradia.\nPersonality: quarrelsome"
            ),
            "{system}"
        );
        assert!(system.contains("do not invent a detailed past"));
        assert!(system.contains("you remember no earlier conversation with Ylva"));
        assert_eq!(chat.messages.len(), 2);
        assert!(chat.fake_reply.contains("(no profile)"));
    }
}
