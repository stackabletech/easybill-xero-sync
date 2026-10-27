//! `sync customers`: map easybill customers to Xero contacts and upsert them.
//!
//! Flow: fetch all easybill customers (optionally filtered by `--since`) and all
//! Xero contacts, link each customer to an existing contact where one exists
//! (see [`Link`]), map it to a Xero `Contact`, disambiguate colliding display
//! names with the ` (<number>)` suffix, then upsert one at a time so a single
//! failure doesn't abort the run.
//!
//! Linking happens fresh on every run; nothing about customers is persisted.
//! The durable link is the contact's `ContactNumber` (the easybill id), which
//! the upsert sets and which `sync documents` uses to resolve contacts.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::NaiveDateTime;

use snafu::{OptionExt, ResultExt};

use crate::config::Config;
use crate::sync::{
    ClientSnafu, EasybillSnafu, EmptyResponseSnafu, ItemsFailedSnafu, Result, TokenSnafu,
};
use crate::xero::client::ApiClient;
use crate::xero::contact::{map_customer, name_with_number_suffix};
use crate::xero::models::{Contact, ContactResult, ContactsRequest};
use crate::xero::token::{AuthMode, TokenManager};
use easybill::client::EasybillClient;
use easybill::models::Customer;

/// Options for a `sync customers` run.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Only sync customers whose easybill `updated_at` is `>=` this value
    /// (easybill local time); `None` syncs all.
    pub since: Option<NaiveDateTime>,

    /// If `true`, print the payloads that would be sent and make no API writes.
    pub dry_run: bool,

    /// If `true`, read the existing Xero contacts and print the planned link for
    /// each customer, making no Xero writes.
    pub preview: bool,
}

/// How a customer was resolved to a Xero contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// An existing contact whose `ContactNumber` is this customer's easybill id.
    ContactNumber,

    /// An unlinked contact (no `ContactNumber`) whose `AccountNumber` equals the
    /// easybill customer number. This is the one-time bootstrap for contacts
    /// that existed in Xero before the first sync; the upsert then sets
    /// `ContactNumber`, so later runs link via [`Link::ContactNumber`].
    AccountNumber,

    /// No existing contact; the upsert creates one.
    New,
}

/// The planned action for one customer.
#[derive(Debug)]
enum Planned<'a> {
    /// Upsert this payload.
    Upsert {
        customer: &'a Customer,
        contact: Box<Contact>,
        link: Link,
    },

    /// Not safe to upsert (would duplicate or mislink); counted as a failure.
    Blocked {
        customer: &'a Customer,
        reason: String,
    },
}

/// Existing Xero contacts, indexed for linking and name-collision checks.
#[derive(Debug, Default)]
struct ExistingIndex<'a> {
    /// Contacts by `ContactNumber` (the easybill id for contacts we created or
    /// linked).
    by_contact_number: HashMap<&'a str, Vec<&'a ContactResult>>,

    /// Contacts *without* a `ContactNumber`, by `AccountNumber`.
    unlinked_by_account_number: HashMap<&'a str, Vec<&'a ContactResult>>,

    /// All contacts by normalised display name.
    by_name: HashMap<String, Vec<&'a ContactResult>>,
}

impl<'a> ExistingIndex<'a> {
    fn new(contacts: &'a [ContactResult]) -> Self {
        let mut index = Self::default();
        for contact in contacts {
            match trimmed(contact.contact_number.as_deref()) {
                Some(number) => index
                    .by_contact_number
                    .entry(number)
                    .or_default()
                    .push(contact),
                None => {
                    if let Some(account) = trimmed(contact.account_number.as_deref()) {
                        index
                            .unlinked_by_account_number
                            .entry(account)
                            .or_default()
                            .push(contact);
                    }
                }
            }
            index
                .by_name
                .entry(normalize_name(&contact.name))
                .or_default()
                .push(contact);
        }
        index
    }
}

