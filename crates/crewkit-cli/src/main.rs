//! crewkit: the command-line front end to the same engine the desktop
//! app uses. Handy for CI, MDM scripts and development.
//!
//! Usage:
//!   crewkit scan                     detected clients + kit inventory
//!   crewkit install                  install the kit into every client
//!   crewkit remove <plugin|mcp> <id> remove one kit item from every client
//!   crewkit authorize <server-id>    run the CrewKit-level OAuth flow
//!   crewkit logout <server-id>       drop the shared session
//!   crewkit rollback                 restore configs from the latest snapshot
//!   crewkit kit keygen <name>        generate a publisher signing keypair
//!   crewkit kit sign <manifest> <secret-key-file>   write <manifest>.sig
//!   crewkit kit login <manifest-url>    sign in to a kit behind a login
//!   crewkit kit logout <manifest-url>   drop that kit host's session
//!
//! Kit payload (skills/*.zip) and the bridge binary resolve from
//! `--assets <dir>` / `$CREWKIT_ASSETS` / the executable's directory /
//! the current directory — the first one that contains `skills/`.
//! The kit manifest resolves from `--kit <manifest.json>` / `$CREWKIT_KIT`
//! / `<assets>/kit.json`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crewkit_core::translate::FrontmatterMap;
use crewkit_core::{bridge, cli, Adapter, Engine, Kit, Paths, StepReport, StepStatus};

const ADAPTER_SOURCES: &[&str] = &[
    include_str!("../../../adapters/claude-code.json"),
    include_str!("../../../adapters/claude-desktop.json"),
    include_str!("../../../adapters/codex.json"),
    include_str!("../../../adapters/chatgpt-desktop.json"),
];
const FRONTMATTER_MAP: &str = include_str!("../../../adapters/frontmatter-map.json");

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let assets = take_flag(&mut args, "--assets").map(PathBuf::from);
    let kit = take_flag(&mut args, "--kit").map(PathBuf::from);

    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["scan"] => scan(assets, kit),
        ["install"] => install(assets, kit),
        ["remove", kind, id] => remove(assets, kit, kind, id),
        ["authorize", id] => bridge_command(assets, "login", id),
        ["logout", id] => bridge_command(assets, "logout", id),
        ["rollback"] => rollback(),
        ["kit", "keygen", name] => keygen(name),
        ["kit", "sign", manifest, key_file] => sign(manifest, key_file),
        ["kit", "login", url] => kit_login(url),
        ["kit", "logout", url] => kit_logout(url),
        _ => {
            eprintln!(
                "usage: crewkit [--assets <dir>] [--kit <manifest.json>] <scan | install | remove <plugin|mcp> <id> | authorize <id> | logout <id> | rollback | kit keygen <name> | kit sign <manifest> <secret-key-file> | kit login <manifest-url> | kit logout <manifest-url>>"
            );
            std::process::exit(2);
        }
    };

    if let Err(message) = result {
        eprintln!("crewkit: {message}");
        std::process::exit(1);
    }
}

/// Sign in to a kit published behind a login: opens the browser and
/// caches the session every AI client on this machine then shares.
fn kit_login(url: &str) -> Result<(), String> {
    let crewkit_dir = Paths::from_env().crewkit_dir();
    crewkit_core::kits::login_to_kit(url, &crewkit_dir).map_err(|e| e.to_string())?;
    eprintln!("crewkit: authorized `{url}`");
    Ok(())
}

fn kit_logout(url: &str) -> Result<(), String> {
    let crewkit_dir = Paths::from_env().crewkit_dir();
    let existed =
        crewkit_core::kits::logout_from_kit(url, &crewkit_dir).map_err(|e| e.to_string())?;
    eprintln!(
        "crewkit: {}",
        if existed {
            format!("logged out of `{url}`")
        } else {
            format!("`{url}` had no session")
        }
    );
    Ok(())
}

fn take_flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let index = args.iter().position(|a| a == name)?;
    args.remove(index);
    (index < args.len()).then(|| args.remove(index))
}

/// The first candidate directory that carries the kit payload.
fn assets_dir(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Some(dir) = explicit {
        candidates.push(dir);
    }
    if let Some(dir) = std::env::var_os("CREWKIT_ASSETS") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.to_path_buf());
        }
    }
    candidates.push(PathBuf::from("."));

    for candidate in &candidates {
        if candidate.join("skills").is_dir() {
            return Ok(candidate.clone());
        }
    }
    Err(format!(
        "no assets directory with a skills/ payload found (tried: {})",
        candidates
            .iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The kit manifest: explicit flag, environment, or `<assets>/kit.json`.
fn kit_manifest(explicit: Option<PathBuf>, assets: &Path) -> Result<Kit, String> {
    let path = explicit
        .or_else(|| std::env::var_os("CREWKIT_KIT").map(PathBuf::from))
        .unwrap_or_else(|| assets.join("kit.json"));
    let json = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "no kit manifest at {} ({e}) — pass --kit <manifest.json> or put kit.json in the assets dir",
            path.display()
        )
    })?;
    Kit::load(&json).map_err(|e| e.to_string())
}

