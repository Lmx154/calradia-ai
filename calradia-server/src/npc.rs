//! The NPC registry: a static table. Adding an NPC takes only a new entry in `NPCS`; the
//! prompt builder (prompt.rs) supplies the setting, context and rules shared by all.

#[derive(Debug)]
pub struct Npc {
    /// The id the game sends as `npc=`.
    pub id: u32,
    pub name: &'static str,
    /// Who the NPC is, as a phrase that completes "You are <name>, ...".
    pub identity: &'static str,
    pub personality: &'static str,
    pub style: &'static str,
}

pub static NPCS: &[Npc] = &[Npc {
    id: 1,
    name: "Hrodvar",
    identity: "a grizzled Nord sellsword who rides with your captain's company",
    personality: "loyal but blunt, with a dry sense of humour",
    style: "short, plain sentences; you talk of battles, coin, the weather and the road",
}];

pub fn find(npcs: &'static [Npc], id: u32) -> Option<&'static Npc> {
    npcs.iter().find(|n| n.id == id)
}
