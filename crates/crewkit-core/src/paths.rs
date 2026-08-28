use std::path::{Path, PathBuf};

/// All base directories CrewKit reads or writes.
///
/// Adapters reference these through `${home}`, `${claudeConfigDir}`,
/// `${codexHome}`, `${appSupport}` and `${localAppData}` template
/// variables, so tests can
/// point everything at a sandbox root and the same code paths run
/// against real client installations in production.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub claude_config_dir: PathBuf,
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
        let claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
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

    /// Expand `${var}` templates used in adapter definitions.
    pub fn expand(&self, template: &str) -> PathBuf {
        let expanded = template
            .replace("${home}", &self.home.to_string_lossy())
            .replace(
                "${claudeConfigDir}",
                &self.claude_config_dir.to_string_lossy(),
            )
            .replace("${codexHome}", &self.codex_home.to_string_lossy())
            .replace("${appSupport}", &self.app_support.to_string_lossy())
            .replace("${localAppData}", &self.local_app_data.to_string_lossy());
        PathBuf::from(expanded)
    }

    /// Environment passed to client CLIs so they operate on the same
    /// directories this process resolved (keeps tests hermetic).
    pub fn cli_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "CLAUDE_CONFIG_DIR".into(),
                self.claude_config_dir.to_string_lossy().into_owned(),
            ),
            (
                "CODEX_HOME".into(),
                self.codex_home.to_string_lossy().into_owned(),
            ),
        ]
    }
}
