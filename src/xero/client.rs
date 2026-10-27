//! Thin authenticated wrapper over the Xero Accounting API.
//!
//! Owns the [`TokenManager`] and the current [`AccessContext`], and issues
//! requests against the Xero Accounting API base with the required
//! `Authorization`, `xero-tenant-id` and `Accept` headers. Access tokens live
//! ~30 minutes, so the client renews the token before it gets old and once on
//! an HTTP 401. A single 429 (rate-limit) response is retried after honouring
//! `Retry-After`. Requests time out.
//!
//! Error bodies are reduced to Xero's error messages ([`error_summary`]):
//! validation responses echo the whole invoice or contact, which must not end
//! up in logs.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use snafu::{ResultExt, Snafu};

use crate::xero::models::{
    AccountsResponse, ContactResult, ContactsRequest, ContactsResponse, CreditNotesRequest,
    CreditNotesResponse, InvoicesRequest, InvoicesResponse, TaxRatesResponse,
};
use crate::xero::token::{self, AccessContext, TokenManager};

/// The Accounting API base URL (trailing slash so paths join cleanly).
const API_BASE: &str = "https://api.xero.com/api.xro/2.0/";

/// Fallback wait (seconds) if a 429 arrives without a parseable `Retry-After`.
const DEFAULT_RETRY_AFTER_SECS: u64 = 5;

/// Renew the access token once it is this old (Xero tokens live 30 minutes).
const TOKEN_RENEW_AFTER: Duration = Duration::from_secs(25 * 60);

/// Timeout for establishing a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Timeout for a whole request, including the body (PDF uploads included).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Longest error summary kept from a response body.
const MAX_ERROR_SUMMARY: usize = 500;

/// Xero returns at most this many contacts per `GET /Contacts` page.
const CONTACTS_PAGE_SIZE: usize = 100;

/// Defensive cap on `GET /Contacts` pages (100k contacts) to bound the loop.
const MAX_CONTACT_PAGES: u32 = 1000;

/// Errors arising from Xero Accounting API calls.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    /// Building the HTTP client failed.
    #[snafu(display("Failed to build the HTTP client"))]
    BuildClient { source: reqwest::Error },

    /// The HTTP request could not be sent or the body could not be read.
    #[snafu(display("HTTP request to Xero failed ({context})"))]
    Http {
        context: &'static str,
        source: reqwest::Error,
    },

    /// Serialising the request body failed.
    #[snafu(display("Failed to serialise the request body for {context}"))]
    Serialise {
        context: &'static str,
        source: serde_json::Error,
    },

    /// Xero returned a non-success status. `message` is the error summary
    /// from [`error_summary`], never the raw body.
    #[snafu(display("Xero returned HTTP {status} for {context}: {message}"))]
    Api {
        context: &'static str,
        status: u16,
        message: String,
    },

    /// Obtaining or renewing the access token failed.
    #[snafu(display("Failed to obtain a Xero access token"))]
    Token {
        #[snafu(source(from(token::Error, Box::new)))]
        source: Box<token::Error>,
    },

    /// The response body could not be parsed into the expected shape.
    #[snafu(display("Failed to parse the Xero response for {context}"))]
    Parse {
        context: &'static str,
        source: serde_json::Error,
    },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An authenticated Xero Accounting API client for one tenant.
#[derive(Debug)]
pub struct ApiClient {
    http: reqwest::Client,
    tokens: TokenManager,
    session: Mutex<Session>,
}

/// The current access token and when it was obtained.
#[derive(Debug, Clone)]
struct Session {
    context: AccessContext,
    obtained: Instant,
}

/// A request body: the bytes plus their content type.
type Body<'a> = Option<(&'a [u8], &'static str)>;

