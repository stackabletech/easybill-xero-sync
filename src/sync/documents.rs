//! `sync documents`: map easybill invoices/credit notes to Xero and push them.
//!
//! Flow: fetch non-draft, non-cancelled `INVOICE`/`CREDIT` documents in the date
//! window (the list already carries each document's `items[]`), drop those in
//! `[sync] skip_documents`, map each to a Xero invoice or credit note (DRAFT),
//! create (`PUT`) or update (`POST` by id) it, verify the returned Total against
//! the easybill `amount`, and record the id in state. Documents already
//! finalised in Xero (not `DRAFT`/`SUBMITTED`) are not modified, only their
//! total is verified; deleted or voided ones are left alone. Per-document
//! failures are logged and the run continues; the exit is non-zero if any failed.
//!
//! The easybill PDF is attached to each Xero document unless `--no-pdf` is given.

use std::path::Path;

use chrono::Utc;
use snafu::{OptionExt, ResultExt};

use crate::config::Config;
use crate::state::{self, DocumentKind, DocumentMapping, State};
use crate::sync::{
    ClientSnafu, EasybillSnafu, EmptyResponseSnafu, Error, ItemsFailedSnafu, Result, StateSnafu,
    TokenSnafu,
};
use crate::xero::attachment::{attachment_filename, attachment_path, Resource};
use crate::xero::client::ApiClient;
use crate::xero::document::{map_document, verify_total, Mapped};
use crate::xero::models::{
    CreditNoteResult, CreditNotesRequest, CreditNotesResponse, InvoiceResult, InvoicesRequest,
    InvoicesResponse,
};
use crate::xero::token::{AuthMode, TokenManager};
use easybill::client::EasybillClient;
use easybill::models::Document;

/// Xero type of a sales invoice.
const TYPE_ACCREC: &str = "ACCREC";

/// Xero type of a sales credit note.
const TYPE_ACCRECCREDIT: &str = "ACCRECCREDIT";

/// Options for a `sync documents` run.
#[derive(Debug, Clone)]
pub struct Options {
    /// Inclusive start of the `document_date` window (`YYYY-MM-DD`).
    pub from: String,

    /// Inclusive end of the window (`YYYY-MM-DD`); open-ended when `None`.
    pub to: Option<String>,

    /// Print the payloads and make no API writes.
    pub dry_run: bool,

    /// Skip attaching the easybill PDF to each Xero document.
    pub no_pdf: bool,
}