/// Runs `sync customers`.
///
/// # Errors
///
/// Returns [`Easybill`](super::Error::Easybill), [`Token`](super::Error::Token)
/// or [`Client`](super::Error::Client) on a run-level failure, and [`ItemsFailed`](super::Error::ItemsFailed) if any
/// individual customer could not be upserted (after attempting the rest).
pub async fn run(
    config: &Config,
    auth_mode: AuthMode,
    state_override: Option<&Path>,
    options: &Options,
) -> Result<()> {
    let easybill = EasybillClient::from_env().context(EasybillSnafu)?;
    let customers = easybill.all_customers().await.context(EasybillSnafu)?;
    let selected = customers
        .iter()
        .filter(|c| modified_since(c, options.since))
        .count();
    tracing::info!(count = selected, "customers selected for sync");

    if options.dry_run {
        // No Xero call in dry-run, so no linking to existing contacts and
        // collisions are detected only among easybill customers. `--preview`
        // runs the full check read-only.
        let planned = plan_selected(&customers, options.since, config, &ExistingIndex::default());
        for planned in &planned {
            print_dry_run(planned);
        }
        let blocked = planned
            .iter()
            .filter(|p| matches!(p, Planned::Blocked { .. }))
            .count();
        println!(
            "dry-run: {} contact(s) would be upserted, {blocked} blocked. \
             (linking to and collision check against existing Xero contacts \
             runs only with --preview or on a real sync)",
            planned.len() - blocked
        );
        if blocked > 0 {
            return ItemsFailedSnafu {
                failed: blocked,
                total: planned.len(),
            }
            .fail();
        }
        return Ok(());
    }

    let manager = TokenManager::from_env(auth_mode, state_override, config.xero.tenant_id.clone())
        .context(TokenSnafu)?;
    let client = ApiClient::connect(manager).await.context(ClientSnafu)?;

    // Pull all existing Xero contacts once, both to link customers to contacts
    // that already exist and so the ` (<number>)` suffix also disambiguates
    // against them, not just collisions within this batch. We have few
    // customers, so one paginated fetch is cheaper than a lookup per contact.
    let existing = client.all_contacts().await.context(ClientSnafu)?;
    tracing::info!(count = existing.len(), "existing Xero contacts loaded");
    let existing_index = ExistingIndex::new(&existing);

    let planned = plan_selected(&customers, options.since, config, &existing_index);

    if options.preview {
        print_preview(&planned);
        return Ok(());
    }

    let total = planned.len();
    let mut failed = 0usize;
    for planned in planned {
        let (customer, contact) = match planned {
            Planned::Upsert {
                customer, contact, ..
            } => (customer, contact),
            Planned::Blocked { customer, reason } => {
                failed += 1;
                tracing::error!(customer_id = customer.id, %reason, "contact not upserted");
                continue;
            }
        };
        match upsert_one(&client, &contact).await {
            Ok(contact_id) => {
                tracing::debug!(customer_id = customer.id, %contact_id, "contact upserted");
            }
            Err(e) => {
                failed += 1;
                tracing::error!(
                    customer_id = customer.id,
                    error = %super::error_chain(&e),
                    "failed to upsert contact; continuing"
                );
            }
        }
    }

    println!(
        "sync customers: {} succeeded, {} failed, of {} selected.",
        total - failed,
        failed,
        total
    );
    if failed > 0 {
        return ItemsFailedSnafu { failed, total }.fail();
    }
    Ok(())
}

/// Whether a customer's `updated_at` is `>= since`. A customer whose
/// `updated_at` is missing or unparseable counts as modified: syncing it again
/// is harmless, skipping it silently is not.
fn modified_since(customer: &Customer, since: Option<NaiveDateTime>) -> bool {
    let Some(since) = since else {
        return true;
    };
    customer
        .updated_at
        .as_deref()
        .and_then(parse_easybill_timestamp)
        .is_none_or(|updated| updated >= since)
}

/// Plans all customers, so name collisions are resolved the same way whatever
/// `--since` selects (names don't flip between full and incremental runs),
/// then keeps the customers modified since `since`.
fn plan_selected<'a>(
    customers: &'a [Customer],
    since: Option<NaiveDateTime>,
    config: &Config,
    existing: &ExistingIndex<'_>,
) -> Vec<Planned<'a>> {
    plan_contacts(customers, config, existing)
        .into_iter()
        .filter(|p| modified_since(p.customer(), since))
        .collect()
}

/// Parses an easybill timestamp (`"2025-01-15 14:20:41"`, local time).
fn parse_easybill_timestamp(ts: &str) -> Option<NaiveDateTime> {
    let ts = ts.trim();
    NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S"))
        .ok()
}

