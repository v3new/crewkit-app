use std::path::{Path, PathBuf};

/// All base directories CrewKit reads or writes.
///
/// Adapters reference these through `${home}`, `${claudeConfigDir}`,
/// `${claudeJson}`, `${codexHome}`, `${appSupport}` and `${localAppData}` template
/// variables, so tests can
/// point everything at a sandbox root and the same code paths run
/// against real client installations in production.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub claude_config_dir: PathBuf,
    /// Claude Code's user config (user-scope MCP servers): inside
    /// `CLAUDE_CONFIG_DIR` when that is set, otherwise `~/.claude.json`.
    pub claude_json: PathBuf,
    pub codex_home: PathBuf,
    pub app_support: PathBuf,
    /// Per-user local (non-roaming) app data: `%LOCALAPPDATA%` on Windows,
    /// where installers like Claude Desktop's put the app itself.
    pub local_app_data: PathBuf,
}

impl Paths {
    /// Resolve from the current environment, honoring the same overrides
    /// the client CLIs honor (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`).
    pub fn from_env() -> Self {
        #[cfg(windows)]
        let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default());
        #[cfg(not(windows))]
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let claude_env = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
        let claude_json = claude_env
            .as_ref()
            .map_or_else(|| home.join(".claude.json"), |dir| dir.join(".claude.json"));
        let claude_config_dir = claude_env.unwrap_or_else(|| home.join(".claude"));
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        // The per-user app data root: where Claude Desktop keeps its config
        // and where CrewKit's own directory lives.
        #[cfg(windows)]
        let app_support = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"));
        #[cfg(not(windows))]
        let app_support = home.join("Library/Application Support");
        #[cfg(windows)]
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"));
        #[cfg(not(windows))]
        let local_app_data = app_support.clone();
        Self {
            home,
            claude_config_dir,
            claude_json,
            codex_home,
            app_support,
            local_app_data,
        }
    }

    /// Everything under one sandbox root — used by integration tests.
    pub fn rooted(root: &Path) -> Self {
        let home = root.join("home");
        Self {
            claude_config_dir: home.join(".claude"),
            claude_json: home.join(".claude/.claude.json"),
            codex_home: home.join(".codex"),
            app_support: home.join("Library/Application Support"),
            local_app_data: home.join("AppData/Local"),
            home,
        }
    }

    /// CrewKit's own data directory (staged marketplaces, state, snapshots).
    pub fn crewkit_dir(&self) -> PathBuf {
        self.app_support.join("CrewKit")
    }

    /// Claude Desktop's data directory, shared with Cowork. The Microsoft
    /// Store (MSIX) build runs under filesystem virtualization: the app's
    /// `%APPDATA%\Claude` reads and writes are redirected into its
    /// package's LocalCache, so that is where the files really are. (The
    /// package family name is stable across machines.)
    pub fn claude_desktop_dir(&self) -> PathBuf {
        let store = self
            .local_app_data
            .join("Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude");
        if store.is_dir() {
            return store;
        }
        self.claude_desktop_dir_seen_by_app()
    }

    /// The same directory as the app itself addresses it — what its own
    /// state files record. Identical to `claude_desktop_dir` except under
    /// the Store build's virtualization.
    pub fn claude_desktop_dir_seen_by_app(&self) -> PathBuf {
        self.app_support.join("Claude")
    }

    pub fn claude_desktop_config(&self) -> PathBuf {
        self.claude_desktop_dir().join("claude_desktop_config.json")
    }

    /// Expand `${var}` templates used in adapter definitions.
    pub fn expand(&self, template: &str) -> PathBuf {
        let expanded = template
            .replace("${home}", &self.home.to_string_lossy())
            .replace(
                "${claudeConfigDir}",
                &self.claude_config_dir.to_string_lossy(),
            )
            .replace("${claudeJson}", &self.claude_json.to_string_lossy())
            .replace("${codexHome}", &self.codex_home.to_string_lossy())
            .replace("${appSupport}", &self.app_support.to_string_lossy())
            .replace("${localAppData}", &self.local_app_data.to_string_lossy());
        PathBuf::from(expanded)
    }

    /// Environment passed to client CLIs so they operate on the same
    /// directories this process resolved (keeps tests hermetic).
    /// `CLAUDE_CONFIG_DIR` goes along only when it was set: passing the
    /// default `~/.claude` would move Claude Code's `.claude.json` into it.
    pub fn cli_env(&self) -> Vec<(String, String)> {
        let mut env = vec![(
            "CODEX_HOME".into(),
            self.codex_home.to_string_lossy().into_owned(),
        )];
        if self.claude_json.parent() == Some(self.claude_config_dir.as_path()) {
            env.push((
                "CLAUDE_CONFIG_DIR".into(),
                self.claude_config_dir.to_string_lossy().into_owned(),
            ));
        }
        env
    }

    /// Where older CrewKit had Claude Code keep user-scope MCP servers
    /// when `CLAUDE_CONFIG_DIR` was not set; `None` when that is the live
    /// config.
    pub fn legacy_claude_json(&self) -> Option<PathBuf> {
        let legacy = self.claude_config_dir.join(".claude.json");
        (legacy != self.claude_json).then_some(legacy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_config_dir_is_passed_only_when_it_holds_claude_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut paths = Paths::rooted(tmp.path());
        let has_env = |p: &Paths| p.cli_env().iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR");
        assert!(has_env(&paths));
        assert_eq!(paths.legacy_claude_json(), None);

        paths.claude_json = paths.home.join(".claude.json");
        assert!(!has_env(&paths));
        assert_eq!(
            paths.legacy_claude_json(),
            Some(paths.claude_config_dir.join(".claude.json"))
        );
    }

    #[test]
    fn desktop_config_prefers_store_localcache_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(tmp.path());

        let default = paths.claude_desktop_config();
        assert_eq!(
            default,
            paths.app_support.join("Claude/claude_desktop_config.json")
        );

        let store = paths
            .local_app_data
            .join("Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude");
        std::fs::create_dir_all(&store).unwrap();
        assert_eq!(
            paths.claude_desktop_config(),
            store.join("claude_desktop_config.json")
        );
    }
}
