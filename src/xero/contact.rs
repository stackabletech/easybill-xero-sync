//! Pure mapping from an easybill [`Customer`] to a Xero [`Contact`].
//!
//! This module performs no I/O so it can be exhaustively unit-tested against
//! fixture customers. The AR tax type is resolved via the config tax table; the
//! ISO country code is translated to an English country name (Xero expects the
//! name, not the code).

use crate::config::Config;
use crate::xero::models::{Address, Contact, ContactPerson, PaymentTerm, PaymentTerms, Phone};
use easybill::models::Customer;

/// The Xero payment-term basis matching easybill's `due_in_days`.
const DAYS_AFTER_BILL_DATE: &str = "DAYSAFTERBILLDATE";

/// Xero accepts at most this many `ContactPersons` per contact.
const MAX_CONTACT_PERSONS: usize = 5;

/// Maps an easybill customer to a Xero contact upsert payload.
///
/// `known_contact_id` is the GUID of the Xero contact this customer was linked
/// to at runtime (see `sync::customers`); supplying it makes the upsert match
/// by `ContactID`.
///
/// The AR tax type is looked up from the config tax table using the customer's
/// `tax_options` as the option and `0` as the percent (AR tax type is a customer
/// default, not a line rate); a missing mapping simply omits the field rather
/// than failing the whole sync.
#[must_use]
pub fn map_customer(
    customer: &Customer,
    config: &Config,
    known_contact_id: Option<&str>,
) -> Contact {
    let name = contact_name(customer);

    let mut contact = Contact {
        contact_id: known_contact_id.map(ToOwned::to_owned),
        contact_number: Some(customer.id.to_string()),
        account_number: customer.number.clone(),
        name,
        first_name: non_empty(&customer.first_name),
        last_name: non_empty(&customer.last_name),
        email_address: primary_email(customer),
        company_number: non_empty(&customer.court_registry_number),
        tax_number: non_empty(&customer.vat_identifier),
        accounts_receivable_tax_type: ar_tax_type(customer, config),
        contact_status: contact_status(customer),
        contact_persons: extra_email_persons(customer),
        addresses: addresses(customer),
        phones: phones(customer),
        payment_terms: payment_terms(customer),
    };

    // Guard: never emit an empty Name (Xero rejects it). Fall back to the id.
    if contact.name.trim().is_empty() {
        contact.name = format!("Customer {}", customer.id);
    }
    contact
}

/// Builds the display name: `company_name` for companies, `first last` for
/// individuals. Collision suffixes are added by the caller.
fn contact_name(customer: &Customer) -> String {
    let base = if customer.personal {
        let full = format!(
            "{} {}",
            customer.first_name.as_deref().unwrap_or("").trim(),
            customer.last_name.as_deref().unwrap_or("").trim(),
        );
        full.trim().to_owned()
    } else {
        customer.company_name.clone().unwrap_or_default()
    };
    base.trim().to_owned()
}

/// Appends the ` (<number>)` collision suffix to a base name.
///
/// Xero requires `Name` to be unique in the org. When two easybill customers
/// share a display name, the caller disambiguates by appending the customer
/// number; this helper builds that suffixed form.
#[must_use]
pub fn name_with_number_suffix(base: &str, number: Option<&str>) -> String {
    match number {
        Some(n) if !n.is_empty() => format!("{base} ({n})"),
        _ => base.to_owned(),
    }
}

/// The primary email (first ASCII-clean address), if any.
///
/// Xero rejects non-ASCII in `EmailAddress`, so a non-ASCII first address is
/// skipped rather than sent.
fn primary_email(customer: &Customer) -> Option<String> {
    customer
        .emails
        .iter()
        .find(|e| !e.trim().is_empty() && e.is_ascii())
        .map(|e| e.trim().to_owned())
}

/// Extra emails (all but the chosen primary) as `ContactPersons`, capped at
/// Xero's limit; further addresses are dropped.
fn extra_email_persons(customer: &Customer) -> Vec<ContactPerson> {
    let primary = primary_email(customer);
    customer
        .emails
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty() && e.is_ascii())
        .filter(|e| Some(*e) != primary.as_deref())
        .map(|e| ContactPerson {
            email_address: e.to_owned(),
            include_in_emails: true,
        })
        .take(MAX_CONTACT_PERSONS)
        .collect()
}

