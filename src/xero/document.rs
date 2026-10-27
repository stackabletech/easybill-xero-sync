//! Pure mapping from an easybill [`Document`] to a Xero [`Invoice`] (`ACCREC`)
//! or [`CreditNote`] (`ACCRECCREDIT`).
//!
//! No I/O, so the mapping is unit-tested against fixture documents. Money is
//! handled as [`Decimal`] throughout (easybill cents / 100), never `f64`; the
//! floats easybill sends are converted once, at the edge. Every
//! line item resolves its `AccountCode` and `TaxType` via the config tables; a
//! missing mapping fails the *document* (returned as an error) rather than
//! silently producing a wrong invoice.

use rust_decimal::prelude::FromPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use snafu::{OptionExt, ResultExt, Snafu};

use crate::config::{self, Config};
use crate::xero::models::{ContactRef, CreditNote, Invoice, LineItem};
use easybill::models::{Document, DocumentItem};

/// The easybill document type for invoices.
const TYPE_INVOICE: &str = "INVOICE";
/// The easybill document type for credit notes.
const TYPE_CREDIT: &str = "CREDIT";

/// The Xero invoice status pushed for all documents.
///
// TODO(accountants): push completed invoices as `AUTHORISED` rather than `DRAFT`
// once the accountants confirm. `DRAFT` is the safe default (nothing posts to the
// ledger until a human authorises it). This constant is the single switch point.
const STATUS_DRAFT: &str = "DRAFT";

/// Errors from mapping a document.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    /// The document had no number.
    #[snafu(display("Document {id} has no number"))]
    MissingNumber { id: i64 },

    /// The document had no `customer_id` to resolve a contact.
    #[snafu(display("Document {id} has no customer_id"))]
    MissingCustomer { id: i64 },

    /// The document had no `document_date`.
    #[snafu(display("Document {id} has no document_date"))]
    MissingDate { id: i64 },

    /// The document type is neither `INVOICE` nor `CREDIT`.
    #[snafu(display("Document {id} has unsupported type {doc_type:?}"))]
    UnsupportedType { id: i64, doc_type: Option<String> },

    /// A line item's account or tax mapping was missing from the config.
    #[snafu(display("Document {id}: {source}"))]
    LineMapping {
        id: i64,
        #[snafu(source(from(config::Error, Box::new)))]
        source: Box<config::Error>,
    },

    /// The document had no mappable `POSITION` line items.
    #[snafu(display("Document {id} has no POSITION line items"))]
    NoLineItems { id: i64 },

    /// A numeric field was not a finite number.
    #[snafu(display("Document {id}: line field {field} is not a valid number"))]
    InvalidNumber { id: i64, field: &'static str },

    /// A discounted line had no easybill line total to derive the discount from.
    #[snafu(display("Document {id}: discounted line has no {field}"))]
    MissingLineTotal { id: i64, field: &'static str },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The result of mapping a document: either an invoice or a credit note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapped {
    /// An `ACCREC` invoice.
    Invoice(Box<Invoice>),
    /// An `ACCRECCREDIT` credit note.
    CreditNote(Box<CreditNote>),
}

/// The base for a document deep link back into easybill.
const EASYBILL_DOC_URL: &str = "https://app.easybill.de/documents/edit/";

/// Maps an easybill document to a Xero invoice or credit note.
///
/// `known_xero_id` is the Xero GUID from state when this document was synced
/// before, enabling an update (`POST`) instead of a create (`PUT`).
///
/// # Errors
///
/// Returns [`Error::MissingNumber`], [`Error::MissingCustomer`],
/// [`Error::MissingDate`], [`Error::UnsupportedType`], [`Error::NoLineItems`],
/// or [`Error::LineMapping`] when the document cannot be mapped.
pub fn map_document(
    document: &Document,
    config: &Config,
    known_xero_id: Option<&str>,
) -> Result<Mapped> {
    let id = document.id;
    let number = document
        .number
        .clone()
        .filter(|n| !n.is_empty())
        .context(MissingNumberSnafu { id })?;
    let customer_id = document.customer_id.context(MissingCustomerSnafu { id })?;
    let date = document
        .document_date
        .clone()
        .filter(|d| !d.is_empty())
        .context(MissingDateSnafu { id })?;

    let doc_type = document.doc_type.as_deref();
    let credit_note = doc_type.is_some_and(|t| t.eq_ignore_ascii_case(TYPE_CREDIT));
    let line_items = map_line_items(document, config, credit_note)?;
    if line_items.is_empty() {
        return NoLineItemsSnafu { id }.fail();
    }

    let reference = document
        .order_number
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| document.buyer_reference.clone().filter(|s| !s.is_empty()));
    let currency = document.currency.clone().filter(|c| !c.is_empty());
    let line_amount_types = line_amount_types(document.calc_vat_from).to_owned();
    let contact = ContactRef {
        contact_number: customer_id.to_string(),
    };
    let url = Some(format!("{EASYBILL_DOC_URL}{id}"));

    match doc_type {
        Some(t) if t.eq_ignore_ascii_case(TYPE_INVOICE) => Ok(Mapped::Invoice(Box::new(Invoice {
            invoice_id: known_xero_id.map(ToOwned::to_owned),
            invoice_type: "ACCREC".to_owned(),
            invoice_number: number,
            reference,
            contact,
            date,
            due_date: document.due_date.clone().filter(|d| !d.is_empty()),
            currency_code: currency,
            line_amount_types,
            url,
            line_items,
            status: STATUS_DRAFT.to_owned(),
        }))),
        Some(t) if t.eq_ignore_ascii_case(TYPE_CREDIT) => {
            Ok(Mapped::CreditNote(Box::new(CreditNote {
                credit_note_id: known_xero_id.map(ToOwned::to_owned),
                credit_note_type: "ACCRECCREDIT".to_owned(),
                credit_note_number: number,
                reference,
                contact,
                date,
                currency_code: currency,
                line_amount_types,
                line_items,
                status: STATUS_DRAFT.to_owned(),
            })))
        }
        other => UnsupportedTypeSnafu {
            id,
            doc_type: other.map(ToOwned::to_owned),
        }
        .fail(),
    }
}

