//! `xero relink`: move an easybill customer's link to the Xero contact the
//! accountants switched it to.
//!
//! A customer is linked to the contact whose `ContactNumber` is its easybill
//! id (see `sync::customers`). When the accountants replace that contact with
//! a new one (they move the easybill customer number, the `AccountNumber`, to
//! a new contact and archive the old one), the link still points at the old
//! contact. Xero keeps `AccountNumber` unique across all contacts, archived
//! ones included, so at most one contact carries the number. `ContactNumber` can't be edited in the Xero UI, so they can't
//! move it themselves.
//!
//! [`find`] detects this: the contact linked by `ContactNumber` and the only
//! other active, unlinked contact with the customer's `AccountNumber` differ.
//! `sync customers` blocks such customers. This command reports them and, with
//! `--apply`, moves the link for each switch:
//!
//! 1. clear the old contact's `ContactNumber`, and check Xero really cleared it
//!    (it may ignore an empty value; then nothing else is changed),
//! 2. archive the old contact,
//! 3. set the easybill id as `ContactNumber` on the new contact.
//!
//! Clearing first means the id is never on two contacts at once. Xero doesn't
//! let the API edit an archived contact, so an archived old contact has to be
//! un-archived in the Xero UI first: the command lists those and waits for
//! Enter, then reads the contacts again and only applies the switches it
//! reported. If a step fails, the switch stops there; whatever was done is
//! safe for `sync customers`, which links the new contact by `AccountNumber`
//! once the old one no longer carries the id.

use std::{
    collections::HashMap,
    io::{IsTerminal as _, Write as _},
    path::Path,
};

use easybill::{client::EasybillClient, models::Customer};
use snafu::{OptionExt as _, ResultExt as _, Snafu};
use tracing::Instrument as _;

use crate::{
    config::Config,
    sync::{customers::trimmed, error_chain},
    xero::{
        client::{self, ApiClient},
        models::{ContactPatch, ContactResult},
        token::{self, AuthMode, TokenManager},
    },
};

/// The Xero status that archives a contact.
const STATUS_ARCHIVED: &str = "ARCHIVED";

/// Errors arising from `xero relink`.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    /// Obtaining an access token failed.
    #[snafu(display("Authentication error"))]
    Token { source: token::Error },

    /// Building the API client, or reading the contacts, failed.
    #[snafu(display("Xero API client error"))]
    Client { source: client::Error },

    /// Reading the customers from easybill failed.
    #[snafu(display("easybill error"))]
    Easybill { source: easybill::client::Error },

    /// Waiting for the operator's Enter failed.
    #[snafu(display("Failed to read from stdin"))]
    ReadInput { source: std::io::Error },

    /// The task reading stdin did not finish.
    #[snafu(display("Failed to wait for input"))]
    WaitForInput { source: tokio::task::JoinError },

    /// stdin was closed instead of confirming.
    #[snafu(display("Aborted: stdin closed before Enter was pressed"))]
    Aborted,

    /// Clearing the old contact's `ContactNumber` failed.
    #[snafu(display("Failed to clear the ContactNumber of the old contact {contact_id}"))]
    ClearContactNumber {
        contact_id: String,
        source: client::Error,
    },

    /// Xero accepted the update but kept the old contact's `ContactNumber`.
    #[snafu(display(
        "Xero did not clear the ContactNumber of the old contact {contact_id}; nothing else was changed"
    ))]
    NotCleared { contact_id: String },

    /// Archiving the old contact failed.
    #[snafu(display(
        "Failed to archive the old contact {contact_id}; its ContactNumber is already cleared, archive it in Xero"
    ))]
    ArchiveOld {
        contact_id: String,
        source: client::Error,
    },

    /// Setting `ContactNumber` on the new contact failed.
    #[snafu(display(
        "Failed to set the ContactNumber of the new contact {contact_id} (the next sync links it by AccountNumber)"
    ))]
    SetContactNumber {
        contact_id: String,
        source: client::Error,
    },

    /// Xero accepted the update but the new contact lacks the `ContactNumber`.
    #[snafu(display("Xero did not set the ContactNumber of the new contact {contact_id}"))]
    NotSet { contact_id: String },

    /// A switch can't be applied in the state found after waiting.
    #[snafu(display("{reason}"))]
    NotApplicable { reason: String },

    /// Some reported customers were not switched (details already printed).
    #[snafu(display("{unresolved} of {total} reported customer(s) not switched"))]
    Unresolved { unresolved: usize, total: usize },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Options for a `xero relink` run.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Make the switches instead of only reporting them.
    pub apply: bool,
}

