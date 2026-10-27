//! Sync orchestration: the `sync customers` / `sync documents` / `sync all`
//! commands that tie the easybill reads, the pure mappings, the Xero writes and
//! the state file together.

pub mod all;
pub mod customers;
pub mod documents;

use snafu::Snafu;

/// Errors that abort a whole sync run (as opposed to per-item failures, which
/// are collected and reported without aborting).
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    /// Obtaining a Xero access token failed.
    #[snafu(display("Authentication error"))]
    Token { source: crate::xero::token::Error },

    /// Building the Xero API client failed.
    #[snafu(display("Xero API client error"))]
    Client { source: crate::xero::client::Error },

    /// Building the easybill client failed.
    #[snafu(display("easybill client error"))]
    Easybill { source: easybill::client::Error },

    /// The state file could not be loaded or saved.
    #[snafu(display("State file error"))]
    State { source: crate::state::Error },

    /// A document could not be mapped to a Xero payload.
    #[snafu(display("Document mapping failed"))]
    Mapping {
        source: Box<crate::xero::document::Error>,
    },

    /// The Xero-computed total did not match the easybill amount.
    #[snafu(display("Total verification failed"))]
    TotalMismatch {
        source: crate::xero::document::TotalMismatch,
    },

    /// A Xero write returned success but no record to read the id back from.
    #[snafu(display("Xero returned an empty response for {context}"))]
    EmptyResponse { context: &'static str },

    /// One or more items failed to sync (details already logged).
    #[snafu(display("{failed} of {total} item(s) failed to sync"))]
    ItemsFailed { failed: usize, total: usize },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Formats an error and its full source chain on one line (`outer: inner: ...`),
/// for per-item log lines where only the top-level message would otherwise show.
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
