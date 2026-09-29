use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::kit::Kit;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    pub kind: String,
    pub id: String,
}

impl ItemRef {
    pub fn plugin(id: &str) -> Self {
        Self {
            kind: "plugin".into(),
            id: id.into(),
        }
    }

    pub fn mcp(id: &str) -> Self {
        Self {
            kind: "mcp".into(),
            id: id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub item: ItemRef,
    pub name: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Default, Clone)]
pub struct KitDiff {
    pub updated: Vec<Change>,
    pub added: Vec<ItemRef>,
    pub removed: Vec<ItemRef>,
}

impl KitDiff {
    pub fn changes_installed(&self) -> bool {
        !self.updated.is_empty() || !self.removed.is_empty()
    }
}

/// What changed between two snapshots of the same kit, both already
/// narrowed to the user's bundle. Tombstones count as removals only when
/// the item was active before.
pub fn diff(old: &Kit, new: &Kit) -> KitDiff {
    let old_items = stamps(old);
    let new_items = stamps(new);
    let mut result = KitDiff::default();
    for (item, (name, stamp)) in &new_items {
        match old_items.get(item) {
            None => result.added.push(item.clone()),
            Some((_, before)) if before != stamp => result.updated.push(Change {
                item: item.clone(),
                name: name.clone(),
                from: before.clone(),
                to: stamp.clone(),
            }),
            Some(_) => {}
        }
    }
    let retired = new
        .plugins
        .iter()
        .filter(|p| p.remove)
        .map(|p| ItemRef::plugin(&new.plugin_id(p)))
        .chain(
            new.mcp_servers
                .iter()
                .filter(|s| s.remove)
                .map(|s| ItemRef::mcp(&s.id)),
        );
    result.removed = retired
        .filter(|item| old_items.contains_key(item))
        .collect();
    result
}

fn stamps(kit: &Kit) -> BTreeMap<ItemRef, (String, String)> {
    let mut map = BTreeMap::new();
    for plugin in kit.active_plugins() {
        let stamp = plugin
            .version
            .clone()
            .or_else(|| plugin.artifact.as_ref().map(|a| a.sha256.clone()))
            .or_else(|| plugin.zip.clone())
            .unwrap_or_default();
        let name = plugin.display_name.clone().unwrap_or(plugin.name.clone());
        map.insert(ItemRef::plugin(&kit.plugin_id(plugin)), (name, stamp));
    }
    for server in kit.active_mcp_servers() {
        let name = server.display_name.clone().unwrap_or(server.id.clone());
        map.insert(ItemRef::mcp(&server.id), (name, server.url.clone()));
    }
    map
}

impl PartialOrd for ItemRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ItemRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.kind, &self.id).cmp(&(&other.kind, &other.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kit(plugins: &str, servers: &str) -> Kit {
        Kit::load(&format!(
            r#"{{ "id": "k", "name": "K", "publisher": "P", "marketplaceName": "mkt",
                 "plugins": [{plugins}], "mcpServers": [{servers}] }}"#
        ))
        .unwrap()
    }

    #[test]
    fn version_bump_is_an_update() {
        let old = kit(r#"{"name": "a", "version": "1.0.0", "zip": "a.zip"}"#, "");
        let new = kit(r#"{"name": "a", "version": "1.1.0", "zip": "a.zip"}"#, "");
        let d = diff(&old, &new);
        assert_eq!(d.updated.len(), 1);
        assert_eq!(d.updated[0].item, ItemRef::plugin("a@mkt"));
        assert_eq!(
            (d.updated[0].from.as_str(), d.updated[0].to.as_str()),
            ("1.0.0", "1.1.0")
        );
        assert!(d.added.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn new_plugin_and_server_are_added_not_updated() {
        let old = kit(r#"{"name": "a", "version": "1", "zip": "a.zip"}"#, "");
        let new = kit(
            r#"{"name": "a", "version": "1", "zip": "a.zip"}, {"name": "b", "version": "1", "zip": "b.zip"}"#,
            r#"{"id": "s", "url": "https://mcp.example.dev/mcp"}"#,
        );
        let d = diff(&old, &new);
        assert_eq!(d.added, vec![ItemRef::mcp("s"), ItemRef::plugin("b@mkt")]);
        assert!(d.updated.is_empty());
    }

    #[test]
    fn tombstone_removes_only_what_was_active() {
        let old = kit(r#"{"name": "a", "version": "1", "zip": "a.zip"}"#, "");
        let new = kit(
            r#"{"name": "a", "remove": true}, {"name": "never", "remove": true}"#,
            "",
        );
        let d = diff(&old, &new);
        assert_eq!(d.removed, vec![ItemRef::plugin("a@mkt")]);
    }

    #[test]
    fn identical_kits_have_no_diff() {
        let a = kit(
            r#"{"name": "a", "version": "1", "zip": "a.zip"}"#,
            r#"{"id": "s", "url": "https://x.dev/mcp"}"#,
        );
        let d = diff(&a, &a);
        assert!(!d.changes_installed() && d.added.is_empty());
    }
}