/// A customer whose `ContactNumber` link and `AccountNumber` match point at
/// different Xero contacts.
#[derive(Debug, Clone)]
pub enum Finding<'a> {
    /// Exactly one other contact claims the customer: switch to it.
    Switch {
        /// The easybill customer.
        customer: &'a Customer,

        /// The contact linked today by `ContactNumber`.
        old: &'a ContactResult,

        /// The active, unlinked contact carrying the customer's number.
        new: &'a ContactResult,
    },

    /// Several other contacts claim the customer; someone has to decide. Xero
    /// keeps `AccountNumber` unique, so this needs numbers that differ only
    /// in surrounding whitespace.
    Ambiguous {
        /// The easybill customer.
        customer: &'a Customer,

        /// The contact linked today by `ContactNumber`.
        old: &'a ContactResult,

        /// The active, unlinked contacts carrying the customer's number.
        candidates: Vec<&'a ContactResult>,
    },
}

impl Finding<'_> {
    /// The easybill customer this finding is about.
    pub fn customer(&self) -> &Customer {
        match self {
            Self::Switch { customer, .. } | Self::Ambiguous { customer, .. } => customer,
        }
    }
}

/// The reason given when `sync customers` blocks the customer. It ends up in
/// logs, so it names contacts by id only, never by name.
impl std::fmt::Display for Finding<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Switch { customer, old, new } => write!(
                f,
                "linked to {old} by ContactNumber, but {new} has AccountNumber {number}; \
                 run `xero relink --apply` to switch to it",
                old = reference(old),
                new = reference(new),
                number = customer.number.as_deref().unwrap_or_default().trim(),
            ),
            Self::Ambiguous {
                customer,
                old,
                candidates,
            } => {
                let candidates: Vec<String> = candidates.iter().map(|c| reference(c)).collect();
                write!(
                    f,
                    "linked to {old} by ContactNumber, but {count} other active contacts \
                     have AccountNumber {number}: {candidates}; keep the number on one of them",
                    old = reference(old),
                    count = candidates.len(),
                    number = customer.number.as_deref().unwrap_or_default().trim(),
                    candidates = candidates.join(", "),
                )
            }
        }
    }
}

/// `contact ContactID (STATUS)`, for messages that may be logged.
fn reference(contact: &ContactResult) -> String {
    format!(
        "contact {id} ({status})",
        id = contact.contact_id,
        status = contact.status_label(),
    )
}

/// `"Name" (ContactID, STATUS)`, for stdout only (never logged).
fn describe(contact: &ContactResult) -> String {
    format!(
        "\"{name}\" ({id}, {status})",
        name = contact.name,
        id = contact.contact_id,
        status = contact.status_label(),
    )
}

