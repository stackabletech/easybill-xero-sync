//! Typed subsets of the Xero JSON responses we consume, plus the request bodies
//! we send.
//!
//! Only the fields actually used are declared; serde ignores unknown fields on
//! responses and skips `None` fields on requests.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Response from `POST /connect/token` for the auth-code exchange and refresh.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    /// Short-lived bearer token (valid ~30 min).
    pub access_token: String,

    /// The new single-use refresh token; the old one is now invalid.
    pub refresh_token: String,
}

/// Response from `POST /connect/token` for the `client_credentials` grant
/// (custom connections): no refresh token is issued.
#[derive(Debug, Deserialize)]
pub struct ClientCredentialsToken {
    /// Short-lived bearer token (valid ~30 min).
    pub access_token: String,
}

/// A single entry from `GET /connections`: one connected Xero organisation.
#[derive(Debug, Deserialize)]
pub struct Connection {
    /// The tenant (organisation) id required by the `xero-tenant-id` header.
    #[serde(rename = "tenantId")]
    pub tenant_id: String,

    /// Human-readable organisation name.
    #[serde(rename = "tenantName")]
    pub tenant_name: String,
}

/// Wrapper for `GET /Organisation` (`Accept: application/json`).
#[derive(Debug, Deserialize)]
pub struct OrganisationsResponse {
    /// The organisations array (normally exactly one for a given tenant).
    #[serde(rename = "Organisations", default)]
    pub organisations: Vec<Organisation>,
}

/// A single organisation record.
#[derive(Debug, Deserialize)]
pub struct Organisation {
    /// The organisation's display name.
    #[serde(rename = "Name")]
    pub name: String,
}

/// Wrapper for `GET /TaxRates`.
#[derive(Debug, Deserialize)]
pub struct TaxRatesResponse {
    /// The configured tax rates for the org.
    #[serde(rename = "TaxRates", default)]
    pub tax_rates: Vec<TaxRate>,
}

/// A single Xero tax rate.
#[derive(Debug, Clone, Deserialize)]
pub struct TaxRate {
    /// The stable `TaxType` code referenced on line items (e.g. `OUTPUT2`).
    #[serde(rename = "TaxType")]
    pub tax_type: String,

    /// The display name shown in the Xero UI (e.g. `20% (VAT on Income)`).
    #[serde(rename = "Name", default)]
    pub name: String,

    /// The effective rate in percent, summed over all components.
    #[serde(rename = "EffectiveRate", default)]
    pub effective_rate: Option<Decimal>,

    /// The status; `ACTIVE` for rates usable on new transactions.
    #[serde(rename = "Status", default)]
    pub status: String,

    /// Whether the rate can be used on revenue (sales) lines; `None` if Xero
    /// didn't say.
    #[serde(rename = "CanApplyToRevenue", default)]
    pub can_apply_to_revenue: Option<bool>,
}

impl TaxRate {
    /// Returns `true` if this tax rate is `ACTIVE` (case-insensitive).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status.eq_ignore_ascii_case("ACTIVE")
    }
}

/// Wrapper for `GET /Accounts`.
#[derive(Debug, Deserialize)]
pub struct AccountsResponse {
    /// The chart of accounts for the org.
    #[serde(rename = "Accounts", default)]
    pub accounts: Vec<Account>,
}

/// A single account from the Xero chart of accounts.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    /// The account code referenced on line items (e.g. `200`).
    #[serde(rename = "Code", default)]
    pub code: String,

    /// The status; `ACTIVE` for accounts usable on new transactions.
    #[serde(rename = "Status", default)]
    pub status: String,
}

impl Account {
    /// Returns `true` if this account is `ACTIVE` (case-insensitive).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status.eq_ignore_ascii_case("ACTIVE")
    }
}

// ---------------------------------------------------------------------------
// Contact request/response models (customer sync).
// ---------------------------------------------------------------------------

