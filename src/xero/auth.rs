//! Xero OAuth2 authorization-code flow and token refresh.
//!
//! Ported from the stdlib Python reference `xero_auth.py`; the flow is
//! deliberately unchanged. Tokens are never printed or logged.

use std::path::Path;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use chrono::Utc;
use reqwest::header::ACCEPT;
use serde::de::DeserializeOwned;
use snafu::{OptionExt, ResultExt, Snafu};
use url::Url;

use crate::state::{self, State, XeroState};
use crate::xero::models::{Connection, OrganisationsResponse, TokenResponse};

const AUTHORIZE_URL: &str = "https://login.xero.com/identity/connect/authorize";
const TOKEN_URL: &str = "https://identity.xero.com/connect/token";
const CONNECTIONS_URL: &str = "https://api.xero.com/connections";
const ORGANISATION_URL: &str = "https://api.xero.com/api.xro/2.0/Organisation";

// Granular Accounting API scopes. `accounting.invoices` (not the older broad
// `accounting.transactions`) covers invoices and credit notes; Xero apps created
// on/after 2026-03-02 can only request the granular scopes. Payments are out of
// scope, so `accounting.payments` is intentionally omitted.
const SCOPES: &str = "openid profile email offline_access \
    accounting.contacts accounting.invoices \
    accounting.attachments accounting.settings.read";

/// Errors arising from the Xero authentication flow.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("Missing required environment variable {var}"))]
    MissingEnv {
        var: &'static str,
        source: std::env::VarError,
    },

    #[snafu(display("Failed to bind the OAuth2 redirect listener on {addr}"))]
    OpenListener {
        addr: String,
        source: std::io::Error,
    },

    #[snafu(display("Failed to accept the OAuth2 redirect connection"))]
    Accept { source: std::io::Error },

    #[snafu(display("Failed to read the OAuth2 redirect request"))]
    ReadRequest { source: std::io::Error },

    #[snafu(display("The OAuth2 redirect request was malformed"))]
    MalformedRequest,

    #[snafu(display("The OAuth2 redirect did not include an authorization code"))]
    MissingCode,

    #[snafu(display("The OAuth2 redirect `state` did not match; possible CSRF, aborting"))]
    StateMismatch,

    #[snafu(display("HTTP request to Xero failed ({context})"))]
    Http {
        context: &'static str,
        source: reqwest::Error,
    },

    #[snafu(display("Xero returned HTTP {status} for {context}: {message}"))]
    Api {
        context: &'static str,
        status: u16,
        message: String,
    },

    #[snafu(display("Failed to parse the Xero response for {context}"))]
    Parse {
        context: &'static str,
        source: serde_json::Error,
    },

    #[snafu(display(
        "Refresh token rejected (invalid_grant). Run `easybill-xero xero login` again."
    ))]
    InvalidGrant,

    #[snafu(display("No Xero organisation is connected to these credentials"))]
    NoConnections,

    #[snafu(display("No connected Xero organisation matched {wanted:?}"))]
    TenantNotFound { wanted: String },

    #[snafu(display(
        "The login connected multiple organisations; pass --tenant. Options: {options}"
    ))]
    AmbiguousTenant { options: String },

    #[snafu(display("Failed to build the Xero authorize URL"))]
    BuildUrl { source: url::ParseError },

    #[snafu(display("State file error"))]
    State { source: state::Error },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Runs the one-time OAuth2 browser login and stores the resulting refresh