/// Resolves the existing Xero contact for a customer: a contact whose
/// `ContactNumber` == easybill id, else (bootstrap) an unlinked contact whose
/// `AccountNumber` == easybill number. Returns the `ContactID` to upsert by
/// (`None` creates a new contact), or a reason when the match is ambiguous.
fn resolve_link(
    customer: &Customer,
    existing: &ExistingIndex<'_>,
) -> std::result::Result<(Option<String>, Link), String> {
    let id = customer.id.to_string();
    if let Some(matches) = existing.by_contact_number.get(id.as_str()) {
        return match matches.as_slice() {
            [one] => Ok((Some(one.contact_id.clone()), Link::ContactNumber)),
            many => Err(format!(
                "{} Xero contacts have ContactNumber {id}",
                many.len()
            )),
        };
    }
    if let Some(number) = trimmed(customer.number.as_deref()) {
        if let Some(matches) = existing.unlinked_by_account_number.get(number) {
            return match matches.as_slice() {
                [one] => Ok((Some(one.contact_id.clone()), Link::AccountNumber)),
                many => Err(format!(
                    "{} unlinked Xero contacts have AccountNumber {number}; \
                     keep it on exactly one",
                    many.len()
                )),
            };
        }
    }
    Ok((None, Link::New))
}

/// Builds the planned upsert for each customer: links it to an existing Xero
/// contact (see [`resolve_link`]), maps it, and resolves display-name
/// collisions.
///
/// A name colliding with another customer in this batch, or with an existing
/// Xero contact linked to a *different* customer, gets the ` (<number>)` suffix.
/// A *new* contact whose name collides with an existing contact that is linked
/// to *nobody* (no `ContactNumber`, not claimed in this batch) is blocked
/// instead: that contact
/// is most likely this same customer, imported before the first sync, and
/// creating a suffixed copy would duplicate it.
///
/// Name comparison is case- and whitespace-insensitive to match Xero's `Name`
/// uniqueness (which also matches on name in its upsert), so distinct customers
/// sharing a name aren't silently merged into one contact.
fn plan_contacts<'a>(
    customers: &'a [Customer],
    config: &Config,
    existing: &ExistingIndex<'_>,
) -> Vec<Planned<'a>> {
    let mut planned: Vec<Planned<'a>> = customers
        .iter()
        .map(|customer| match resolve_link(customer, existing) {
            Ok((contact_id, link)) => Planned::Upsert {
                customer,
                contact: Box::new(map_customer(customer, config, contact_id.as_deref())),
                link,
            },
            Err(reason) => Planned::Blocked { customer, reason },
        })
        .collect();

    // Contacts claimed by a customer in this batch count as linked even before
    // the upsert sets their `ContactNumber`.
    let mut claimed: HashSet<String> = HashSet::new();
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for entry in &planned {
        if let Planned::Upsert { contact, .. } = entry {
            claimed.extend(contact.contact_id.clone());
            *name_counts
                .entry(normalize_name(&contact.name))
                .or_default() += 1;
        }
    }

    for entry in &mut planned {
        let Planned::Upsert {
            customer,
            contact,
            link,
        } = entry
        else {
            continue;
        };
        let customer = *customer;
        let link = *link;
        let normalized = normalize_name(&contact.name);
        let id = customer.id.to_string();
        let batch_collision = name_counts.get(&normalized).copied().unwrap_or(0) > 1;
        // Our own contact (same `ContactID`, or `ContactNumber` == our id) must
        // keep its clean name; only other contacts count as collisions.
        let others: Vec<&ContactResult> = existing
            .by_name
            .get(&normalized)
            .into_iter()
            .flatten()
            .copied()
            .filter(|c| {
                contact.contact_id.as_deref() != Some(c.contact_id.as_str())
                    && trimmed(c.contact_number.as_deref()) != Some(id.as_str())
            })
            .collect();
        // Only a customer about to get a *new* contact is blocked; one already
        // linked to its contact just gets the suffix.
        let unlinked = others.iter().find(|c| {
            link == Link::New
                && trimmed(c.contact_number.as_deref()).is_none()
                && !claimed.contains(&c.contact_id)
        });
        if let Some(unlinked) = unlinked {
            let reason = format!(
                "Xero contact \"{}\" ({}) has the same name but is not linked to any \
                 easybill customer; set its AccountNumber to {} to link it, or rename \
                 one side",
                unlinked.name,
                unlinked.contact_id,
                customer
                    .number
                    .as_deref()
                    .unwrap_or("<the easybill customer number>"),
            );
            *entry = Planned::Blocked { customer, reason };
        } else if batch_collision || !others.is_empty() {
            contact.name = name_with_number_suffix(&contact.name, customer.number.as_deref());
        }
    }
    planned
}