/// A Xero Contact as sent to `POST /Contacts` (upsert).
///
/// Fields set to `None` are skipped so Xero preserves any existing value; the
/// upsert matches on `ContactID` when known, else `ContactNumber`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Contact {
    /// The Xero GUID; set only when updating a known contact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<String>,

    /// The immutable sync key (easybill customer id), shown as "Contact Code".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_number: Option<String>,

    /// The human-visible customer number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_number: Option<String>,

    /// The display name; must be unique in the org.
    pub name: String,

    /// Given name (individuals).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,

    /// Family name (individuals).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,

    /// Primary email address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_address: Option<String>,

    /// The commercial register / company number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub company_number: Option<String>,

    /// The VAT / tax number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tax_number: Option<String>,

    /// Accounts-receivable tax type (via the config tax table).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts_receivable_tax_type: Option<String>,

    /// `ACTIVE` (default) or `ARCHIVED`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_status: Option<String>,

    /// Additional email recipients (from `emails[1..]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contact_persons: Vec<ContactPerson>,

    /// Postal addresses (STREET and POBOX).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<Address>,

    /// Phone numbers by type.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub phones: Vec<Phone>,

    /// Payment terms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_terms: Option<PaymentTerms>,
}

/// An additional contact person (used to carry extra email addresses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContactPerson {
    /// The person's email address.
    pub email_address: String,

    /// Whether Xero includes this person on emails.
    pub include_in_emails: bool,
}

/// A postal address.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Address {
    /// `STREET` or `POBOX`.
    pub address_type: String,
    /// First address line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_line1: Option<String>,
    /// Second address line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_line2: Option<String>,
    /// Third address line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_line3: Option<String>,
    /// City / town.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    /// Region / state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Postal code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
    /// Country (English name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

/// A phone number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Phone {
    /// `DEFAULT`, `OFFICE`, `MOBILE` or `FAX`.
    pub phone_type: String,
    /// The phone number (kept whole in `PhoneNumber`).
    pub phone_number: String,
}

/// Sales/bills payment terms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PaymentTerms {
    /// The sales payment term.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sales: Option<PaymentTerm>,
}

/// A single payment term (day count + basis).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PaymentTerm {
    /// Number of days.
    pub day: i64,
    /// The basis, e.g. `DAYSAFTERBILLDATE`.
    #[serde(rename = "Type")]
    pub term_type: String,
}

/// Wrapper used for both the `POST /Contacts` request and its response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContactsRequest {
    /// The contacts being upserted.
    pub contacts: Vec<Contact>,
}

/// The `POST /Contacts` (and `GET /Contacts`) response envelope.
#[derive(Debug, Deserialize)]
pub struct ContactsResponse {
    /// The affected contacts, echoed back with their `ContactID`s.
    #[serde(rename = "Contacts", default)]
    pub contacts: Vec<ContactResult>,
}

/// A contact as returned by Xero (subset).
#[derive(Debug, Clone, Deserialize)]
pub struct ContactResult {
    /// The Xero GUID assigned to (or matched for) the contact.
    #[serde(rename = "ContactID")]
    pub contact_id: String,

    /// The immutable sync key we sent as `ContactNumber`.
    #[serde(rename = "ContactNumber", default)]
    pub contact_number: Option<String>,

    /// The display name.
    #[serde(rename = "Name", default)]
    pub name: String,

    /// The human-visible customer number; used once to link contacts that
    /// already existed in Xero before the first sync.
    #[serde(rename = "AccountNumber", default)]
    pub account_number: Option<String>,
}

// ---------------------------------------------------------------------------
// Invoice / CreditNote request/response models (document sync).
// ---------------------------------------------------------------------------

/// A Xero invoice or credit-note line item.
///
/// `UnitAmount` and `DiscountAmount` are [`Decimal`] to preserve exact cents;
/// amounts are never handled as `f64`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct LineItem {
    /// Line description.
    pub description: String,

    /// Quantity.
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,

    /// Net (Exclusive) or gross (Inclusive) unit price.
    #[serde(with = "rust_decimal::serde::str")]
    pub unit_amount: Decimal,

    /// Absolute line discount, when present. Xero supports it on `ACCREC`
    /// invoices only, not on credit notes.
    #[serde(skip_serializing_if = "Option::is_none", with = "decimal_opt")]
    pub discount_amount: Option<Decimal>,

    /// Revenue account code (via the config account table).
    pub account_code: String,

    /// Tax type (via the config tax table).
    pub tax_type: String,
}

/// A Xero invoice (`ACCREC`) as sent to `PUT`/`POST /Invoices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Invoice {
    /// The Xero GUID; set only when updating a known invoice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invoice_id: Option<String>,

    /// Always `ACCREC` for a sales invoice.
    #[serde(rename = "Type")]
    pub invoice_type: String,

    /// The invoice number.
    pub invoice_number: String,

    /// Optional purchase-order / buyer reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,

    /// The contact, matched by `ContactNumber`.
    pub contact: ContactRef,

    /// Issue date (`YYYY-MM-DD`).
    pub date: String,

    /// Due date (`YYYY-MM-DD`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,

    /// ISO currency code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency_code: Option<String>,

    /// `Exclusive` or `Inclusive`.
    pub line_amount_types: String,

    /// Deep-link back into easybill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// The line items.
    pub line_items: Vec<LineItem>,

    /// Document status; `DRAFT` (safe default) unless changed.
    pub status: String,
}