/// token and tenant in the state file.
///
/// # Errors
///
/// Returns a [`enum@Error`] variant on any failure of the flow: missing
/// credentials, listener/HTTP errors, `state` mismatch, tenant ambiguity, or
/// state persistence.
pub async fn login(state_override: Option<&Path>, port: u16, tenant: Option<&str>) -> Result<()> {
    let client_id = env_var("XERO_CLIENT_ID")?;
    let client_secret = env_var("XERO_CLIENT_SECRET")?;

    let redirect_uri = format!("http://localhost:{port}/callback");
    let csrf_state = random_state();
    let auth_url = build_authorize_url(&client_id, &redirect_uri, &csrf_state)?;

    println!(
        "Open this URL in your browser to authorise easybill-xero with Xero:\n\n  {auth_url}\n"
    );
    try_open_browser(&auth_url);

    let code = wait_for_callback(port, &csrf_state).await?;
    tracing::debug!("received authorization code, exchanging for tokens");

    let token: TokenResponse = request_token(
        &client_id,
        &client_secret,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
        ],
    )
    .await?;

    let connections = fetch_connections(&token.access_token).await?;
    let selected = select_tenant(&connections, tenant)?;

    let path = state::resolve_path(state_override).context(StateSnafu)?;
    let mut current = State::load(&path).context(StateSnafu)?;
    current.xero = Some(XeroState {
        refresh_token: token.refresh_token.clone(),
        tenant_id: selected.tenant_id.clone(),
        obtained_at: Utc::now(),
    });
    current.save(&path).context(StateSnafu)?;

    report_connection(&token.access_token, selected).await;
    println!("Login stored in {}", path.display());
    Ok(())
}

/// Refreshes the stored token, rotating the single-use refresh token and
/// persisting it *before* making any further API call.
///
/// # Errors
///
/// Returns [`Error::InvalidGrant`] if the refresh token is no longer valid
/// (the user must re-run `xero login`), or any other [`enum@Error`] variant on
/// I/O, HTTP or state failure.
pub async fn refresh(state_override: Option<&Path>) -> Result<()> {
    let client_id = env_var("XERO_CLIENT_ID")?;
    let client_secret = env_var("XERO_CLIENT_SECRET")?;

    let path = state::resolve_path(state_override).context(StateSnafu)?;
    let mut current = State::load(&path).context(StateSnafu)?;
    let existing = current.require_xero(&path).context(StateSnafu)?.clone();

    let token: TokenResponse = request_token(
        &client_id,
        &client_secret,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &existing.refresh_token),
        ],
    )
    .await?;

    // Persist the rotated refresh token before touching any API with the new
    // access token: if we crash after this point, the stored token is still valid.
    current.xero = Some(XeroState {
        refresh_token: token.refresh_token.clone(),
        tenant_id: existing.tenant_id.clone(),
        obtained_at: Utc::now(),
    });
    current.save(&path).context(StateSnafu)?;
    tracing::debug!("rotated refresh token persisted");

    match fetch_organisation(&token.access_token, &existing.tenant_id).await {
        Ok(name) => println!("Token refreshed; Xero org '{name}' reachable."),
        Err(e) => println!("Token refreshed and stored, but the connectivity check failed: {e}"),
    }
    Ok(())
}

/// Reads a required environment variable.
///
/// # Errors
///
/// Returns [`Error::MissingEnv`] if the variable is unset.
pub(crate) fn env_var(var: &'static str) -> Result<String> {
    std::env::var(var).context(MissingEnvSnafu { var })
}

/// Generates a random, URL-safe `state` value for CSRF protection.
fn random_state() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Builds the Xero authorize URL with the required query parameters.
///
/// Parameters are encoded per `application/x-www-form-urlencoded` (as OAuth2
/// specifies); Xero accepts the resulting `+`-for-space scope separators.
fn build_authorize_url(client_id: &str, redirect_uri: &str, csrf_state: &str) -> Result<String> {
    let mut url = Url::parse(AUTHORIZE_URL).context(BuildUrlSnafu)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("state", csrf_state);
    Ok(url.into())
}

