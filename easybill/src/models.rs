//! Typed subsets of the easybill JSON responses we consume.
//!
//! Only the fields the sync actually uses are declared; serde ignores the rest.
//! All money fields are integer **cents** (`i64`); `vat_percent` is a float.
//! Field names match the easybill REST v1 API verbatim (snake_case).

use serde::Deserialize;

/// The paginated envelope easybill wraps list responses in.
#[derive(Debug, Clone, Deserialize)]
pub struct Page<T> {
    /// The current page number (1-based).
    pub page: u32,

    /// The total number of pages available.
    pub pages: u32,

    /// The page size used for this response.
    pub limit: u32,

    /// The total number of items across all pages.
    pub total: u32,

    /// The items on this page.
    pub items: Vec<T>,
}

/// A subset of an easybill customer record.
#[derive(Debug, Clone, Deserialize)]
pub struct Customer {
    /// Immutable numeric id; the stable sync key (→ Xero `ContactNumber`).
    pub id: i64,

    /// Human-visible customer number (→ Xero `AccountNumber`).
    #[serde(default)]
    pub number: Option<String>,

    /// `true` for a private individual (use first/last name as the display name);
    /// `false` for a company (use `company_name`).
    #[serde(default)]
    pub personal: bool,

    /// Company name (used as `Name` when `personal` is false).
    #[serde(default)]
    pub company_name: Option<String>,

    /// Given name.
    #[serde(default)]
    pub first_name: Option<String>,

    /// Family name.
    #[serde(default)]
    pub last_name: Option<String>,

    /// Email addresses (plain strings); `emails[0]` maps to `EmailAddress`.
    #[serde(default)]
    pub emails: Vec<String>,

    // --- Street address ---
    /// Street line.
    #[serde(default)]
    pub street: Option<String>,
    /// Address suffix line 1.
    #[serde(default)]
    pub suffix_1: Option<String>,
    /// Address suffix line 2.
    #[serde(default)]
    pub suffix_2: Option<String>,
    /// Postal code.
    #[serde(default)]
    pub zip_code: Option<String>,
    /// City.
    #[serde(default)]
    pub city: Option<String>,
    /// Region / federal state.
    #[serde(default)]
    pub state: Option<String>,
    /// ISO country code (mapped to an English country name for Xero).
    #[serde(default)]
    pub country: Option<String>,

    // --- PO box address ---
    /// PO box number/line.
    #[serde(default)]
    pub postbox: Option<String>,
    /// PO box postal code.
    #[serde(default)]
    pub postbox_zip_code: Option<String>,
    /// PO box city.
    #[serde(default)]
    pub postbox_city: Option<String>,
    /// PO box region.
    #[serde(default)]
    pub postbox_state: Option<String>,
    /// PO box ISO country code.
    #[serde(default)]
    pub postbox_country: Option<String>,

    // --- Phones ---
    /// Primary phone (→ `DEFAULT`).
    #[serde(default)]
    pub phone_1: Option<String>,
    /// Secondary phone (→ `OFFICE`).
    #[serde(default)]
    pub phone_2: Option<String>,
    /// Mobile (→ `MOBILE`).
    #[serde(default)]
    pub mobile: Option<String>,
    /// Fax (→ `FAX`).
    #[serde(default)]
    pub fax: Option<String>,

    /// VAT identifier (→ `TaxNumber`).
    #[serde(default)]
    pub vat_identifier: Option<String>,

    /// Commercial register number (→ `CompanyNumber`).
    #[serde(default)]
    pub court_registry_number: Option<String>,

    /// Default payment term in days (→ `PaymentTerms.Sales`).
    #[serde(default)]
    pub due_in_days: Option<i64>,

    /// easybill tax option code driving the AR tax type (via the tax table).
    #[serde(default)]
    pub tax_options: Option<String>,

    /// `true` if the customer is archived (→ `ContactStatus = ARCHIVED`).
    #[serde(default)]
    pub archived: bool,

    /// Last-modified timestamp (`YYYY-MM-DD HH:MM:SS`, easybill local time);
    /// used for `--since` filtering.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// A subset of an easybill document (invoice or credit note) record.
#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    /// Immutable numeric id (used for the easybill deep-link `Url`).
    pub id: i64,

    /// Document type, e.g. `INVOICE` or `CREDIT`.
    #[serde(rename = "type", default)]
    pub doc_type: Option<String>,

    /// Human-visible document number (→ `InvoiceNumber` / `CreditNoteNumber`).
    #[serde(default)]
    pub number: Option<String>,

    /// Purchase order number (preferred `Reference`).
    #[serde(default)]
    pub order_number: Option<String>,

    /// Buyer reference (fallback `Reference`).
    #[serde(default)]
    pub buyer_reference: Option<String>,

    /// The easybill customer id this document belongs to (→ `ContactNumber`).
    #[serde(default)]
    pub customer_id: Option<i64>,

