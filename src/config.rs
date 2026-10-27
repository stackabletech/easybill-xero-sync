//! Configuration loading.
//!
//! The tool reads a TOML file (default `./config.toml`, overridable with
//! `--config`) that maps easybill tax combinations and booking accounts onto the
//! org-specific Xero `TaxType` and `AccountCode` values. These tables are
//! deployment-specific and validated against the live org by `xero check`, so
//! they live in config rather than in code.
//!
//! Example (see `config.example.toml`):
//!
//! ```toml
//! [xero]
//! tenant_id = "…"          # optional override of the state file
//!
//! [tax]                    # key = "null/<vat_percent>" or "<vat_option>"
//! "null/20" = "OUTPUT2"
//! "null/0"  = "ZERORATEDOUTPUT"
//! "nStb"    = "NONE"
//!
//! [accounts]               # optional overrides; unmapped booking accounts
//! default = "200"          # pass through as the Xero AccountCode, `default`
//!                          # covers lines without one
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use snafu::{OptionExt, ResultExt, Snafu};

/// The default config path used when `--config` is not given.
pub const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// The key used for the fallback account in the `[accounts]` table.
const ACCOUNTS_DEFAULT_KEY: &str = "default";

/// Errors arising from loading or querying the configuration.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    /// The config file could not be read.
    #[snafu(display("Failed to read config file {path:?}"))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    /// The config file was not valid TOML or did not match the schema.
    #[snafu(display("Failed to parse config file {path:?}"))]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },

    /// A line has no booking account and no `default` account is configured.
    #[snafu(display("Line has no booking account and no [accounts] default is set"))]
    NoAccountMapping,

    /// No tax mapping matched the given easybill tax combination.
    #[snafu(display("No TaxType configured for tax key {key:?} (from vat_option/vat_percent)"))]
    NoTaxMapping { key: String },

    /// A `[tax]` key uses the old `<vat_option>/<vat_percent>` format for a
    /// non-taxable option, which no longer matches anything.
    #[snafu(display(
        "[tax] key {key:?} in {path:?} uses the old \"<vat_option>/<vat_percent>\" format; \
         non-taxable options are keyed by the option alone: use {replacement:?}"
    ))]
    ObsoleteTaxKey {
        path: PathBuf,
        key: String,
        replacement: String,
    },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The parsed configuration document.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The `[xero]` section (all fields optional).
    #[serde(default)]
    pub xero: XeroConfig,

    /// The `[tax]` table: easybill tax key → Xero `TaxType`.
    #[serde(default)]
    pub tax: BTreeMap<String, String>,

    /// The `[accounts]` table: explicit easybill `booking_account` → Xero
    /// `AccountCode` overrides, plus the reserved `default` key for lines
    /// without a booking account (see [`Config::account_code`]).
    #[serde(default)]
    pub accounts: BTreeMap<String, String>,

    /// The `[sync]` section.
    #[serde(default)]
    pub sync: SyncConfig,
}

/// The `[sync]` configuration section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    /// easybill documents never to sync, by document number (e.g.
    /// `"RE-0001"`) or easybill document id.
    #[serde(default)]
    pub skip_documents: Vec<String>,
}

/// The `[xero]` configuration section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XeroConfig {
    /// Optional override of the tenant id stored in the state file.
    #[serde(default)]
    pub tenant_id: Option<String>,
}

