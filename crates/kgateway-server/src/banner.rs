//! Startup banner — an ASCII wordmark plus a one-glance summary of the running
//! gateway (version/build, listen address, and the salient bits of the loaded
//! config: providers, storage, auth, cache, MCP). Printed once, right after the
//! listener binds, so it reflects a real successful start.
//!
//! Purely cosmetic: it writes to stdout (not the tracing log) so it stays legible
//! even when `RUST_LOG` is turned down, and colour auto-disables when stdout is not
//! a TTY or `NO_COLOR` is set. It reads config but never secrets — auth/cache lines
//! report only presence, never key material.

use std::io::IsTerminal;

use crate::config::Config;

/// ANSI palette, resolved once against the terminal. When colour is off every field
/// is the empty string, so the same format strings render as clean plain text.
struct Palette {
    reset: &'static str,
    bold: &'static str,
    dim: &'static str,
    brand: &'static str,
    accent: &'static str,
    good: &'static str,
    warn: &'static str,
}

impl Palette {
    fn resolve() -> Self {
        // Honour the NO_COLOR convention and skip colour when piped/redirected.
        let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        if color {
            Palette {
                reset: "\x1b[0m",
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                brand: "\x1b[38;5;44m",   // teal
                accent: "\x1b[38;5;213m", // magenta/pink
                good: "\x1b[38;5;42m",    // green
                warn: "\x1b[38;5;214m",   // amber
            }
        } else {
            Palette {
                reset: "",
                bold: "",
                dim: "",
                brand: "",
                accent: "",
                good: "",
                warn: "",
            }
        }
    }
}

/// Classify the storage backend from the (optional) database URL, without echoing
/// the URL itself (it may carry credentials).
fn storage_kind(database: &Option<String>) -> &'static str {
    match database {
        None => "in-memory",
        Some(url) => {
            let u = url.to_ascii_lowercase();
            if u.starts_with("postgres") {
                "postgres"
            } else if u.starts_with("sqlite") || u.ends_with(".db") || u.ends_with(".sqlite") {
                "sqlite"
            } else {
                "external"
            }
        }
    }
}