/// Runs `sync documents`.
///
/// # Errors
///
/// Returns [`Error::Easybill`], [`Error::Token`], [`Error::Client`] or
/// [`Error::State`] on a run-level failure, and [`Error::ItemsFailed`] if any
/// individual document could not be synced (after attempting the rest).
pub async fn run(
    config: &Config,
    auth_mode: AuthMode,
    state_override: Option<&Path>,
    options: &Options,
) -> Result<()> {
    let easybill = EasybillClient::from_env().context(EasybillSnafu)?;
    let (documents, skipped) = without_skipped(config, fetch_documents(&easybill, options).await?);
    tracing::info!(
        count = documents.len(),
        skipped,
        "documents selected for sync"
    );

    let state_path = state::resolve_path(state_override).context(StateSnafu)?;

    if options.dry_run {
        let current = State::load(&state_path).context(StateSnafu)?;
        return dry_run(config, &current, &documents);
    }

    let manager = TokenManager::from_env(auth_mode, state_override, config.xero.tenant_id.clone())
        .context(TokenSnafu)?;
    let client = ApiClient::connect(manager).await.context(ClientSnafu)?;
    // Read-only snapshot of the known mappings. Writes go through
    // `State::update`, which re-reads the file, because the client may rotate
    // the refresh token in it during the run.
    let current = State::load(&state_path).context(StateSnafu)?;

    let total = documents.len();
    let mut failed = 0usize;
    let mut finalised = 0usize;
    let mut removed = 0usize;
    for summary in &documents {
        match sync_one(&client, config, &current, summary).await {
            Ok(Synced {
                mut mapping,
                outcome,
            }) => {
                let number = summary.number.as_deref().unwrap_or("");
                match &outcome {
                    Outcome::Pushed => {}
                    Outcome::Finalised(status) => {
                        finalised += 1;
                        tracing::info!(
                            document_id = summary.id,
                            number,
                            %status,
                            "already finalised in Xero; left unchanged (total matches)"
                        );
                    }
                    Outcome::Removed(status) => {
                        removed += 1;
                        tracing::warn!(
                            document_id = summary.id,
                            number,
                            %status,
                            "deleted or voided in Xero; left alone (add it to [sync] skip_documents if that's intended)"
                        );
                    }
                }
                // Keep the uploaded flag if the document is still the same one.
                mapping.pdf_uploaded = current
                    .documents
                    .get(&summary.id.to_string())
                    .is_some_and(|m| m.pdf_uploaded && m.xero_id == mapping.xero_id);
                // Attach the easybill PDF unless suppressed, already uploaded,
                // or the Xero document is gone.
                let removed_in_xero = matches!(outcome, Outcome::Removed(_));
                if !options.no_pdf && !mapping.pdf_uploaded && !removed_in_xero {
                    match attach_pdf(&easybill, &client, summary.id, &mapping).await {
                        Ok(()) => mapping.pdf_uploaded = true,
                        Err(e) => {
                            failed += 1;
                            tracing::error!(document_id = summary.id, error = %super::error_chain(&e), "PDF attach failed; document synced but marked failed");
                        }
                    }
                }
                State::update(&state_path, |s| {
                    s.documents.insert(summary.id.to_string(), mapping);
                })
                .context(StateSnafu)?;
            }
            Err(e) => {
                failed += 1;
                tracing::error!(document_id = summary.id, error = %super::error_chain(&e), "failed to sync document; continuing");
            }
        }
    }

    println!(
        "sync documents: {} succeeded ({} already finalised in Xero, left unchanged; {} deleted/voided in Xero, left alone), {} failed, of {} selected ({} skipped by config).",
        total - failed,
        finalised,
        removed,
        failed,
        total,
        skipped
    );
    if failed > 0 {
        return ItemsFailedSnafu { failed, total }.fail();
    }
    Ok(())
}

/// The upper bound used for an open-ended (`--to` omitted) date window.
///
/// easybill's `document_date` range requires **both** bounds — a trailing comma
/// (`from,`) matches nothing — so an absent `--to` maps to a far-future date.
const OPEN_ENDED_TO: &str = "2999-12-31";

/// Fetches the documents selected for sync.
async fn fetch_documents(easybill: &EasybillClient, options: &Options) -> Result<Vec<Document>> {
    let filters = document_filters(&options.from, options.to.as_deref());
    easybill
        .all_documents(&filters)
        .await
        .context(EasybillSnafu)
}

/// Drops the documents listed in `[sync] skip_documents`, logging each, and
/// returns the rest with the number dropped. Shared with `xero check`.
pub(crate) fn without_skipped(config: &Config, documents: Vec<Document>) -> (Vec<Document>, usize) {
    let (skipped, kept): (Vec<_>, Vec<_>) = documents
        .into_iter()
        .partition(|d| config.skips_document(d.id, d.number.as_deref()));
    for document in &skipped {
        tracing::info!(
            document_id = document.id,
            number = document.number.as_deref().unwrap_or(""),
            "skipped: listed in [sync] skip_documents"
        );
    }
    (kept, skipped.len())
}

/// The easybill filters selecting syncable documents (non-draft, non-cancelled
/// invoices and credit notes) in a `document_date` window. Shared with
/// `xero check` so it scans exactly the documents a sync would push.
pub(crate) fn document_filters(from: &str, to: Option<&str>) -> Vec<(&'static str, String)> {
    vec![
        ("type", "INVOICE,CREDIT".to_owned()),
        ("is_draft", "false".to_owned()),
        ("cancel_id", "null".to_owned()),
        ("document_date", date_range(from, to)),
    ]
}

/// Builds the easybill `document_date` range value, always with both bounds.
fn date_range(from: &str, to: Option<&str>) -> String {
    format!("{from},{}", to.unwrap_or(OPEN_ENDED_TO))
}

