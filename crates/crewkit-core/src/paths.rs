use std::path::{Path, PathBuf};

/// All base directories CrewKit reads or writes.
///
/// Adapters reference these through `${home}`, `${claudeConfigDir}`,
/// `${codexHome}` and `${appSupport}` template variables, so tests can
/// point everything at a sandbox root and the same code paths run
/// against real client installations in production.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub claude_config_dir: PathBuf,
    pub codex_home: PathBuf,
    pub app_support: PathBuf,
}

impl Paths {
    /// Resolve from the current environment, honoring the same overrides
    /// the client CLIs honor (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`).
    pub fn from_env() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let app_support = home.join("Library/Application Support");
        Self {
            home,
            claude_config_dir,
            codex_home,
            app_support,
        }
    }

    /// Everything under one sandbox root — used by integration tests.
    pub fn rooted(root: &Path) -> Self {
        let home = root.join("home");
        Self {
            claude_config_dir: home.join(".claude"),
            codex_home: home.join(".codex"),
            app_support: home.join("Library/Application Support"),
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
            .replace("${appSupport}", &self.app_support.to_string_lossy());
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
