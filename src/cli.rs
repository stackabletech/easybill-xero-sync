//! Command-line interface definition (via `clap` derive).

use std::path::PathBuf;

use chrono::{NaiveDate, NaiveDateTime};
use clap::{Args, Parser, Subcommand};

use crate::xero::token::AuthMode;

/// Top-level CLI parser.
#[derive(Debug, Parser)]
#[command(
    name = "easybill-xero",
    version,
    about = "One-way sync from easybill into Xero."
)]
pub struct Cli {
    /// Path to the state file.
    ///
    /// Defaults to `$XDG_STATE_HOME/easybill-xero/state.json`, falling back to
    /// `~/.local/state/easybill-xero/state.json`.
    #[arg(long, global = true, env = "EASYBILL_XERO_STATE_FILE")]
    pub state_file: Option<PathBuf>,

    /// Path to the TOML config file (tax + account tables).
    ///
    /// Defaults to `./config.toml`. Required by `xero check` and the `sync`
    /// commands; unused by `xero login` / `xero refresh`.
    #[arg(long, global = true, env = "EASYBILL_XERO_CONFIG")]
    pub config: Option<PathBuf>,

    /// How to authenticate against Xero.
    ///
    /// `oauth`: browser login via `xero login`, refresh token kept in the state
    /// file (development). `client-credentials`: a Xero custom connection, no
    /// login and no stored token (production).
    #[arg(long, global = true, env = "XERO_AUTH", value_enum, default_value_t = AuthMode::Oauth)]
    pub xero_auth: AuthMode,

    /// Log format on stderr: human-readable `text`, or `json` (one object per
    /// line) for log collectors.
    #[arg(
        long,
        global = true,
        env = "EASYBILL_XERO_LOG_FORMAT",
        value_enum,
        default_value_t = LogFormat::Text
    )]
    pub log_format: LogFormat,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The `--log-format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human-readable lines (coloured on a terminal).
    Text,
    /// One JSON object per line.
    Json,
}

/// Top-level command groups.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Xero authentication and connectivity.
    Xero {
        /// The Xero subcommand to run.
        #[command(subcommand)]
        command: XeroCommand,
    },

    /// Push easybill data into Xero.
    Sync {
        /// The sync subcommand to run.
        #[command(subcommand)]
        command: SyncCommand,
    },
}

/// Subcommands under `sync`.
#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    /// Upsert easybill customers as Xero contacts.
    Customers(SyncCustomersArgs),

    /// Push easybill invoices and credit notes into Xero (as DRAFT).
    Documents(SyncDocumentsArgs),

    /// Run `customers` then `documents` in one pass.
    All(SyncAllArgs),
}

/// Arguments for `sync all` (union of the customer + document options).
#[derive(Debug, Args)]
pub struct SyncAllArgs {
    /// Only sync customers modified on or after this point in easybill's local
    /// time: `YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS` (or `T` instead of the space).
    #[arg(long, value_parser = parse_since)]
    pub since: Option<NaiveDateTime>,

    /// Inclusive start of the document-date window (`YYYY-MM-DD`).
    #[arg(long)]
    pub from: NaiveDate,

    /// Inclusive end of the document-date window (`YYYY-MM-DD`); open-ended if
    /// omitted.
    #[arg(long)]
    pub to: Option<NaiveDate>,

    /// Print the Xero payloads instead of sending them (no API writes).
    #[arg(long)]
    pub dry_run: bool,

    /// Skip attaching the easybill PDF to each Xero document.
    #[arg(long)]
    pub no_pdf: bool,
}

/// Arguments for `sync documents`.
#[derive(Debug, Args)]
pub struct SyncDocumentsArgs {
    /// Inclusive start of the document-date window (`YYYY-MM-DD`).
    #[arg(long)]
    pub from: NaiveDate,

    /// Inclusive end of the document-date window (`YYYY-MM-DD`); open-ended if
    /// omitted.
    #[arg(long)]
    pub to: Option<NaiveDate>,

    /// Print the Xero payloads instead of sending them (no API writes).
    #[arg(long)]
    pub dry_run: bool,

    /// Skip attaching the easybill PDF to each Xero document.
    #[arg(long)]
    pub no_pdf: bool,
}

/// Arguments for `sync customers`.
#[derive(Debug, Args)]
pub struct SyncCustomersArgs {
    /// Only sync customers modified on or after this point in easybill's local
    /// time: `YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS` (or `T` instead of the space).
    #[arg(long, value_parser = parse_since)]
    pub since: Option<NaiveDateTime>,

    /// Print the Xero payloads instead of sending them (no API writes).
    #[arg(long)]
    pub dry_run: bool,

    /// Read the existing Xero contacts and print how each customer would be
    /// linked (update / create / blocked), without writing to Xero.
    #[arg(long, conflicts_with = "dry_run")]
    pub preview: bool,
}

/// Subcommands under `xero`.
#[derive(Debug, Subcommand)]
pub enum XeroCommand {
    /// Perform the one-time OAuth2 browser login and store the refresh token.
    Login(LoginArgs),

    /// Refresh the stored token to keep it alive (single-use rotation).
    Refresh,

    /// Validate the tax/account config against the connected org.
    Check(CheckArgs),
}

/// Arguments for `xero check`.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Also scan easybill documents dated from this day (`YYYY-MM-DD`), plus
    /// all customers, and list every tax key in use with its `[tax]` mapping.
    /// Needs `EASYBILL_API_TOKEN`.
    #[arg(long)]
    pub from: Option<NaiveDate>,

    /// Inclusive end of the scanned document-date window (`YYYY-MM-DD`);
    /// open-ended if omitted.
    #[arg(long, requires = "from")]
    pub to: Option<NaiveDate>,

    /// Also list every scanned document line (document number, line, VAT
    /// rate, tax key and TaxType, booking account and AccountCode), flagging
    /// lines that would fail to sync.
    #[arg(long, requires = "from")]
    pub verbose: bool,
}

/// Arguments for `xero login`.
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Local TCP port for the OAuth2 redirect listener.
    ///
    /// Must match the redirect URI registered on the Xero app
    /// (`http://localhost:<port>/callback`).
    #[arg(long, default_value_t = 8080)]
    pub port: u16,

    /// Tenant name or id to select when the login connects more than one org.
    #[arg(long)]
    pub tenant: Option<String>,
}

