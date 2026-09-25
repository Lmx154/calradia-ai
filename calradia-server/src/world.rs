//! World awareness (Milestone 4): what happens in Calradia outside conversations.
//!
//! The game forwards two kinds of world nodes (protocol.rs `Payload`):
//! - every entry of Native's own event log (`script_add_log_entry`): the player's battles
//!   and sieges, lords' quarrels and insults, war declarations and their reasons, pledges,
//!   fief grants, marriages. Each becomes one sentence, from `world/log_entries.toml`.
//! - a daily snapshot of the realms, wars, walled-center owners and the allegiance and
//!   captivity of every king, lord and claimant. Comparing a snapshot with the previous one
//!   on the same chain yields what the log misses: castles and towns changing hands, wars
//!   and peace, lords defecting, lords taken prisoner or released, realms falling.
//!
//! Nodes are chained per savegame exactly like conversation turns (memory.rs), so a reloaded
//! save only knows the events of its own past. This module is pure: rendering, diffing and
//! selecting; storage is in memory.rs.

use crate::ids::{self, place_kind};
use crate::names::names;
use crate::protocol::{LogEntry, Snapshot};
use crate::realms::Realms;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// A fact of the world, as NPCs may recall it.
#[derive(Clone, Debug, PartialEq)]
pub struct WorldEvent {
    pub day: u32,
    /// `log:<type>` or `snapshot:<what>`.
    pub kind: String,
    /// One sentence without the day.
    pub text: String,
    /// Troops personally involved (their own memory), e.g. `trp_knight_1_1`, `trp_player`.
    pub troops: Vec<String>,
    /// Realms for which this is news of their own realm, e.g. `fac_kingdom_1`.
    pub factions: Vec<String>,
    /// 1 gossip, 2 notable, 3 major.
    pub importance: u8,
}

// ---------------------------------------------------------------- log entries

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogTemplate {
    #[serde(rename = "type")]
    pub kind: u32,
    pub name: String,
    pub text: String,
    #[serde(default)]
    pub involves: Vec<String>,
    #[serde(default)]
    pub realms: Vec<String>,
    pub importance: u8,
    #[serde(default)]
    pub skip: bool,
    /// What `actor` holds: "troop" (default) or "faction" (war declarations).
    #[serde(default)]
    pub actor: Option<String>,
    /// Render only if these fields are -1 (type 3 is two events; see its note).
    #[serde(default)]
    pub only_if_unset: Vec<String>,
    /// Why the sentence reads as it does (for editors; not used).
    #[serde(default)]
    #[allow(dead_code)]
    pub note: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogFile {
    entry: Vec<LogTemplate>,
}

const TROOP_FIELDS: [&str; 3] = ["actor", "troop", "center_lord"];
const FACTION_FIELDS: [&str; 3] = ["faction", "center_faction", "troop_faction"];

#[derive(Debug, Default)]
pub struct LogTemplates {
    by_kind: HashMap<u32, LogTemplate>,
}

impl LogTemplates {
    pub fn load(path: &Path) -> Result<LogTemplates, String> {
        let bad = |e: String| format!("{}: {e}", path.display());
        let text = std::fs::read_to_string(path).map_err(|e| bad(e.to_string()))?;
        Self::parse(&text).map_err(bad)
    }

    pub fn parse(text: &str) -> Result<LogTemplates, String> {
        let file: LogFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut by_kind = HashMap::new();
        for t in file.entry {
            let placeholders = placeholders(&t.text);
            let known =
                |p: &str| p == "center" || TROOP_FIELDS.contains(&p) || FACTION_FIELDS.contains(&p);
            if let Some(p) = placeholders.iter().find(|p| !known(p)) {
                return Err(format!("type {}: unknown placeholder {{{p}}}", t.kind));
            }
            let actor_faction = match t.actor.as_deref() {
                None | Some("troop") => false,
                Some("faction") => true,
                Some(other) => return Err(format!("type {}: actor {other:?}", t.kind)),
            };
            let is_troop = |f: &str| TROOP_FIELDS.contains(&f) && !(actor_faction && f == "actor");
            let is_faction =
                |f: &str| FACTION_FIELDS.contains(&f) || (actor_faction && f == "actor");
            if let Some(f) = t.involves.iter().find(|f| !is_troop(f)) {
                return Err(format!(
                    "type {}: involves {f:?} is not a troop field",
                    t.kind
                ));
            }
            if let Some(f) = t.realms.iter().find(|f| !is_faction(f)) {
                return Err(format!(
                    "type {}: realms {f:?} is not a faction field",
                    t.kind
                ));
            }
            let fields = |f: &String| {
                f == "center"
                    || TROOP_FIELDS.contains(&f.as_str())
                    || FACTION_FIELDS.contains(&f.as_str())
            };
            if let Some(f) = t.only_if_unset.iter().find(|f| !fields(f)) {
                return Err(format!(
                    "type {}: only_if_unset {f:?} is not a field",
                    t.kind
                ));
            }
            if !(1..=3).contains(&t.importance) || !t.text.is_ascii() {
                return Err(format!("type {}: importance 1..3 and ASCII text", t.kind));
            }
            let kind = t.kind;
            if by_kind.insert(kind, t).is_some() {
                return Err(format!("type {kind} appears twice"));
            }
        }
        Ok(LogTemplates { by_kind })
    }

    pub fn len(&self) -> usize {
        self.by_kind.len()
    }
}

fn placeholders(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { break };
        out.push(rest[i + 1..i + j].to_string());
        rest = &rest[i + j + 1..];
    }
    out
}

