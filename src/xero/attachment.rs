//! PDF attachment helpers for Xero invoices and credit notes.
//!
//! Xero attaches a file at
//! `PUT /Invoices/{InvoiceID}/Attachments/{FileName}` (and the same under
//! `/CreditNotes/{CreditNoteID}/…`), with the raw bytes and
//! `Content-Type: application/pdf`. `POST` on the same path replaces an existing
//! attachment, making re-runs idempotent. This module builds the filename and
//! path; the byte upload itself lives on the API client.

/// The resource a document maps to in Xero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// `Invoices`.
    Invoices,
    /// `CreditNotes`.
    CreditNotes,
}

impl Resource {
    /// The URL path segment for this resource.
    #[must_use]
    pub fn path_segment(self) -> &'static str {
        match self {
            Resource::Invoices => "Invoices",
            Resource::CreditNotes => "CreditNotes",
        }
    }
}

/// Builds the attachment filename from a document identifier (the sync passes
/// the easybill document id, which is stable and always URL-safe).
///
/// The identifier is sanitised to a filesystem/URL-safe form (Xero uses the name in
/// the URL path) and given a `.pdf` extension. Characters outside
/// `[A-Za-z0-9._-]` become `_`.
#[must_use]
pub fn attachment_filename(document_number: &str) -> String {
    let sanitised: String = document_number
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = if sanitised.is_empty() {
        "document".to_owned()
    } else {
        sanitised
    };
    format!("{base}.pdf")
}

/// Builds the API path (relative to the Accounting base) for an attachment.
///
/// e.g. `Invoices/{id}/Attachments/{file}`.
#[must_use]
pub fn attachment_path(resource: Resource, xero_id: &str, file_name: &str) -> String {
    format!(
        "{}/{}/Attachments/{}",
        resource.path_segment(),
        xero_id,
        file_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_sanitises_and_adds_extension() {
        assert_eq!(attachment_filename("RE-2026-001"), "RE-2026-001.pdf");
        assert_eq!(attachment_filename("2025/10001"), "2025_10001.pdf");
        assert_eq!(attachment_filename("in voice #7"), "in_voice__7.pdf");
    }

    #[test]
    fn empty_number_gets_placeholder() {
        assert_eq!(attachment_filename(""), "document.pdf");
    }

    #[test]
    fn path_for_invoice_and_credit_note() {
        assert_eq!(
            attachment_path(Resource::Invoices, "guid-1", "RE-1.pdf"),
            "Invoices/guid-1/Attachments/RE-1.pdf"
        );
        assert_eq!(
            attachment_path(Resource::CreditNotes, "guid-2", "GS-2.pdf"),
            "CreditNotes/guid-2/Attachments/GS-2.pdf"
        );
    }
}
