//! Read-only easybill REST v1 client.
//!
//! The tool never writes to easybill. This client covers the endpoints the sync
//! reads: paginated `GET /customers`, `GET /customers/{id}`, `GET /documents`
//! and `GET /documents/{id}/pdf`. Auth is a static bearer token from
//! `EASYBILL_API_TOKEN`. Requests time out, and a rate-limited (HTTP 429)
//! request is retried once after `Retry-After`.

use std::time::Duration;

use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use snafu::{ResultExt, Snafu};

use crate::models::{Customer, Document, Page};

/// The easybill REST v1 base URL (trailing slash so paths join cleanly).
const API_BASE: &str = "https://api.easybill.de/rest/v1/";

/// The maximum page size easybill allows.
const MAX_LIMIT: u32 = 1000;

/// Timeout for establishing a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Timeout for a whole request, including reading the body (PDFs included).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Fallback wait if a 429 arrives without a parseable `Retry-After`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(10);

/// Errors arising from easybill API calls.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    /// A required environment variable was not set.
    #[snafu(display("Missing required environment variable {var}"))]
    MissingEnv {
        var: &'static str,
        source: std::env::VarError,
    },

    /// Building the HTTP client failed.
    #[snafu(display("Failed to build the HTTP client"))]
    BuildClient { source: reqwest::Error },

    /// The HTTP request could not be sent or the body could not be read.
    #[snafu(display("HTTP request to easybill failed ({context})"))]
    Http {
        context: String,
        source: reqwest::Error,
    },

    /// easybill returned a non-success status. `message` is easybill's error
    /// message, if the body carried one.
    #[snafu(display("easybill returned HTTP {status} for {context}: {message}"))]
    Api {
        context: String,
        status: u16,
        message: String,
    },

    /// The response body could not be parsed into the expected shape.
    #[snafu(display("Failed to parse the easybill response for {context}"))]
    Parse {
        context: String,
        source: serde_json::Error,
    },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A read-only easybill API client. `Debug` redacts the token.
pub struct EasybillClient {
    http: reqwest::Client,
    token: String,
}