/// Prints the mapped payload for each document without making API writes.
///
/// The `GET /documents` list already returns each document in full (including
/// `items[]`), so no per-document re-fetch is needed.
fn dry_run(config: &Config, state: &State, documents: &[Document]) -> Result<()> {
    let total = documents.len();
    let mut failed = 0usize;
    for document in documents {
        let known = known_id(state, document.id);
        match map_document(document, config, known) {
            Ok(mapped) => print_dry_run(document.id, &mapped),
            Err(e) => {
                failed += 1;
                println!("--- document {} -> mapping error: {e}", document.id);
            }
        }
    }
    println!(
        "dry-run: {} document(s) would be synced, {failed} would fail mapping.",
        total - failed
    );
    if failed > 0 {
        return ItemsFailedSnafu { failed, total }.fail();
    }
    Ok(())
}

/// The outcome of syncing one document.
#[derive(Debug)]
struct Synced {
    /// The state mapping to record.
    mapping: DocumentMapping,

    /// What happened in Xero.
    outcome: Outcome,
}

/// What a sync did with a document in Xero.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// Created or updated.
    Pushed,

    /// Already finalised (e.g. `AUTHORISED`, `PAID`): left unchanged, total
    /// verified.
    Finalised(String),

    /// Deleted or voided in Xero: left alone, total not checked.
    Removed(String),
}

/// How an existing Xero document may be treated, by status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    /// `DRAFT` / `SUBMITTED`: may be overwritten.
    Editable,
    /// Approved or paid: the API won't modify it.
    Finalised,
    /// `DELETED` / `VOIDED`: someone removed it in Xero.
    Removed,
}

/// Classifies an existing Xero document by its status. A missing status is
/// treated as editable (Xero always returns one in practice).
fn disposition(status: Option<&str>) -> Disposition {
    match status {
        None | Some("DRAFT" | "SUBMITTED") => Disposition::Editable,
        Some("DELETED" | "VOIDED") => Disposition::Removed,
        Some(_) => Disposition::Finalised,
    }
}

/// An invoice or credit note that already exists in Xero.
#[derive(Debug)]
struct Existing {
    xero_id: String,
    status: Option<String>,
    total: Option<rust_decimal::Decimal>,
}

impl From<InvoiceResult> for Existing {
    fn from(i: InvoiceResult) -> Self {
        Self {
            xero_id: i.invoice_id,
            status: i.status,
            total: i.total,
        }
    }
}

impl From<CreditNoteResult> for Existing {
    fn from(c: CreditNoteResult) -> Self {
        Self {
            xero_id: c.credit_note_id,
            status: c.status,
            total: c.total,
        }
    }
}

/// Syncs a single document end to end, returning its state mapping on success.
///
/// `document` is the full record from the list fetch; there is no per-document
/// re-fetch. A document that already exists in Xero (known from state, or found
/// by number) is only updated while still editable; a finalised one is left
/// as is and only its total is verified, a deleted or voided one is left alone.
async fn sync_one(
    client: &ApiClient,
    config: &Config,
    state: &State,
    document: &Document,
) -> Result<Synced> {
    let id = document.id;
    let known = known_id(state, id);
    let mapped = map_document(document, config, known).map_err(|e| Error::Mapping {
        source: Box::new(e),
    })?;

    let (kind, existing) = match &mapped {
        Mapped::Invoice(invoice) => (
            DocumentKind::Invoice,
            find_invoice(client, known, &invoice.invoice_number).await?,
        ),
        Mapped::CreditNote(credit_note) => (
            DocumentKind::CreditNote,
            find_credit_note(client, known, &credit_note.credit_note_number).await?,
        ),
    };
    let mapping = |xero_id: String| DocumentMapping {
        xero_id,
        kind,
        pdf_uploaded: false,
        synced_at: Utc::now(),
    };

    if let Some(existing) = &existing {
        let status = existing.status.clone().unwrap_or_default();
        match disposition(existing.status.as_deref()) {
            Disposition::Editable => {}
            Disposition::Finalised => {
                check_total(id, document.amount, existing.total)?;
                return Ok(Synced {
                    mapping: mapping(existing.xero_id.clone()),
                    outcome: Outcome::Finalised(status),
                });
            }
            Disposition::Removed => {
                return Ok(Synced {
                    mapping: mapping(existing.xero_id.clone()),
                    outcome: Outcome::Removed(status),
                });
            }
        }
    }
    // Update the existing document by id, or create a new one. A stale id from
    // state that Xero no longer knows is dropped here.
    let existing_id = existing.map(|e| e.xero_id);

    let (xero_id, total) = match mapped {
        Mapped::Invoice(mut invoice) => {
            invoice.invoice_id = existing_id;
            let update = invoice.invoice_id.is_some();
            let request = InvoicesRequest {
                invoices: vec![*invoice],
            };
            let response = if update {
                client.update_invoices(&request).await
            } else {
                client.create_invoices(&request).await
            }
            .context(ClientSnafu)?;
            let result = first_invoice(response)?;
            (result.invoice_id, result.total)
        }
        Mapped::CreditNote(mut credit_note) => {
            credit_note.credit_note_id = existing_id;
            let update = credit_note.credit_note_id.is_some();
            let request = CreditNotesRequest {
                credit_notes: vec![*credit_note],
            };
            let response = if update {
                client.update_credit_notes(&request).await
            } else {
                client.create_credit_notes(&request).await
            }
            .context(ClientSnafu)?;
            let result = first_credit_note(response)?;
            (result.credit_note_id, result.total)
        }
    };
    check_total(id, document.amount, total)?;
    Ok(Synced {
        mapping: mapping(xero_id),
        outcome: Outcome::Pushed,
    })
}

