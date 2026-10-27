//! `xero check`: validate the tax/account config against the live org.
//!
//! Every `TaxType` referenced by `[tax]` and every `AccountCode` referenced by
//! `[accounts]` must exist **and be `ACTIVE`** in the connected organisation.
//! The check fails fast but reports **all** problems at once so the operator can
//! fix the config in a single pass.
//!
//! It also lists every easybill tax key with its `[tax]` mapping. With `--from`,
//! it scans easybill for the keys actually used; a key used on a document line
//! without a mapping is a problem, since `sync documents` would fail that
//! document. The scan also lists every booking account used on document lines
//! with the Xero `AccountCode` it resolves to; one that is missing or inactive
//! in the org is a problem, since Xero would reject the document.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use snafu::{ResultExt, Snafu};

use crate::config::Config;
use crate::sync::documents::{document_filters, without_skipped};
use crate::xero::client::{self, ApiClient};
use crate::xero::models::{Account, TaxRate};
use crate::xero::token::{self, AuthMode, TokenManager};
use easybill::client::EasybillClient;
use easybill::models::{Customer, Document};

/// Errors arising from `xero check`.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    /// Obtaining an access token failed.
    #[snafu(display("Authentication error"))]
    Token { source: token::Error },

    /// Building the API client failed.
    #[snafu(display("API client error"))]
    Client { source: client::Error },

    /// Reading from easybill (for the tax-key scan) failed.
    #[snafu(display("easybill error"))]
    Easybill { source: easybill::client::Error },

    /// One or more configured values were missing or inactive.
    #[snafu(display("Config validation failed:\n{report}"))]
    Validation { report: String },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Options for a `xero check` run.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Start of the easybill document-date window to scan for tax keys;
    /// `None` skips the easybill scan entirely.
    pub from: Option<String>,

    /// Inclusive end of that window; open-ended if `None`.
    pub to: Option<String>,

    /// Also list every scanned document line with its resolved mappings.
    pub verbose: bool,
}

/// A single problem found while validating the config against the org.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A configured `TaxType` does not exist in the org.
    MissingTaxType { tax_type: String },

    /// A configured `TaxType` exists but is not `ACTIVE`.
    InactiveTaxType { tax_type: String, status: String },

    /// A configured `TaxType` can't be used on sales (e.g. a purchase rate).
    NotRevenueTaxType { tax_type: String },

    /// A configured `AccountCode` does not exist in the org.
    MissingAccount { code: String },

    /// A configured `AccountCode` exists but is not `ACTIVE`.
    InactiveAccount { code: String, status: String },

    /// An easybill tax key used on document line items has no `[tax]` entry.
    UnmappedTaxKey { key: String, line_items: usize },

    /// Lines without a booking account exist, but no `[accounts]` default is
    /// set.
    NoDefaultAccount { line_items: usize },

    /// A booking account used on document lines resolves to an `AccountCode`
    /// that is missing (`status: None`) or not `ACTIVE` in the org.
    UnusableBookingAccount {
        booking_account: String,
        account_code: String,
        line_items: usize,
        status: Option<String>,
    },
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::MissingTaxType { tax_type } => {
                write!(f, "  - TaxType {tax_type:?} is not defined in the org")
            }
            Problem::InactiveTaxType { tax_type, status } => {
                write!(f, "  - TaxType {tax_type:?} is {status}, not ACTIVE")
            }
            Problem::NotRevenueTaxType { tax_type } => {
                write!(
                    f,
                    "  - TaxType {tax_type:?} can't be used on sales (revenue)"
                )
            }
            Problem::MissingAccount { code } => {
                write!(f, "  - AccountCode {code:?} is not defined in the org")
            }
            Problem::InactiveAccount { code, status } => {
                write!(f, "  - AccountCode {code:?} is {status}, not ACTIVE")
            }
            Problem::UnmappedTaxKey { key, line_items } => write!(
                f,
                "  - easybill tax key {key:?} is used on {line_items} line item(s) \
                 but has no [tax] entry"
            ),
            Problem::NoDefaultAccount { line_items } => write!(
                f,
                "  - {line_items} line item(s) have no booking account, \
                 but [accounts] has no default"
            ),
            Problem::UnusableBookingAccount {
                booking_account,
                account_code,
                line_items,
                status,
            } => {
                let state = match status {
                    None => "not defined in the org".to_owned(),
                    Some(status) => format!("{status}, not ACTIVE"),
                };
                write!(
                    f,
                    "  - booking account {booking_account:?} ({line_items} line item(s)) \
                     -> AccountCode {account_code:?} is {state}"
                )
            }
        }
    }
}

