//! `easybill-xero`: a one-way sync CLI pushing accounting data from easybill into
//! a Xero organisation.
//!
//! Subcommands: `xero login` / `xero refresh` (OAuth2 + token keepalive, dev
//! only), `xero check` (validate the tax/account config against the org), and
//! `sync customers|documents|all` (push easybill data into Xero). State lives in
//! a single on-disk file. `--xero-auth` selects OAuth (dev) or a custom
//! connection via client credentials (production).
//!
//! Each invocation runs in one `run` span with a `run_id`; see [`telemetry`].

mod cli;
mod config;
mod state;
mod sync;
mod telemetry;
mod xero;

use std::error::Error as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use snafu::{ResultExt, Snafu};
use tracing::Instrument as _;

use cli::{Cli, Command, LogFormat, SyncCommand, XeroCommand};
use config::Config;
use xero::token::AuthMode;

/// Top-level binary error wrapping each subsystem's module-local error.
#[derive(Debug, Snafu)]
enum Error {
    /// A Xero authentication (`login` / `refresh`) command failed.
    #[snafu(display("Xero authentication failed"))]
    Auth { source: xero::auth::Error },

    /// `xero login` / `xero refresh` were run in client-credentials mode.
    #[snafu(display(
        "`xero login` and `xero refresh` only apply to --xero-auth oauth; a custom connection (client-credentials) needs no login or refresh"
    ))]
    NoLoginWithClientCredentials,

    /// `--to` is before `--from`.
    #[snafu(display("--to {to} is before --from {from}"))]
    InvalidWindow {
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    },

    /// The configuration file could not be loaded.
    #[snafu(display("Failed to load configuration"))]
    Config { source: config::Error },

    /// The `xero check` command failed.
    #[snafu(display("Config validation against the org failed"))]
    Check { source: xero::check::Error },

    /// A `sync` command failed.
    #[snafu(display("Sync failed"))]
    Sync { source: sync::Error },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let log_format = cli.log_format;
    let (telemetry, otlp_errors) = telemetry::init(log_format);
    for error in otlp_errors {
        // Traces and metrics are optional: log and carry on without them.
        tracing::warn!(
            error = %sync::error_chain(&error),
            "OTLP export disabled"
        );
    }
    let state_override = cli.state_file.clone();
    let report = reported_command(&cli.command);

    let span = tracing::info_span!(
        "run",
        run_id = tracing::field::Empty,
        command = command_name(&cli.command),
        otel.status_code = tracing::field::Empty,
    );
    span.record("run_id", telemetry::run_id(&span));

    let result = async {
        tracing::info!(version = env!("CARGO_PKG_VERSION"), "run started");
        let result = run(cli).await;
        if let Some(command) = report {
            let run_ok = matches!(
                result,
                Ok(())
                    | Err(Error::Sync {
                        source: sync::Error::ItemsFailed { .. }
                    })
            );
            sync::report::publish(state_override.as_deref(), command, run_ok);
        }
        match &result {
            Ok(()) => tracing::info!("run finished"),
            Err(error) => {
                tracing::Span::current().record("otel.status_code", "error");
                // In text mode the error is printed below, readably.
                if log_format == LogFormat::Json {
                    tracing::error!(error = %sync::error_chain(error), "run failed");
                }
            }
        }
        result
    }
    .instrument(span)
    .await;
    telemetry.shutdown().await;

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if log_format == LogFormat::Text {
                // Print the human-readable message plus its cause chain, not
                // the Debug representation (which would include a backtrace).
                eprintln!("Error: {error}");
                let mut source = error.source();
                while let Some(cause) = source {
                    eprintln!("  caused by: {cause}");
                    source = cause.source();
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// The command to record in the status snapshot: real `sync` runs only, not
/// dry-runs or previews (which change nothing).
fn reported_command(command: &Command) -> Option<&'static str> {
    let reported = match command {
        Command::Sync { command } => match command {
            SyncCommand::Customers(args) => !args.dry_run && !args.preview,
            SyncCommand::Documents(args) => !args.dry_run,
            SyncCommand::All(args) => !args.dry_run,
        },
        Command::Xero { .. } => false,
    };
    reported.then(|| command_name(command))
}

/// The subcommand as typed (`sync documents`), for the `run` span.
fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Xero { command } => match command {
            XeroCommand::Login(_) => "xero login",
            XeroCommand::Refresh => "xero refresh",
            XeroCommand::Check(_) => "xero check",
        },
        Command::Sync { command } => match command {
            SyncCommand::Customers(_) => "sync customers",
            SyncCommand::Documents(_) => "sync documents",
            SyncCommand::All(_) => "sync all",
        },
    }
}

/// Dispatches to the selected command.
async fn run(cli: Cli) -> Result<(), Error> {
    let state_override = cli.state_file.as_deref();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(config::DEFAULT_CONFIG_PATH));

    match cli.command {
        Command::Xero {
            command: XeroCommand::Login(_) | XeroCommand::Refresh,
        } if cli.xero_auth == AuthMode::ClientCredentials => {
            NoLoginWithClientCredentialsSnafu.fail()
        }
        Command::Xero { command } => match command {
            XeroCommand::Login(args) => {
                xero::auth::login(state_override, args.port, args.tenant.as_deref())
                    .await
                    .context(AuthSnafu)
            }
            XeroCommand::Refresh => xero::auth::refresh(state_override).await.context(AuthSnafu),
            XeroCommand::Check(args) => {
                let config = Config::load(&config_path).context(ConfigSnafu)?;
                check_window(args.from, args.to)?;
                let options = xero::check::Options {
                    from: args.from.map(|d| d.to_string()),
                    to: args.to.map(|d| d.to_string()),
                    verbose: args.verbose,
                };
                xero::check::run(&config, cli.xero_auth, state_override, &options)
                    .await
                    .context(CheckSnafu)
            }
        },
        Command::Sync { command } => match command {
            SyncCommand::Customers(args) => {
                let config = Config::load(&config_path).context(ConfigSnafu)?;
                let options = sync::customers::Options {
                    since: args.since,
                    dry_run: args.dry_run,
                    preview: args.preview,
                };
                sync::customers::run(&config, cli.xero_auth, state_override, &options)
                    .await
                    .context(SyncSnafu)
            }
            SyncCommand::Documents(args) => {
                let config = Config::load(&config_path).context(ConfigSnafu)?;
                check_window(Some(args.from), args.to)?;
                let options = sync::documents::Options {
                    from: args.from.to_string(),
                    to: args.to.map(|d| d.to_string()),
                    dry_run: args.dry_run,
                    no_pdf: args.no_pdf,
                };
                sync::documents::run(&config, cli.xero_auth, state_override, &options)
                    .await
                    .context(SyncSnafu)
            }
            SyncCommand::All(args) => {
                let config = Config::load(&config_path).context(ConfigSnafu)?;
                check_window(Some(args.from), args.to)?;
                let options = sync::all::Options {
                    since: args.since,
                    from: args.from.to_string(),
                    to: args.to.map(|d| d.to_string()),
                    dry_run: args.dry_run,
                    no_pdf: args.no_pdf,
                };
                sync::all::run(&config, cli.xero_auth, state_override, &options)
                    .await
                    .context(SyncSnafu)
            }
        },
    }
}

/// Rejects a date window whose end is before its start.
fn check_window(
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
) -> Result<(), Error> {
    match (from, to) {
        (Some(from), Some(to)) if to < from => InvalidWindowSnafu { from, to }.fail(),
        _ => Ok(()),
    }
}