/// Finds a switched contact for one customer, if there is one.
///
/// There is one when exactly one contact has the easybill id as
/// `ContactNumber` (more is a different problem, which `sync customers`
/// reports), and at least one *other* contact is active, has no
/// `ContactNumber`, and has the customer's number as `AccountNumber`.
/// Archived candidates don't count: they are retired, not the replacement.
/// Contacts linked to another customer don't count either: `sync customers`
/// sets that customer's own number on them again.
pub fn find<'a>(customer: &'a Customer, contacts: &'a [ContactResult]) -> Option<Finding<'a>> {
    let id = customer.id.to_string();
    let number = trimmed(customer.number.as_deref())?;
    let mut linked = contacts
        .iter()
        .filter(|c| trimmed(c.contact_number.as_deref()) == Some(id.as_str()));
    let old = linked.next()?;
    if linked.next().is_some() {
        return None;
    }
    let candidates: Vec<&ContactResult> = contacts
        .iter()
        .filter(|c| {
            c.contact_id != old.contact_id
                && c.is_active()
                && trimmed(c.contact_number.as_deref()).is_none()
                && trimmed(c.account_number.as_deref()) == Some(number)
        })
        .collect();
    match candidates.as_slice() {
        [] => None,
        [new] => Some(Finding::Switch { customer, old, new }),
        _ => Some(Finding::Ambiguous {
            customer,
            old,
            candidates,
        }),
    }
}

/// A switch as reported, kept by id so it survives re-reading the contacts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedSwitch {
    /// The easybill customer id.
    customer_id: i64,

    /// The old contact's `ContactID`.
    old_id: String,

    /// The new contact's `ContactID`.
    new_id: String,
}

/// Runs `xero relink`.
///
/// # Errors
///
/// Returns [`Error::Easybill`], [`Error::Token`] or [`Error::Client`] on a
/// run-level failure, [`Error::Aborted`] if stdin closes at the prompt, and
/// [`Error::Unresolved`] with `--apply` if any reported customer was not
/// switched (after attempting the rest).
#[tracing::instrument(name = "relink", skip_all, fields(apply = options.apply))]
pub async fn run(
    config: &Config,
    auth_mode: AuthMode,
    state_override: Option<&Path>,
    options: &Options,
) -> Result<()> {
    let easybill = EasybillClient::from_env().context(EasybillSnafu)?;
    let customers = easybill.all_customers().await.context(EasybillSnafu)?;
    let manager = TokenManager::from_env(auth_mode, state_override, config.xero.tenant_id.clone())
        .context(TokenSnafu)?;
    let client = ApiClient::connect(manager).await.context(ClientSnafu)?;
    let contacts = client.all_contacts().await.context(ClientSnafu)?;

    let findings: Vec<Finding<'_>> = customers
        .iter()
        .filter_map(|customer| find(customer, &contacts))
        .collect();
    print_report(&findings);
    if !options.apply {
        if !findings.is_empty() {
            println!("Report only, nothing changed. Run with --apply to make the switches.");
        }
        return Ok(());
    }

    let total = findings.len();
    let ambiguous = findings
        .iter()
        .filter(|f| matches!(f, Finding::Ambiguous { .. }))
        .count();
    let mut planned = Vec::new();
    let mut archived = Vec::new();
    for finding in &findings {
        if let Finding::Switch { customer, old, new } = finding {
            planned.push(PlannedSwitch {
                customer_id: customer.id,
                old_id: old.contact_id.clone(),
                new_id: new.contact_id.clone(),
            });
            if !old.is_active() {
                archived.push(format!(
                    "  - \"{name}\" ({id}, AccountNumber {account}), easybill {label}; \
                     new contact: \"{new_name}\" ({new_id})",
                    name = old.name,
                    id = old.contact_id,
                    account = old.account_number.as_deref().unwrap_or("none"),
                    label = customer_label(customer),
                    new_name = new.name,
                    new_id = new.contact_id,
                ));
            }
        }
    }

    // After the operator un-archived contacts in Xero, read them again so the
    // switches run against what Xero has now.
    let refreshed;
    let contacts = if archived.is_empty() {
        &contacts
    } else {
        println!(
            "\nWARNING: Xero doesn't let the API edit archived contacts. Un-archive \
             these old contacts in Xero first; this command archives them again:"
        );
        for line in &archived {
            println!("{line}");
        }
        if std::io::stdin().is_terminal() {
            print!("Press Enter when done to continue (Ctrl-C to abort) ");
            // A failed flush only delays the prompt; reading still works.
            let _ = std::io::stdout().flush();
            wait_for_enter().await?;
            refreshed = client.all_contacts().await.context(ClientSnafu)?;
            &refreshed
        } else {
            println!("stdin is not a terminal, not waiting: the switches above are skipped.");
            &contacts
        }
    };

    let by_id: HashMap<i64, &Customer> = customers.iter().map(|c| (c.id, c)).collect();
    let mut failed = 0usize;
    for switch in &planned {
        let span = tracing::info_span!(
            "customer",
            customer_id = switch.customer_id,
            old_contact_id = switch.old_id.as_str(),
            new_contact_id = switch.new_id.as_str(),
            otel.status_code = tracing::field::Empty,
        );
        let customer = by_id
            .get(&switch.customer_id)
            .copied()
            .expect("planned switches must come from the fetched customers");
        let result = async {
            let (old, new) = current_switch(customer, contacts, switch)?;
            apply_switch(&client, customer, old, new).await
        }
        .instrument(span.clone())
        .await;
        let label = customer_label(customer);
        match result {
            Ok(()) => println!("easybill {label}: switched to {}", switch.new_id),
            Err(error) => {
                failed += 1;
                span.record("otel.status_code", "error");
                span.in_scope(|| {
                    tracing::error!(error = %error_chain(&error), "contact switch failed; continuing");
                });
                println!("easybill {label}: NOT switched: {}", error_chain(&error));
            }
        }
    }

    let switched = planned.len() - failed;
    println!(
        "relink: {switched} switched, {failed} failed, {ambiguous} ambiguous, of {total} reported."
    );
    let unresolved = failed + ambiguous;
    if unresolved > 0 {
        return UnresolvedSnafu { unresolved, total }.fail();
    }
    Ok(())
}