/// Validates the configured tax types and account codes against the org's tax
/// rates and chart of accounts, returning every problem found.
///
/// This is the pure core of `xero check`; it performs no I/O so it can be unit
/// tested against fixture data.
#[must_use]
pub fn validate(config: &Config, tax_rates: &[TaxRate], accounts: &[Account]) -> Vec<Problem> {
    let mut problems = Vec::new();

    for tax_type in config.referenced_tax_types() {
        match tax_rates.iter().find(|r| r.tax_type == tax_type) {
            None => problems.push(Problem::MissingTaxType {
                tax_type: tax_type.to_owned(),
            }),
            Some(rate) if !rate.is_active() => problems.push(Problem::InactiveTaxType {
                tax_type: tax_type.to_owned(),
                status: rate.status.clone(),
            }),
            Some(rate) if rate.can_apply_to_revenue == Some(false) => {
                problems.push(Problem::NotRevenueTaxType {
                    tax_type: tax_type.to_owned(),
                });
            }
            Some(_) => {}
        }
    }

    for code in config.referenced_account_codes() {
        match accounts.iter().find(|a| a.code == code) {
            None => problems.push(Problem::MissingAccount {
                code: code.to_owned(),
            }),
            Some(account) if !account.is_active() => problems.push(Problem::InactiveAccount {
                code: code.to_owned(),
                status: account.status.clone(),
            }),
            Some(_) => {}
        }
    }

    problems
}

/// How often an easybill tax key (see [`Config::tax_key`]) is used.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TaxKeyUsage {
    /// `POSITION` line items with this key; each needs a `TaxType` to sync.
    pub line_items: usize,

    /// Customers whose `tax_options` give this key, used for the
    /// contact's default `AccountsReceivableTaxType`. A missing mapping only
    /// leaves that default unset.
    pub customers: usize,
}

/// Collects the tax keys used by the given documents and customers, derived
/// exactly as the sync derives them.
#[must_use]
pub fn collect_tax_keys(
    documents: &[Document],
    customers: &[Customer],
) -> BTreeMap<String, TaxKeyUsage> {
    let mut usage: BTreeMap<String, TaxKeyUsage> = BTreeMap::new();
    for document in documents {
        for item in document.items.iter().filter(|i| i.is_position()) {
            let key = Config::tax_key(document.vat_option.as_deref(), item.vat_percent);
            usage.entry(key).or_default().line_items += 1;
        }
    }
    for customer in customers {
        let key = Config::tax_key(customer.tax_options.as_deref(), 0.0);
        usage.entry(key).or_default().customers += 1;
    }
    usage
}

/// Returns a problem for every tax key used on a document line item that has
/// no `[tax]` entry. Customer-only keys are not problems (see
/// [`TaxKeyUsage::customers`]).
#[must_use]
pub fn unmapped_tax_keys(config: &Config, usage: &BTreeMap<String, TaxKeyUsage>) -> Vec<Problem> {
    usage
        .iter()
        .filter(|(key, u)| u.line_items > 0 && config.tax_type_for_key(key).is_none())
        .map(|(key, u)| Problem::UnmappedTaxKey {
            key: key.clone(),
            line_items: u.line_items,
        })
        .collect()
}

/// Every non-default easybill `vat_option` (from the easybill API spec). These
/// are all non-taxable cases (exempt, reverse charge, export, …), keyed by the
/// option alone. The default, taxable option (`NULL`) is keyed by rate.
const EXEMPT_VAT_OPTIONS: &[&str] = &[
    "nStb",
    "nStbUstID",
    "nStbNoneUstID",
    "nStbIm",
    "revc",
    "IG",
    "AL",
    "sStfr",
    "smallBusiness",
];

/// Rates listed for the taxable default: our standard rate (20%) and 0%.
/// easybill has no API listing its configured rates, so any other rate only
/// appears once it is used on a document.
const TAXABLE_RATES: &[f64] = &[20.0, 0.0];

/// Every tax key worth mapping: each easybill `vat_option` at its expected
/// rate(s), every key in the `[tax]` table, and every key in `usage`.
#[must_use]
pub fn all_tax_keys(
    config: &Config,
    usage: Option<&BTreeMap<String, TaxKeyUsage>>,
) -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = TAXABLE_RATES
        .iter()
        .map(|&rate| Config::tax_key(None, rate))
        .chain(
            EXEMPT_VAT_OPTIONS
                .iter()
                .map(|&option| Config::tax_key(Some(option), 0.0)),
        )
        .collect();
    keys.extend(config.tax.keys().cloned());
    keys.extend(usage.into_iter().flat_map(|u| u.keys().cloned()));
    keys
}

