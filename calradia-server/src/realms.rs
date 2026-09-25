//! Kingdom lore: one TOML file per realm in a directory (default
//! `calradia-server/factions/`), keyed by the faction's Module System identifier
//! (`fac_kingdom_1.toml`). Vanilla lords and ladies have no individual lore -- their
//! personalities and families are rolled per campaign and arrive live -- so every member of
//! a realm shares its lore: land, people, ruler and claimant, and old rivalries with the
//! other realms. As with character profiles, `[canon]` (with `sources`) is kept apart from
//! what was written for this mod (`[mod]`).

use crate::ids;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Lore {
    pub land: Option<String>,
    pub culture: Option<String>,
    pub politics: Option<String>,
    #[serde(default)]
    pub neighbours: Vec<String>,
    /// `[canon]` only.
    #[serde(default)]
    pub sources: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Realm {
    /// The faction identifier, e.g. `fac_kingdom_1`.
    pub id: String,
    /// The realm's name, e.g. "Kingdom of Swadia".
    pub name: String,
    /// The people's adjective, e.g. "Swadian".
    pub people: String,
    pub canon: Option<Lore>,
    #[serde(rename = "mod", default)]
    pub mod_lore: Lore,
}

impl Realm {
    fn both(&self) -> impl Iterator<Item = &Lore> {
        self.canon.iter().chain([&self.mod_lore])
    }

    /// The lore as prompt text, canon before mod.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut field = |label: &str, items: Vec<&str>| {
            if !items.is_empty() {
                let text = items
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                out += &format!("{label}: {text}\n");
            }
        };
        let text = |f: fn(&Lore) -> &Option<String>| -> Vec<&str> {
            self.both().filter_map(|l| f(l).as_deref()).collect()
        };
        field("Land", text(|l| &l.land));
        field("People and customs", text(|l| &l.culture));
        field("Rule", text(|l| &l.politics));
        field(
            "Neighbours",
            self.both()
                .flat_map(|l| l.neighbours.iter().map(String::as_str))
                .collect(),
        );
        out.truncate(out.trim_end().len());
        out
    }

    fn validate(&self, stem: &str) -> Result<(), String> {
        if self.id != stem {
            return Err(format!("id {:?} does not match the file name", self.id));
        }
        if ids::ids().factions.index(&self.id).is_none() {
            return Err(format!("{} is not a faction in ID_factions.py", self.id));
        }
        if self.name.trim().is_empty() || self.people.trim().is_empty() {
            return Err("name and people must not be empty".into());
        }
        if self.canon.as_ref().is_some_and(|c| c.sources.is_empty()) {
            return Err("[canon] needs sources".into());
        }
        if !self.mod_lore.sources.is_empty() {
            return Err("sources belong in [canon]; [mod] lore is the mod's own".into());
        }
        if self.render().is_empty() {
            return Err("no lore".into());
        }
        if !(self.render() + &self.name + &self.people).is_ascii() {
            return Err("lore must be plain ASCII".into());
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct Realms {
    realms: BTreeMap<String, Realm>,
}

impl Realms {
    /// Reads every `*.toml` in `dir`. Any bad file is an error that names it.
    pub fn load(dir: &Path) -> Result<Realms, String> {
        let entries =
            fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        let mut realms = Realms::default();
        for path in paths {
            let bad = |e: String| format!("{}: {e}", path.display());
            let text = fs::read_to_string(&path).map_err(|e| bad(e.to_string()))?;
            let realm: Realm = toml::from_str(&text).map_err(|e| bad(e.to_string()))?;
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            realm.validate(stem).map_err(bad)?;
            realms.realms.insert(realm.id.clone(), realm);
        }
        Ok(realms)
    }

    pub fn get(&self, faction: &str) -> Option<&Realm> {
        self.realms.get(faction)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.realms.keys().map(String::as_str)
    }

    /// Realms from TOML texts, for tests.
    #[cfg(test)]
    pub fn from_toml(texts: &[&str]) -> Realms {
        let realms = texts
            .iter()
            .map(|t| toml::from_str::<Realm>(t).unwrap())
            .map(|r| (r.id.clone(), r))
            .collect();
        Realms { realms }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("calradia-realms-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const GOOD: &str = r#"
id = "fac_kingdom_1"
name = "Kingdom of Swadia"
people = "Swadian"
[canon]
land = """Rolling hills
and warhorses."""
neighbours = ["Vaegirs: raiders.", "Khergits: horsemen."]
sources = ["module_strings.py: journey_to_praven"]
[mod]
culture = "Tournaments."
"#;

    #[test]
    fn loads_and_renders_realms() {
        let dir = temp_dir("good");
        fs::write(dir.join("fac_kingdom_1.toml"), GOOD).unwrap();
        let r = Realms::load(&dir).unwrap();
        assert_eq!(r.ids().collect::<Vec<_>>(), ["fac_kingdom_1"]);
        assert_eq!(
            r.get("fac_kingdom_1").unwrap().render(),
            "Land: Rolling hills and warhorses.\nPeople and customs: Tournaments.\n\
             Neighbours: Vaegirs: raiders. Khergits: horsemen."
        );
        assert!(r.get("fac_kingdom_2").is_none());
    }

    #[test]
    fn shipped_realms_cover_every_kingdom_by_its_vanilla_name() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("factions");
        let r = Realms::load(&dir).unwrap();
        // module_factions.py and kingdom_N_adjective in module_strings.py.
        for (n, name, people) in [
            (1, "Kingdom of Swadia", "Swadian"),
            (2, "Kingdom of Vaegirs", "Vaegir"),
            (3, "Khergit Khanate", "Khergit"),
            (4, "Kingdom of Nords", "Nord"),
            (5, "Kingdom of Rhodoks", "Rhodok"),
            (6, "Sarranid Sultanate", "Sarranid"),
        ] {
            let realm = r.get(&format!("fac_kingdom_{n}")).unwrap();
            assert_eq!((realm.name.as_str(), realm.people.as_str()), (name, people));
            assert!(realm.canon.is_some(), "fac_kingdom_{n} has no canon");
            let len = realm.render().len();
            assert!(
                (300..1600).contains(&len),
                "fac_kingdom_{n} is {len} characters"
            );
        }
    }

    #[test]
    fn rejects_bad_realms_naming_the_file() {
        let cases = [
            (
                "fac_kingdom_1",
                GOOD.replace("fac_kingdom_1", "fac_kingdom_2"),
                "does not match",
            ),
            (
                "fac_nowhere",
                GOOD.replace("fac_kingdom_1", "fac_nowhere"),
                "not a faction",
            ),
            (
                "fac_kingdom_1",
                GOOD.replace("neighbours", "neighbors"),
                "unknown field",
            ),
            (
                "fac_kingdom_1",
                GOOD.replace("people = \"Swadian\"", "people = \"\""),
                "empty",
            ),
            (
                "fac_kingdom_1",
                GOOD.replace("[mod]", "[mod]\nsources = [\"x\"]"),
                "belong in [canon]",
            ),
        ];
        for (i, (stem, text, expected)) in cases.into_iter().enumerate() {
            let dir = temp_dir(&format!("bad{i}"));
            fs::write(dir.join(format!("{stem}.toml")), text).unwrap();
            let err = Realms::load(&dir).unwrap_err();
            assert!(err.contains(expected), "case {i}: {err}");
            assert!(err.contains(&format!("{stem}.toml")), "case {i}: {err}");
        }
    }
}