/// A Xero credit note (`ACCRECCREDIT`) as sent to `PUT`/`POST /CreditNotes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreditNote {
    /// The Xero GUID; set only when updating a known credit note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit_note_id: Option<String>,

    /// Always `ACCRECCREDIT` for a sales credit note.
    #[serde(rename = "Type")]
    pub credit_note_type: String,

    /// The credit-note number.
    pub credit_note_number: String,

    /// Optional reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,

    /// The contact, matched by `ContactNumber`.
    pub contact: ContactRef,

    /// Issue date (`YYYY-MM-DD`).
    pub date: String,

    /// ISO currency code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency_code: Option<String>,

    /// `Exclusive` or `Inclusive`.
    pub line_amount_types: String,

    /// The line items.
    pub line_items: Vec<LineItem>,

    /// Document status; `DRAFT` (safe default) unless changed.
    pub status: String,
}

/// A minimal contact reference used inside invoice/credit-note payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContactRef {
    /// The immutable sync key (easybill customer id).
    pub contact_number: String,
}

/// Request/response envelope for `PUT`/`POST /Invoices`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct InvoicesRequest {
    /// The invoices being created or updated.
    pub invoices: Vec<Invoice>,
}

/// Request/response envelope for `PUT`/`POST /CreditNotes`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreditNotesRequest {
    /// The credit notes being created or updated.
    pub credit_notes: Vec<CreditNote>,
}

/// The `/Invoices` response envelope.
#[derive(Debug, Deserialize)]
pub struct InvoicesResponse {
    /// The affected invoices.
    #[serde(rename = "Invoices", default)]
    pub invoices: Vec<InvoiceResult>,
}

/// An invoice as returned by Xero (subset needed for id + total check).
#[derive(Debug, Clone, Deserialize)]
pub struct InvoiceResult {
    /// The Xero GUID.
    #[serde(rename = "InvoiceID")]
    pub invoice_id: String,

    /// `ACCREC` (sales invoice) or `ACCPAY` (supplier bill).
    #[serde(rename = "Type", default)]
    pub invoice_type: Option<String>,

    /// The invoice number.
    #[serde(rename = "InvoiceNumber", default)]
    pub invoice_number: String,

    /// The computed total (tax-inclusive).
    #[serde(rename = "Total", default)]
    pub total: Option<Decimal>,

    /// The status (`DRAFT`, `SUBMITTED`, `AUTHORISED`, `PAID`, `VOIDED`, …).
    #[serde(rename = "Status", default)]
    pub status: Option<String>,
}

/// The `/CreditNotes` response envelope.
#[derive(Debug, Deserialize)]
pub struct CreditNotesResponse {
    /// The affected credit notes.
    #[serde(rename = "CreditNotes", default)]
    pub credit_notes: Vec<CreditNoteResult>,
}

/// A credit note as returned by Xero (subset needed for id + total check).
#[derive(Debug, Clone, Deserialize)]
pub struct CreditNoteResult {
    /// The Xero GUID.
    #[serde(rename = "CreditNoteID")]
    pub credit_note_id: String,

    /// `ACCRECCREDIT` (sales) or `ACCPAYCREDIT` (supplier).
    #[serde(rename = "Type", default)]
    pub credit_note_type: Option<String>,

    /// The credit-note number.
    #[serde(rename = "CreditNoteNumber", default)]
    pub credit_note_number: String,

    /// The computed total.
    #[serde(rename = "Total", default)]
    pub total: Option<Decimal>,

    /// The status (`DRAFT`, `SUBMITTED`, `AUTHORISED`, `PAID`, `VOIDED`, …).
    #[serde(rename = "Status", default)]
    pub status: Option<String>,
}

/// serde helper for `Option<Decimal>` serialised as a string.
mod decimal_opt {
    use rust_decimal::Decimal;
    use serde::Serializer;

    /// Serialises an optional decimal as a string (the `Some` case only; the
    /// field uses `skip_serializing_if` for `None`).
    pub fn serialize<S>(value: &Option<Decimal>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(d) => serializer.serialize_str(&d.to_string()),
            None => serializer.serialize_none(),
        }
    }
}