/// Fetches the easybill PDF and attaches it to the Xero invoice/credit note.
///
/// Idempotent: uses `POST …/Attachments/{FileName}`, which replaces an existing
/// attachment of the same name.
async fn attach_pdf(
    easybill: &EasybillClient,
    client: &ApiClient,
    easybill_id: i64,
    mapping: &DocumentMapping,
) -> Result<()> {
    let bytes = easybill
        .document_pdf(easybill_id)
        .await
        .context(EasybillSnafu)?;
    let resource = match mapping.kind {
        DocumentKind::Invoice => Resource::Invoices,
        DocumentKind::CreditNote => Resource::CreditNotes,
    };
    // Name the file after the easybill document id (stable, always URL-safe).
    let file_name = attachment_filename(&easybill_id.to_string());
    let path = attachment_path(resource, &mapping.xero_id, &file_name);
    client
        .upload_pdf_attachment(&path, bytes)
        .await
        .context(ClientSnafu)
}

/// Finds an existing sales invoice: by the id from state when known, else
/// (state-loss safety net, avoiding duplicate-number errors) by number. A known
/// id Xero doesn't have (404, e.g. state from another org) falls back to the
/// number lookup. Supplier bills (`ACCPAY`) with the same number are ignored.
async fn find_invoice(
    client: &ApiClient,
    known: Option<&str>,
    number: &str,
) -> Result<Option<Existing>> {
    if let Some(xero_id) = known {
        match client.invoice_by_id(xero_id).await {
            Ok(response) => {
                if let Some(found) = response
                    .invoices
                    .into_iter()
                    .find(|i| i.invoice_id == xero_id)
                {
                    return Ok(Some(found.into()));
                }
            }
            Err(e) if is_not_found(&e) => {
                tracing::warn!(
                    xero_id,
                    number,
                    "mapped Xero invoice not found; looking up by number"
                );
            }
            Err(e) => return Err(e).context(ClientSnafu),
        }
    }
    let response = client
        .invoices_by_number(&[number.to_owned()])
        .await
        .context(ClientSnafu)?;
    Ok(response
        .invoices
        .into_iter()
        .find(|i| i.invoice_type.as_deref() == Some(TYPE_ACCREC) && i.invoice_number == number)
        .map(Into::into))
}