impl ApiClient {
    /// Obtains an access token from `tokens` and builds a client for its tenant.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Token`] if no access token can be obtained and
    /// [`Error::BuildClient`] if the HTTP client cannot be constructed.
    pub async fn connect(tokens: TokenManager) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context(BuildClientSnafu)?;
        let context = tokens.access().await.context(TokenSnafu)?;
        Ok(Self {
            http,
            tokens,
            session: Mutex::new(Session {
                context,
                obtained: Instant::now(),
            }),
        })
    }

    /// The current access context, renewed first if the token is getting old.
    async fn access(&self) -> Result<AccessContext> {
        let session = self.session().clone();
        if session.obtained.elapsed() < TOKEN_RENEW_AFTER {
            return Ok(session.context);
        }
        self.renew().await
    }

    /// Obtains a fresh access token and makes it the current one.
    async fn renew(&self) -> Result<AccessContext> {
        tracing::debug!("renewing the Xero access token");
        let context = self.tokens.access().await.context(TokenSnafu)?;
        *self.session() = Session {
            context: context.clone(),
            obtained: Instant::now(),
        };
        Ok(context)
    }

    /// Locks the session. A poisoned lock still holds a valid session (it is
    /// only ever replaced whole), so poisoning is ignored.
    fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sends a request to `path` (relative to the API base) and returns the
    /// successful response's body text.
    ///
    /// Renews the token and retries once on HTTP 401, and waits for
    /// `Retry-After` and retries once on HTTP 429.
    async fn send(
        &self,
        method: Method,
        path: &str,
        context: &'static str,
        body: Body<'_>,
    ) -> Result<String> {
        let url = format!("{API_BASE}{path}");
        let mut access = self.access().await?;
        let mut renewed = false;
        let mut rate_limited = false;
        loop {
            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&access.access_token)
                .header("xero-tenant-id", &access.tenant_id)
                .header(ACCEPT, "application/json");
            if let Some((bytes, content_type)) = body {
                req = req
                    .header(reqwest::header::CONTENT_TYPE, content_type)
                    .body(bytes.to_vec());
            }

            let response = req.send().await.context(HttpSnafu { context })?;
            let status = response.status();

            if status == StatusCode::UNAUTHORIZED && !renewed {
                renewed = true;
                tracing::warn!(context, "Xero rejected the access token; renewing");
                access = self.renew().await?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS && !rate_limited {
                rate_limited = true;
                let wait = retry_after_secs(response.headers());
                tracing::warn!(
                    context,
                    seconds = wait,
                    "Xero rate limit hit; waiting before retry"
                );
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }

            let text = response.text().await.context(HttpSnafu { context })?;
            if !status.is_success() {
                return ApiSnafu {
                    context,
                    status: status.as_u16(),
                    message: error_summary(&text),
                }
                .fail();
            }
            return Ok(text);
        }
    }

    /// Issues a request to `path` (relative to the API base) and deserialises a
    /// successful JSON response into `T`. `body` is sent as JSON when `Some`.
    ///
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] on transport failure, [`Error::Serialise`] if
    /// `body` cannot be serialised, [`Error::Api`] on a non-success status,
    /// [`Error::Token`] if the token can't be renewed, and [`Error::Parse`] if
    /// the response is not the expected shape.
    pub async fn request_json<B, T>(
        &self,
        method: Method,
        path: &str,
        context: &'static str,
        body: Option<&B>,
    ) -> Result<T>
    where
        B: Serialize,
        T: DeserializeOwned,
    {
        let text = self.request_text(method, path, context, body).await?;
        serde_json::from_str(&text).context(ParseSnafu { context })
    }

    /// Like [`Self::request_json`] but returns the raw response body text.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn request_text<B>(
        &self,
        method: Method,
        path: &str,
        context: &'static str,
        body: Option<&B>,
    ) -> Result<String>
    where
        B: Serialize,
    {
        let payload = match body {
            Some(b) => Some(serde_json::to_vec(b).context(SerialiseSnafu { context })?),
            None => None,
        };
        self.send(
            method,
            path,
            context,
            payload.as_deref().map(|p| (p, "application/json")),
        )
        .await
    }

    /// Fetches the org's tax rates via `GET /TaxRates`.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn tax_rates(&self) -> Result<TaxRatesResponse> {
        self.request_json::<(), _>(Method::GET, "TaxRates", "GET /TaxRates", None)
            .await
    }

    /// Fetches the org's chart of accounts via `GET /Accounts`.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn accounts(&self) -> Result<AccountsResponse> {
        self.request_json::<(), _>(Method::GET, "Accounts", "GET /Accounts", None)
            .await
    }

    /// Fetches all contacts (including archived) via paginated `GET /Contacts`.
    ///
    /// Pages are walked until a short or empty page is returned. Archived
    /// contacts are included because Xero's `Name` uniqueness and contact
    /// matching consider them too, so they can still cause a collision.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn all_contacts(&self) -> Result<Vec<ContactResult>> {
        let mut all = Vec::new();
        for page in 1..=MAX_CONTACT_PAGES {
            let path = format!("Contacts?includeArchived=true&page={page}");
            let response: ContactsResponse = self
                .request_json::<(), _>(Method::GET, &path, "GET /Contacts", None)
                .await?;
            let count = response.contacts.len();
            all.extend(response.contacts);
            if count < CONTACTS_PAGE_SIZE {
                return Ok(all);
            }
        }
        tracing::warn!(
            pages = MAX_CONTACT_PAGES,
            "GET /Contacts hit the page cap; the collision index may be incomplete"
        );
        Ok(all)
    }

    /// Upserts one or more contacts via `POST /Contacts`.
    ///
    /// Xero matches each contact on `ContactID` when present, else on
    /// `ContactNumber`, and also matches on `Name`, which is unique in an org
    /// (hence the collision handling in `sync::customers`); omitted fields are
    /// preserved.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn upsert_contacts(&self, request: &ContactsRequest) -> Result<ContactsResponse> {
        self.request_json(Method::POST, "Contacts", "POST /Contacts", Some(request))
            .await
    }

    /// Creates invoices via `PUT /Invoices`.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn create_invoices(&self, request: &InvoicesRequest) -> Result<InvoicesResponse> {
        self.request_json(Method::PUT, "Invoices", "PUT /Invoices", Some(request))
            .await
    }

    /// Creates or updates invoices via `POST /Invoices` (update needs
    /// `InvoiceID`).
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn update_invoices(&self, request: &InvoicesRequest) -> Result<InvoicesResponse> {
        self.request_json(Method::POST, "Invoices", "POST /Invoices", Some(request))
            .await
    }

    /// Looks up invoices by number via `GET /Invoices?InvoiceNumbers=…`.
    ///
    /// Used as a state-loss safety net before creating (Xero rejects a duplicate
    /// `InvoiceNumber`). Numbers are comma-joined.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn invoices_by_number(&self, numbers: &[String]) -> Result<InvoicesResponse> {
        let encoded = urlencoding_query(&numbers.join(","));
        let path = format!("Invoices?InvoiceNumbers={encoded}");
        self.request_json::<(), _>(Method::GET, &path, "GET /Invoices?InvoiceNumbers", None)
            .await
    }

    /// Fetches one invoice via `GET /Invoices/{id}`.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn invoice_by_id(&self, id: &str) -> Result<InvoicesResponse> {
        let path = format!("Invoices/{}", urlencoding_query(id));
        self.request_json::<(), _>(Method::GET, &path, "GET /Invoices/{id}", None)
            .await
    }

    /// Creates credit notes via `PUT /CreditNotes`.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn create_credit_notes(
        &self,
        request: &CreditNotesRequest,
    ) -> Result<CreditNotesResponse> {
        self.request_json(
            Method::PUT,
            "CreditNotes",
            "PUT /CreditNotes",
            Some(request),
        )
        .await
    }

    /// Creates or updates credit notes via `POST /CreditNotes`.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn update_credit_notes(
        &self,
        request: &CreditNotesRequest,
    ) -> Result<CreditNotesResponse> {
        self.request_json(
            Method::POST,
            "CreditNotes",
            "POST /CreditNotes",
            Some(request),
        )
        .await
    }

    /// Looks up credit notes by number via
    /// `GET /CreditNotes?CreditNoteNumber=…` (one number at a time).
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn credit_note_by_number(&self, number: &str) -> Result<CreditNotesResponse> {
        let path = format!("CreditNotes?CreditNoteNumber={}", urlencoding_query(number));
        self.request_json::<(), _>(
            Method::GET,
            &path,
            "GET /CreditNotes?CreditNoteNumber",
            None,
        )
        .await
    }

    /// Fetches one credit note via `GET /CreditNotes/{id}`.
    ///
    /// # Errors
    ///
    /// See [`Self::request_json`].
    pub async fn credit_note_by_id(&self, id: &str) -> Result<CreditNotesResponse> {
        let path = format!("CreditNotes/{}", urlencoding_query(id));
        self.request_json::<(), _>(Method::GET, &path, "GET /CreditNotes/{id}", None)
            .await
    }

    /// Uploads (or replaces) a PDF attachment with raw bytes.
    ///
    /// Uses `POST` so a re-run replaces any existing attachment of the same name,
    /// making the operation idempotent. `path` is the attachment path relative to
    /// the API base (see [`crate::xero::attachment::attachment_path`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] on transport failure and [`Error::Api`] on a
    /// non-success status.
    pub async fn upload_pdf_attachment(&self, path: &str, bytes: Vec<u8>) -> Result<()> {
        self.send(
            Method::POST,
            path,
            "attachment upload",
            Some((&bytes, "application/pdf")),
        )
        .await
        .map(drop)
    }
}