/// The STREET address, plus a POBOX address (falling back to a copy of STREET
/// when no PO box fields are present, since Xero prints POBOX on invoices).
fn addresses(customer: &Customer) -> Vec<Address> {
    let mut out = Vec::new();

    let street = Address {
        address_type: "STREET".to_owned(),
        address_line1: non_empty(&customer.street),
        address_line2: non_empty(&customer.suffix_1),
        address_line3: non_empty(&customer.suffix_2),
        city: non_empty(&customer.city),
        region: non_empty(&customer.state),
        postal_code: non_empty(&customer.zip_code),
        country: country_name(customer.country.as_deref()),
    };
    let has_street = !is_empty_address(&street);
    if has_street {
        out.push(street.clone());
    }

    let has_pobox = non_empty(&customer.postbox).is_some()
        || non_empty(&customer.postbox_zip_code).is_some()
        || non_empty(&customer.postbox_city).is_some();
    if has_pobox {
        out.push(Address {
            address_type: "POBOX".to_owned(),
            address_line1: non_empty(&customer.postbox),
            address_line2: None,
            address_line3: None,
            city: non_empty(&customer.postbox_city),
            region: non_empty(&customer.postbox_state),
            postal_code: non_empty(&customer.postbox_zip_code),
            country: country_name(customer.postbox_country.as_deref()),
        });
    } else if has_street {
        // Copy STREET into POBOX so Xero has something to print.
        let mut pobox = street;
        pobox.address_type = "POBOX".to_owned();
        out.push(pobox);
    }

    out
}

/// Returns `true` if an address carries no usable fields.
fn is_empty_address(a: &Address) -> bool {
    a.address_line1.is_none()
        && a.address_line2.is_none()
        && a.address_line3.is_none()
        && a.city.is_none()
        && a.region.is_none()
        && a.postal_code.is_none()
        && a.country.is_none()
}

/// The customer's phones, in Xero's typed slots.
fn phones(customer: &Customer) -> Vec<Phone> {
    let mut out = Vec::new();
    for (ty, value) in [
        ("DEFAULT", &customer.phone_1),
        ("OFFICE", &customer.phone_2),
        ("MOBILE", &customer.mobile),
        ("FAX", &customer.fax),
    ] {
        if let Some(number) = non_empty(value) {
            out.push(Phone {
                phone_type: ty.to_owned(),
                phone_number: number,
            });
        }
    }
    out
}

/// Payment terms from `due_in_days`, when set.
fn payment_terms(customer: &Customer) -> Option<PaymentTerms> {
    customer.due_in_days.map(|day| PaymentTerms {
        sales: Some(PaymentTerm {
            day,
            term_type: DAYS_AFTER_BILL_DATE.to_owned(),
        }),
    })
}

/// The AR tax type from the config tax table: the `tax_options` key, or
/// `null/0` for a customer without one.
fn ar_tax_type(customer: &Customer, config: &Config) -> Option<String> {
    let option = customer.tax_options.as_deref();
    config.tax_type(option, 0.0).ok().map(ToOwned::to_owned)
}

/// `ARCHIVED` when the customer is archived, else `None` (defaults to ACTIVE).
fn contact_status(customer: &Customer) -> Option<String> {
    if customer.archived {
        Some("ARCHIVED".to_owned())
    } else {
        None
    }
}