fn build_engine(assets: Option<PathBuf>, kit: Option<PathBuf>) -> Result<Engine, String> {
    let assets = assets_dir(assets)?;
    let kit = kit_manifest(kit, &assets)?;
    let adapters = ADAPTER_SOURCES
        .iter()
        .map(|json| Adapter::load(json))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    // Bridge binary: assets dir, an already-installed bridge, or a local
    // cargo build (installing from the installed path is a clean no-op).
    let paths = Paths::from_env();
    let installed_bridge = crewkit_core::bridge::bridge_path(&paths.crewkit_dir());
    let bridge_source = [
        assets.join("bin").join(bridge::BRIDGE_BIN_NAME),
        installed_bridge.clone(),
        assets.join("target/release").join(bridge::BRIDGE_BIN_NAME),
    ]
    .into_iter()
    .find(|p| p.exists())
    .unwrap_or(installed_bridge);
    Ok(Engine {
        paths,
        adapters,
        kit,
        zips_dir: assets.join("skills"),
        bridge_source,
        frontmatter_map: FrontmatterMap::load(FRONTMATTER_MAP).map_err(|e| e.to_string())?,
    })
}

fn scan(assets: Option<PathBuf>, kit: Option<PathBuf>) -> Result<(), String> {
    let engine = build_engine(assets, kit)?;
    let report = engine.scan().map_err(|e| e.to_string())?;

    println!("Clients:");
    for client in &report.clients {
        let mark = if client.present { "+" } else { "-" };
        let app = client
            .app_version
            .as_ref()
            .map(|v| format!("  app: v{v}"))
            .unwrap_or_default();
        let cli = client
            .cli_path
            .as_ref()
            .map(|p| format!("  cli: {}", p.display()))
            .unwrap_or_default();
        let version = client
            .cli_version
            .as_ref()
            .map(|v| format!(" (v{v})"))
            .unwrap_or_default();
        println!("  [{mark}] {}{app}{cli}{version}", client.name);
    }

    println!("\nKit items:");
    for item in &report.items {
        let version = item
            .version
            .as_ref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        println!(
            "  {:6} {:32} {:14} {:?}{version}",
            item.kind, item.id, item.client, item.status
        );
    }

    println!("\nSessions:");
    for auth in &report.auth {
        let mark = if auth.authorized {
            "authorized"
        } else {
            "not authorized"
        };
        println!("  {:32} {mark}", auth.id);
    }
    Ok(())
}

fn print_step(step: &StepReport) {
    let mark = match step.status {
        StepStatus::Ok => "ok",
        StepStatus::Skipped => "--",
        StepStatus::Failed => "!!",
    };
    println!(
        "[{mark}] {:14} {} — {}",
        step.client, step.step, step.message
    );
}

fn install(assets: Option<PathBuf>, kit: Option<PathBuf>) -> Result<(), String> {
    let engine = build_engine(assets, kit)?;
    let report = engine.install(print_step).map_err(|e| e.to_string())?;
    if !report.restart_needed.is_empty() {
        println!(
            "\nRestart {} to pick up the changes.",
            report.restart_needed.join(" and ")
        );
    }
    if report.steps.iter().any(|s| s.status == StepStatus::Failed) {
        return Err("some steps failed".into());
    }
    Ok(())
}

fn remove(
    assets: Option<PathBuf>,
    kit: Option<PathBuf>,
    kind: &str,
    id: &str,
) -> Result<(), String> {
    let engine = build_engine(assets, kit)?;
    let report = engine
        .remove_item(kind, id, print_step)
        .map_err(|e| e.to_string())?;
    if report.steps.iter().any(|s| s.status == StepStatus::Failed) {
        return Err("some steps failed".into());
    }
    Ok(())
}

fn rollback() -> Result<(), String> {
    let paths = Paths::from_env();
    let restored =
        crewkit_core::fsops::rollback_latest(&paths.crewkit_dir()).map_err(|e| e.to_string())?;
    if restored.is_empty() {
        println!("No snapshot to roll back.");
    } else {
        for path in restored {
            println!("restored {}", path.display());
        }
        println!("Restart your AI clients to pick up the restored configs.");
    }
    Ok(())
}

fn keygen(name: &str) -> Result<(), String> {
    let (secret, public) = crewkit_core::kits::generate_keypair();
    let secret_path = PathBuf::from(format!("{name}.key"));
    let public_path = PathBuf::from(format!("{name}.pub"));
    if secret_path.exists() {
        return Err(format!("{} already exists", secret_path.display()));
    }
    std::fs::write(&secret_path, &secret).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::write(&public_path, &public).map_err(|e| e.to_string())?;
    println!(
        "secret key: {}  (keep private, never commit)",
        secret_path.display()
    );
    println!(
        "public key: {}  (embed as publisherKey in the manifest)",
        public_path.display()
    );
    println!("publisherKey value: {public}");
    Ok(())
}

fn sign(manifest: &str, key_file: &str) -> Result<(), String> {
    let bytes = std::fs::read(manifest).map_err(|e| format!("{manifest}: {e}"))?;
    let secret = std::fs::read_to_string(key_file).map_err(|e| format!("{key_file}: {e}"))?;
    let signature =
        crewkit_core::kits::sign_manifest(&bytes, &secret).map_err(|e| e.to_string())?;
    let sig_path = format!("{manifest}.sig");
    std::fs::write(&sig_path, &signature).map_err(|e| e.to_string())?;
    println!("wrote {sig_path}");
    Ok(())
}

fn bridge_command(_assets: Option<PathBuf>, command: &str, server_id: &str) -> Result<(), String> {
    // Auth talks only to the installed bridge; no kit manifest needed.
    let bridge_bin = bridge::bridge_path(&Paths::from_env().crewkit_dir());
    if !bridge_bin.exists() {
        return Err("crewkit-bridge is not installed yet — run `crewkit install` first".into());
    }
    let output = cli::run(
        &bridge_bin,
        &[command, server_id],
        &[],
        Duration::from_secs(300),
    )
    .map_err(|e| e.to_string())?;
    print!("{}", output.stdout);
    eprint!("{}", output.stderr);
    if output.success() {
        Ok(())
    } else {
        Err(format!("{command} failed"))
    }
}
