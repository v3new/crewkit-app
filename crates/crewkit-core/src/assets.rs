use crate::adapter::Adapter;
use crate::error::Result;
use crate::translate::FrontmatterMap;

const ADAPTERS: &[&str] = &[
    include_str!("../../../adapters/claude-code.json"),
    include_str!("../../../adapters/claude-desktop.json"),
    include_str!("../../../adapters/codex.json"),
    include_str!("../../../adapters/chatgpt-desktop.json"),
];
const FRONTMATTER_MAP: &str = include_str!("../../../adapters/frontmatter-map.json");

pub fn adapters() -> Result<Vec<Adapter>> {
    ADAPTERS.iter().map(|json| Adapter::load(json)).collect()
}

pub fn frontmatter_map() -> Result<FrontmatterMap> {
    FrontmatterMap::load(FRONTMATTER_MAP)
}