/// Normalises an optional string to `None` when empty/whitespace.
fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Translates an ISO 3166-1 alpha-2 country code to an English country name.
///
/// Only the codes seen in the customer base are mapped; an unknown code is
/// passed through unchanged so Xero at least receives *something* (and the
/// operator can extend this table). Returns `None` for an absent code.
fn country_name(code: Option<&str>) -> Option<String> {
    let code = code.map(str::trim).filter(|c| !c.is_empty())?;
    let name = match code.to_ascii_uppercase().as_str() {
        "DE" => "Germany",
        "AT" => "Austria",
        "CH" => "Switzerland",
        "GB" | "UK" => "United Kingdom",
        "US" => "United States",
        "FR" => "France",
        "NL" => "Netherlands",
        "BE" => "Belgium",
        "IT" => "Italy",
        "ES" => "Spain",
        "PL" => "Poland",
        "DK" => "Denmark",
        "SE" => "Sweden",
        "NO" => "Norway",
        "FI" => "Finland",
        "IE" => "Ireland",
        "LU" => "Luxembourg",
        "CZ" => "Czech Republic",
        _ => code, // pass through unknown codes unchanged
    };
    Some(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
[tax]
"null/0" = "OUTPUT2"
"revc" = "ECZROUTPUTSERVICES"
"#,
        )
        .expect("config parses")
    }

    fn base_customer() -> Customer {
        serde_json::from_value(serde_json::json!({
            "id": 1000001i64,
            "number": "10001",
            "personal": false,
            "company_name": "Acme GmbH",
            "emails": [],
        }))
        .expect("customer fixture")
    }

    #[test]
    fn company_name_and_sync_keys() {
        let c = base_customer();
        let contact = map_customer(&c, &config(), None);
        assert_eq!(contact.name, "Acme GmbH");
        assert_eq!(contact.contact_number.as_deref(), Some("1000001"));
        assert_eq!(contact.account_number.as_deref(), Some("10001"));
        assert!(contact.contact_id.is_none());
    }

    #[test]
    fn personal_uses_first_last() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": true,
            "first_name": "Ada", "last_name": "Lovelace", "emails": [],
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        assert_eq!(contact.name, "Ada Lovelace");
        assert_eq!(contact.first_name.as_deref(), Some("Ada"));
        assert_eq!(contact.last_name.as_deref(), Some("Lovelace"));
    }

    #[test]
    fn known_contact_id_makes_upsert_by_id() {
        let contact = map_customer(&base_customer(), &config(), Some("guid-123"));
        assert_eq!(contact.contact_id.as_deref(), Some("guid-123"));
    }

    #[test]
    fn empty_name_falls_back_to_id() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 99, "personal": false, "emails": [],
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        assert_eq!(contact.name, "Customer 99");
    }

    #[test]
    fn name_suffix_disambiguates() {
        assert_eq!(
            name_with_number_suffix("Acme GmbH", Some("10001")),
            "Acme GmbH (10001)"
        );
        assert_eq!(name_with_number_suffix("Acme GmbH", None), "Acme GmbH");
        assert_eq!(name_with_number_suffix("Acme GmbH", Some("")), "Acme GmbH");
    }

    #[test]
    fn emails_split_primary_and_persons() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X",
            "emails": ["a@example.com", "b@example.com", "c@example.com"],
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        assert_eq!(contact.email_address.as_deref(), Some("a@example.com"));
        assert_eq!(contact.contact_persons.len(), 2);
        assert_eq!(contact.contact_persons[0].email_address, "b@example.com");
    }

    #[test]
    fn non_ascii_email_is_skipped_as_primary() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X",
            "emails": ["nöl@example.com", "ok@example.com"],
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        // Non-ASCII address is dropped entirely; first ASCII one is primary.
        assert_eq!(contact.email_address.as_deref(), Some("ok@example.com"));
        assert!(contact.contact_persons.is_empty());
    }

    #[test]
    fn contact_persons_are_capped() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X",
            "emails": ["a@example.com", "b@example.com", "c@example.com", "d@example.com", "e@example.com", "f@example.com", "g@example.com"],
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        assert_eq!(contact.contact_persons.len(), MAX_CONTACT_PERSONS);
    }

    #[test]
    fn street_address_and_pobox_copy() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X", "emails": [],
            "street": "Main St 1", "zip_code": "12345", "city": "Berlin",
            "state": "BE", "country": "DE",
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        assert_eq!(contact.addresses.len(), 2);
        let street = &contact.addresses[0];
        assert_eq!(street.address_type, "STREET");
        assert_eq!(street.country.as_deref(), Some("Germany"));
        assert_eq!(street.city.as_deref(), Some("Berlin"));
        // POBOX copied from STREET.
        let pobox = &contact.addresses[1];
        assert_eq!(pobox.address_type, "POBOX");
        assert_eq!(pobox.address_line1.as_deref(), Some("Main St 1"));
    }

    #[test]
    fn empty_pobox_fields_fall_back_to_street() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X", "emails": [],
            "street": "Main St 1", "city": "Berlin", "country": "DE",
            "postbox": "", "postbox_city": " ", "postbox_zip_code": "",
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        let pobox = &contact.addresses[1];
        assert_eq!(pobox.address_type, "POBOX");
        assert_eq!(pobox.address_line1.as_deref(), Some("Main St 1"));
    }

    #[test]
    fn explicit_pobox_is_used() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X", "emails": [],
            "street": "Main St 1", "city": "Berlin", "country": "DE",
            "postbox": "PO 42", "postbox_city": "Berlin", "postbox_zip_code": "10000",
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        let pobox = contact
            .addresses
            .iter()
            .find(|a| a.address_type == "POBOX")
            .expect("pobox present");
        assert_eq!(pobox.address_line1.as_deref(), Some("PO 42"));
        assert_eq!(pobox.postal_code.as_deref(), Some("10000"));
    }

    #[test]
    fn phones_map_to_typed_slots() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X", "emails": [],
            "phone_1": "111", "phone_2": "222", "mobile": "333", "fax": "444",
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        let types: Vec<&str> = contact
            .phones
            .iter()
            .map(|p| p.phone_type.as_str())
            .collect();
        assert_eq!(types, vec!["DEFAULT", "OFFICE", "MOBILE", "FAX"]);
    }

    #[test]
    fn tax_company_and_payment_terms() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X", "emails": [],
            "vat_identifier": "DE123456789", "court_registry_number": "HRB 42",
            "due_in_days": 14, "tax_options": "revc",
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        assert_eq!(contact.tax_number.as_deref(), Some("DE123456789"));
        assert_eq!(contact.company_number.as_deref(), Some("HRB 42"));
        assert_eq!(
            contact
                .payment_terms
                .expect("terms")
                .sales
                .expect("sales")
                .day,
            14
        );
        assert_eq!(
            contact.accounts_receivable_tax_type.as_deref(),
            Some("ECZROUTPUTSERVICES")
        );
    }

    #[test]
    fn missing_tax_mapping_omits_field_not_errors() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X", "emails": [],
            "tax_options": "unknown_option",
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        assert!(contact.accounts_receivable_tax_type.is_none());
    }

    #[test]
    fn archived_maps_to_status() {
        let c: Customer = serde_json::from_value(serde_json::json!({
            "id": 5, "personal": false, "company_name": "X", "emails": [],
            "archived": true,
        }))
        .expect("fixture");
        let contact = map_customer(&c, &config(), None);
        assert_eq!(contact.contact_status.as_deref(), Some("ARCHIVED"));
    }
}
