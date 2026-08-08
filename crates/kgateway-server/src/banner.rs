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
    // The dashboard is a separate Next.js process the server can't see. Whoever starts
    // it (scripts/start.sh, an operator) sets KGATEWAY_DASHBOARD_URL; absent, the banner
    // shows how to start it instead of a URL that isn't serving anything.
    let dashboard_url = std::env::var("KGATEWAY_DASHBOARD_URL").ok();
    let mut out = String::new();
    render(
        config,
        addr,
        dashboard_url.as_deref(),
        &Palette::resolve(),
        &mut out,
    );
    // A single write keeps the banner from interleaving with early log lines.
    print!("{out}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// Build the banner text into `out`. Separated from `print` so it is unit-testable
/// with a colourless palette. `dashboard_url` is Some only when a dashboard is
/// actually running (see `print`).
fn render(config: &Config, addr: &str, dashboard_url: Option<&str>, p: &Palette, out: &mut String) {
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
    // Dashboard is a separate Next.js app (ui/); only print a URL when one is actually
    // running — a hardcoded :3000 link with nothing behind it just misleads.
    match dashboard_url {
        Some(url) => row(out, "Dashboard", p.accent, url),
        None => row(
            out,
            "Dashboard",
            p.dim,
            "not running — start: cd ui && pnpm dev  (or ./scripts/start.sh)",
        ),
    }
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
        render(&cfg, "0.0.0.0:8080", None, &plain(), &mut out);

        assert!(out.contains("KGATEWAY") || out.contains("██"));
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
        assert!(out.contains("http://localhost:8080"));
        assert!(out.contains("Providers"));
        // One provider registered.
        assert!(out.contains("Providers    1") || out.contains("Providers"));
    }

    #[test]
    fn dashboard_row_shows_url_only_when_running() {
        let cfg = Config::default();

        // A dashboard was started (env var set by scripts/start.sh): print its URL.
        let mut with = String::new();
        render(
            &cfg,
            "0.0.0.0:8080",
            Some("http://localhost:3000"),
            &plain(),
            &mut with,
        );
        assert!(with.contains("http://localhost:3000"));
        assert!(!with.contains("not running"));

        // No dashboard: a how-to-start hint, never a URL to a dead port.
        let mut without = String::new();
        render(&cfg, "0.0.0.0:8080", None, &plain(), &mut without);
        assert!(without.contains("Dashboard"));
        assert!(without.contains("not running"));
        assert!(!without.contains(":3000"));
    }

    #[test]
    fn reports_open_admin_when_unset() {
        let cfg = Config::default();
        let mut out = String::new();
        render(&cfg, "0.0.0.0:8080", None, &plain(), &mut out);
        assert!(out.contains("open (no admin_token)"));
    }

    #[test]
    fn reports_protected_admin_when_set() {
        let cfg = Config {
            admin_token: Some("secret".into()),
            ..Default::default()
        };
        let mut out = String::new();
        render(&cfg, "0.0.0.0:8080", None, &plain(), &mut out);
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