/// Best-effort attempt to open the system browser; failures are ignored, since
/// headless hosts have no browser (the URL is always printed too).
fn try_open_browser(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Listens on `localhost:<port>` for exactly one `GET /callback`, verifies the
/// `state` parameter and returns the authorization `code`.
async fn wait_for_callback(port: u16, expected_state: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .context(OpenListenerSnafu { addr: addr.clone() })?;
    println!("Waiting for the Xero redirect on http://localhost:{port}/callback ...");

    let (mut stream, _) = listener.accept().await.context(AcceptSnafu)?;

    // The request line is the first thing on the wire, so a single read of a
    // reasonably sized buffer is enough to recover the path and query.
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.context(ReadRequestSnafu)?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let outcome = parse_callback(&request, expected_state);

    let body = match &outcome {
        Ok(_) => "OK, you can close this tab.",
        Err(_) => "Login failed. Check the terminal running easybill-xero.",
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Best effort: the user still gets the result on the terminal regardless.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;

    outcome
}

/// Parses the raw HTTP request line, validating `state` and extracting `code`.
fn parse_callback(request: &str, expected_state: &str) -> Result<String> {
    let request_line = request.lines().next().context(MalformedRequestSnafu)?;
    // e.g. "GET /callback?code=...&state=... HTTP/1.1"
    let target = request_line
        .split_whitespace()
        .nth(1)
        .context(MalformedRequestSnafu)?;
    let parsed = Url::parse(&format!("http://localhost{target}"))
        .ok()
        .context(MalformedRequestSnafu)?;

    let mut code = None;
    let mut got_state = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => got_state = Some(value.into_owned()),
            _ => {}
        }
    }

    if got_state.as_deref() != Some(expected_state) {
        return StateMismatchSnafu.fail();
    }
    code.context(MissingCodeSnafu)
}

/// Selects the tenant to use from the connected organisations.
pub(crate) fn select_tenant<'a>(
    connections: &'a [Connection],
    wanted: Option<&str>,
) -> Result<&'a Connection> {
    if connections.is_empty() {
        return NoConnectionsSnafu.fail();
    }

    if let Some(wanted) = wanted {
        return connections
            .iter()
            .find(|c| c.tenant_id == wanted || c.tenant_name.eq_ignore_ascii_case(wanted))
            .context(TenantNotFoundSnafu {
                wanted: wanted.to_owned(),
            });
    }

    match connections {
        [only] => Ok(only),
        many => {
            let options = many
                .iter()
                .map(|c| format!("{} ({})", c.tenant_name, c.tenant_id))
                .collect::<Vec<_>>()
                .join(", ");
            AmbiguousTenantSnafu { options }.fail()
        }
    }
}

/// Prints the connected organisation, verifying it via `GET /Organisation`.
/// A failed check is reported but not fatal — the login is already stored.
async fn report_connection(access_token: &str, selected: &Connection) {
    match fetch_organisation(access_token, &selected.tenant_id).await {
        Ok(name) => println!(
            "Connected to Xero org '{name}' (tenant {}).",
            selected.tenant_id
        ),
        Err(e) => println!(
            "Connected to '{}' (tenant {}), but the GET /Organisation check failed: {e}",
            selected.tenant_name, selected.tenant_id
        ),
    }
}

/// Builds an HTTP client configured with rustls TLS.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder().build().context(HttpSnafu {
        context: "building HTTP client",
    })
}

/// Posts to the token endpoint with HTTP Basic client authentication.
///
/// # Errors
///
/// Returns [`Error::InvalidGrant`] if the refresh token was rejected, or
/// [`Error::Http`], [`Error::Api`] or [`Error::Parse`] on transport or
/// deserialisation failure.
pub(crate) async fn request_token<T: DeserializeOwned>(
    client_id: &str,
    client_secret: &str,
    form: &[(&str, &str)],
) -> Result<T> {
    let creds = STANDARD.encode(format!("{client_id}:{client_secret}"));

    let response = http_client()?
        .post(TOKEN_URL)
        .header("Authorization", format!("Basic {creds}"))
        .form(form)
        .send()
        .await
        .context(HttpSnafu {
            context: "token endpoint",
        })?;

    let status = response.status();
    let body = response.text().await.context(HttpSnafu {
        context: "token endpoint body",
    })?;

    if status.is_success() {
        return serde_json::from_str(&body).context(ParseSnafu {
            context: "token endpoint",
        });
    }

    if body.contains("invalid_grant") {
        return InvalidGrantSnafu.fail();
    }
    ApiSnafu {
        context: "token endpoint",
        status: status.as_u16(),
        message: crate::xero::client::error_summary(&body),
    }
    .fail()
}