impl Config {
    /// Loads and parses the configuration from `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Read`] if the file cannot be read and [`Error::Parse`]
    /// if its contents are not valid TOML matching the schema.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).context(ReadSnafu {
            path: path.to_path_buf(),
        })?;
        let config: Self = toml::from_str(&text).context(ParseSnafu {
            path: path.to_path_buf(),
        })?;
        config.check_tax_keys(path)?;
        Ok(config)
    }

    /// Rejects `[tax]` keys in the old `<vat_option>/<vat_percent>` format for
    /// non-taxable options (e.g. `"revc/0"`), which would otherwise silently
    /// never match. Only taxable keys (`"null/<vat_percent>"`) carry a percent.
    fn check_tax_keys(&self, path: &Path) -> Result<()> {
        for key in self.tax.keys() {
            if let Some((option, _)) = key.split_once('/') {
                if !option.eq_ignore_ascii_case("null") {
                    return ObsoleteTaxKeySnafu {
                        path: path.to_path_buf(),
                        key: key.clone(),
                        replacement: option.to_owned(),
                    }
                    .fail();
                }
            }
        }
        Ok(())
    }

    /// Builds the easybill tax lookup key from a `vat_option` and `vat_percent`,
    /// matching the `[tax]` table keys.
    ///
    /// - Taxable (no `vat_option`, or easybill's literal `NULL`): keyed by rate,
    ///   `"null/<vat_percent>"`, e.g. `"null/20"`. The percent is formatted
    ///   without trailing zeros (`0.0` → `0`, `7.50` → `7.5`).
    /// - Any other `vat_option` (exempt, reverse charge, export, …): easybill
    ///   charges no VAT on the document, and the line's `vat_percent` is just
    ///   the position's leftover default, so the key is the option alone, e.g.
    ///   `"nStbUstID"`.
    #[must_use]
    pub fn tax_key(vat_option: Option<&str>, vat_percent: f64) -> String {
        let option = vat_option
            .map(str::trim)
            .filter(|o| !o.is_empty() && !o.eq_ignore_ascii_case("null"));
        match option {
            Some(option) => option.to_owned(),
            None => format!("null/{}", format_percent(vat_percent)),
        }
    }

    /// Whether `[sync] skip_documents` lists this document, by number or by
    /// easybill id.
    #[must_use]
    pub fn skips_document(&self, id: i64, number: Option<&str>) -> bool {
        let id = id.to_string();
        self.sync
            .skip_documents
            .iter()
            .map(|entry| entry.trim())
            .any(|entry| entry == id || number.is_some_and(|n| n.trim() == entry))
    }

    /// Resolves the Xero `TaxType` for a tax key built by [`Self::tax_key`],
    /// or `None` if the `[tax]` table has no entry for it.
    #[must_use]
    pub fn tax_type_for_key(&self, key: &str) -> Option<&str> {
        self.tax.get(key).map(String::as_str)
    }

    /// Resolves the Xero `TaxType` for an easybill tax combination.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoTaxMapping`] if no `[tax]` entry matches.
    pub fn tax_type(&self, vat_option: Option<&str>, vat_percent: f64) -> Result<&str> {
        let key = Self::tax_key(vat_option, vat_percent);
        self.tax
            .get(&key)
            .map(String::as_str)
            .context(NoTaxMappingSnafu { key })
    }

    /// Resolves the Xero `AccountCode` for an easybill `booking_account`:
    ///
    /// 1. an explicit `[accounts]` entry for it, else
    /// 2. the booking account itself, passed through unchanged (easybill holds
    ///    the Xero account codes directly), else
    /// 3. for a line without a booking account, the configured `default`.
    ///
    /// A passed-through code that doesn't exist in Xero makes Xero reject the
    /// document; `xero check --from` reports such codes up front.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoAccountMapping`] if the line has no booking account
    /// and no `default` is configured.
    pub fn account_code<'a>(&'a self, booking_account: &'a str) -> Result<&'a str> {
        let booking_account = booking_account.trim();
        if let Some(code) = self.accounts.get(booking_account) {
            return Ok(code);
        }
        if !booking_account.is_empty() {
            return Ok(booking_account);
        }
        self.accounts
            .get(ACCOUNTS_DEFAULT_KEY)
            .map(String::as_str)
            .context(NoAccountMappingSnafu)
    }

    /// Returns every distinct Xero `TaxType` referenced by the `[tax]` table.
    #[must_use]
    pub fn referenced_tax_types(&self) -> Vec<&str> {
        let mut values: Vec<&str> = self.tax.values().map(String::as_str).collect();
        values.sort_unstable();
        values.dedup();
        values
    }

    /// Returns every distinct Xero `AccountCode` referenced by the `[accounts]`
    /// table (including the `default`).
    #[must_use]
    pub fn referenced_account_codes(&self) -> Vec<&str> {
        let mut values: Vec<&str> = self.accounts.values().map(String::as_str).collect();
        values.sort_unstable();
        values.dedup();
        values
    }
}