/// Finds an existing sales credit note, like [`find_invoice`]. Supplier
/// credit notes (`ACCPAYCREDIT`) with the same number are ignored.
async fn find_credit_note(
    client: &ApiClient,
    known: Option<&str>,
    number: &str,
) -> Result<Option<Existing>> {
    if let Some(xero_id) = known {
        match client.credit_note_by_id(xero_id).await {
            Ok(response) => {
                if let Some(found) = response
                    .credit_notes
                    .into_iter()
                    .find(|c| c.credit_note_id == xero_id)
                {
                    return Ok(Some(found.into()));
                }
            }
            Err(e) if is_not_found(&e) => {
                tracing::warn!(
                    xero_id,
                    number,
                    "mapped Xero credit note not found; looking up by number"
                );
            }
            Err(e) => return Err(e).context(ClientSnafu),
        }
    }
    let response = client
        .credit_note_by_number(number)
        .await
        .context(ClientSnafu)?;
    Ok(response
        .credit_notes
        .into_iter()
        .find(|c| {
            c.credit_note_type.as_deref() == Some(TYPE_ACCRECCREDIT)
                && c.credit_note_number == number
        })
        .map(Into::into))
}

/// Whether a Xero call failed with HTTP 404.
fn is_not_found(error: &crate::xero::client::Error) -> bool {
    matches!(error, crate::xero::client::Error::Api { status: 404, .. })
}

/// The known Xero id for a document from state, if any.
fn known_id(state: &State, id: i64) -> Option<&str> {
    state
        .documents
        .get(&id.to_string())
        .map(|m| m.xero_id.as_str())
}

/// Verifies the Xero total against the easybill amount, erroring on mismatch.
fn check_total(
    id: i64,
    amount_cents: i64,
    xero_total: Option<rust_decimal::Decimal>,
) -> Result<()> {
    if let Err(mismatch) = verify_total(amount_cents, xero_total) {
        tracing::error!(document_id = id, %mismatch, "total mismatch; marking document failed");
        return Err(Error::TotalMismatch { source: mismatch });
    }
    Ok(())
}

/// Extracts the first invoice from a response, or errors.
fn first_invoice(response: InvoicesResponse) -> Result<InvoiceResult> {
    response
        .invoices
        .into_iter()
        .next()
        .context(EmptyResponseSnafu {
            context: "PUT/POST /Invoices",
        })
}

/// Extracts the first credit note from a response, or errors.
fn first_credit_note(response: CreditNotesResponse) -> Result<CreditNoteResult> {
    response
        .credit_notes
        .into_iter()
        .next()
        .context(EmptyResponseSnafu {
            context: "PUT/POST /CreditNotes",
        })
}

/// Prints a mapped invoice/credit-note payload as pretty JSON.
fn print_dry_run(id: i64, mapped: &Mapped) {
    let json = match mapped {
        Mapped::Invoice(inv) => serde_json::to_string_pretty(inv.as_ref()),
        Mapped::CreditNote(cn) => serde_json::to_string_pretty(cn.as_ref()),
    };
    match json {
        Ok(text) => println!("--- document {id} ->\n{text}"),
        Err(e) => println!("--- document {id} -> <unserialisable: {e}>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_range_open_ended_has_both_bounds() {
        // easybill needs both bounds; an absent --to becomes the far-future date.
        assert_eq!(date_range("2026-01-01", None), "2026-01-01,2999-12-31");
    }

    #[test]
    fn date_range_closed() {
        assert_eq!(
            date_range("2026-01-01", Some("2026-03-31")),
            "2026-01-01,2026-03-31"
        );
    }

    #[test]
    fn disposition_by_status() {
        assert_eq!(disposition(Some("DRAFT")), Disposition::Editable);
        assert_eq!(disposition(Some("SUBMITTED")), Disposition::Editable);
        assert_eq!(disposition(None), Disposition::Editable);
        assert_eq!(disposition(Some("AUTHORISED")), Disposition::Finalised);
        assert_eq!(disposition(Some("PAID")), Disposition::Finalised);
        assert_eq!(disposition(Some("DELETED")), Disposition::Removed);
        assert_eq!(disposition(Some("VOIDED")), Disposition::Removed);
    }

    #[test]
    fn known_id_reads_state() {
        let mut state = State::default();
        state.documents.insert(
            "7".to_owned(),
            DocumentMapping {
                xero_id: "guid-7".to_owned(),
                kind: DocumentKind::Invoice,
                pdf_uploaded: false,
                synced_at: Utc::now(),
            },
        );
        assert_eq!(known_id(&state, 7), Some("guid-7"));
        assert_eq!(known_id(&state, 8), None);
    }
}