/// Fetches the organisations connected to the given access token.
pub(crate) async fn fetch_connections(access_token: &str) -> Result<Vec<Connection>> {
    let response = http_client()?
        .get(CONNECTIONS_URL)
        .bearer_auth(access_token)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .context(HttpSnafu {
            context: "connections",
        })?;

    let status = response.status();
    let body = response.text().await.context(HttpSnafu {
        context: "connections body",
    })?;

    if !status.is_success() {
        return ApiSnafu {
            context: "connections",
            status: status.as_u16(),
            message: crate::xero::client::error_summary(&body),
        }
        .fail();
    }
    serde_json::from_str(&body).context(ParseSnafu {
        context: "connections",
    })
}

/// Fetches the organisation name for a tenant via `GET /Organisation`.
async fn fetch_organisation(access_token: &str, tenant_id: &str) -> Result<String> {
    let response = http_client()?
        .get(ORGANISATION_URL)
        .bearer_auth(access_token)
        .header("xero-tenant-id", tenant_id)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .context(HttpSnafu {
            context: "organisation",
        })?;

    let status = response.status();
    let body = response.text().await.context(HttpSnafu {
        context: "organisation body",
    })?;

    if !status.is_success() {
        return ApiSnafu {
            context: "organisation",
            status: status.as_u16(),
            message: crate::xero::client::error_summary(&body),
        }
        .fail();
    }

    let parsed: OrganisationsResponse = serde_json::from_str(&body).context(ParseSnafu {
        context: "organisation",
    })?;
    parsed
        .organisations
        .into_iter()
        .next()
        .map(|o| o.name)
        .context(MalformedRequestSnafu)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, name: &str) -> Connection {
        // Deserialise from JSON so we build the struct through its real shape.
        serde_json::from_value(serde_json::json!({
            "tenantId": id,
            "tenantName": name,
        }))
        .expect("valid connection fixture")
    }

    #[test]
    fn authorize_url_has_required_params() {
        let url = build_authorize_url("cid", "http://localhost:8080/callback", "xyz")
            .expect("url builds");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("state=xyz"));
        // Granular scope for invoices + credit notes (not the broad transactions scope).
        assert!(url.contains("accounting.invoices"));
        assert!(url.contains("accounting.contacts"));
    }

    #[test]
    fn parse_callback_extracts_code() {
        let req = "GET /callback?code=abc123&state=tok HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(parse_callback(req, "tok").expect("code"), "abc123");
    }

    #[test]
    fn parse_callback_rejects_bad_state() {
        let req = "GET /callback?code=abc123&state=wrong HTTP/1.1\r\n\r\n";
        assert!(matches!(
            parse_callback(req, "tok"),
            Err(Error::StateMismatch)
        ));
    }

    #[test]
    fn parse_callback_requires_code() {
        let req = "GET /callback?state=tok HTTP/1.1\r\n\r\n";
        assert!(matches!(
            parse_callback(req, "tok"),
            Err(Error::MissingCode)
        ));
    }

    #[test]
    fn select_tenant_single_is_automatic() {
        let conns = vec![conn("t1", "Org One")];
        assert_eq!(
            select_tenant(&conns, None).expect("selected").tenant_id,
            "t1"
        );
    }

    #[test]
    fn select_tenant_multiple_needs_flag() {
        let conns = vec![conn("t1", "Org One"), conn("t2", "Org Two")];
        assert!(matches!(
            select_tenant(&conns, None),
            Err(Error::AmbiguousTenant { .. })
        ));
        assert_eq!(
            select_tenant(&conns, Some("Org Two"))
                .expect("selected")
                .tenant_id,
            "t2"
        );
        assert_eq!(
            select_tenant(&conns, Some("t1"))
                .expect("selected")
                .tenant_id,
            "t1"
        );
    }

    #[test]
    fn select_tenant_unknown_errors() {
        let conns = vec![conn("t1", "Org One")];
        assert!(matches!(
            select_tenant(&conns, Some("nope")),
            Err(Error::TenantNotFound { .. })
        ));
    }

    #[test]
    fn select_tenant_empty_errors() {
        assert!(matches!(
            select_tenant(&[], None),
            Err(Error::NoConnections)
        ));
    }
}