/// Prints one line per finding, plus a summary.
fn print_report(findings: &[Finding<'_>]) {
    if findings.is_empty() {
        println!("relink: no customer is linked to a different contact than its AccountNumber.");
        return;
    }
    let mut switches = 0usize;
    for finding in findings {
        let label = customer_label(finding.customer());
        match finding {
            Finding::Switch { old, new, .. } => {
                switches += 1;
                println!(
                    "easybill {label}: switch {old} -> {new}",
                    old = describe(old),
                    new = describe(new),
                );
            }
            Finding::Ambiguous {
                old, candidates, ..
            } => {
                let candidates: Vec<String> = candidates.iter().map(|c| describe(c)).collect();
                println!(
                    "easybill {label}: AMBIGUOUS: linked to {old}, but several contacts \
                     have its AccountNumber: {candidates}; keep the number on one of them",
                    old = describe(old),
                    candidates = candidates.join(", "),
                );
            }
        }
    }
    println!(
        "relink: {switches} switch(es), {ambiguous} ambiguous.",
        ambiguous = findings.len() - switches
    );
}

/// `<id> (number <n>)` for output lines.
fn customer_label(customer: &Customer) -> String {
    match trimmed(customer.number.as_deref()) {
        Some(number) => format!("{} (number {number})", customer.id),
        None => format!("{} (no number)", customer.id),
    }
}

/// Waits for the operator to press Enter.
async fn wait_for_enter() -> Result<()> {
    let read = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)
    })
    .await
    .context(WaitForInputSnafu)?
    .context(ReadInputSnafu)?;
    if read == 0 {
        return AbortedSnafu.fail();
    }
    Ok(())
}