/// Names indices as the player sees them.
pub struct Namer<'a> {
    pub player: &'a str,
    /// The name of a realm the player founded (`fac_player_supporters_faction`), if known.
    pub player_realm: &'a str,
    pub realms: &'a Realms,
}

impl Namer<'_> {
    pub fn troop(&self, index: i32) -> Option<String> {
        let id = ids::ids().troops.name(u32::try_from(index).ok()?)?;
        if id == "trp_player" {
            return Some(
                if self.player.is_empty() {
                    "the player"
                } else {
                    self.player
                }
                .into(),
            );
        }
        names().troop(id).map(str::to_string)
    }

    pub fn party(&self, index: i32) -> Option<String> {
        let index = u32::try_from(index).ok().filter(|&i| i > 0)?;
        names()
            .party(ids::ids().parties.name(index)?)
            .map(str::to_string)
    }

    pub fn faction(&self, index: i32) -> Option<String> {
        let id = ids::ids()
            .factions
            .name(u32::try_from(index).ok().filter(|&i| i > 0)?)?;
        if let Some(realm) = self.realms.get(id) {
            return Some(format!("the {}", realm.name));
        }
        Some(match id {
            "fac_player_supporters_faction" | "fac_player_faction" => {
                if self.player_realm.is_empty() {
                    format!("the realm of {}", self.troop(0).unwrap_or_default())
                } else {
                    self.player_realm.to_string()
                }
            }
            other => {
                let plain = other.trim_start_matches("fac_").replace('_', " ");
                format!("the {plain}")
            }
        })
    }
}

fn troop_id(index: i32) -> Option<String> {
    ids::ids()
        .troops
        .name(u32::try_from(index).ok()?)
        .map(str::to_string)
}

fn faction_id(index: i32) -> Option<String> {
    ids::ids()
        .factions
        .name(u32::try_from(index).ok().filter(|&i| i > 0)?)
        .map(str::to_string)
}