/// Maps the `POSITION` line items of a document, resolving account + tax codes.
fn map_line_items(
    document: &Document,
    config: &Config,
    credit_note: bool,
) -> Result<Vec<LineItem>> {
    let id = document.id;
    let vat_option = document.vat_option.as_deref();
    // Inclusive documents price Xero line items on the gross (tax-inclusive)
    // amount; Exclusive documents on the net amount.
    let inclusive = document.calc_vat_from == 1;
    let mut items = Vec::new();
    for raw in &document.items {
        if !raw.is_position() {
            continue;
        }
        items.push(map_line_item(
            id,
            raw,
            vat_option,
            inclusive,
            credit_note,
            config,
        )?);
    }
    Ok(items)
}

/// Maps one `POSITION` row to a Xero line item.
///
/// A discount is taken from easybill's own line total (whatever the
/// `discount_type`), so the Xero line amount matches easybill to the cent:
/// invoices carry it as `DiscountAmount`; credit notes, which Xero can't
/// discount, become one unit at the discounted line total.
fn map_line_item(
    id: i64,
    raw: &DocumentItem,
    vat_option: Option<&str>,
    inclusive: bool,
    credit_note: bool,
    config: &Config,
) -> Result<LineItem> {
    let account_code = config
        .account_code(raw.booking_account.as_deref().unwrap_or_default())
        .context(LineMappingSnafu { id })?
        .to_owned();
    let tax_type = config
        .tax_type(vat_option, raw.vat_percent)
        .context(LineMappingSnafu { id })?
        .to_owned();

    // `UnitAmount` must match `LineAmountTypes`: gross for Inclusive, net for
    // Exclusive. Using the net price on an Inclusive document would understate
    // the line by the VAT fraction.
    let (unit_field, unit_cents, total_field, total_cents) = if inclusive {
        (
            "single_price_gross",
            raw.single_price_gross,
            "total_price_gross",
            raw.total_price_gross,
        )
    } else {
        (
            "single_price_net",
            raw.single_price_net,
            "total_price_net",
            raw.total_price_net,
        )
    };
    let quantity = to_decimal(id, "quantity", raw.quantity)?.normalize();
    let unit_amount = cents_to_amount(id, unit_field, unit_cents)?;
    let description = description(raw);

    let mut line = LineItem {
        description,
        quantity,
        unit_amount,
        discount_amount: None,
        account_code,
        tax_type,
    };
    if raw.discount.is_none_or(|d| d == 0.0) {
        return Ok(line);
    }

    let total_cents = total_cents.context(MissingLineTotalSnafu {
        id,
        field: total_field,
    })?;
    let line_total = cents_to_amount(id, total_field, total_cents)?;
    let undiscounted =
        (quantity * unit_amount).round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    let discount = undiscounted - line_total;
    if discount <= Decimal::ZERO {
        // easybill didn't reduce the line (e.g. a discount without a type).
        return Ok(line);
    }
    if credit_note {
        line.description = format!(
            "{} ({quantity} x {unit_amount}, less {discount} discount)",
            line.description
        );
        line.quantity = Decimal::ONE;
        line.unit_amount = line_total;
    } else {
        line.discount_amount = Some(discount);
    }
    Ok(line)
}

