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
use crate::memory::{Initiative, Plan, Recall, Turn};
use crate::npc::Npc;
use crate::protocol::{
    TalkV2, ST_BETROTHED, ST_FACTION_LEADER, ST_IN_PARTY, ST_MARSHAL, ST_OTHER_PRISONER,
    ST_PLAYER_PRISONER, ST_PLAYER_RULER, ST_PLAYER_VASSAL, ST_SPOUSE,
};
use crate::realms::Realms;
use crate::world::WorldEvent;
use std::fmt::Write as _;

const SETTING: &str = "Calradia, the world of Mount&Blade: Warband. You know its six \
    factions: the Kingdom of Swadia, the Vaegirs, the Khergit Khanate, the Nords, the \
    Rhodoks and the Sarranid Sultanate, and its well-known towns such as Praven, Suno, \
    Reyvadin, Tulga, Sargoth, Jelkala and Shariz.";

const RULES: &str = "Stay in character. Reply in 1 to 3 sentences of plain text. No \
    emojis, markdown, asterisks or stage directions. Never mention being an AI or anything \
    modern.";

/// Tokens a spoken reply may take: 1 to 3 sentences and an `ACTION:` line.
pub const TALK_MAX_TOKENS: u32 = 220;
/// Tokens a planning answer may take: JSON with a goal, a plan and a letter of up to 60
/// words (planner::instructions), with room to spare so it is never cut mid-object.
pub const PLAN_MAX_TOKENS: u32 = 400;

/// The messages of one chat completion, and what `--fake-llm` answers instead.
#[derive(Debug, PartialEq)]
pub struct Chat {
    /// (role, content) pairs: "system", then "user"/"assistant" turns, ending with "user".
    pub messages: Vec<(&'static str, String)>,
    /// A deterministic summary of what the prompt contains (v2 only; empty for v1).
    pub fake_reply: String,
    /// The completion's token limit.
    pub max_tokens: u32,
    /// Ask the model server for a JSON object (planning).
    pub json: bool,
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
        max_tokens: TALK_MAX_TOKENS,
        json: false,
    }
}

// ---------------------------------------------------------------- v2

/// Upper bound on the characters of all v2 messages together (about 3000 tokens).
pub const MAX_PROMPT_CHARS: usize = 12000;

/// What a v2 prompt adds beyond the talk and its memories (Milestones 4-6).
#[derive(Debug, Default)]
pub struct Extras {
    /// World events the character knows of, oldest first (world::recall).
    pub world: Vec<WorldEvent>,
    /// The character's current aims, and what it did of its own accord lately.
    pub plan: Option<Plan>,
    pub deeds: Vec<Initiative>,
    /// The character may propose an action (actions.rs).
    pub actions: bool,
}

