//! Display names of troops and parties, read from the Module System sources compiled into
//! the server (module_troops.py, module_parties.py).
//!
//! World events arrive as troop, party and faction indices; the names of lords, kings,
//! claimants, towns, castles and villages never change during a campaign, so the server can
//! name them itself instead of the game sending every name. The player and a kingdom the
//! player founds are named from what the game sends.

use std::collections::HashMap;
use std::sync::OnceLock;

const TROOPS: &[u8] = include_bytes!("../../game/module_system/module_troops.py");
const PARTIES: &[u8] = include_bytes!("../../game/module_system/module_parties.py");

pub struct Names {
    troops: HashMap<String, String>,
    parties: HashMap<String, String>,
}

/// Collects `<open>"id","Name"` definitions: `["knight_1_1","Count Klargus",...` in
/// module_troops.py and `("town_1","Sargoth",...` in module_parties.py. Underscores in
/// names stand for spaces (quick-string convention).
fn parse(source: &[u8], open: char, prefix: &str) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(source);
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix(open) else {
            continue;
        };
        let mut fields = rest.splitn(3, ',').map(str::trim);
        let (Some(id), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        let unquote = |s: &str| {
            s.strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(str::to_string)
        };
        let (Some(id), Some(name)) = (unquote(id), unquote(name)) else {
            continue;
        };
        if id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && !name.is_empty() {
            let name = name.replace('_', " ");
            out.entry(format!("{prefix}{id}")).or_insert(name);
        }
    }
    out
}

pub fn names() -> &'static Names {
    static NAMES: OnceLock<Names> = OnceLock::new();
    NAMES.get_or_init(|| Names {
        troops: parse(TROOPS, '[', "trp_"),
        parties: parse(PARTIES, '(', "p_"),
    })
}

impl Names {
    /// The name of a troop identifier such as `trp_knight_1_1`.
    pub fn troop(&self, id: &str) -> Option<&str> {
        self.troops.get(id).map(String::as_str)
    }

    /// The name of a party identifier such as `p_town_1`.
    pub fn party(&self, id: &str) -> Option<&str> {
        self.parties.get(id).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ids, troop_kind, Kind};

    #[test]
    fn names_every_character_and_settlement() {
        let n = names();
        assert_eq!(n.troop("trp_npc1"), Some("Borcha"));
        assert_eq!(n.troop("trp_kingdom_1_lord"), Some("King Harlaus"));
        assert_eq!(n.troop("trp_knight_1_1"), Some("Count Klargus"));
        assert_eq!(
            n.troop("trp_kingdom_6_pretender"),
            Some("Arwa the Pearled One")
        );
        assert_eq!(n.party("p_town_1"), Some("Sargoth"));
        assert_eq!(n.party("p_castle_1"), Some("Culmarr Castle"));
        assert_eq!(n.party("p_village_1"), Some("Yaragar"));
        let ids = ids();
        for i in 0..ids.troops.len() as u32 {
            let id = ids.troops.name(i).unwrap();
            if troop_kind(id) != Kind::Other {
                assert!(n.troop(id).is_some(), "{id} has no name");
            }
        }
        for i in 0..ids.parties.len() as u32 {
            let id = ids.parties.name(i).unwrap();
            if ["p_town_", "p_castle_", "p_village_"]
                .iter()
                .any(|p| id.starts_with(p))
            {
                assert!(n.party(id).is_some(), "{id} has no name");
            }
        }
    }
}