/// Lists every easybill tax key (see [`all_tax_keys`]) with its `[tax]` mapping
/// (or `UNMAPPED`), one per line, plus usage counts when easybill was scanned.
#[must_use]
pub fn tax_keys_report(config: &Config, usage: Option<&BTreeMap<String, TaxKeyUsage>>) -> String {
    let keys = all_tax_keys(config, usage);
    let mapped: Vec<&str> = keys
        .iter()
        .map(|key| config.tax_type_for_key(key).unwrap_or("UNMAPPED"))
        .collect();
    let key_width = keys.iter().map(String::len).max().unwrap_or(0);
    let mapped_width = mapped.iter().map(|m| m.len()).max().unwrap_or(0);

    let mut report =
        String::from("easybill tax keys (null/<vat_percent> = taxable, else the vat_option):");
    for (key, tax_type) in keys.iter().zip(&mapped) {
        let line = format!("\n  {key:<key_width$}  -> {tax_type:<mapped_width$}");
        match usage {
            Some(usage) => {
                let u = usage.get(key).cloned().unwrap_or_default();
                report.push_str(&format!(
                    "{line}  {} line item(s), {} customer(s)",
                    u.line_items, u.customers
                ));
            }
            None => report.push_str(line.trim_end()),
        }
    }
    if usage.is_none() {
        report.push_str("\n(pass --from <YYYY-MM-DD> to see which keys easybill actually uses)");
    }
    report
}

/// Counts the `POSITION` line items per easybill booking account; a line
/// without one is counted under `""`.
#[must_use]
pub fn collect_booking_accounts(documents: &[Document]) -> BTreeMap<String, usize> {
    let mut usage: BTreeMap<String, usize> = BTreeMap::new();
    for document in documents {
        for item in document.items.iter().filter(|i| i.is_position()) {
            let account = item.booking_account.as_deref().unwrap_or_default().trim();
            *usage.entry(account.to_owned()).or_default() += 1;
        }
    }
    usage
}

/// How a booking account resolves against the config and the org.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BookingStatus<'a> {
    /// Resolves to this `AccountCode`, which is `ACTIVE` in the org.
    Ok(&'a str),
    /// A line without a booking account, and no `[accounts]` default.
    NoDefault,
    /// Resolves to this `AccountCode`, which the org doesn't define.
    Missing(&'a str),
    /// Resolves to this `AccountCode`, which is in the given non-`ACTIVE` status.
    Inactive(&'a str, String),
}

fn booking_status<'a>(
    config: &'a Config,
    accounts: &[Account],
    booking_account: &'a str,
) -> BookingStatus<'a> {
    let Ok(code) = config.account_code(booking_account) else {
        return BookingStatus::NoDefault;
    };
    match accounts.iter().find(|a| a.code == code) {
        None => BookingStatus::Missing(code),
        Some(account) if !account.is_active() => {
            BookingStatus::Inactive(code, account.status.clone())
        }
        Some(_) => BookingStatus::Ok(code),
    }
}

/// Returns a problem for every booking account in `usage` that can't be
/// booked: no default for lines without one, or an `AccountCode` that is
/// missing or inactive in the org.
#[must_use]
pub fn booking_account_problems(
    config: &Config,
    usage: &BTreeMap<String, usize>,
    accounts: &[Account],
) -> Vec<Problem> {
    usage
        .iter()
        .filter_map(|(booking_account, &line_items)| {
            let unusable = |code: &str, status| Problem::UnusableBookingAccount {
                booking_account: booking_account.clone(),
                account_code: code.to_owned(),
                line_items,
                status,
            };
            match booking_status(config, accounts, booking_account) {
                BookingStatus::Ok(_) => None,
                BookingStatus::NoDefault => Some(Problem::NoDefaultAccount { line_items }),
                BookingStatus::Missing(code) => Some(unusable(code, None)),
                BookingStatus::Inactive(code, status) => Some(unusable(code, Some(status))),
            }
        })
        .collect()
}

