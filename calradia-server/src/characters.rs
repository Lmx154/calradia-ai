//! The character registry: one TOML file per character in a directory (default
//! `calradia-server/characters/`), read at startup, so profiles can be edited without
//! recompiling. A file is keyed by the troop's stable Module System identifier, which is
//! also its file name: `trp_npc1.toml`.
//!
//! Each profile keeps what comes from the vanilla game (`[canon]`, with `sources`) apart
//! from what was written for this mod (`[mod]`). Characters without a file still talk,
//! with a generic profile built from live game data (prompt.rs).

use crate::ids::{self, troop_kind, Kind};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Traits {
    pub background: Option<String>,
    pub personality: Option<String>,
    #[serde(default)]
    pub motivations: Vec<String>,
    pub speaking_style: Option<String>,
    #[serde(default)]
    pub relationships: Vec<String>,
    #[serde(default)]
    pub tendencies: Vec<String>,
    /// `[canon]` only: where each claim comes from, e.g. "module_strings.py: npc1_intro".
    #[serde(default)]
    pub sources: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The troop identifier, e.g. `trp_npc1`.
    pub id: String,
    pub name: String,
    pub canon: Option<Traits>,
    #[serde(rename = "mod", default)]
    pub mod_traits: Traits,
}

impl Profile {
    /// Canon first, then the mod's traits.
    fn both(&self) -> impl Iterator<Item = &Traits> {
        self.canon.iter().chain([&self.mod_traits])
    }

