//! `hivemind init`: set this machine up end to end (SPEC §2).
//!
//! Every step is skippable and every step is idempotent, which is the whole
//! reason this is one command rather than a page of instructions.

use std::path::Path;

use anyhow::{Context as _, Result};
use hivemind_core::config::Config;
use hivemind_core::identity::Identity;

use crate::colour::Paint as _;
use crate::paths;

/// What `init` was asked to do (SPEC §2).
#[derive(Debug, Default)]
pub(crate) struct InitOptions {
    pub(crate) name: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) launchd: bool,
    pub(crate) mcp: bool,
    pub(crate) hooks: bool,
}

/// Set this machine up end to end (SPEC §2).
///
/// Every step is skippable and every step is idempotent: running this twice
/// must not generate a second identity, clobber a config somebody edited, or
/// duplicate the hooks. Somebody who ran it, read the output, and ran it again
/// is the normal case, not a misuse.
pub(crate) fn init(home: Option<&Path>, api: &str, options: InitOptions) -> Result<()> {
    let home = paths::home(home)?;
    std::fs::create_dir_all(&home)
        .with_context(|| format!("could not create {}", home.display()))?;

    // 1. Identity. `load_or_create` is the idempotence: a second run keeps the
    //    keypair, because changing it would orphan every message already sent.
    let identity = Identity::load_or_create(&home.join("identity"))
        .context("could not create this node's identity")?;

    // 2. Config, with whatever was asked for folded in.
    let config_path = home.join("config.toml");
    let existed = config_path.is_file();
    let mut config = Config::load(&home).context("could not read the configuration")?;
    if let Some(name) = options.name {
        config.name = name;
    }
    if let Some(owner) = options.owner {
        config.owner = Some(owner);
    }
    config.validate()?;
    write_config(&config_path, &config)?;

    println!("{}", "hivemind is set up".green().bold());
    println!();
    println!("  {}   {}", "name".dimmed(), config.name.bold());
    if let Some(owner) = &config.owner {
        println!("  {}  {owner}", "owner".dimmed());
    }
    println!("  {}     {}", "id".dimmed(), identity.node_id());
    println!("  {}  {}", "short".dimmed(), identity.node_id().short());
    println!(
        "  {} {}",
        "config".dimmed(),
        if existed {
            format!("{} (kept your settings)", config_path.display())
        } else {
            config_path.display().to_string()
        }
    );
    println!();

    // 3. launchd.
    if options.launchd {
        crate::service::install(Some(&home))?;
    } else {
        println!(
            "{} launchd skipped — run `hivemind daemon` yourself",
            "--".dimmed()
        );
    }

    // 4. MCP registration. Shelling out to `claude` is what SPEC §2 asks for;
    //    when it is not there, the exact command is printed rather than a
    //    vague suggestion to install something.
    if options.mcp {
        register_mcp(api);
    } else {
        println!("{} MCP registration skipped", "--".dimmed());
    }

    // 5. Hooks, merged rather than clobbered (SPEC §9.3).
    if options.hooks {
        match crate::hooks::install() {
            Ok(()) => {}
            Err(error) => println!("{} could not install the hooks: {error}", "!!".red()),
        }
    } else {
        println!("{} hooks skipped", "--".dimmed());
    }

    // 6. Where other machines can reach this one.
    println!();
    println!("  {}", "reachable at".dimmed());
    for addr in local_addresses(config.peer_port) {
        println!("    {addr}");
    }

    println!();
    println!("Next: `hivemind group create` on the first machine, and");
    println!("`hivemind pair <code>` with the code it prints on every other.");
    Ok(())
}

/// Write `config.toml`, preserving anything already in it.
fn write_config(path: &Path, config: &Config) -> Result<()> {
    let text = toml::to_string_pretty(config).context("could not render the configuration")?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, text.as_bytes())
        .with_context(|| format!("could not write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("could not move {} into place", temporary.display()))?;
    Ok(())
}

/// Tell Claude Code about the MCP server (SPEC §2 step 4).
fn register_mcp(api: &str) {
    let command = format!(
        "claude mcp add --scope user --transport http hivemind {}/mcp",
        api.trim_end_matches('/')
    );

    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("claude").is_file()));

    if !on_path {
        println!(
            "{} `claude` is not on PATH. Run this when it is:",
            "--".dimmed()
        );
        println!("    {command}");
        return;
    }

    let result = std::process::Command::new("claude")
        .args([
            "mcp",
            "add",
            "--scope",
            "user",
            "--transport",
            "http",
            "hivemind",
        ])
        .arg(format!("{}/mcp", api.trim_end_matches('/')))
        .output();

    match result {
        Ok(output) if output.status.success() => {
            println!("{} registered with Claude Code", "ok".green());
        }
        // Already registered is the overwhelmingly likely failure, and not one
        // worth alarming anybody about. The command is printed either way.
        Ok(_) | Err(_) => {
            println!("{} could not register automatically. Run:", "--".dimmed());
            println!("    {command}");
        }
    }
}

/// Addresses another machine could reach this one on (SPEC §2 step 6).
fn local_addresses(port: u16) -> Vec<String> {
    let mut addrs = Vec::new();

    // The LAN address, via the routing table rather than by enumerating
    // interfaces: the one that would be used to reach the internet is the one
    // a peer on the same network will see.
    if let Some(ip) = outbound_ip() {
        addrs.push(format!("{ip}:{port}"));
    }

    // Tailscale's, if there is one. Never required (SPEC §5.2).
    if let Ok(output) = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let line = line.trim();
            if !line.is_empty() {
                addrs.push(format!("{line}:{port} (tailscale)"));
            }
        }
    }

    if addrs.is_empty() {
        addrs.push(format!("127.0.0.1:{port} (no network found)"));
    }
    addrs
}

/// This machine's address on the network it would use to reach the internet.
///
/// A UDP connect to a public address, which sends nothing — it only asks the
/// routing table which local address would be used. No packets, no DNS, and it
/// works offline as long as there is a route.
fn outbound_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}