/// Converts an easybill float to a `Decimal`, rejecting NaN and infinity.
///
/// Uses the shortest decimal representation of the float (`0.1` → `0.1`), not
/// its exact binary value.
fn to_decimal(id: i64, field: &'static str, value: f64) -> Result<Decimal> {
    Decimal::from_f64(value).context(InvalidNumberSnafu { id, field })
}

/// Converts an easybill float cents value to a currency amount (cents / 100).
fn cents_to_amount(id: i64, field: &'static str, cents: f64) -> Result<Decimal> {
    Ok((to_decimal(id, field, cents)? / Decimal::ONE_HUNDRED).normalize())
}

/// Builds the line description, prefixing the article `number` when present.
fn description(raw: &DocumentItem) -> String {
    let desc = raw.description.clone().unwrap_or_default();
    match raw.number.as_deref().filter(|n| !n.is_empty()) {
        Some(number) => {
            if desc.is_empty() {
                number.to_owned()
            } else {
                format!("{number} {desc}")
            }
        }
        None => desc,
    }
}

/// Converts integer cents to a `Decimal` amount (cents / 100), exactly.
#[must_use]
pub fn cents_to_decimal(cents: i64) -> Decimal {
    Decimal::new(cents, 2)
}

/// Maps easybill `calc_vat_from` to Xero `LineAmountTypes`.
fn line_amount_types(calc_vat_from: i64) -> &'static str {
    match calc_vat_from {
        1 => "Inclusive",
        _ => "Exclusive",
    }
}

/// Verifies that the Xero-computed `total` equals the easybill `amount` (cents).
///
/// Returns `Ok(())` when they match exactly (both compared as `Decimal`), or an
/// `Err` carrying the two values for logging. Callers mark the document failed on
/// mismatch (rounding differences must not silently pass).
///
/// # Errors
///
/// Returns [`TotalMismatch`] if the totals differ.
pub fn verify_total(
    easybill_amount_cents: i64,
    xero_total: Option<Decimal>,
) -> Result<(), TotalMismatch> {
    let expected = cents_to_decimal(easybill_amount_cents);
    match xero_total {
        Some(actual) if actual == expected => Ok(()),
        other => Err(TotalMismatch {
            expected,
            actual: other,
        }),
    }
}

/// A total-verification mismatch between easybill and Xero.
#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
#[snafu(display("Total mismatch: easybill {expected} vs Xero {actual:?}"))]
pub struct TotalMismatch {
    /// The expected total (easybill `amount` / 100).
    pub expected: Decimal,
    /// The Xero-reported total, if any.
    pub actual: Option<Decimal>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn config() -> Config {
        toml::from_str(
            r#"
[tax]
"null/19" = "OUTPUT2"
"null/0" = "ZERORATEDOUTPUT"

[accounts]
default = "200"
"4401" = "210"
"#,
        )
        .expect("config parses")
    }