/// Lists every booking account used on document lines with the `AccountCode`
/// it resolves to (and how), its usage count, and whether the org accepts it.
#[must_use]
pub fn booking_accounts_report(
    config: &Config,
    usage: &BTreeMap<String, usize>,
    accounts: &[Account],
) -> String {
    if usage.is_empty() {
        return "No booking accounts in use on easybill document lines.".to_owned();
    }
    let rows: Vec<(String, String, String)> = usage
        .keys()
        .map(|booking_account| {
            let shown = if booking_account.is_empty() {
                "(none)".to_owned()
            } else {
                booking_account.clone()
            };
            let status = booking_status(config, accounts, booking_account);
            let target = match &status {
                BookingStatus::NoDefault => "-".to_owned(),
                BookingStatus::Ok(code)
                | BookingStatus::Missing(code)
                | BookingStatus::Inactive(code, _) => {
                    if booking_account.is_empty() {
                        format!("{code} (default)")
                    } else if *code == booking_account.as_str() {
                        (*code).to_owned()
                    } else {
                        format!("{code} (mapped)")
                    }
                }
            };
            let state = match status {
                BookingStatus::Ok(_) => "ok".to_owned(),
                BookingStatus::NoDefault => "NO DEFAULT".to_owned(),
                BookingStatus::Missing(_) => "NOT IN XERO".to_owned(),
                BookingStatus::Inactive(_, status) => status,
            };
            (shown, target, state)
        })
        .collect();
    let shown_width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let target_width = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);

    let mut report = String::from("easybill booking accounts in use (-> Xero AccountCode):");
    for ((shown, target, state), line_items) in rows.iter().zip(usage.values()) {
        report.push_str(&format!(
            "\n  {shown:<shown_width$}  -> {target:<target_width$}  {line_items} line item(s)  {state}"
        ));
    }
    report
}

/// Lists every `POSITION` line of the given documents, one per row: document
/// number, line number (counting `POSITION` rows from 1, as easybill numbers
/// them), VAT (`vat_option` and percent as in easybill), tax key → `TaxType`,
/// booking account → `AccountCode`, and what would stop the line from syncing.
#[must_use]
pub fn line_items_report(config: &Config, documents: &[Document], accounts: &[Account]) -> String {
    let mut rows: Vec<[String; 6]> = vec![[
        "DOCUMENT".to_owned(),
        "LINE".to_owned(),
        "VAT".to_owned(),
        "TAX KEY -> TAXTYPE".to_owned(),
        "BOOKING -> ACCOUNT".to_owned(),
        "STATUS".to_owned(),
    ]];
    for document in documents {
        let number = document
            .number
            .clone()
            .unwrap_or_else(|| format!("#{}", document.id));
        let option = document.vat_option.as_deref();
        let positions = document.items.iter().filter(|i| i.is_position());
        for (line, item) in positions.enumerate() {
            // The rate as easybill shows it, even where the tax key ignores it.
            let rate = Config::tax_key(None, item.vat_percent);
            let rate = rate.trim_start_matches("null/");
            let vat = match option {
                Some(option) => format!("{option} {rate}%"),
                None => format!("{rate}%"),
            };

            let key = Config::tax_key(option, item.vat_percent);
            let tax_type = config.tax_type_for_key(&key);
            let booking = item.booking_account.as_deref().unwrap_or_default().trim();
            let status = booking_status(config, accounts, booking);
            let account = match &status {
                BookingStatus::NoDefault => "-",
                BookingStatus::Ok(code)
                | BookingStatus::Missing(code)
                | BookingStatus::Inactive(code, _) => code,
            };

            let mut issues = Vec::new();
            if tax_type.is_none() {
                issues.push("tax UNMAPPED".to_owned());
            }
            match &status {
                BookingStatus::Ok(_) => {}
                BookingStatus::NoDefault => issues.push("account NO DEFAULT".to_owned()),
                BookingStatus::Missing(_) => issues.push("account NOT IN XERO".to_owned()),
                BookingStatus::Inactive(_, state) => issues.push(format!("account {state}")),
            }

            rows.push([
                number.clone(),
                (line + 1).to_string(),
                vat,
                format!("{key} -> {}", tax_type.unwrap_or("UNMAPPED")),
                format!(
                    "{} -> {account}",
                    if booking.is_empty() {
                        "(none)"
                    } else {
                        booking
                    }
                ),
                if issues.is_empty() {
                    "ok".to_owned()
                } else {
                    issues.join(", ")
                },
            ]);
        }
    }
    if rows.len() == 1 {
        return "No POSITION lines in the scanned easybill documents.".to_owned();
    }

    let mut widths = [0usize; 6];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut report = String::from("easybill document lines:");
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        report.push_str("\n  ");
        report.push_str(cells.join("  ").trim_end());
    }
    report
}

