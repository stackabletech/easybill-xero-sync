//! Access-token acquisition for API calls.
//!
//! Every command that talks to the Xero API needs a short-lived access token and
//! a tenant id. Two [`AuthMode`]s are supported:
//!
//! - [`AuthMode::Oauth`] (development): the [`TokenManager`] loads the refresh
//!   token stored by `xero login`, exchanges it for a fresh access token, and —
//!   critically — **persists the rotated refresh token before returning**, so a
//!   crash mid-run never strands the setup on an already-invalidated token
//!   (Xero refresh tokens are single-use).
//! - [`AuthMode::ClientCredentials`] (production): a Xero *custom connection*.
//!   The access token is requested with the client id/secret alone; nothing is
//!   read from or written to the state file, and the tenant is the single
//!   organisation the connection is authorised for.

use std::path::{Path, PathBuf};

use chrono::Utc;
use snafu::{ResultExt, Snafu};

use crate::state::{self, State, XeroState};
use crate::xero::auth;
use crate::xero::models::{ClientCredentialsToken, TokenResponse};

/// How the tool authenticates against Xero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AuthMode {
    /// OAuth2 authorization-code flow: interactive `xero login`, refresh token
    /// rotated in the state file. Meant for development.
    Oauth,

    /// OAuth2 `client_credentials` grant for a Xero custom connection: no login,
    /// no stored token. Meant for unattended production runs.
    ClientCredentials,
}

/// Errors arising while obtaining an access token.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    /// The state file could not be read or written.
    #[snafu(display("State file error"))]
    State { source: state::Error },

    /// The rotated refresh token could not be saved. The previous token is
    /// already invalid, so this needs a new login.
    #[snafu(display(
        "Xero issued a new refresh token but it could not be saved; the old one is no longer valid, so run `easybill-xero xero login` again"
    ))]
    PersistRotated { source: state::Error },

    /// The OAuth token endpoint failed (including `invalid_grant`).
    #[snafu(display("Failed to obtain an access token from Xero"))]
    Auth { source: auth::Error },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A usable API session: a fresh access token and the tenant to call.
///
/// `Debug` redacts the token.
#[derive(Clone)]
pub struct AccessContext {
    /// The short-lived bearer token (valid ~30 min).
    pub access_token: String,

    /// The tenant (organisation) id for the `xero-tenant-id` header.
    pub tenant_id: String,
}

/// Turns the configured credentials into a usable [`AccessContext`].
///
/// The state file path is resolved once and reused for the rotation write.
/// `Debug` redacts the client secret.
pub struct TokenManager {
    mode: AuthMode,
    client_id: String,
    client_secret: String,
    state_path: PathBuf,
    tenant_override: Option<String>,
}

impl std::fmt::Debug for AccessContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessContext")
            .field("access_token", &"<redacted>")
            .field("tenant_id", &self.tenant_id)
            .finish()
    }
}

impl std::fmt::Debug for TokenManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenManager")
            .field("mode", &self.mode)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("state_path", &self.state_path)
            .field("tenant_override", &self.tenant_override)
            .finish()
    }
}

impl TokenManager {
    /// Builds a token manager from the environment and the resolved state path.
    ///
    /// `tenant_override` (from `[xero].tenant_id` in config) takes precedence
    /// over the tenant stored in the state file (OAuth) or picks one of the
    /// connected organisations (client credentials) when present.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if `XERO_CLIENT_ID` / `XERO_CLIENT_SECRET` are
    /// unset, or [`Error::State`] if the state path cannot be resolved.
    pub fn from_env(
        mode: AuthMode,
        state_override: Option<&Path>,
        tenant_override: Option<String>,
    ) -> Result<Self> {
        let client_id = auth::env_var("XERO_CLIENT_ID").context(AuthSnafu)?;
        let client_secret = auth::env_var("XERO_CLIENT_SECRET").context(AuthSnafu)?;
        let state_path = state::resolve_path(state_override).context(StateSnafu)?;
        Ok(Self {
            mode,
            client_id,
            client_secret,
            state_path,
            tenant_override,
        })
    }

    /// Returns a fresh [`AccessContext`] for the configured [`AuthMode`].
    ///
    /// # Errors
    ///
    /// OAuth: returns [`Error::State`] if no login is stored or the rotated
    /// token cannot be persisted, and [`Error::Auth`] (wrapping
    /// [`auth::Error::InvalidGrant`]) if the refresh token was rejected.
    /// Client credentials: returns [`Error::Auth`] if the token request fails
    /// or no (or no matching) organisation is connected.
    pub async fn access(&self) -> Result<AccessContext> {
        match self.mode {
            AuthMode::Oauth => self.refresh_access().await,
            AuthMode::ClientCredentials => self.client_credentials_access().await,
        }
    }

    /// Requests an access token via the `client_credentials` grant and resolves
    /// the custom connection's organisation via `GET /connections`.
    async fn client_credentials_access(&self) -> Result<AccessContext> {
        let token: ClientCredentialsToken = auth::request_token(
            &self.client_id,
            &self.client_secret,
            &[("grant_type", "client_credentials")],
        )
        .await
        .context(AuthSnafu)?;

        let connections = auth::fetch_connections(&token.access_token)
            .await
            .context(AuthSnafu)?;
        let tenant_id = auth::select_tenant(&connections, self.tenant_override.as_deref())
            .context(AuthSnafu)?
            .tenant_id
            .clone();

        Ok(AccessContext {
            access_token: token.access_token,
            tenant_id,
        })
    }

    /// Loads the stored refresh token, refreshes it, persists the rotated token,
    /// and returns a fresh [`AccessContext`].
    async fn refresh_access(&self) -> Result<AccessContext> {
        let current = State::load(&self.state_path).context(StateSnafu)?;
        let existing = current
            .require_xero(&self.state_path)
            .context(StateSnafu)?
            .clone();

        let token: TokenResponse = auth::request_token(
            &self.client_id,
            &self.client_secret,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &existing.refresh_token),
            ],
        )
        .await
        .context(AuthSnafu)?;

        // Persist the rotated (single-use) refresh token *before* returning the
        // access token to any caller that will hit the API with it.
        State::update(&self.state_path, |s| {
            s.xero = Some(XeroState {
                refresh_token: token.refresh_token.clone(),
                tenant_id: existing.tenant_id.clone(),
                obtained_at: Utc::now(),
            });
        })
        .context(PersistRotatedSnafu)?;
        tracing::debug!("rotated refresh token persisted");

        let tenant_id = self.tenant_override.clone().unwrap_or(existing.tenant_id);

        Ok(AccessContext {
            access_token: token.access_token,
            tenant_id,
        })
    }
}