impl<'a> Planned<'a> {
    /// The easybill customer this plan is for.
    fn customer(&self) -> &'a Customer {
        match self {
            Self::Upsert { customer, .. } | Self::Blocked { customer, .. } => customer,
        }
    }
}

/// Normalises a display name for case-insensitive, whitespace-insensitive
/// collision detection, matching Xero's case-insensitive `Name` uniqueness.
fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Trims an optional string, treating empty/whitespace as absent.
fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Upserts a single contact and returns its Xero `ContactID`.
async fn upsert_one(client: &ApiClient, contact: &Contact) -> Result<String> {
    let request = ContactsRequest {
        contacts: vec![contact.clone()],
    };
    let response = client
        .upsert_contacts(&request)
        .await
        .context(ClientSnafu)?;
    response
        .contacts
        .into_iter()
        .next()
        .map(|c| c.contact_id)
        .context(EmptyResponseSnafu {
            context: "POST /Contacts",
        })
}

/// Prints a single dry-run contact payload as pretty JSON.
fn print_dry_run(planned: &Planned<'_>) {
    match planned {
        Planned::Upsert {
            customer, contact, ..
        } => match serde_json::to_string_pretty(contact) {
            Ok(json) => println!("--- customer {} ->\n{json}", customer.id),
            Err(e) => println!("--- customer {} -> <unserialisable: {e}>", customer.id),
        },
        Planned::Blocked { customer, reason } => {
            println!("--- customer {} -> BLOCKED: {reason}", customer.id);
        }
    }
}