/// Re-checks a reported switch against the current contacts and returns the
/// old and new contact, so nothing is applied that the operator didn't see.
fn current_switch<'a>(
    customer: &'a Customer,
    contacts: &'a [ContactResult],
    switch: &PlannedSwitch,
) -> Result<(&'a ContactResult, &'a ContactResult)> {
    match find(customer, contacts) {
        Some(Finding::Switch { old, new, .. })
            if old.contact_id == switch.old_id && new.contact_id == switch.new_id =>
        {
            if old.is_active() {
                Ok((old, new))
            } else {
                NotApplicableSnafu {
                    reason: format!(
                        "the old contact {} is still {}; un-archive it in Xero and run again",
                        old.contact_id,
                        old.status_label()
                    ),
                }
                .fail()
            }
        }
        _ => NotApplicableSnafu {
            reason: "the contacts changed in Xero since the report; run again".to_owned(),
        }
        .fail(),
    }
}

/// Moves the link from `old` to `new` (see the module docs for the steps).
async fn apply_switch(
    client: &ApiClient,
    customer: &Customer,
    old: &ContactResult,
    new: &ContactResult,
) -> Result<()> {
    let id = customer.id.to_string();

    let cleared = client
        .patch_contact(ContactPatch {
            contact_id: old.contact_id.clone(),
            contact_number: Some(String::new()),
            contact_status: None,
        })
        .await
        .context(ClearContactNumberSnafu {
            contact_id: old.contact_id.as_str(),
        })?;
    let still_linked = cleared
        .contacts
        .iter()
        .any(|c| trimmed(c.contact_number.as_deref()).is_some());
    if cleared.contacts.is_empty() || still_linked {
        return NotClearedSnafu {
            contact_id: old.contact_id.as_str(),
        }
        .fail();
    }
    tracing::info!("old contact's ContactNumber cleared");

    client
        .patch_contact(ContactPatch {
            contact_id: old.contact_id.clone(),
            contact_number: None,
            contact_status: Some(STATUS_ARCHIVED.to_owned()),
        })
        .await
        .context(ArchiveOldSnafu {
            contact_id: old.contact_id.as_str(),
        })?;
    tracing::info!("old contact archived");

    let set = client
        .patch_contact(ContactPatch {
            contact_id: new.contact_id.clone(),
            contact_number: Some(id.clone()),
            contact_status: None,
        })
        .await
        .context(SetContactNumberSnafu {
            contact_id: new.contact_id.as_str(),
        })?;
    set.contacts
        .iter()
        .find(|c| trimmed(c.contact_number.as_deref()) == Some(id.as_str()))
        .context(NotSetSnafu {
            contact_id: new.contact_id.as_str(),
        })?;
    tracing::info!("new contact linked");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn customer(id: i64, number: Option<&str>) -> Customer {
        serde_json::from_value(serde_json::json!({
            "id": id, "number": number, "personal": false,
            "company_name": "Acme", "emails": [],
        }))
        .expect("customer fixture should parse")
    }

    fn contact(
        id: &str,
        number: Option<&str>,
        account: Option<&str>,
        status: &str,
    ) -> ContactResult {
        ContactResult {
            contact_id: id.to_owned(),
            contact_number: number.map(ToOwned::to_owned),
            name: "Acme".to_owned(),
            account_number: account.map(ToOwned::to_owned),
            contact_status: Some(status.to_owned()),
        }
    }

    /// The (old, new) contact ids of a switch, or a description of the finding.
    fn ids(finding: Option<Finding<'_>>) -> Option<(String, String)> {
        match finding {
            Some(Finding::Switch { old, new, .. }) => {
                Some((old.contact_id.clone(), new.contact_id.clone()))
            }
            Some(other @ Finding::Ambiguous { .. }) => {
                Some(("ambiguous".to_owned(), other.to_string()))
            }
            None => None,
        }
    }

    #[test]
    fn archived_old_contact_and_new_contact_with_account_number_is_a_switch() {
        let customer = customer(7, Some("1001"));
        let contacts = vec![
            contact("old", Some("7"), Some("1001-old"), "ARCHIVED"),
            contact("new", None, Some("1001"), "ACTIVE"),
        ];
        assert_eq!(
            ids(find(&customer, &contacts)),
            Some(("old".to_owned(), "new".to_owned()))
        );
    }

    #[test]
    fn active_old_contact_is_a_switch_too() {
        let customer = customer(7, Some("1001"));
        let contacts = vec![
            contact("old", Some("7"), Some("0815"), "ACTIVE"),
            contact("new", None, Some(" 1001 "), "ACTIVE"),
        ];
        assert_eq!(
            ids(find(&customer, &contacts)),
            Some(("old".to_owned(), "new".to_owned()))
        );
    }

    #[test]
    fn contact_linked_and_matching_by_account_number_is_no_finding() {
        let customer = customer(7, Some("1001"));
        let contacts = vec![contact("own", Some("7"), Some("1001"), "ACTIVE")];
        assert_eq!(ids(find(&customer, &contacts)), None);
    }

    #[test]
    fn archived_candidate_is_ignored() {
        let customer = customer(7, Some("1001"));
        let contacts = vec![
            contact("own", Some("7"), Some("1001"), "ACTIVE"),
            contact("retired", None, Some("1001"), "ARCHIVED"),
        ];
        assert_eq!(ids(find(&customer, &contacts)), None);
    }

    #[test]
    fn candidate_linked_to_another_customer_is_ignored() {
        let customer = customer(7, Some("1001"));
        let contacts = vec![
            contact("own", Some("7"), Some("1001"), "ACTIVE"),
            contact("other", Some("8"), Some("1001"), "ACTIVE"),
        ];
        assert_eq!(ids(find(&customer, &contacts)), None);
    }

    #[test]
    fn customer_without_link_or_number_is_no_finding() {
        let unlinked = customer(7, Some("1001"));
        let contacts = vec![contact("new", None, Some("1001"), "ACTIVE")];
        assert_eq!(ids(find(&unlinked, &contacts)), None);

        let no_number = customer(7, None);
        let contacts = vec![
            contact("old", Some("7"), None, "ARCHIVED"),
            contact("new", None, None, "ACTIVE"),
        ];
        assert_eq!(ids(find(&no_number, &contacts)), None);
    }

    #[test]
    fn several_candidates_are_ambiguous() {
        let customer = customer(7, Some("1001"));
        let contacts = vec![
            contact("old", Some("7"), None, "ARCHIVED"),
            contact("a", None, Some("1001"), "ACTIVE"),
            contact("b", None, Some("1001"), "ACTIVE"),
        ];
        let finding = find(&customer, &contacts);
        assert!(matches!(
            &finding,
            Some(Finding::Ambiguous { candidates, .. }) if candidates.len() == 2
        ));
    }

    #[test]
    fn reported_switch_is_rechecked_against_current_contacts() {
        let customer = customer(7, Some("1001"));
        let planned = PlannedSwitch {
            customer_id: 7,
            old_id: "old".to_owned(),
            new_id: "new".to_owned(),
        };

        let unarchived = vec![
            contact("old", Some("7"), None, "ACTIVE"),
            contact("new", None, Some("1001"), "ACTIVE"),
        ];
        let (old, new) =
            current_switch(&customer, &unarchived, &planned).expect("switch should apply");
        assert_eq!(
            (old.contact_id.as_str(), new.contact_id.as_str()),
            ("old", "new")
        );

        let still_archived = vec![
            contact("old", Some("7"), None, "ARCHIVED"),
            contact("new", None, Some("1001"), "ACTIVE"),
        ];
        let error = current_switch(&customer, &still_archived, &planned)
            .expect_err("archived old contact should not apply");
        assert!(error.to_string().contains("still ARCHIVED"));

        let changed = vec![
            contact("old", Some("7"), None, "ACTIVE"),
            contact("other", None, Some("1001"), "ACTIVE"),
        ];
        let error = current_switch(&customer, &changed, &planned)
            .expect_err("a different new contact should not apply");
        assert!(error.to_string().contains("changed in Xero"));
    }
}