/// Formats a VAT percent for use in a tax key, dropping any trailing `.0`.
///
/// Whole numbers render without a fractional part (`20.0` → `20`); fractional
/// values use their shortest `f64` representation (`7.50` → `7.5`).
fn format_percent(percent: f64) -> String {
    if percent.fract() == 0.0 {
        format!("{}", percent as i64)
    } else {
        format!("{percent}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[xero]
tenant_id = "tenant-123"

[tax]
"null/20" = "OUTPUT2"
"null/0" = "ZERORATEDOUTPUT"
"revc" = "ECZROUTPUTSERVICES"

[accounts]
default = "200"
"8400" = "200"
"8300" = "210"
"#;

    fn sample() -> Config {
        toml::from_str(SAMPLE).expect("sample config parses")
    }

    #[test]
    fn skip_documents_matches_number_or_id() {
        let config: Config = toml::from_str(
            r#"
            [sync]
            skip_documents = ["RE-0001", " 900003 "]
            "#,
        )
        .expect("parses");
        assert!(config.skips_document(1, Some("RE-0001")));
        assert!(config.skips_document(900_003, Some("RE-0003")));
        assert!(config.skips_document(900_003, None));
        assert!(!config.skips_document(2, Some("RE-0002")));
        assert!(!config.skips_document(2, None));
        assert!(!Config::default().skips_document(1, Some("RE-0001")));
    }

    #[test]
    fn parses_all_sections() {
        let cfg = sample();
        assert_eq!(cfg.xero.tenant_id.as_deref(), Some("tenant-123"));
        assert_eq!(cfg.tax.len(), 3);
        assert_eq!(cfg.accounts.len(), 3);
    }

    #[test]
    fn tax_key_formats_percent_without_trailing_zero() {
        assert_eq!(Config::tax_key(None, 20.0), "null/20");
        assert_eq!(Config::tax_key(None, 0.0), "null/0");
        assert_eq!(Config::tax_key(None, 7.5), "null/7.5");
    }

    #[test]
    fn non_taxable_option_key_ignores_percent() {
        assert_eq!(Config::tax_key(Some("revc"), 0.0), "revc");
        assert_eq!(Config::tax_key(Some("nStbUstID"), 20.0), "nStbUstID");
        assert_eq!(Config::tax_key(Some("nStb"), 19.0), "nStb");
    }

    #[test]
    fn literal_null_option_is_taxable() {
        // The easybill spec spells the taxable default as the string "NULL".
        assert_eq!(Config::tax_key(Some("NULL"), 20.0), "null/20");
        assert_eq!(Config::tax_key(Some(""), 20.0), "null/20");
    }

    #[test]
    fn old_option_percent_key_is_rejected_on_load() {
        let dir = std::env::temp_dir().join(format!("easybill-xero-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[tax]\n\"null/20\" = \"OUTPUT2\"\n\"revc/0\" = \"NONE\"\n",
        )
        .expect("write config");
        let err = Config::load(&path).expect_err("old key must be rejected");
        assert!(
            matches!(&err, Error::ObsoleteTaxKey { key, replacement, .. }
                if key == "revc/0" && replacement == "revc"),
            "unexpected error: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tax_type_lookup() {
        let cfg = sample();
        assert_eq!(cfg.tax_type(None, 20.0).expect("mapped"), "OUTPUT2");
        assert_eq!(
            cfg.tax_type(Some("revc"), 0.0).expect("mapped"),
            "ECZROUTPUTSERVICES"
        );
    }

    #[test]
    fn tax_type_missing_errors() {
        let cfg = sample();
        assert!(matches!(
            cfg.tax_type(Some("unknown"), 19.0),
            Err(Error::NoTaxMapping { .. })
        ));
    }

    #[test]
    fn account_code_mapped_then_passthrough_then_default() {
        let cfg = sample();
        assert_eq!(cfg.account_code("8400").expect("mapped"), "200");
        assert_eq!(cfg.account_code("8300").expect("mapped"), "210");
        // Unmapped: passed through unchanged as the Xero AccountCode.
        assert_eq!(cfg.account_code("260").expect("passthrough"), "260");
        assert_eq!(cfg.account_code(" 260 ").expect("passthrough"), "260");
        // No booking account on the line: default.
        assert_eq!(cfg.account_code("").expect("default"), "200");
    }

    #[test]
    fn account_code_no_default_errors() {
        let cfg = Config::default();
        assert!(matches!(cfg.account_code(""), Err(Error::NoAccountMapping)));
        // A booking account still passes through without a default.
        assert_eq!(cfg.account_code("8400").expect("passthrough"), "8400");
    }

    #[test]
    fn referenced_values_are_deduped_and_sorted() {
        let cfg = sample();
        assert_eq!(
            cfg.referenced_tax_types(),
            vec!["ECZROUTPUTSERVICES", "OUTPUT2", "ZERORATEDOUTPUT"]
        );
        assert_eq!(cfg.referenced_account_codes(), vec!["200", "210"]);
    }

    #[test]
    fn unknown_section_is_rejected() {
        let bad = "[bogus]\nx = 1\n";
        assert!(toml::from_str::<Config>(bad).is_err());
    }
}