/// Parses `--since`: a date (midnight) or a date and time, without an offset
/// (easybill timestamps are local time).
fn parse_since(value: &str) -> Result<NaiveDateTime, String> {
    let value = value.trim();
    ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })
        .ok_or_else(|| {
            String::from(
                "expected YYYY-MM-DD or YYYY-MM-DD HH:MM:SS in easybill's local time, without an offset",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xero_auth_defaults_to_oauth() {
        let cli = Cli::try_parse_from(["easybill-xero", "xero", "refresh"]).expect("parses");
        // XERO_AUTH is not set in the test environment.
        if std::env::var_os("XERO_AUTH").is_none() {
            assert_eq!(cli.xero_auth, AuthMode::Oauth);
        }
    }

    #[test]
    fn xero_auth_is_global_and_accepts_client_credentials() {
        let cli = Cli::try_parse_from([
            "easybill-xero",
            "sync",
            "documents",
            "--from",
            "2026-09-01",
            "--xero-auth",
            "client-credentials",
        ])
        .expect("parses");
        assert_eq!(cli.xero_auth, AuthMode::ClientCredentials);
    }

    #[test]
    fn dates_are_validated() {
        assert!(
            Cli::try_parse_from(["easybill-xero", "sync", "documents", "--from", "yesterday"])
                .is_err()
        );
        assert!(Cli::try_parse_from([
            "easybill-xero",
            "sync",
            "documents",
            "--from",
            "2026-02-30"
        ])
        .is_err());
    }

    #[test]
    fn since_accepts_date_or_datetime_without_offset() {
        let midnight = parse_since("2026-06-01").expect("date");
        assert_eq!(midnight.to_string(), "2026-06-01 00:00:00");
        assert_eq!(
            parse_since("2026-06-01T09:30:00").expect("datetime"),
            parse_since("2026-06-01 09:30:00").expect("datetime")
        );
        assert!(parse_since("2026-06-01T09:30:00Z").is_err());
        assert!(parse_since("yesterday").is_err());
    }

    #[test]
    fn xero_auth_rejects_unknown_mode() {
        assert!(
            Cli::try_parse_from(["easybill-xero", "--xero-auth", "bogus", "xero", "refresh"])
                .is_err()
        );
    }
}
