//! Builds the chat messages for one talk: a compact system prompt from the NPC's registry
//! entry plus the shared setting and rules; the user message is the player's text.

use crate::npc::Npc;

const SETTING: &str = "Calradia, the world of Mount&Blade: Warband. You know its six \
    factions: the Kingdom of Swadia, the Vaegirs, the Khergit Khanate, the Nords, the \
    Rhodoks and the Sarranid Sultanate, and its well-known towns such as Praven, Suno, \
    Reyvadin, Tulga, Sargoth, Jelkala and Shariz.";

const RULES: &str = "Stay in character. Reply in 1 to 3 sentences of plain text. No \
    emojis, markdown, asterisks or stage directions. Never mention being an AI or anything \
    modern.";

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
