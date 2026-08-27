//! Skill translation between hosts (the brief's `skill-translate`).
//!
//! Both ecosystems read the same Agent Skills format (`SKILL.md`), so the
//! skill body is carried as-is. What differs is the frontmatter surface:
//! host-specific keys are checked against a declarative mapping table
//! (data, not code — `adapters/frontmatter-map.json`), and anything a
//! host does not recognize is reported as a partial-support warning in
//! the install log. For the OpenAI side, `agents/openai.yaml` with UI
//! metadata is generated when the skill does not ship one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{io_ctx, Error, Result};
use crate::fsops;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrontmatterMap {
    pub required: Vec<String>,
    pub hosts: BTreeMap<String, HostKeys>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostKeys {
    pub known_keys: Vec<String>,
}

impl FrontmatterMap {
    pub fn load(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| Error::Parse {
            path: PathBuf::from("frontmatter-map.json"),
            message: e.to_string(),
        })
    }
}

/// Validate one skill directory and generate the OpenAI UI metadata.
/// Returns human-readable warnings; none of them block installation.
pub fn process_skill(
    skill_dir: &Path,
    map: &FrontmatterMap,
    publisher: &str,
) -> Result<Vec<String>> {
    let skill_md = skill_dir.join("SKILL.md");
    let text = std::fs::read_to_string(&skill_md)
        .map_err(io_ctx(format!("reading {}", skill_md.display())))?;
    let skill_label = skill_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut warnings = Vec::new();
    let frontmatter = extract_frontmatter(&text);
    let keys = top_level_keys(&frontmatter);

    for required in &map.required {
        if !keys.contains(required) {
            warnings.push(format!(
                "{skill_label}: missing required frontmatter key `{required}`"
            ));
        }
    }

    if let Some(name) = value_of(&frontmatter, "name") {
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            warnings.push(format!(
                "{skill_label}: skill name `{name}` is not lowercase-hyphen"
            ));
        }
    }

    for (host, host_keys) in &map.hosts {
        for key in &keys {
            if !host_keys.known_keys.contains(key) {
                warnings.push(format!(
                    "{skill_label}: frontmatter key `{key}` is not recognized by {host} — kept as-is (partial support)"
                ));
            }
        }
    }

    ensure_openai_metadata(skill_dir, &frontmatter, publisher)?;
    Ok(warnings)
}

/// Generate `agents/openai.yaml` (ChatGPT Desktop UI metadata) from the
/// skill's own frontmatter plus the kit publisher, unless the skill
/// already ships one.
fn ensure_openai_metadata(skill_dir: &Path, frontmatter: &str, publisher: &str) -> Result<()> {
    let target = skill_dir.join("agents/openai.yaml");
    if target.exists() {
        return Ok(());
    }
    let name = value_of(frontmatter, "name").unwrap_or_else(|| {
        skill_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let display_name = title_case(&name);
    let description = value_of(frontmatter, "description").unwrap_or_default();
    let short: String = description.chars().take(120).collect();

    let yaml = format!(
        "interface:\n  display_name: {}\n  short_description: {}\n  developer_name: {}\n",
        yaml_quote(&display_name),
        yaml_quote(&short),
        yaml_quote(publisher),
    );
    fsops::atomic_write(&target, yaml.as_bytes())
}

/// The YAML block between the first pair of `---` lines.
fn extract_frontmatter(text: &str) -> String {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return String::new();
    }
    lines
        .take_while(|line| line.trim() != "---")
        .collect::<Vec<_>>()
        .join("\n")
}

fn top_level_keys(frontmatter: &str) -> Vec<String> {
    frontmatter
        .lines()
        .filter(|line| !line.starts_with([' ', '\t', '#']))
        .filter_map(|line| {
            let (key, _) = line.split_once(':')?;
            let key = key.trim();
            (!key.is_empty()).then(|| key.to_string())
        })
        .collect()
}

/// Single-line scalar value of a top-level key (enough for name/description
/// headlines; folded blocks return their first fragment).
fn value_of(frontmatter: &str, key: &str) -> Option<String> {
    let mut lines = frontmatter.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            let inline = rest.trim().trim_matches(['"', '\'']).to_string();
            if !inline.is_empty() && inline != ">-" && inline != "|" && inline != ">" {
                return Some(inline);
            }
            // Block scalar: take the first indented line.
            if let Some(next) = lines.peek() {
                return Some(next.trim().to_string());
            }
        }
    }
    None
}

fn title_case(name: &str) -> String {
    name.split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