/// The sentence for a log entry, or None if its type is skipped, unknown, or names something
/// the server cannot name.
pub fn render_log(e: &LogEntry, templates: &LogTemplates, namer: &Namer) -> Option<WorldEvent> {
    let t = templates.by_kind.get(&e.kind).filter(|t| !t.skip)?;
    let actor_faction = t.actor.as_deref() == Some("faction");
    let field = |f: &str| -> i32 {
        match f {
            "actor" => e.actor,
            "troop" => e.troop,
            "center_lord" => e.center_lord,
            "center" => e.center,
            "faction" => e.faction,
            "center_faction" => e.center_faction,
            _ => e.troop_faction,
        }
    };
    if t.only_if_unset.iter().any(|f| field(f) != -1) {
        return None;
    }
    let mut text = t.text.clone();
    for p in placeholders(&t.text) {
        let value = field(&p);
        let named = if p == "center" {
            namer.party(value)
        } else if p == "actor" && actor_faction {
            namer.faction(value)
        } else if TROOP_FIELDS.contains(&p.as_str()) {
            namer.troop(value)
        } else {
            namer.faction(value)
        }?;
        text = text.replace(&format!("{{{p}}}"), &named);
    }
    let mut text = capitalize(&text);
    text.truncate(240);
    Some(WorldEvent {
        day: e.hours / 24,
        kind: format!("log:{}", t.name),
        text,
        troops: t
            .involves
            .iter()
            .filter_map(|f| troop_id(field(f)))
            .collect(),
        factions: t.realms.iter().filter_map(|f| faction_id(field(f))).fold(
            Vec::new(),
            |mut all, f| {
                if !all.contains(&f) {
                    all.push(f);
                }
                all
            },
        ),
        importance: t.importance,
    })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- snapshots

/// The events between two consecutive snapshots of one chain (none for the first).
pub fn diff(prev: Option<&Snapshot>, next: &Snapshot, day: u32, namer: &Namer) -> Vec<WorldEvent> {
    let Some(prev) = prev else { return Vec::new() };
    let ids = ids::ids();
    let first_realm = ids
        .factions
        .index("fac_player_supporters_faction")
        .expect("vanilla") as i32;
    let first_center = ids.parties.index("p_town_1").expect("vanilla") as i32;
    let first_lord = ids.troops.index("trp_kingdom_1_lord").expect("vanilla") as i32;
    let realm = |k: usize| first_realm + k as i32;
    let mut out = Vec::new();
    let mut push =
        |kind: &str, text: String, troops: Vec<String>, factions: Vec<i32>, importance| {
            out.push(WorldEvent {
                day,
                kind: format!("snapshot:{kind}"),
                text: capitalize(&text),
                troops,
                factions: factions.into_iter().filter_map(faction_id).collect(),
                importance,
            });
        };
    let (realm_count, _, _) = crate::protocol::snapshot_shape();
    for k in 0..realm_count {
        let (was, is) = (prev.alive >> k & 1, next.alive >> k & 1);
        let Some(name) = namer.faction(realm(k)) else {
            continue;
        };
        if was == 1 && is == 0 {
            push(
                "realm_fell",
                format!("{name} has fallen."),
                vec![],
                vec![realm(k)],
                3,
            );
        } else if was == 0 && is == 1 {
            push(
                "realm_rose",
                format!("{name} has risen as a realm."),
                vec![],
                vec![realm(k)],
                3,
            );
        }
    }
    let mut pair = 0;
    for i in 0..realm_count {
        for j in i + 1..realm_count {
            let (was, is) = (prev.wars.get(pair), next.wars.get(pair));
            pair += 1;
            let (Some(&was), Some(&is)) = (was, is) else {
                continue;
            };
            if was == is {
                continue;
            }
            let (Some(a), Some(b)) = (namer.faction(realm(i)), namer.faction(realm(j))) else {
                continue;
            };
            let (kind, text) = if is == 1 {
                ("war", format!("{a} and {b} went to war."))
            } else {
                ("peace", format!("{a} and {b} made peace."))
            };
            push(kind, text, vec![], vec![realm(i), realm(j)], 3);
        }
    }
    for (i, (&was, &is)) in prev.owners.iter().zip(&next.owners).enumerate() {
        if was == is {
            continue;
        }
        let center = first_center + i as i32;
        let (Some(place), Some(to)) = (namer.party(center), namer.faction(is as i32)) else {
            continue;
        };
        let kind = place_kind(ids.parties.name(center as u32).unwrap_or(""));
        let text = match namer.faction(was as i32) {
            Some(from) => format!("The {kind} of {place} passed from {from} to {to}."),
            None => format!("The {kind} of {place} passed to {to}."),
        };
        let importance = if kind == "town" { 3 } else { 2 };
        push(
            "center",
            text,
            vec![],
            vec![was as i32, is as i32],
            importance,
        );
    }
    for (i, (&was, &is)) in prev.lords.iter().zip(&next.lords).enumerate() {
        let lord = first_lord + i as i32;
        let (Some(name), Some(id)) = (namer.troop(lord), troop_id(lord)) else {
            continue;
        };
        let (was_faction, is_faction) = ((was / 2) as i32, (is / 2) as i32);
        if was_faction != is_faction {
            let text = match (namer.faction(was_faction), namer.faction(is_faction)) {
                (Some(from), Some(to)) => format!("{name} left {from} and joined {to}."),
                (Some(from), None) => format!("{name} left {from}."),
                (None, Some(to)) => format!("{name} joined {to}."),
                (None, None) => continue,
            };
            push(
                "allegiance",
                text,
                vec![id.clone()],
                vec![was_faction, is_faction],
                3,
            );
        }
        match (was & 1, is & 1) {
            (0, 1) => push(
                "captured",
                format!("{name} was taken prisoner."),
                vec![id],
                vec![is_faction],
                2,
            ),
            (1, 0) => push(
                "released",
                format!("{name} was released from captivity."),
                vec![id],
                vec![is_faction],
                2,
            ),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------- recall

/// At most this many world events go into one prompt.
pub const MAX_RECALLED_EVENTS: usize = 10;

/// What `character` of `faction` knows of the world, oldest first: events that involved
/// them personally (their last 5), news of their realm (last 4), what the player did (last
/// 3 notable), and major events of the last 30 days (last 3), without repeats.
pub fn recall(
    events: &[WorldEvent],
    character: &str,
    faction: &str,
    today: u32,
) -> Vec<WorldEvent> {
    let mut picked: Vec<usize> = Vec::new();
    let take = |pred: &dyn Fn(&WorldEvent) -> bool, n: usize, picked: &mut Vec<usize>| {
        let chosen: Vec<usize> = events
            .iter()
            .enumerate()
            .rev()
            .filter(|(i, e)| !picked.contains(i) && pred(e))
            .take(n)
            .map(|(i, _)| i)
            .collect();
        picked.extend(chosen);
    };
    take(&|e| e.troops.iter().any(|t| t == character), 5, &mut picked);
    if !faction.is_empty() {
        take(&|e| e.factions.iter().any(|f| f == faction), 4, &mut picked);
    }
    take(
        &|e| e.importance >= 2 && e.troops.iter().any(|t| t == "trp_player"),
        3,
        &mut picked,
    );
    take(
        &|e| e.importance >= 3 && e.day + 30 >= today,
        3,
        &mut picked,
    );
    picked.sort_unstable();
    picked.truncate(MAX_RECALLED_EVENTS);
    picked.into_iter().map(|i| events[i].clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATES: &str = r#"
[[entry]]
type = 11
name = "logent_lord_defeated_by_player"
text = "{actor} defeated {troop} in battle."
involves = ["actor", "troop"]
realms = ["troop_faction"]
importance = 2
[[entry]]
type = 5
name = "logent_party_traded"
text = "{actor} traded."
importance = 1
skip = true
[[entry]]
type = 92
name = "logent_faction_declares_war_to_regain_territory"
text = "{actor} declared war on {faction} to regain lost territory."
actor = "faction"
realms = ["actor", "faction"]
importance = 3
[[entry]]
type = 3
name = "logent_caravan_accosted"
text = "{actor} waylaid travellers of {faction}."
involves = ["actor"]
realms = ["faction"]
only_if_unset = ["center"]
importance = 1
[[entry]]
type = 22
name = "logent_liege_grants_fief_to_vassal"
text = "{actor} granted {center} to {troop}."
involves = ["actor", "troop"]
realms = ["center_faction"]
importance = 2
"#;

    fn idx(table: &ids::Table, id: &str) -> i32 {
        table.index(id).unwrap() as i32
    }

    fn namer(realms: &Realms) -> Namer<'_> {
        Namer {
            player: "Ylva",
            player_realm: "",
            realms,
        }
    }

    #[test]
    fn log_entries_render_with_names_and_relevance() {
        let ids = ids::ids();
        let t = LogTemplates::parse(TEMPLATES).unwrap();
        let realms = Realms::default();
        let n = namer(&realms);
        let e = LogEntry {
            index: 1,
            kind: 11,
            hours: 24 * 7 + 5,
            actor: 0,
            center: -1,
            center_lord: -1,
            center_faction: -1,
            troop: idx(&ids.troops, "trp_knight_1_1"),
            troop_faction: idx(&ids.factions, "fac_kingdom_1"),
            faction: -1,
        };
        let ev = render_log(&e, &t, &n).unwrap();
        assert_eq!(ev.day, 7);
        assert_eq!(ev.text, "Ylva defeated Count Klargus in battle.");
        assert_eq!(ev.troops, ["trp_player", "trp_knight_1_1"]);
        assert_eq!(ev.factions, ["fac_kingdom_1"]);
        let grant = LogEntry {
            kind: 22,
            actor: idx(&ids.troops, "trp_kingdom_1_lord"),
            center: idx(&ids.parties, "p_castle_1"),
            center_faction: idx(&ids.factions, "fac_kingdom_1"),
            ..e.clone()
        };
        assert_eq!(
            render_log(&grant, &t, &n).unwrap().text,
            "King Harlaus granted Culmarr Castle to Count Klargus."
        );
        let war = LogEntry {
            kind: 92,
            actor: idx(&ids.factions, "fac_kingdom_3"),
            faction: idx(&ids.factions, "fac_kingdom_2"),
            ..e.clone()
        };
        let war = render_log(&war, &t, &n).unwrap();
        assert_eq!(
            war.text,
            "The kingdom 3 declared war on the kingdom 2 to regain lost territory."
        );
        assert_eq!(
            (war.troops.len(), war.factions.clone()),
            (
                0,
                vec!["fac_kingdom_3".to_string(), "fac_kingdom_2".to_string()]
            )
        );
        let accosted = LogEntry {
            kind: 3,
            center: idx(&ids.parties, "p_town_1"),
            faction: idx(&ids.factions, "fac_kingdom_2"),
            ..e.clone()
        };
        assert!(
            render_log(&accosted, &t, &n).is_none(),
            "center is set: the traveller variant"
        );
        let accosted = LogEntry {
            center: -1,
            ..accosted
        };
        assert_eq!(
            render_log(&accosted, &t, &n).unwrap().text,
            "Ylva waylaid travellers of the kingdom 2."
        );
        // Skipped, unknown and unnamable entries give nothing.
        assert!(render_log(
            &LogEntry {
                kind: 5,
                ..e.clone()
            },
            &t,
            &n
        )
        .is_none());
        assert!(render_log(
            &LogEntry {
                kind: 99,
                ..e.clone()
            },
            &t,
            &n
        )
        .is_none());
        assert!(render_log(&LogEntry { troop: -1, ..e }, &t, &n).is_none());
        for bad in [
            TEMPLATES.replace("{troop} in", "{nobody} in"),
            TEMPLATES.replace("type = 5", "type = 11"),
            TEMPLATES.replace("realms = [\"troop_faction\"]", "realms = [\"actor\"]"),
            TEMPLATES.replace("importance = 1\nskip", "importance = 4\nskip"),
            TEMPLATES.replace("actor = \"faction\"", "actor = \"party\""),
            TEMPLATES.replace(
                "only_if_unset = [\"center\"]",
                "only_if_unset = [\"nothing\"]",
            ),
        ] {
            assert!(LogTemplates::parse(&bad).is_err());
        }
    }

    fn snapshot(owners: Vec<u32>, lords: Vec<u32>) -> Snapshot {
        Snapshot {
            alive: 0b111_1110,
            wars: vec![0; 21],
            owners,
            lords,
        }
    }

    #[test]
    fn snapshot_diffs_report_what_changed() {
        let ids = ids::ids();
        let realms = Realms::default();
        let n = namer(&realms);
        let (_, centers, lords) = crate::protocol::snapshot_shape();
        assert_eq!((centers, lords), (70, 132));
        let swadia = idx(&ids.factions, "fac_kingdom_1") as u32;
        let vaegirs = idx(&ids.factions, "fac_kingdom_2") as u32;
        let a = snapshot(vec![swadia; centers], vec![swadia * 2; lords]);
        assert!(diff(None, &a, 1, &n).is_empty());
        assert!(diff(Some(&a), &a, 1, &n).is_empty());
        let mut b = a.clone();
        b.owners[0] = vaegirs; // p_town_1, Sargoth
        b.lords[6] = vaegirs * 2 + 1; // trp_knight_1_1: defected, and captured
        b.wars[6] = 1; // pairs (0, 1)..(0, 6) come first, so (1, 2) is 6: kingdoms 1 and 2
        b.alive &= !(1 << 6); // kingdom_6 fell
        let texts: Vec<String> = diff(Some(&a), &b, 9, &n)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            texts,
            [
                "The kingdom 6 has fallen.",
                "The kingdom 1 and the kingdom 2 went to war.",
                "The town of Sargoth passed from the kingdom 1 to the kingdom 2.",
                "Count Klargus left the kingdom 1 and joined the kingdom 2.",
                "Count Klargus was taken prisoner.",
            ]
        );
        let back = diff(Some(&b), &a, 10, &n);
        assert!(back
            .iter()
            .any(|e| e.text == "The kingdom 1 and the kingdom 2 made peace."));
        assert!(back
            .iter()
            .any(|e| e.text == "Count Klargus was released from captivity."));
        assert!(back.iter().all(|e| e.day == 10));
    }

    #[test]
    fn recall_picks_personal_realm_player_and_major_news() {
        let ev = |day, troops: &[&str], factions: &[&str], importance| WorldEvent {
            day,
            kind: "k".into(),
            text: format!("event {day}"),
            troops: troops.iter().map(|s| s.to_string()).collect(),
            factions: factions.iter().map(|s| s.to_string()).collect(),
            importance,
        };
        let mut events = vec![ev(1, &["trp_knight_1_1"], &[], 2)];
        events.extend((2..20).map(|d| ev(d, &[], &["fac_kingdom_3"], 1)));
        events.push(ev(20, &[], &["fac_kingdom_1"], 1));
        events.push(ev(21, &["trp_player"], &[], 2));
        events.push(ev(22, &[], &[], 3));
        events.push(ev(23, &[], &[], 1));
        let r = recall(&events, "trp_knight_1_1", "fac_kingdom_1", 40);
        let days: Vec<u32> = r.iter().map(|e| e.day).collect();
        assert_eq!(days, [1, 20, 21, 22]);
        // Old major news is forgotten after 30 days; limits hold.
        assert_eq!(recall(&events, "x", "", 60).len(), 1);
        assert!(recall(&events, "x", "fac_kingdom_3", 40).len() <= MAX_RECALLED_EVENTS);
    }
}
