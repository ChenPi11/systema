mod yaml;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, ValueEnum};
use colored::*;
use serde_json::Value;
use sysa::l10n;
use sysa::unitstate_admin::UnitStateAdmin;
use sysa_pager::{pager_eprintln, pager_println, PagerConfig, PagerGuard};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "systema-unitstatectl",
    version,
    about = "System A — Unit State Controller"
)]
struct Cli {
    #[arg(long, global = true, short = 'D', help = "Enable debug-level logging")]
    debug: bool,

    #[arg(
        long,
        global = true,
        default_value = "warn",
        help = "Log level (trace, debug, info, warn, error)"
    )]
    log_level: String,

    #[arg(long, global = true, help = "Do not pipe output into a pager")]
    no_pager: bool,

    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = ColorChoice::Auto,
        help = "When to use colors"
    )]
    color: ColorChoice,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ColorChoice {
    Always,
    Auto,
    Never,
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| level.parse().unwrap());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn render_unit(name: &str, json: &[u8]) -> Result<()> {
    let doc: Value =
        serde_json::from_slice(json).context(l10n::t_("Failed to parse unit state JSON."))?;
    let mut buf = String::new();
    let render_error = l10n::t_("Failed to render unit as YAML.");
    yaml::render_doc(&mut buf, name, &doc)
        .map_err(|e| anyhow::anyhow!("{}: {e}", render_error))?;
    pager_println!("{buf}");
    Ok(())
}

async fn run() -> Result<()> {
    let admin = UnitStateAdmin::new();
    let result = admin.list(render_unit).await?;

    if !result.message.is_empty() {
        pager_eprintln!(
            "{}",
            l10n::fmt(
                l10n::t_("Warning: {message}"),
                &[("message", &result.message)]
            )
            .yellow()
        );
    } else {
        pager_eprintln!(
            "{}",
            l10n::fmt(
                l10n::t_("Total {total} unit(s)."),
                &[("total", &result.total.to_string())]
            )
            .dimmed()
        );
    }
    Ok(())
}

fn build_localized_cli() -> clap::Command {
    Cli::command()
        .about(l10n::t_("System A — Unit State Controller"))
        .mut_arg("debug", |a| a.help(l10n::t_("Enable debug-level logging.")))
        .mut_arg("log_level", |a| {
            a.help(l10n::t_("Log level (trace, debug, info, warn, error)."))
        })
        .mut_arg("no_pager", |a| {
            a.help(l10n::t_("Do not pipe output into a pager."))
        })
        .mut_arg("color", |a| {
            a.help(l10n::t_("When to use colors (always, auto, never)."))
        })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    sysa::paths::init();
    sysa::l10n::init();

    let matches = build_localized_cli().get_matches();
    let debug = *matches.get_one::<bool>("debug").unwrap_or(&false);
    let log_level = matches
        .get_one::<String>("log_level")
        .map(|s| s.as_str())
        .unwrap_or("warn");
    let no_pager = *matches.get_one::<bool>("no_pager").unwrap_or(&false);
    let color = *matches
        .get_one::<ColorChoice>("color")
        .unwrap_or(&ColorChoice::Auto);

    let level = if debug { "debug" } else { log_level };
    init_tracing(level);

    let use_pager = !no_pager;
    let _guard: PagerGuard = sysa_pager::open(PagerConfig {
        disable: !use_pager,
    })?;

    match color {
        ColorChoice::Always => colored::control::set_override(true),
        ColorChoice::Never => colored::control::set_override(false),
        ColorChoice::Auto => {
            if use_pager {
                colored::control::set_override(true);
            }
        }
    }

    let result = run().await;

    match result {
        Err(e) => match e.downcast_ref::<std::io::Error>() {
            Some(ioe) if ioe.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            _ => Err(e),
        },
        Ok(()) => Ok(()),
    }
}