/// Prints one line per customer with the planned link, plus a summary.
fn print_preview(planned: &[Planned<'_>]) {
    let (mut updates, mut creates, mut blocked) = (0usize, 0usize, 0usize);
    for entry in planned {
        match entry {
            Planned::Upsert {
                customer,
                contact,
                link,
            } => {
                let label = customer_label(customer);
                match (link, contact.contact_id.as_deref()) {
                    (Link::New, _) | (_, None) => {
                        creates += 1;
                        println!("{label} -> create \"{}\"", contact.name);
                    }
                    (link, Some(contact_id)) => {
                        updates += 1;
                        println!(
                            "{label} -> update {contact_id} as \"{}\" [linked by {link:?}]",
                            contact.name
                        );
                    }
                }
            }
            Planned::Blocked { customer, reason } => {
                blocked += 1;
                println!("{} -> BLOCKED: {reason}", customer_label(customer));
            }
        }
    }
    println!(
        "preview: {updates} update(s), {creates} create(s), {blocked} blocked, of {} selected. \
         No Xero writes made.",
        planned.len()
    );
}

/// `easybill <id> (number <n>)` for log and preview lines.
fn customer_label(customer: &Customer) -> String {
    match trimmed(customer.number.as_deref()) {
        Some(number) => format!("easybill {} (number {number})", customer.id),
        None => format!("easybill {} (no number)", customer.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str("[tax]\n").expect("config parses")
    }

    fn customer(id: i64, number: &str, company: &str, updated: Option<&str>) -> Customer {
        serde_json::from_value(serde_json::json!({
            "id": id, "number": number, "personal": false,
            "company_name": company, "emails": [],
            "updated_at": updated,
        }))
        .expect("customer fixture")
    }

    #[test]
    fn since_filter_is_inclusive() {
        let customers = [
            customer(1, "1", "A", Some("2026-01-01 00:00:00")),
            customer(2, "2", "B", Some("2026-06-01 00:00:00")),
            customer(3, "3", "C", None),
            customer(4, "4", "D", Some("garbage")),
        ];
        let since = Some(at("2026-06-01 00:00:00"));
        let kept: Vec<&Customer> = customers
            .iter()
            .filter(|c| modified_since(c, since))
            .collect();
        // Customer 2 is >= since; 3 and 4 have no usable timestamp and are kept.
        let ids: Vec<i64> = kept.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![2, 3, 4]);
    }

    fn at(ts: &str) -> NaiveDateTime {
        parse_easybill_timestamp(ts).expect("valid timestamp")
    }

    #[test]
    fn no_since_keeps_all() {
        let customers = [customer(1, "1", "A", None)];
        assert!(modified_since(&customers[0], None));
    }

    #[test]
    fn since_filter_compares_timestamps_not_text() {
        let customers = [
            customer(1, "1", "A", Some("2025-05-31 23:59:59")),
            customer(2, "2", "B", Some("2025-06-01 09:00:00")),
        ];
        let since = Some(at("2025-06-01 00:00:00"));
        let kept: Vec<&Customer> = customers
            .iter()
            .filter(|c| modified_since(c, since))
            .collect();
        let ids: Vec<i64> = kept.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![2]);
    }

    /// An existing Xero contact fixture.
    fn xero(
        contact_id: &str,
        name: &str,
        number: Option<&str>,
        account: Option<&str>,
    ) -> ContactResult {
        ContactResult {
            contact_id: contact_id.to_owned(),
            contact_number: number.map(ToOwned::to_owned),
            name: name.to_owned(),
            account_number: account.map(ToOwned::to_owned),
        }
    }

    /// The upserted contact and link of a planned entry; panics if blocked.
    fn upsert<'p>(planned: &'p Planned<'_>) -> (&'p Contact, Link) {
        match planned {
            Planned::Upsert { contact, link, .. } => (contact, *link),
            Planned::Blocked { reason, .. } => panic!("unexpectedly blocked: {reason}"),
        }
    }

    fn names(planned: &[Planned<'_>]) -> Vec<String> {
        planned.iter().map(|p| upsert(p).0.name.clone()).collect()
    }

    #[test]
    fn linked_customer_is_suffixed_not_blocked_by_stray_same_name_contact() {
        let customers = vec![customer(1, "1001", "ACME", None)];
        let existing = vec![
            xero("c-own", "ACME (old)", Some("1"), None),
            xero("c-stray", "ACME", None, None),
        ];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::new(&existing));
        let (contact, link) = upsert(&planned[0]);
        assert_eq!(link, Link::ContactNumber);
        assert_eq!(contact.name, "ACME (1001)");
    }

    #[test]
    fn since_does_not_change_collision_suffixes() {
        let customers = vec![
            customer(1, "1001", "ACME", Some("2026-01-01 00:00:00")),
            customer(2, "1002", "ACME", Some("2026-06-01 00:00:00")),
        ];
        let since = Some(at("2026-06-01 00:00:00"));
        let planned = plan_selected(&customers, since, &config(), &ExistingIndex::default());
        // Only customer 2 is selected, but it still collides with customer 1.
        assert_eq!(names(&planned), vec!["ACME (1002)"]);
    }

    #[test]
    fn collision_suffix_applied_only_on_duplicates() {
        let customers = vec![
            customer(1, "1001", "Acme", None),
            customer(2, "1002", "Acme", None),
            customer(3, "1003", "Unique", None),
        ];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::default());
        let names = names(&planned);
        assert!(names.contains(&"Acme (1001)".to_owned()));
        assert!(names.contains(&"Acme (1002)".to_owned()));
        assert!(names.contains(&"Unique".to_owned()));
    }

    #[test]
    fn collision_detection_is_case_and_whitespace_insensitive() {
        // Xero merges "Airplus" and "AirPlus" (case-insensitive Name); both must
        // get a disambiguating suffix here.
        let customers = vec![
            customer(1, "10001", "Airplus International GmbH", None),
            customer(2, "100260", "AirPlus International GmbH", None),
        ];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::default());
        let names = names(&planned);
        assert!(names.contains(&"Airplus International GmbH (10001)".to_owned()));
        assert!(names.contains(&"AirPlus International GmbH (100260)".to_owned()));
    }

    #[test]
    fn existing_xero_contact_triggers_suffix_across_runs() {
        // "Acme" already exists in Xero under a DIFFERENT customer's
        // ContactNumber (9999); a new customer named "Acme" must be suffixed
        // even though it is alone in this batch.
        let existing = vec![xero("guid-other", "Acme", Some("9999"), None)];
        let customers = vec![customer(1, "1001", "Acme", None)];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::new(&existing));
        let (contact, link) = upsert(&planned[0]);
        assert_eq!(contact.name, "Acme (1001)");
        assert_eq!(link, Link::New);
    }

    #[test]
    fn own_existing_contact_keeps_clean_name() {
        // The existing "Acme" is THIS customer's own contact (its ContactNumber
        // is the easybill id "1"), so no suffix — its name stays clean, and the
        // upsert targets that contact.
        let existing = vec![xero("guid-own", "Acme", Some("1"), None)];
        let customers = vec![customer(1, "1001", "Acme", None)];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::new(&existing));
        let (contact, link) = upsert(&planned[0]);
        assert_eq!(contact.name, "Acme");
        assert_eq!(contact.contact_id.as_deref(), Some("guid-own"));
        assert_eq!(link, Link::ContactNumber);
    }

    #[test]
    fn unlinked_contact_is_linked_by_account_number_and_keeps_name() {
        // A contact imported before the first sync: no ContactNumber, but its
        // AccountNumber is the easybill customer number. It must be updated in
        // place (by ContactID), not suffixed or duplicated.
        let existing = vec![xero("guid-imported", "Acme", None, Some("1001"))];
        let customers = vec![customer(1, "1001", "Acme", None)];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::new(&existing));
        let (contact, link) = upsert(&planned[0]);
        assert_eq!(link, Link::AccountNumber);
        assert_eq!(contact.contact_id.as_deref(), Some("guid-imported"));
        assert_eq!(contact.contact_number.as_deref(), Some("1"));
        assert_eq!(contact.name, "Acme");
    }

    #[test]
    fn account_number_link_ignores_contacts_linked_to_another_customer() {
        // AccountNumber matches, but that contact already belongs to easybill
        // customer 9999; it must not be taken over.
        let existing = vec![xero("guid-other", "Other Name", Some("9999"), Some("1001"))];
        let customers = vec![customer(1, "1001", "Acme", None)];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::new(&existing));
        let (contact, link) = upsert(&planned[0]);
        assert_eq!(link, Link::New);
        assert!(contact.contact_id.is_none());
    }

    #[test]
    fn ambiguous_account_number_is_blocked() {
        let existing = vec![
            xero("guid-a", "Acme", None, Some("1001")),
            xero("guid-b", "Acme Ltd", None, Some("1001")),
        ];
        let customers = vec![customer(1, "1001", "Acme", None)];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::new(&existing));
        assert!(
            matches!(&planned[0], Planned::Blocked { reason, .. } if reason.contains("AccountNumber 1001"))
        );
    }

    #[test]
    fn same_name_as_unlinked_contact_is_blocked_not_duplicated() {
        // "Acme" exists in Xero without ContactNumber or AccountNumber. Creating
        // "Acme (1001)" would very likely duplicate the same company.
        let existing = vec![xero("guid-imported", "ACME ", None, None)];
        let customers = vec![customer(1, "1001", "Acme", None)];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::new(&existing));
        assert!(
            matches!(&planned[0], Planned::Blocked { reason, .. } if reason.contains("guid-imported"))
        );
    }

    #[test]
    fn contact_claimed_in_batch_counts_as_linked_for_collisions() {
        // Customer 1 links the imported "Acme" via AccountNumber; customer 2 is
        // a different company also named "Acme". Customer 2 must be suffixed,
        // not blocked on the contact customer 1 is about to claim.
        let existing = vec![xero("guid-imported", "Acme", None, Some("1001"))];
        let customers = vec![
            customer(1, "1001", "Acme", None),
            customer(2, "1002", "Acme", None),
        ];
        let planned = plan_contacts(&customers, &config(), &ExistingIndex::new(&existing));
        let (first, first_link) = upsert(&planned[0]);
        let (second, second_link) = upsert(&planned[1]);
        assert_eq!(first_link, Link::AccountNumber);
        assert_eq!(second_link, Link::New);
        assert_eq!(first.name, "Acme (1001)");
        assert_eq!(second.name, "Acme (1002)");
    }
}