/// Reduces a Xero error body to its error messages: the top-level `Message`
/// / `Detail` / OAuth `error`, plus every `ValidationErrors[].Message` at any
/// depth. Everything else — the echoed invoice or contact — is dropped. A
/// non-JSON body is truncated.
pub(crate) fn error_summary(body: &str) -> String {
    fn collect(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if key == "ValidationErrors" {
                        for error in child.as_array().into_iter().flatten() {
                            if let Some(message) = error.get("Message").and_then(|m| m.as_str()) {
                                if !out.iter().any(|m| m == message) {
                                    out.push(message.to_owned());
                                }
                            }
                        }
                    } else {
                        collect(child, out);
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|i| collect(i, out)),
            _ => {}
        }
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return truncate(body.trim(), MAX_ERROR_SUMMARY);
    };
    let mut messages: Vec<String> = ["Message", "Detail", "error", "error_description"]
        .iter()
        .filter_map(|key| value.get(key)?.as_str().map(ToOwned::to_owned))
        .collect();
    collect(&value, &mut messages);
    if messages.is_empty() {
        return String::from("(no error message in response)");
    }
    truncate(&messages.join("; "), MAX_ERROR_SUMMARY)
}

/// Truncates `text` to at most `max` characters, marking the cut.
fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// Minimal percent-encoding for a query-string value, encoding everything
/// outside the RFC 3986 unreserved set (so spaces, `,`, `&`, `#`, `/`, etc. are
/// escaped).
fn urlencoding_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Extracts the `Retry-After` delay in seconds, defaulting on absence/parse
/// failure. Only the delta-seconds form is handled (Xero uses this form).
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_summary_keeps_messages_only() {
        let body = r#"{
          "ErrorNumber": 10,
          "Type": "ValidationException",
          "Message": "A validation exception occurred",
          "Elements": [{
            "InvoiceNumber": "INV-1",
            "Contact": {"Name": "ACME Ltd", "ValidationErrors": []},
            "LineItems": [{"Description": "Consulting for Jane Doe",
                           "ValidationErrors": [{"Message": "Account code is not valid"}]}],
            "ValidationErrors": [{"Message": "Invoice not of valid status for modification"}]
          }]
        }"#;
        let summary = error_summary(body);
        assert!(summary.starts_with("A validation exception occurred; "));
        assert!(summary.contains("Account code is not valid"));
        assert!(summary.contains("Invoice not of valid status for modification"));
        assert!(!summary.contains("ACME"));
        assert!(!summary.contains("Jane Doe"));
        assert!(!summary.contains("INV-1"));
    }

    #[test]
    fn error_summary_handles_oauth_and_non_json() {
        assert_eq!(
            error_summary(r#"{"error":"invalid_client"}"#),
            "invalid_client"
        );
        assert_eq!(
            error_summary(r#"{"Elements":[]}"#),
            "(no error message in response)"
        );
        assert_eq!(
            error_summary(&"x".repeat(600)).chars().count(),
            MAX_ERROR_SUMMARY + 1
        );
    }

    #[test]
    fn retry_after_parses_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(RETRY_AFTER, "12".parse().expect("valid header value"));
        assert_eq!(retry_after_secs(&headers), 12);
    }

    #[test]
    fn retry_after_defaults_when_absent() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_secs(&headers), DEFAULT_RETRY_AFTER_SECS);
    }

    #[test]
    fn urlencoding_escapes_reserved() {
        assert_eq!(urlencoding_query("RE-2026-001"), "RE-2026-001");
        assert_eq!(urlencoding_query("RE 1,RE 2"), "RE%201%2CRE%202");
        assert_eq!(urlencoding_query("a&b/c"), "a%26b%2Fc");
    }

    #[test]
    fn retry_after_defaults_when_unparseable() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT"
                .parse()
                .expect("valid value"),
        );
        assert_eq!(retry_after_secs(&headers), DEFAULT_RETRY_AFTER_SECS);
    }
}