/// Lists the org's `ACTIVE` tax rates (code, rate, name), one per line, so a
/// missing `TaxType` can be fixed without looking the codes up elsewhere.
/// Custom rates get generated codes (e.g. `TAX001`) that the Xero UI never shows.
#[must_use]
pub fn active_tax_rates_report(tax_rates: &[TaxRate]) -> String {
    let active: Vec<&TaxRate> = tax_rates.iter().filter(|r| r.is_active()).collect();
    if active.is_empty() {
        return "The org has no ACTIVE tax rates.".to_owned();
    }

    let rates: Vec<String> = active
        .iter()
        .map(|r| {
            r.effective_rate
                .map_or_else(|| "?".to_owned(), |rate| format!("{}%", rate.normalize()))
        })
        .collect();
    let code_width = active.iter().map(|r| r.tax_type.len()).max().unwrap_or(0);
    let rate_width = rates.iter().map(String::len).max().unwrap_or(0);

    let mut report = String::from("ACTIVE tax rates in the org:");
    for (rate, formatted) in active.iter().zip(&rates) {
        report.push_str(&format!(
            "\n  {:<code_width$}  {:>rate_width$}  {}",
            rate.tax_type, formatted, rate.name
        ));
    }
    report
}

/// Runs `xero check`: fetch the org's tax rates and accounts, validate the
/// config against them, optionally scan easybill for tax keys in use, and fail
/// listing every problem.
///
/// # Errors
///
/// Returns [`Error::Token`], [`Error::Client`] or [`Error::Easybill`] on API
/// failure, and [`Error::Validation`] if any configured value is missing or
/// inactive, a tax key used on an easybill document is unmapped, or a booking
/// account used on one resolves to a missing or inactive `AccountCode`.
pub async fn run(
    config: &Config,
    auth_mode: AuthMode,
    state_override: Option<&Path>,
    options: &Options,
) -> Result<()> {
    let easybill_usage = match options.from.as_deref() {
        Some(from) => {
            let easybill = EasybillClient::from_env().context(EasybillSnafu)?;
            let filters = document_filters(from, options.to.as_deref());
            let documents = easybill
                .all_documents(&filters)
                .await
                .context(EasybillSnafu)?;
            let (documents, skipped) = without_skipped(config, documents);
            if skipped > 0 {
                println!("{skipped} document(s) excluded by [sync] skip_documents.");
            }
            let customers = easybill.all_customers().await.context(EasybillSnafu)?;
            Some((documents, customers))
        }
        None => None,
    };

    let manager = TokenManager::from_env(auth_mode, state_override, config.xero.tenant_id.clone())
        .context(TokenSnafu)?;
    let client = ApiClient::connect(manager).await.context(ClientSnafu)?;

    let tax_rates = client.tax_rates().await.context(ClientSnafu)?.tax_rates;
    let accounts = client.accounts().await.context(ClientSnafu)?.accounts;

    let mut problems = validate(config, &tax_rates, &accounts);
    let tax_usage = easybill_usage
        .as_ref()
        .map(|(documents, customers)| collect_tax_keys(documents, customers));
    println!("{}", tax_keys_report(config, tax_usage.as_ref()));
    if let (Some(tax_usage), Some((documents, _))) = (&tax_usage, &easybill_usage) {
        problems.extend(unmapped_tax_keys(config, tax_usage));
        let account_usage = collect_booking_accounts(documents);
        println!(
            "{}",
            booking_accounts_report(config, &account_usage, &accounts)
        );
        problems.extend(booking_account_problems(config, &account_usage, &accounts));
        if options.verbose {
            println!("{}", line_items_report(config, documents, &accounts));
        }
    }
    if problems.is_empty() {
        println!(
            "xero check OK: {} tax type(s) and {} account code(s) all exist and are ACTIVE.",
            config.referenced_tax_types().len(),
            config.referenced_account_codes().len(),
        );
        return Ok(());
    }

    let mut report = problems
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let tax_problem = problems.iter().any(|p| {
        matches!(
            p,
            Problem::MissingTaxType { .. }
                | Problem::InactiveTaxType { .. }
                | Problem::NotRevenueTaxType { .. }
                | Problem::UnmappedTaxKey { .. }
        )
    });
    if tax_problem {
        report.push('\n');
        report.push_str(&active_tax_rates_report(&tax_rates));
    }
    ValidationSnafu { report }.fail()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
[tax]
"null/20" = "OUTPUT2"
"null/0" = "ZERORATEDOUTPUT"

[accounts]
default = "200"
"8400" = "200"
"8300" = "210"
"#,
        )
        .expect("config parses")
    }

    fn tax(tax_type: &str, status: &str) -> TaxRate {
        serde_json::from_value(serde_json::json!({
            "TaxType": tax_type, "Name": tax_type, "Status": status,
        }))
        .expect("tax rate fixture")
    }

    fn account(code: &str, status: &str) -> Account {
        serde_json::from_value(serde_json::json!({
            "Code": code, "Name": code, "Status": status,
        }))
        .expect("account fixture")
    }

    #[test]
    fn all_present_and_active_is_ok() {
        let rates = vec![tax("OUTPUT2", "ACTIVE"), tax("ZERORATEDOUTPUT", "ACTIVE")];
        let accounts = vec![account("200", "ACTIVE"), account("210", "ACTIVE")];
        assert!(validate(&config(), &rates, &accounts).is_empty());
    }

    #[test]
    fn purchase_only_tax_type_is_flagged() {
        let mut input = tax("ZERORATEDOUTPUT", "ACTIVE");
        input.can_apply_to_revenue = Some(false);
        let rates = vec![tax("OUTPUT2", "ACTIVE"), input];
        let accounts = vec![account("200", "ACTIVE"), account("210", "ACTIVE")];
        assert_eq!(
            validate(&config(), &rates, &accounts),
            vec![Problem::NotRevenueTaxType {
                tax_type: "ZERORATEDOUTPUT".to_owned()
            }]
        );
    }

    #[test]
    fn missing_tax_type_is_named() {
        let rates = vec![tax("OUTPUT2", "ACTIVE")]; // ZERORATEDOUTPUT missing
        let accounts = vec![account("200", "ACTIVE"), account("210", "ACTIVE")];
        let problems = validate(&config(), &rates, &accounts);
        assert!(problems.contains(&Problem::MissingTaxType {
            tax_type: "ZERORATEDOUTPUT".to_owned(),
        }));
    }

    #[test]
    fn inactive_account_is_flagged() {
        let rates = vec![tax("OUTPUT2", "ACTIVE"), tax("ZERORATEDOUTPUT", "ACTIVE")];
        let accounts = vec![account("200", "ACTIVE"), account("210", "ARCHIVED")];
        let problems = validate(&config(), &rates, &accounts);
        assert!(problems.contains(&Problem::InactiveAccount {
            code: "210".to_owned(),
            status: "ARCHIVED".to_owned(),
        }));
    }

    #[test]
    fn reports_all_problems_at_once() {
        let rates = vec![]; // both tax types missing
        let accounts = vec![]; // both accounts missing
        let problems = validate(&config(), &rates, &accounts);
        // 2 tax types + 2 account codes = 4 problems.
        assert_eq!(problems.len(), 4);
    }

    #[test]
    fn tax_rates_report_lists_only_active_rates_aligned() {
        let rates: Vec<TaxRate> = serde_json::from_value(serde_json::json!([
            {"TaxType": "OUTPUT2", "Name": "20% (VAT on Income)", "Status": "ACTIVE", "EffectiveRate": 20.0},
            {"TaxType": "TAX001", "Name": "DE USt 19 %", "Status": "ACTIVE", "EffectiveRate": 19.0},
            {"TaxType": "NONE", "Name": "No VAT", "Status": "ACTIVE", "EffectiveRate": 0.0},
            {"TaxType": "OLDRATE", "Name": "Old", "Status": "DELETED", "EffectiveRate": 17.5},
        ]))
        .expect("tax rates fixture");
        assert_eq!(
            active_tax_rates_report(&rates),
            "ACTIVE tax rates in the org:\n  \
             OUTPUT2  20%  20% (VAT on Income)\n  \
             TAX001   19%  DE USt 19 %\n  \
             NONE      0%  No VAT"
        );
    }

    #[test]
    fn tax_rates_report_handles_missing_rate_and_empty_org() {
        assert_eq!(
            active_tax_rates_report(&[]),
            "The org has no ACTIVE tax rates."
        );
        let rates = vec![tax("NONE", "ACTIVE")];
        assert_eq!(
            active_tax_rates_report(&rates),
            "ACTIVE tax rates in the org:\n  NONE  ?  NONE"
        );
    }

    #[test]
    fn status_matching_is_case_insensitive() {
        let rates = vec![tax("OUTPUT2", "active"), tax("ZERORATEDOUTPUT", "Active")];
        let accounts = vec![account("200", "ACTIVE"), account("210", "active")];
        assert!(validate(&config(), &rates, &accounts).is_empty());
    }

    /// The report's lines with runs of whitespace collapsed, so assertions don't
    /// depend on column widths.
    fn report_lines(report: &str) -> Vec<String> {
        report
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    }

    fn document(vat_option: Option<&str>, items: serde_json::Value) -> Document {
        serde_json::from_value(serde_json::json!({
            "id": 1, "type": "INVOICE", "vat_option": vat_option, "items": items,
        }))
        .expect("document fixture")
    }

    fn customer(tax_options: Option<&str>) -> Customer {
        serde_json::from_value(serde_json::json!({
            "id": 1, "personal": false, "company_name": "X", "emails": [],
            "tax_options": tax_options,
        }))
        .expect("customer fixture")
    }

    #[test]
    fn tax_keys_are_collected_from_positions_and_customers() {
        let documents = vec![
            document(
                None,
                serde_json::json!([
                    {"type": "POSITION", "vat_percent": 20.0},
                    {"type": "POSITION", "vat_percent": 20.0},
                    {"type": "TEXT", "vat_percent": null},
                ]),
            ),
            document(
                Some("revc"),
                serde_json::json!([{"type": "POSITION", "vat_percent": 0.0}]),
            ),
            // A non-taxable document keeps the position's default rate; the
            // key ignores it.
            document(
                Some("revc"),
                serde_json::json!([{"type": "POSITION", "vat_percent": 20.0}]),
            ),
        ];
        let customers = vec![customer(None), customer(Some("revc"))];
        let usage = collect_tax_keys(&documents, &customers);

        let keys: Vec<&str> = usage.keys().map(String::as_str).collect();
        // The TEXT row contributes nothing.
        assert_eq!(keys, vec!["null/0", "null/20", "revc"]);
        assert_eq!(
            usage["null/20"],
            TaxKeyUsage {
                line_items: 2,
                customers: 0
            }
        );
        assert_eq!(
            usage["revc"],
            TaxKeyUsage {
                line_items: 2,
                customers: 1
            }
        );
        assert_eq!(
            usage["null/0"],
            TaxKeyUsage {
                line_items: 0,
                customers: 1
            }
        );
    }

    #[test]
    fn only_unmapped_keys_on_line_items_are_problems() {
        let mut usage = BTreeMap::new();
        // Mapped in config().
        usage.insert(
            "null/20".to_owned(),
            TaxKeyUsage {
                line_items: 3,
                customers: 0,
            },
        );
        // Unmapped, used on documents: a problem.
        usage.insert(
            "revc".to_owned(),
            TaxKeyUsage {
                line_items: 2,
                customers: 1,
            },
        );
        // Unmapped, customer-only: reported, but not a problem.
        usage.insert(
            "IC".to_owned(),
            TaxKeyUsage {
                line_items: 0,
                customers: 4,
            },
        );
        let problems = unmapped_tax_keys(&config(), &usage);
        assert_eq!(
            problems,
            vec![Problem::UnmappedTaxKey {
                key: "revc".to_owned(),
                line_items: 2,
            }]
        );

        let lines = report_lines(&tax_keys_report(&config(), Some(&usage)));
        for expected in [
            "null/20 -> OUTPUT2 3 line item(s), 0 customer(s)",
            "revc -> UNMAPPED 2 line item(s), 1 customer(s)",
            "IC -> UNMAPPED 0 line item(s), 4 customer(s)",
            // Unused catalogue keys are listed with zero counts.
            "smallBusiness -> UNMAPPED 0 line item(s), 0 customer(s)",
        ] {
            assert!(lines.iter().any(|l| l == expected), "missing: {expected}");
        }
    }

    #[test]
    fn report_lists_every_option_config_key_and_used_key() {
        let mut usage = BTreeMap::new();
        usage.insert("null/16".to_owned(), TaxKeyUsage::default());
        let keys = all_tax_keys(&config(), Some(&usage));
        for option in EXEMPT_VAT_OPTIONS {
            assert!(keys.contains(*option), "{option} missing");
        }
        for key in ["null/20", "null/0", "null/16"] {
            assert!(keys.contains(key), "{key} missing");
        }

        // Without a scan: no counts, plus a hint.
        let report = tax_keys_report(&config(), None);
        assert!(report_lines(&report)
            .iter()
            .any(|l| l == "null/20 -> OUTPUT2"));
        assert!(report.contains("pass --from"));
    }

    #[test]
    fn booking_accounts_are_counted_per_position() {
        let documents = vec![document(
            None,
            serde_json::json!([
                {"type": "POSITION", "booking_account": "200", "vat_percent": 20.0},
                {"type": "POSITION", "booking_account": "200", "vat_percent": 20.0},
                {"type": "POSITION", "booking_account": null, "vat_percent": 20.0},
                {"type": "TEXT", "booking_account": "999"},
            ]),
        )];
        let usage = collect_booking_accounts(&documents);
        assert_eq!(usage.get("200"), Some(&2));
        assert_eq!(usage.get(""), Some(&1));
        assert!(!usage.contains_key("999"));
    }

    #[test]
    fn booking_accounts_resolve_and_flag_unusable_codes() {
        // config(): default 200, "8400" -> 200, "8300" -> 210.
        let accounts = vec![
            account("200", "ACTIVE"),
            account("210", "ARCHIVED"),
            account("260", "ACTIVE"),
        ];
        let usage: BTreeMap<String, usize> = [
            ("260", 5),  // passthrough, ok
            ("8400", 1), // mapped to 200, ok
            ("8300", 2), // mapped to 210, archived
            ("4401", 3), // passthrough, not in Xero
            ("", 4),     // default 200, ok
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();

        let problems = booking_account_problems(&config(), &usage, &accounts);
        assert_eq!(problems.len(), 2);
        assert!(problems.contains(&Problem::UnusableBookingAccount {
            booking_account: "4401".to_owned(),
            account_code: "4401".to_owned(),
            line_items: 3,
            status: None,
        }));
        assert!(problems.contains(&Problem::UnusableBookingAccount {
            booking_account: "8300".to_owned(),
            account_code: "210".to_owned(),
            line_items: 2,
            status: Some("ARCHIVED".to_owned()),
        }));

        let lines = report_lines(&booking_accounts_report(&config(), &usage, &accounts));
        for expected in [
            "(none) -> 200 (default) 4 line item(s) ok",
            "260 -> 260 5 line item(s) ok",
            "4401 -> 4401 3 line item(s) NOT IN XERO",
            "8300 -> 210 (mapped) 2 line item(s) ARCHIVED",
            "8400 -> 200 (mapped) 1 line item(s) ok",
        ] {
            assert!(lines.iter().any(|l| l == expected), "missing: {expected}");
        }
    }

    #[test]
    fn lines_without_booking_account_need_a_default() {
        let config: Config = toml::from_str("[tax]\n").expect("config parses");
        let usage: BTreeMap<String, usize> = [(String::new(), 2)].into_iter().collect();
        assert_eq!(
            booking_account_problems(&config, &usage, &[]),
            vec![Problem::NoDefaultAccount { line_items: 2 }]
        );
    }

    #[test]
    fn line_items_report_lists_each_position_with_issues() {
        // config(): "null/20" -> OUTPUT2, default account 200, "8300" -> 210.
        let mut first = document(
            None,
            serde_json::json!([
                {"type": "POSITION", "booking_account": "200", "vat_percent": 20.0},
                {"type": "TEXT"},
                {"type": "POSITION", "booking_account": "4401", "vat_percent": 20.0},
            ]),
        );
        first.number = Some("RE-1".to_owned());
        let second = document(
            Some("nStb"),
            serde_json::json!([
                {"type": "POSITION", "booking_account": "8300", "vat_percent": 19.0},
            ]),
        );
        let accounts = vec![account("200", "ACTIVE"), account("210", "ACTIVE")];

        let lines = report_lines(&line_items_report(&config(), &[first, second], &accounts));
        for expected in [
            "DOCUMENT LINE VAT TAX KEY -> TAXTYPE BOOKING -> ACCOUNT STATUS",
            "RE-1 1 20% null/20 -> OUTPUT2 200 -> 200 ok",
            // The TEXT row isn't counted, so the second position is line 2.
            "RE-1 2 20% null/20 -> OUTPUT2 4401 -> 4401 account NOT IN XERO",
            // No number: falls back to the easybill id.
            "#1 1 nStb 19% nStb -> UNMAPPED 8300 -> 210 tax UNMAPPED",
        ] {
            assert!(
                lines.iter().any(|l| l == expected),
                "missing: {expected}\n{lines:#?}"
            );
        }
    }
}