/// Render the startup banner to stdout. `addr` is the bound listen address
/// (e.g. `0.0.0.0:8080`).
pub fn print(config: &Config, addr: &str) {
    let mut out = String::new();
    render(config, addr, &Palette::resolve(), &mut out);
    // A single write keeps the banner from interleaving with early log lines.
    print!("{out}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// Build the banner text into `out`. Separated from `print` so it is unit-testable
/// with a colourless palette.
fn render(config: &Config, addr: &str, p: &Palette, out: &mut String) {
    use std::fmt::Write;

    let version = env!("CARGO_PKG_VERSION");
    let msrv = "1.88";
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    // Wordmark. Block-letter "KGATEWAY".
    const LOGO: [&str; 6] = [
        r"  ██╗  ██╗ ██████╗  █████╗ ████████╗███████╗██╗    ██╗ █████╗ ██╗   ██╗",
        r"  ██║ ██╔╝██╔════╝ ██╔══██╗╚══██╔══╝██╔════╝██║    ██║██╔══██╗╚██╗ ██╔╝",
        r"  █████╔╝ ██║  ███╗███████║   ██║   █████╗  ██║ █╗ ██║███████║ ╚████╔╝ ",
        r"  ██╔═██╗ ██║   ██║██╔══██║   ██║   ██╔══╝  ██║███╗██║██╔══██║  ╚██╔╝  ",
        r"  ██║  ██╗╚██████╔╝██║  ██║   ██║   ███████╗╚███╔███╔╝██║  ██║   ██║   ",
        r"  ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═╝   ╚═╝   ╚══════╝ ╚══╝╚══╝ ╚═╝  ╚═╝   ╚═╝   ",
    ];

    out.push('\n');
    for line in LOGO {
        let _ = writeln!(out, "{}{}{}", p.brand, line, p.reset);
    }

    // Tagline + version line.
    let _ = writeln!(
        out,
        "  {}{}OpenAI-compatible AI/LLM Gateway{}  {}v{}{}",
        p.bold, p.accent, p.reset, p.dim, version, p.reset,
    );
    let _ = writeln!(
        out,
        "  {}rustc ≥ {} · {}-{} · edition 2021{}",
        p.dim, msrv, os, arch, p.reset,
    );
    out.push('\n');

    // --- Runtime summary ------------------------------------------------------
    // Storage.
    let storage = storage_kind(&config.database);

    // Auth: admin control-plane token present or open.
    let (auth_col, auth_txt): (&str, &str) = match &config.admin_token {
        Some(_) => (p.good, "protected"),
        None => (p.warn, "open (no admin_token)"),
    };

    // Governance / virtual keys.
    let vkeys = config.virtual_keys.len();

    // Optional subsystems.
    let cache = if config.semantic_cache.is_some() {
        (p.good, "on")
    } else {
        (p.dim, "off")
    };
    let mcp = if config.mcp.is_some() {
        (p.good, "on")
    } else {
        (p.dim, "off")
    };

    let row = |out: &mut String, label: &str, col: &str, value: &str| {
        let _ = writeln!(
            out,
            "  {}{:<12}{} {}{}{}",
            p.dim, label, p.reset, col, value, p.reset,
        );
    };

    // Listen address, rendered as a clickable localhost URL for convenience.
    let display_host = addr.replace("0.0.0.0", "localhost");
    row(
        out,
        "Listening",
        p.accent,
        &format!("http://{display_host}"),
    );
    // Dashboard is a separate Next.js app (ui/), served on port 3000 in dev — not the
    // backend port. It talks to the gateway via NEXT_PUBLIC_KGATEWAY_URL.
    let dashboard_host = display_host
        .split(':')
        .next()
        .unwrap_or("localhost")
        .to_owned();
    row(
        out,
        "Dashboard",
        p.accent,
        &format!("http://{dashboard_host}:3000"),
    );
    row(
        out,
        "Providers",
        p.brand,
        &config.providers.len().to_string(),
    );
    row(out, "Storage", p.brand, storage);
    row(out, "Admin API", auth_col, auth_txt);
    row(out, "Virtual keys", p.brand, &vkeys.to_string());
    row(out, "Sem. cache", cache.0, cache.1);
    row(out, "MCP gateway", mcp.0, mcp.1);

    out.push('\n');
    let _ = writeln!(out, "  {}Ready. Press Ctrl-C to stop.{}", p.dim, p.reset,);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Colourless palette so assertions match plain text.
    fn plain() -> Palette {
        Palette {
            reset: "",
            bold: "",
            dim: "",
            brand: "",
            accent: "",
            good: "",
            warn: "",
        }
    }

    #[test]
    fn renders_core_facts() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "openai".into(),
            crate::config::ProviderConfig {
                kind: None,
                base_url: None,
                keys: Vec::new(),
            },
        );
        let mut out = String::new();
        render(&cfg, "0.0.0.0:8080", &plain(), &mut out);

        assert!(out.contains("KGATEWAY") || out.contains("██"));
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
        assert!(out.contains("http://localhost:8080"));
        // Dashboard is a separate Next.js app on port 3000, not the backend port.
        assert!(out.contains("http://localhost:3000"));
        assert!(out.contains("Providers"));
        // One provider registered.
        assert!(out.contains("Providers    1") || out.contains("Providers"));
    }

    #[test]
    fn reports_open_admin_when_unset() {
        let cfg = Config::default();
        let mut out = String::new();
        render(&cfg, "0.0.0.0:8080", &plain(), &mut out);
        assert!(out.contains("open (no admin_token)"));
    }

    #[test]
    fn reports_protected_admin_when_set() {
        let cfg = Config {
            admin_token: Some("secret".into()),
            ..Default::default()
        };
        let mut out = String::new();
        render(&cfg, "0.0.0.0:8080", &plain(), &mut out);
        assert!(out.contains("protected"));
        // Never echo the token itself.
        assert!(!out.contains("secret"));
    }

    #[test]
    fn storage_kinds() {
        assert_eq!(storage_kind(&None), "in-memory");
        assert_eq!(storage_kind(&Some("postgres://x".into())), "postgres");
        assert_eq!(storage_kind(&Some("sqlite://logs.db".into())), "sqlite");
        assert_eq!(storage_kind(&Some("mysql://x".into())), "external");
    }
}