    fn invoice_doc() -> Document {
        serde_json::from_value(serde_json::json!({
            "id": 555,
            "type": "INVOICE",
            "number": "RE-2026-001",
            "order_number": "PO-42",
            "customer_id": 1000001i64,
            "is_draft": false,
            "document_date": "2026-09-01",
            "due_date": "2026-09-15",
            "currency": "EUR",
            "calc_vat_from": 0,
            "vat_option": null,
            "amount": 2618000,
            "items": [
                {"type":"POSITION","description":"Support","number":"ART-1",
                 "quantity":5.0,"single_price_net":440000,"single_price_gross":523600,
                 "discount":null,"booking_account":"4401","vat_percent":19.0},
                {"type":"POSITION","description":"License",
                 "quantity":1.0,"single_price_net":1600000,"single_price_gross":1904000,
                 "discount":10.0,"discount_type":"PERCENT",
                 "total_price_net":1440000,"total_price_gross":1713600,
                 "booking_account":null,"vat_percent":19.0},
                {"type":"TEXT","description":"a note","quantity":null,
                 "single_price_net":null,"single_price_gross":null,"vat_percent":null}
            ]
        }))
        .expect("invoice fixture")
    }

    #[test]
    fn cents_convert_exactly() {
        assert_eq!(cents_to_decimal(440000), dec!(4400.00));
        assert_eq!(cents_to_decimal(1), dec!(0.01));
        assert_eq!(cents_to_decimal(-2500), dec!(-25.00));
    }

    #[test]
    fn maps_invoice_with_line_items() {
        let mapped = map_document(&invoice_doc(), &config(), None).expect("maps");
        let inv = match mapped {
            Mapped::Invoice(i) => i,
            Mapped::CreditNote(_) => panic!("expected invoice"),
        };
        assert_eq!(inv.invoice_type, "ACCREC");
        assert_eq!(inv.invoice_number, "RE-2026-001");
        assert_eq!(inv.reference.as_deref(), Some("PO-42"));
        assert_eq!(inv.contact.contact_number, "1000001");
        assert_eq!(inv.line_amount_types, "Exclusive");
        assert_eq!(inv.status, "DRAFT");
        assert_eq!(inv.currency_code.as_deref(), Some("EUR"));
        assert!(inv.url.as_deref().unwrap().ends_with("/555"));
        // TEXT row dropped; 2 POSITION rows kept.
        assert_eq!(inv.line_items.len(), 2);

        let first = &inv.line_items[0];
        assert_eq!(first.description, "ART-1 Support");
        assert_eq!(first.unit_amount, dec!(4400.00));
        assert_eq!(first.quantity, dec!(5));
        assert_eq!(first.account_code, "210"); // 4401 -> 210
        assert_eq!(first.tax_type, "OUTPUT2"); // null/19
        assert!(first.discount_amount.is_none());

        let second = &inv.line_items[1];
        assert_eq!(second.description, "License");
        assert_eq!(second.account_code, "200"); // no booking account -> default
                                                // 10% of 16000.00, taken from easybill's line total.
        assert_eq!(second.discount_amount, Some(dec!(1600)));
    }

    #[test]
    fn maps_credit_note() {
        let mut doc = invoice_doc();
        doc.doc_type = Some("CREDIT".to_owned());
        let mapped = map_document(&doc, &config(), None).expect("maps");
        match mapped {
            Mapped::CreditNote(cn) => {
                assert_eq!(cn.credit_note_type, "ACCRECCREDIT");
                assert_eq!(cn.credit_note_number, "RE-2026-001");
                assert_eq!(cn.status, "DRAFT");
                // Xero can't discount credit-note lines: one unit at the
                // discounted total instead.
                let line = &cn.line_items[1];
                assert_eq!(line.quantity, dec!(1));
                assert_eq!(line.unit_amount, dec!(14400));
                assert!(line.discount_amount.is_none());
                assert_eq!(line.description, "License (1 x 16000, less 1600 discount)");
                // Undiscounted lines are unchanged.
                assert_eq!(cn.line_items[0].quantity, dec!(5));
            }
            Mapped::Invoice(_) => panic!("expected credit note"),
        }
    }

    #[test]
    fn inclusive_uses_gross_unit_amount() {
        let mut doc = invoice_doc();
        doc.calc_vat_from = 1;
        let mapped = map_document(&doc, &config(), None).expect("maps");
        if let Mapped::Invoice(inv) = mapped {
            assert_eq!(inv.line_amount_types, "Inclusive");
            // Inclusive → UnitAmount must be the gross (tax-inclusive) price,
            // not the net price used for Exclusive documents.
            assert_eq!(inv.line_items[0].unit_amount, dec!(5236.00));
            assert_eq!(inv.line_items[1].unit_amount, dec!(19040.00));
            assert_eq!(inv.line_items[1].discount_amount, Some(dec!(1904)));
        } else {
            panic!("expected invoice");
        }
    }