    /// The profile as prompt text: one line or list per field, canon before mod.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut field = |label: &str, items: Vec<&str>| {
            if !items.is_empty() {
                // Collapse the TOML's line breaks and indentation inside fields.
                let text = items
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                out += &format!("{label}: {text}\n");
            }
        };
        field(
            "Background",
            self.both()
                .filter_map(|t| t.background.as_deref())
                .collect(),
        );
        field(
            "Personality",
            self.both()
                .filter_map(|t| t.personality.as_deref())
                .collect(),
        );
        let list = |f: fn(&Traits) -> &Vec<String>| -> Vec<&str> {
            self.both()
                .flat_map(|t| f(t).iter().map(String::as_str))
                .collect()
        };
        field("Motivations", list(|t| &t.motivations));
        field("Important relationships", list(|t| &t.relationships));
        field("Behavioural tendencies", list(|t| &t.tendencies));
        field(
            "Speaking style",
            self.both()
                .filter_map(|t| t.speaking_style.as_deref())
                .collect(),
        );
        out.truncate(out.trim_end().len());
        out
    }

    fn validate(&self, stem: &str) -> Result<(), String> {
        if self.id != stem {
            return Err(format!("id {:?} does not match the file name", self.id));
        }
        match ids::ids().troops.index(&self.id) {
            Some(_) if troop_kind(&self.id) != Kind::Other => {}
            Some(_) => return Err(format!("{} is not a lord, lady or companion", self.id)),
            None => return Err(format!("{} is not a troop in ID_troops.py", self.id)),
        }
        if self.name.trim().is_empty() {
            return Err("name is empty".into());
        }
        if self.canon.as_ref().is_some_and(|c| c.sources.is_empty()) {
            return Err("[canon] needs sources".into());
        }
        if !self.mod_traits.sources.is_empty() {
            return Err("sources belong in [canon]; [mod] traits are the mod's own".into());
        }
        if !self.both().any(|t| t.personality.is_some()) {
            return Err("no personality in [canon] or [mod]".into());
        }
        if !self.both().any(|t| t.speaking_style.is_some()) {
            return Err("no speaking_style in [canon] or [mod]".into());
        }
        if !self.render().is_ascii() {
            return Err("profiles must be plain ASCII".into());
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct Registry {
    profiles: BTreeMap<String, Profile>,
}

impl Registry {
    /// Reads every `*.toml` in `dir`. Any bad file is an error that names it.
    pub fn load(dir: &Path) -> Result<Registry, String> {
        let entries =
            fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        let mut registry = Registry::default();
        for path in paths {
            let bad = |e: String| format!("{}: {e}", path.display());
            let text = fs::read_to_string(&path).map_err(|e| bad(e.to_string()))?;
            let profile: Profile = toml::from_str(&text).map_err(|e| bad(e.to_string()))?;
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            profile.validate(stem).map_err(bad)?;
            registry.profiles.insert(profile.id.clone(), profile);
        }
        Ok(registry)
    }

    pub fn get(&self, character: &str) -> Option<&Profile> {
        self.profiles.get(character)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("calradia-characters-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const GOOD: &str = r#"
id = "trp_npc1"
name = "Borcha"
[canon]
background = """A tracker from
the high steppe."""
personality = "Roguish."
relationships = ["Marnid: a friend."]
sources = ["module_strings.py: npc1_backstory_b"]
[mod]
speaking_style = "Calls the player boss."
tendencies = ["Lies about small things."]
"#;

    #[test]
    fn loads_and_renders_canon_before_mod() {
        let dir = temp_dir("good");
        fs::write(dir.join("trp_npc1.toml"), GOOD).unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let r = Registry::load(&dir).unwrap();
        assert_eq!(r.ids().collect::<Vec<_>>(), ["trp_npc1"]);
        let p = r.get("trp_npc1").unwrap();
        assert_eq!(
            p.render(),
            "Background: A tracker from the high steppe.\nPersonality: Roguish.\n\
             Important relationships: Marnid: a friend.\n\
             Behavioural tendencies: Lies about small things.\n\
             Speaking style: Calls the player boss."
        );
        assert!(r.get("trp_npc2").is_none());
    }

    #[test]
    fn rejects_bad_profiles_naming_the_file() {
        let cases = [
            (
                "trp_npc1",
                GOOD.replace("trp_npc1", "trp_npc2"),
                "does not match",
            ),
            (
                "trp_nobody",
                GOOD.replace("trp_npc1", "trp_nobody"),
                "not a troop",
            ),
            (
                "trp_player",
                GOOD.replace("trp_npc1", "trp_player"),
                "not a lord",
            ),
            (
                "trp_npc1",
                GOOD.replace("tendencies", "tendencys"),
                "unknown field",
            ),
            (
                "trp_npc1",
                GOOD.replace(
                    "sources = [\"module_strings.py: npc1_backstory_b\"]",
                    "sources = []",
                ),
                "needs sources",
            ),
            (
                "trp_npc1",
                GOOD.replace("speaking_style", "motivations = [\"x\"]\n#"),
                "speaking_style",
            ),
            (
                "trp_npc1",
                GOOD.replace("Roguish.", "Rogu\u{E9}sh."),
                "ASCII",
            ),
            (
                "trp_npc1",
                GOOD.replace("[mod]", "[mod]\nsources = [\"x\"]"),
                "belong in [canon]",
            ),
            ("trp_npc1", "id = ".to_string(), "trp_npc1.toml"),
        ];
        for (i, (stem, text, expected)) in cases.into_iter().enumerate() {
            let dir = temp_dir(&format!("bad{i}"));
            fs::write(dir.join(format!("{stem}.toml")), text).unwrap();
            let err = Registry::load(&dir).unwrap_err();
            assert!(err.contains(expected), "case {i}: {err}");
            assert!(err.contains(&format!("{stem}.toml")), "case {i}: {err}");
        }
        assert!(Registry::load(Path::new("/nonexistent/characters")).is_err());
    }

    #[test]
    fn shipped_profiles_are_valid_and_distinct() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("characters");
        let r = Registry::load(&dir).unwrap();
        let ids: Vec<&str> = r.ids().collect();
        // Every character with a history of its own in vanilla has a profile.
        let expected: Vec<String> = (1..=16)
            .map(|n| format!("trp_npc{n}"))
            .chain((1..=6).map(|n| format!("trp_kingdom_{n}_lord")))
            .chain((1..=6).map(|n| format!("trp_kingdom_{n}_pretender")))
            .collect();
        for id in &expected {
            assert!(r.get(id).is_some(), "no profile for {id}");
        }
        let rendered: Vec<String> = ids.iter().map(|id| r.get(id).unwrap().render()).collect();
        for (i, a) in rendered.iter().enumerate() {
            assert!(a.len() < 2400, "{} is {} characters", ids[i], a.len());
            for b in &rendered[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
