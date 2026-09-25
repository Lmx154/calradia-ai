//! The Module System's identifier tables, compiled into the server.
//!
//! The game can only send numbers: a troop is sent as its index, a faction or a party as
//! its index. The stable identifiers (`trp_npc1`, `fac_kingdom_1`, `p_town_1`) come from the
//! ID files that the build uses. The CalradiaAI overlay does not replace these three files
//! (tests/test_calradia_ai.py checks that), so the vanilla ones are the right ones.

use std::sync::OnceLock;

const ID_TROOPS: &str = include_str!("../../game/module_system/ID_troops.py");
const ID_FACTIONS: &str = include_str!("../../game/module_system/ID_factions.py");
const ID_PARTIES: &str = include_str!("../../game/module_system/ID_parties.py");

/// One ID file: index -> identifier, in order.
#[derive(Debug)]
pub struct Table {
    names: Vec<String>,
}

impl Table {
    /// Parses `prefix_name = N` lines. The indices must be 0, 1, 2, ... in order, which is
    /// how the Module System writes them.
    pub fn parse(text: &str, prefix: &str) -> Result<Table, String> {
        let mut names = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (name, index) = line
                .split_once('=')
                .map(|(n, i)| (n.trim(), i.trim()))
                .ok_or_else(|| format!("bad ID line {line:?}"))?;
            if !name.starts_with(prefix) || index.parse::<usize>() != Ok(names.len()) {
                return Err(format!("unexpected ID line {line:?}"));
            }
            names.push(name.to_string());
        }
        Ok(Table { names })
    }

    pub fn name(&self, index: u32) -> Option<&str> {
        self.names.get(index as usize).map(String::as_str)
    }

    pub fn index(&self, name: &str) -> Option<u32> {
        self.names.iter().position(|n| n == name).map(|i| i as u32)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }
}

pub struct Ids {
    pub troops: Table,
    pub factions: Table,
    pub parties: Table,
}

/// The embedded tables. They are part of the source tree, so a parse failure is a bug.
pub fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| Ids {
        troops: Table::parse(ID_TROOPS, "trp_").expect("ID_troops.py"),
        factions: Table::parse(ID_FACTIONS, "fac_").expect("ID_factions.py"),
        parties: Table::parse(ID_PARTIES, "p_").expect("ID_parties.py"),
    })
}

/// What kind of character a troop is, from its identifier (module_constants.py ranges).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Companion,
    King,
    Lord,
    Pretender,
    Lady,
    Other,
}

pub fn troop_kind(id: &str) -> Kind {
    let rest = |p: &str| id.strip_prefix(p);
    if rest("trp_npc").is_some_and(|n| n.parse::<u32>().is_ok()) {
        Kind::Companion
    } else if id.starts_with("trp_kingdom_") && id.ends_with("_lord") {
        Kind::King
    } else if id.starts_with("trp_kingdom_") && id.ends_with("_pretender") {
        Kind::Pretender
    } else if id.starts_with("trp_kingdom_") && id.contains("_lady_")
        || id.starts_with("trp_knight_") && (id.ends_with("_wife") || id.ends_with("_daughter"))
    {
        Kind::Lady
    } else if id.starts_with("trp_knight_") {
        Kind::Lord
    } else {
        Kind::Other
    }
}

/// What kind of place a party is, from its identifier.
pub fn place_kind(id: &str) -> &'static str {
    if id.starts_with("p_town_") {
        "town"
    } else if id.starts_with("p_castle_") {
        "castle"
    } else if id.starts_with("p_village_") {
        "village"
    } else {
        "place"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tables_parse_with_known_ids() {
        let ids = ids();
        assert_eq!(ids.troops.name(0), Some("trp_player"));
        for (troop, kind) in [
            ("trp_npc1", Kind::Companion),
            ("trp_npc16", Kind::Companion),
            ("trp_kingdom_1_lord", Kind::King),
            ("trp_kingdom_4_lord", Kind::King),
            ("trp_knight_1_1", Kind::Lord),
            ("trp_kingdom_1_pretender", Kind::Pretender),
            ("trp_knight_1_1_wife", Kind::Lady),
            ("trp_kingdom_1_lady_1", Kind::Lady),
            ("trp_player", Kind::Other),
        ] {
            let i = ids.troops.index(troop).unwrap_or_else(|| panic!("{troop}"));
            assert_eq!(ids.troops.name(i), Some(troop));
            assert_eq!(troop_kind(troop), kind, "{troop}");
        }
        assert_eq!(ids.factions.name(0), Some("fac_no_faction"));
        assert_eq!(ids.factions.index("fac_player_faction"), Some(13));
        assert_eq!(ids.factions.index("fac_kingdom_1"), Some(15));
        assert_eq!(ids.parties.name(0), Some("p_main_party"));
        assert_eq!(
            place_kind(
                ids.parties
                    .name(ids.parties.index("p_town_1").unwrap())
                    .unwrap()
            ),
            "town"
        );
        assert!(ids.troops.len() > 1000 && ids.parties.len() > 200);
    }

    #[test]
    fn rejects_malformed_tables() {
        assert!(Table::parse("trp_a = 0\ntrp_b = 2\n", "trp_").is_err());
        assert!(Table::parse("trp_a = 0\nfac_b = 1\n", "trp_").is_err());
        assert!(Table::parse("trp_a 0\n", "trp_").is_err());
        assert_eq!(Table::parse("\ntrp_a = 0\n\n", "trp_").unwrap().len(), 1);
    }
}