/// The shape of the answer, and what the character may and may not claim.
fn rules(name: &str, player: &str, t: &TalkV2, actions: bool) -> String {
    if actions {
        return format!(
            "Rules: Speak only as {name}, in the first person, answering what {player} just \
             said. Reply in 1 to 3 sentences of plain speech: no narration, stage directions, \
             asterisks, emojis or markdown, and never speak or act for {player}. You know only \
             what is written above and what someone of your station in Calradia would know. \
             The situation, events and memories above are true; do not present any other event \
             between you and {player} as having happened, and do not state rumours as fact. \
             You have your own goals and values: you need not agree with, like, trust or help \
             {player}, and you may be proud, rude, evasive or deceitful when it suits you. \
             Never mention being an AI, a game or anything modern.\n{}",
            deed_rules(player, t)
        );
    }
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

/// The deeds a reply may carry, with what the character and the player can afford.
fn deed_rules(player: &str, t: &TalkV2) -> String {
    let noble = matches!(
        troop_kind(&t.character),
        Kind::King | Kind::Lord | Kind::Pretender | Kind::Lady
    );
    let mut deeds = vec![format!(
        "'relation <number from -3 to 3>' when this exchange truly changes how you regard \
         {player}"
    )];
    if let (true, Some(gold)) = (noble, t.context.npc_gold) {
        deeds.push(format!(
            "'give <denars>' to offer {player} money from your own purse (you have about {gold} \
             denars)"
        ));
    }
    if let Some(gold) = t.context.player_gold {
        deeds.push(format!(
            "'ask <denars>' to ask {player} for money ({player} carries {gold} denars)"
        ));
    }
    format!(
        "Deeds: your reply may also do one thing. Only if it truly does, end it with one line \
         'ACTION: <deed> <number>', where the deed is {}. {player} must accept or refuse any \
         money. Most replies need no ACTION line. Beyond these deeds, words are all you can \
         give: you cannot hand over goods, troops or land in this talk, so do not claim to.",
        join_or(&deeds)
    )
}

fn join_or(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// The world events a character knows of.
fn world_section(events: &[WorldEvent]) -> String {
    if events.is_empty() {
        return String::new();
    }
    let mut out =
        "\nEvents in Calradia that you know of (from the game's record; these happened):\n"
            .to_string();
    for e in events {
        let _ = writeln!(out, "- Day {}: {}", e.day, e.text);
    }
    out
}

/// The character's own aims and deeds (Milestone 6).
fn aims_section(plan: Option<&Plan>, deeds: &[Initiative], player: &str) -> String {
    let mut out = String::new();
    if let Some(p) = plan {
        let _ = writeln!(
            out,
            "\nYour private aims (decided on day {}; reveal them only if it serves you):\n- Goal: {}\n- Plan: {}",
            p.day, p.goal, p.plan
        );
    }
    if !deeds.is_empty() {
        out += "What you did of your own accord lately:\n";
        for d in deeds {
            let what = match d.kind {
                crate::protocol::INIT_LETTER => format!("you wrote to {player}: \"{}\"", d.text),
                crate::protocol::INIT_ATTITUDE => format!(
                    "your regard for {player} changed by {:+}, and you wrote: \"{}\"",
                    d.amount, d.text
                ),
                _ => d.text.clone(),
            };
            let _ = writeln!(out, "- Day {}: {what}", d.day);
        }
    }
    out
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
fn situation(t: &TalkV2, player: &str, realms: &Realms) -> String {
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
        if !c.ruler_name.is_empty() && !has(ST_FACTION_LEADER) {
            line(format!("Your liege is {}.", c.ruler_name));
        }
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
    if !c.spouse_name.is_empty() && !has(ST_SPOUSE) {
        line(format!("You are married to {}.", c.spouse_name));
    }
    if !c.father_name.is_empty() {
        line(format!("Your father: {}.", c.father_name));
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
        if let Some(wars) = c.wars {
            let names = war_names(wars, c, realms);
            line(if names.is_empty() {
                "Your realm is at war with no other realm.".to_string()
            } else {
                format!("Your realm is at war with {}.", join_and(&names))
            });
        }
    }
    out
}

/// The realms in a `wars=` mask, by name.
fn war_names(wars: u32, c: &crate::protocol::GameContext, realms: &Realms) -> Vec<String> {
    let ids = ids::ids();
    let first = ids
        .factions
        .index("fac_player_supporters_faction")
        .expect("vanilla faction");
    (0..7)
        .filter(|bit| wars & (1 << bit) != 0)
        .filter_map(|bit| ids.factions.name(first + bit))
        .map(|id| match realms.get(id) {
            Some(realm) => realm.name.clone(),
            None if id == "fac_player_supporters_faction" => {
                match ids.factions.name(c.player_faction) {
                    Some(p) if p == id && !c.player_faction_name.is_empty() => {
                        c.player_faction_name.clone()
                    }
                    _ => "a rebel realm".to_string(),
                }
            }
            None => id.to_string(),
        })
        .collect()
}

/// "a", "a and b", "a, b and c".
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
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
    out += "No personal history has been written for you: do not invent past deeds or \
            relatives beyond those named below. Keep to your station, your realm's ways and \
            your temperament.\nSpeaking style: as befits your station; brief and direct.";
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
    let deed = crate::actions::describe(turn.action_kind, turn.action_amount, turn.outcome, player)
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    format!(
        "Day {}: {player} said \"{}\" and you answered \"{}\"{deed}",
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
pub fn v2_chat(
    t: &TalkV2,
    profile: Option<&Profile>,
    realms: &Realms,
    recall: &Recall,
    extras: &Extras,
) -> Chat {
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
    let faction = ids::ids().factions.name(t.context.faction).unwrap_or("");
    let lore = match realms.get(faction) {
        Some(realm) => format!(
            "\n\nWhat you know as one of the {} ({}):\n{}",
            realm.people,
            realm.name,
            realm.render()
        ),
        None => String::new(),
    };
    let fixed = format!(
        "{who}{lore}\n\nSetting: {SETTING}\n\n{}{}",
        situation(t, player, realms),
        aims_section(extras.plan.as_ref(), &extras.deeds, player)
    );
    let mut r = Recall {
        current: recall.current.clone(),
        recent: recall.recent.clone(),
        relevant: recall.relevant.clone(),
        ..*recall
    };
    let mut world = extras.world.clone();
    loop {
        let chat = assemble(
            &fixed,
            &world,
            &r,
            t,
            player,
            name,
            profile.is_some(),
            extras,
        );
        let over = chat.len() > MAX_PROMPT_CHARS;
        // Drop the least important memory first: relevant turns, then world events, then
        // recent turns, then the oldest turns of the conversation in progress.
        if over && !r.relevant.is_empty() {
            r.relevant.remove(0);
        } else if over && !world.is_empty() {
            world.remove(0);
        } else if over && !r.recent.is_empty() {
            r.recent.remove(0);
        } else if over && !r.current.is_empty() {
            r.current.remove(0);
        } else {
            return chat;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    fixed: &str,
    world: &[WorldEvent],
    r: &Recall,
    t: &TalkV2,
    player: &str,
    name: &str,
    has_profile: bool,
    extras: &Extras,
) -> Chat {
    let system = format!(
        "{fixed}{}\n{}\n\n{}",
        world_section(world),
        memories(r, player),
        rules(name, player, t, extras.actions)
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
    if !world.is_empty() {
        let _ = write!(
            fake,
            "; I know of {} events, lately: {}",
            world.len(),
            world[world.len() - 1].text
        );
    }
    if let Some(p) = &extras.plan {
        let _ = write!(fake, "; my aim: {}", p.goal);
    }
    fake += ".";
    // A test hook for --fake-llm: an "ACTION: ..." in the player's message is echoed, so the
    // game's action handling can be tried without a model. It is validated like any other.
    if let Some(i) = t.msg.to_ascii_lowercase().find("action:") {
        let _ = write!(fake, "\n{}", &t.msg[i..]);
    }
    Chat {
        messages,
        fake_reply: fake,
        max_tokens: TALK_MAX_TOKENS,
        json: false,
    }
}

/// The chat for a planning task (Milestone 6): what `character` knows, then the planner's
/// instructions. `turns` are its conversations with the player on the save's chain.
#[allow(clippy::too_many_arguments)]
pub fn plan_chat(
    character: &str,
    profile: &Profile,
    realm: Option<&crate::realms::Realm>,
    world: &[WorldEvent],
    turns: &[Turn],
    previous: Option<&Plan>,
    player: &str,
    day: u32,
) -> Chat {
    let player = if player.is_empty() {
        "the player"
    } else {
        player
    };
    let name = crate::names::names()
        .troop(character)
        .unwrap_or(&profile.name);
    let mut system = format!("You are {name}.\n{}", profile.render());
    if let Some(r) = realm {
        let _ = write!(
            system,
            "\n\nWhat you know as one of the {} ({}):\n{}",
            r.people,
            r.name,
            r.render()
        );
    }
    let _ = write!(system, "\n\nSetting: {SETTING}\n{}", world_section(world));
    let recent = &turns[turns.len().saturating_sub(4)..];
    if recent.is_empty() {
        let _ = write!(system, "\nYou have not spoken with {player} yet.\n");
    } else {
        let _ = writeln!(system, "\nYour latest conversations with {player}:");
        for t in recent {
            let _ = writeln!(system, "- {}", quote(t, player));
        }
    }
    if let Some(p) = previous {
        let _ = write!(
            system,
            "\nYour aims until now (day {}): goal: {} Plan: {}\n",
            p.day, p.goal, p.plan
        );
    }
    let _ = write!(
        system,
        "\n{}",
        crate::planner::instructions(name, player, day)
    );
    let fake_reply = serde_json::json!({
        "goal": format!("[fake] {name} means to hold what is theirs."),
        "plan": format!("[fake] Watch the {} events I know of.", world.len()),
        "act": {
            "kind": "letter",
            "text": format!("[fake] {name} writes to {player} on day {day}, knowing of {} events.", world.len()),
        },
    })
    .to_string();
    Chat {
        messages: vec![("system", system), ("user", "Decide now.".to_string())],
        fake_reply,
        max_tokens: PLAN_MAX_TOKENS,
        json: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::GameContext;

    const SWADIA: &str = r#"
id = "fac_kingdom_1"
name = "Kingdom of Swadia"
people = "Swadian"
[canon]
land = "Green hills and warhorses."
sources = ["module_strings.py: journey_to_praven"]
"#;
    const KHERGITS: &str = r#"
id = "fac_kingdom_3"
name = "Khergit Khanate"
people = "Khergit"
[mod]
culture = "Horse archers."
"#;

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
                wars: None,
                ruler_name: String::new(),
                spouse_name: String::new(),
                father_name: String::new(),
                npc_gold: None,
                player_gold: None,
            },
            msg: "Hail.".into(),
            frame: 1,
            world_head: None,
            head_outcome: 0,
        }
    }

    #[test]
    fn situation_states_only_what_the_game_sent() {
        let t = talk("trp_knight_1_1");
        let s = situation(&t, "Ylva", &Realms::default());
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
        for absent in [
            "prisoner",
            "married",
            "company",
            "exile",
            "liege",
            "father",
            "at war with",
        ] {
            assert!(!s.contains(absent), "{absent} in {s}");
        }
    }

    #[test]
    fn situation_names_liege_family_and_wars() {
        let realms = Realms::from_toml(&[SWADIA, KHERGITS]);
        let mut t = talk("trp_knight_1_1");
        let c = &mut t.context;
        c.status = 0;
        c.ruler_name = "King Harlaus".into();
        c.spouse_name = "Lady Ada".into();
        c.father_name = "Count Old".into();
        // Bits: 3 = fac_kingdom_3 (a file), 4 = fac_kingdom_4 (no file), 0 = the player's realm.
        c.wars = Some(1 << 3 | 1 << 4 | 1);
        c.player_faction = ids::ids()
            .factions
            .index("fac_player_supporters_faction")
            .unwrap();
        c.player_faction_name = "Ylvaland".into();
        let s = situation(&t, "Ylva", &realms);
        for part in [
            "You belong to Kingdom of Swadia.\n- Your liege is King Harlaus.",
            "You are married to Lady Ada.",
            "Your father: Count Old.",
            "Your realm is at war with Ylvaland, Khergit Khanate and fac_kingdom_4.",
        ] {
            assert!(s.contains(part), "{part:?} missing from {s}");
        }
        t.context.wars = Some(0);
        t.context.status = ST_FACTION_LEADER;
        let s = situation(&t, "Ylva", &realms);
        assert!(
            s.contains("at war with no other realm") && !s.contains("liege"),
            "{s}"
        );
        // Realm lore follows the profile for members of a realm with a file.
        let system =
            &v2_chat(&t, None, &realms, &Recall::default(), &Extras::default()).messages[0].1;
        assert!(
            system.contains(
                "What you know as one of the Swadian (Kingdom of Swadia):\nLand: Green hills"
            ),
            "{system}"
        );
    }

    #[test]
    fn generic_profile_uses_the_campaigns_reputation() {
        let chat = v2_chat(
            &talk("trp_knight_1_1"),
            None,
            &Realms::default(),
            &Recall::default(),
            &Extras::default(),
        );
        let system = &chat.messages[0].1;
        assert!(
            system.starts_with(
                "You are Count Klargus, a lord of Calradia.\nPersonality: quarrelsome"
            ),
            "{system}"
        );
        assert!(system.contains("do not invent past deeds or relatives beyond those named below"));
        assert!(system.contains("you remember no earlier conversation with Ylva"));
        assert_eq!(chat.messages.len(), 2);
        assert!(chat.fake_reply.contains("(no profile)"));
    }
}