impl std::fmt::Debug for EasybillClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EasybillClient")
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl EasybillClient {
    /// Builds a client, reading the bearer token from `EASYBILL_API_TOKEN`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingEnv`] if `EASYBILL_API_TOKEN` is unset and
    /// [`Error::BuildClient`] if the HTTP client cannot be constructed.
    pub fn from_env() -> Result<Self> {
        let var = "EASYBILL_API_TOKEN";
        let token = std::env::var(var).context(MissingEnvSnafu { var })?;
        Self::new(token)
    }

    /// Builds a client from an explicit bearer token.
    ///
    /// Prefer this over [`Self::from_env`] when the token comes from somewhere
    /// other than the environment (a secret store, a config file, a test).
    ///
    /// # Errors
    ///
    /// Returns [`Error::BuildClient`] if the HTTP client cannot be constructed.
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context(BuildClientSnafu)?;
        Ok(Self {
            http,
            token: token.into(),
        })
    }

    /// Issues a GET to `path` (relative to the API base) with the given query
    /// pairs, retrying once on HTTP 429, and returns the successful response.
    async fn get(
        &self,
        path: &str,
        query: &[(&str, String)],
        accept: &'static str,
        context: &str,
    ) -> Result<reqwest::Response> {
        let url = format!("{API_BASE}{path}");
        let mut retried = false;
        loop {
            let response = self
                .http
                .get(&url)
                .query(query)
                .bearer_auth(&self.token)
                .header(ACCEPT, accept)
                .send()
                .await
                .context(HttpSnafu { context })?;

            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS && !retried {
                retried = true;
                tokio::time::sleep(retry_after(response.headers())).await;
                continue;
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return ApiSnafu {
                    context,
                    status: status.as_u16(),
                    message: error_message(&body),
                }
                .fail();
            }
            return Ok(response);
        }
    }

    /// Issues a GET and deserialises the JSON response into `T`.
    async fn get_json<T>(&self, path: &str, query: &[(&str, String)], context: String) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self.get(path, query, "application/json", &context).await?;
        let body = response.text().await.context(HttpSnafu {
            context: context.clone(),
        })?;
        serde_json::from_str(&body).context(ParseSnafu { context })
    }

    /// Fetches one page of customers.
    ///
    /// # Errors
    ///
    /// See [`Self::get_json`].
    pub async fn customers_page(&self, page: u32, limit: u32) -> Result<Page<Customer>> {
        self.get_json(
            "customers",
            &[
                ("page", page.to_string()),
                ("limit", limit.min(MAX_LIMIT).to_string()),
            ],
            format!("GET /customers?page={page}"),
        )
        .await
    }

    /// Fetches **all** customers across every page.
    ///
    /// # Errors
    ///
    /// Returns any error from the underlying page fetches.
    pub async fn all_customers(&self) -> Result<Vec<Customer>> {
        let mut all = Vec::new();
        let mut page = 1;
        loop {
            let response = self.customers_page(page, MAX_LIMIT).await?;
            let last = response.page >= response.pages || response.items.is_empty();
            all.extend(response.items);
            if last {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    /// Fetches a single customer by id.
    ///
    /// # Errors
    ///
    /// See [`Self::get_json`].
    pub async fn customer(&self, id: i64) -> Result<Customer> {
        self.get_json(
            &format!("customers/{id}"),
            &[],
            format!("GET /customers/{id}"),
        )
        .await
    }

    /// Fetches one page of documents matching the given raw query filters
    /// (e.g. `("type", "INVOICE,CREDIT")`, `("is_draft", "false")`).
    ///
    /// # Errors
    ///
    /// See [`Self::get_json`].
    pub async fn documents_page(
        &self,
        page: u32,
        limit: u32,
        filters: &[(&str, String)],
    ) -> Result<Page<Document>> {
        let mut query = vec![
            ("page", page.to_string()),
            ("limit", limit.min(MAX_LIMIT).to_string()),
        ];
        query.extend(filters.iter().cloned());
        self.get_json("documents", &query, format!("GET /documents?page={page}"))
            .await
    }

    /// Fetches **all** documents matching the given filters, across every page.
    ///
    /// # Errors
    ///
    /// Returns any error from the underlying page fetches.
    pub async fn all_documents(&self, filters: &[(&str, String)]) -> Result<Vec<Document>> {
        let mut all = Vec::new();
        let mut page = 1;
        loop {
            let response = self.documents_page(page, MAX_LIMIT, filters).await?;
            let last = response.page >= response.pages || response.items.is_empty();
            all.extend(response.items);
            if last {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    /// Fetches the raw PDF bytes for a document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] on transport failure and [`Error::Api`] on a
    /// non-success status.
    pub async fn document_pdf(&self, id: i64) -> Result<Vec<u8>> {
        let context = format!("GET /documents/{id}/pdf");
        let response = self
            .get(
                &format!("documents/{id}/pdf"),
                &[],
                "application/pdf",
                &context,
            )
            .await?;
        let bytes = response.bytes().await.context(HttpSnafu { context })?;
        Ok(bytes.to_vec())
    }
}

/// Extracts the `Retry-After` delay (delta-seconds form), with a fallback.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER, Duration::from_secs)
}

/// Extracts easybill's error `message` from an error body. Anything else in the
/// body is dropped so error logs never echo document or customer data.
fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| String::from("(no error message in response)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_message_keeps_only_the_message() {
        assert_eq!(
            error_message(r#"{"code":400,"message":"Invalid filter","arguments":{"name":"ACME"}}"#),
            "Invalid filter"
        );
        assert_eq!(
            error_message("<html>oops</html>"),
            "(no error message in response)"
        );
    }

    #[test]
    fn retry_after_parses_seconds_or_falls_back() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&headers), DEFAULT_RETRY_AFTER);
        headers.insert(RETRY_AFTER, "3".parse().expect("valid header value"));
        assert_eq!(retry_after(&headers), Duration::from_secs(3));
    }
}
