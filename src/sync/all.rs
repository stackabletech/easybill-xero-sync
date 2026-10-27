//! `sync all`: run the customer sync, then the document sync, in one pass.
//!
//! Customers are synced first so invoices can resolve their contact by
//! `ContactNumber`. Per-customer failures (including blocked customers) don't
//! stop the document phase — they usually wait for a human, and only that
//! customer's documents are affected. A run-level failure in the customer phase
//! (auth, easybill, state) aborts before touching documents. The run fails if
//! either phase reported per-item failures.

use std::path::Path;

use crate::config::Config;
use crate::sync::{customers, documents, Error, ItemsFailedSnafu, Result};
use crate::xero::token::AuthMode;

/// Options for a `sync all` run.
#[derive(Debug, Clone)]
pub struct Options {
    /// `--since` passed to the customer phase.
    pub since: Option<chrono::NaiveDateTime>,

    /// `--from` passed to the document phase.
    pub from: String,

    /// `--to` passed to the document phase.
    pub to: Option<String>,

    /// Print payloads only; no API writes.
    pub dry_run: bool,

    /// Skip PDF attachment in the document phase.
    pub no_pdf: bool,
}

/// Runs the customer sync, then the document sync.
///
/// # Errors
///
/// Propagates a run-level error from either phase. Per-item failures of both
/// phases are added up into one [`Error::ItemsFailed`].
pub async fn run(
    config: &Config,
    auth_mode: AuthMode,
    state_override: Option<&Path>,
    options: &Options,
) -> Result<()> {
    let customer_options = customers::Options {
        since: options.since,
        dry_run: options.dry_run,
        preview: false,
    };
    let customer_failures =
        match customers::run(config, auth_mode, state_override, &customer_options).await {
            Ok(()) => None,
            Err(Error::ItemsFailed { failed, total }) => {
                tracing::warn!(
                    failed,
                    total,
                    "some customers failed; continuing with documents"
                );
                Some((failed, total))
            }
            Err(e) => return Err(e),
        };

    let document_options = documents::Options {
        from: options.from.clone(),
        to: options.to.clone(),
        dry_run: options.dry_run,
        no_pdf: options.no_pdf,
    };
    let documents = documents::run(config, auth_mode, state_override, &document_options).await;
    match (customer_failures, documents) {
        (None, result) => result,
        (Some((failed, total)), Ok(())) => ItemsFailedSnafu { failed, total }.fail(),
        (
            Some((failed, total)),
            Err(Error::ItemsFailed {
                failed: doc_failed,
                total: doc_total,
            }),
        ) => ItemsFailedSnafu {
            failed: failed + doc_failed,
            total: total + doc_total,
        }
        .fail(),
        (Some(_), Err(e)) => Err(e),
    }
}