    /// Whether the document is still a draft (real invoices have `false`).
    #[serde(default)]
    pub is_draft: bool,

    /// If set, the id of the document this one cancels (cancellation handling
    /// is pending an accountant decision; out of scope).
    #[serde(default)]
    pub cancel_id: Option<i64>,

    /// Issue date (`YYYY-MM-DD`) (→ `Date`).
    #[serde(default)]
    pub document_date: Option<String>,

    /// Due date (`YYYY-MM-DD`) (→ `DueDate`).
    #[serde(default)]
    pub due_date: Option<String>,

    /// ISO currency code (→ `CurrencyCode`).
    #[serde(default)]
    pub currency: Option<String>,

    /// VAT calculation basis: `0` = net/Exclusive, `1` = gross/Inclusive.
    #[serde(default, deserialize_with = "null_as_default")]
    pub calc_vat_from: i64,

    /// Document-level tax option code (part of the tax-table key).
    #[serde(default)]
    pub vat_option: Option<String>,

    /// Document total in **cents** (used for the post-create total check).
    #[serde(default, deserialize_with = "null_as_default")]
    pub amount: i64,

    /// The document line items.
    #[serde(default)]
    pub items: Vec<DocumentItem>,
}

/// A subset of an easybill document line item.
#[derive(Debug, Clone, Deserialize)]
pub struct DocumentItem {
    /// Row type; only `POSITION` rows become Xero line items.
    #[serde(rename = "type", default)]
    pub row_type: Option<String>,

    /// Free-text description (prefixed with `number` when present).
    #[serde(default)]
    pub description: Option<String>,

    /// Optional article number, prefixed onto the description.
    #[serde(default)]
    pub number: Option<String>,

    /// Quantity.
    #[serde(default, deserialize_with = "null_as_default")]
    pub quantity: f64,

    /// Net unit price in **cents** (→ `UnitAmount = single_price_net / 100`
    /// for Exclusive documents). easybill types prices as floats, so fractional
    /// cents are possible.
    #[serde(default, deserialize_with = "null_as_default")]
    pub single_price_net: f64,

    /// Gross unit price in **cents** (→ `UnitAmount` for Inclusive documents,
    /// where Xero expects the tax-inclusive amount).
    #[serde(default, deserialize_with = "null_as_default")]
    pub single_price_gross: f64,

    /// Line discount; its unit depends on `discount_type`.
    #[serde(default)]
    pub discount: Option<f64>,

    /// How `discount` applies: `PERCENT`, `AMOUNT`, `QUANTITY` or `FIX`.
    #[serde(default)]
    pub discount_type: Option<String>,

    /// Net line total in **cents** as computed by easybill (after discount).
    #[serde(default)]
    pub total_price_net: Option<f64>,

    /// Gross line total in **cents** as computed by easybill (after discount).
    #[serde(default)]
    pub total_price_gross: Option<f64>,

    /// easybill booking account (→ Xero `AccountCode` via the account table).
    #[serde(default)]
    pub booking_account: Option<String>,

    /// VAT percentage (part of the tax-table key).
    #[serde(default, deserialize_with = "null_as_default")]
    pub vat_percent: f64,
}

/// Deserializes a value easybill may send as an explicit `null` on non-position
/// rows (e.g. text/section lines carry `null` for `quantity`, `single_price_net`,
/// `vat_percent`). Maps both `null` and a missing key to `T::default()`.
///
/// Without this, `#[serde(default)]` alone handles a *missing* key but not an
/// explicit `null`, and a single text row fails the whole page parse.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

impl DocumentItem {
    /// Returns `true` if this row is a real position (not a text/section row).
    #[must_use]
    pub fn is_position(&self) -> bool {
        self.row_type.as_deref() == Some("POSITION")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_row_with_null_numeric_fields_parses() {
        // easybill sends explicit `null` for numeric fields on text/section rows;
        // this must not fail the parse (regression: it aborted the whole page).
        let item: DocumentItem = serde_json::from_value(serde_json::json!({
            "type": "TEXT",
            "description": "A section heading",
            "quantity": null,
            "single_price_net": null,
            "vat_percent": null,
            "discount": null,
        }))
        .expect("text row with null numerics parses");
        assert!(!item.is_position());
        assert_eq!(item.quantity, 0.0);
        assert_eq!(item.single_price_net, 0.0);
        assert_eq!(item.vat_percent, 0.0);
    }

    #[test]
    fn position_row_keeps_numeric_values() {
        let item: DocumentItem = serde_json::from_value(serde_json::json!({
            "type": "POSITION",
            "quantity": 2.0,
            "single_price_net": 1500,
            "vat_percent": 19.0,
        }))
        .expect("position row parses");
        assert!(item.is_position());
        assert_eq!(item.single_price_net, 1500.0);
        assert_eq!(item.quantity, 2.0);
    }
}