    #[test]
    fn absolute_discount_comes_from_line_total() {
        let mut doc = invoice_doc();
        let item = &mut doc.items[0];
        // 5 x 4400.00 = 22000.00, minus a 250.00 absolute discount.
        item.discount = Some(25000.0);
        item.discount_type = Some("AMOUNT".to_owned());
        item.total_price_net = Some(2_175_000.0);
        let Mapped::Invoice(inv) = map_document(&doc, &config(), None).expect("maps") else {
            panic!("expected invoice");
        };
        assert_eq!(inv.line_items[0].quantity, dec!(5));
        assert_eq!(inv.line_items[0].discount_amount, Some(dec!(250)));
    }

    #[test]
    fn discount_not_applied_by_easybill_is_ignored() {
        let mut doc = invoice_doc();
        let item = &mut doc.items[0];
        item.discount = Some(10.0);
        item.discount_type = None;
        item.total_price_net = Some(2_200_000.0);
        let Mapped::Invoice(inv) = map_document(&doc, &config(), None).expect("maps") else {
            panic!("expected invoice");
        };
        assert!(inv.line_items[0].discount_amount.is_none());
    }

    #[test]
    fn discount_without_line_total_fails_document() {
        let mut doc = invoice_doc();
        doc.items[1].total_price_net = None;
        assert!(matches!(
            map_document(&doc, &config(), None),
            Err(Error::MissingLineTotal { id: 555, .. })
        ));
    }

    #[test]
    fn floats_convert_without_binary_noise() {
        let mut doc = invoice_doc();
        doc.items[0].quantity = 0.1;
        doc.items[0].single_price_net = 150.5;
        let Mapped::Invoice(inv) = map_document(&doc, &config(), None).expect("maps") else {
            panic!("expected invoice");
        };
        assert_eq!(inv.line_items[0].quantity.to_string(), "0.1");
        assert_eq!(inv.line_items[0].unit_amount.to_string(), "1.505");
    }

    #[test]
    fn non_finite_quantity_fails_document() {
        let mut doc = invoice_doc();
        doc.items[0].quantity = f64::NAN;
        assert!(matches!(
            map_document(&doc, &config(), None),
            Err(Error::InvalidNumber {
                id: 555,
                field: "quantity"
            })
        ));
    }

    #[test]
    fn known_id_enables_update() {
        let mapped = map_document(&invoice_doc(), &config(), Some("guid-9")).expect("maps");
        if let Mapped::Invoice(inv) = mapped {
            assert_eq!(inv.invoice_id.as_deref(), Some("guid-9"));
        } else {
            panic!("expected invoice");
        }
    }

    #[test]
    fn missing_tax_mapping_fails_document() {
        let mut doc = invoice_doc();
        // vat_percent 7 has no mapping in the config.
        doc.items[0].vat_percent = 7.0;
        assert!(matches!(
            map_document(&doc, &config(), None),
            Err(Error::LineMapping { id: 555, .. })
        ));
    }

    #[test]
    fn unsupported_type_errors() {
        let mut doc = invoice_doc();
        doc.doc_type = Some("OFFER".to_owned());
        assert!(matches!(
            map_document(&doc, &config(), None),
            Err(Error::UnsupportedType { .. })
        ));
    }

    #[test]
    fn verify_total_matches_and_mismatches() {
        // 2618000 cents == 26180.00
        assert!(verify_total(2618000, Some(dec!(26180.00))).is_ok());
        assert!(verify_total(2618000, Some(dec!(26180.01))).is_err());
        assert!(verify_total(2618000, None).is_err());
    }

    #[test]
    fn no_position_items_errors() {
        let mut doc = invoice_doc();
        doc.items.retain(|i| !i.is_position());
        assert!(matches!(
            map_document(&doc, &config(), None),
            Err(Error::NoLineItems { .. })
        ));
    }
}
