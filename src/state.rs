//! Persistent state file.
//!
//! A single JSON file holds the Xero login (refresh token + tenant) plus the
//! easybill→Xero id mapping for documents. Customers need no mapping: they are
//! linked to Xero contacts at runtime via `ContactNumber` (see
//! `sync::customers`). A `customers` section left by older versions is ignored
//! on load and dropped on the next save.
//!
//! Secrets other than the refresh token (client id/secret, easybill API key)
//! never live here — they come from environment variables.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use snafu::{IntoError, OptionExt, ResultExt, Snafu};

/// Errors arising from reading or writing the state file.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display(
        "Could not determine a state directory; pass --state-file or set HOME / XDG_STATE_HOME"
    ))]
    NoStateDir,

    #[snafu(display("Failed to create state directory {path:?}"))]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("Failed to read state file {path:?}"))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("Failed to parse state file {path:?}"))]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },

    #[snafu(display("Failed to serialise state"))]
    Serialise { source: serde_json::Error },

    #[snafu(display("Failed to write temporary state file {path:?}"))]
    WriteTemp {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("Failed to persist state file {path:?}"))]
    Persist {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display(
        "No Xero login found in state file {path:?}; run `easybill-xero xero login` first"
    ))]
    NotAuthenticated { path: PathBuf },
}

/// Convenience result alias for this module.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The full state document.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// The Xero login, if `xero login` has been run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xero: Option<XeroState>,

    /// easybill document id → Xero invoice/credit-note mapping.
    #[serde(default)]
    pub documents: BTreeMap<String, DocumentMapping>,
}

/// The kind of a synced document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// An accounts-receivable invoice (`ACCREC`).
    Invoice,
    /// An accounts-receivable credit note (`ACCRECCREDIT`).
    CreditNote,
}

/// State for a single synced document (invoice or credit note).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentMapping {
    /// The Xero invoice/credit-note GUID.
    pub xero_id: String,

    /// Whether this is an invoice or a credit note.
    pub kind: DocumentKind,

    /// Whether the easybill PDF has been attached in Xero.
    #[serde(default)]
    pub pdf_uploaded: bool,

    /// When this document was last synced (ISO 8601).
    pub synced_at: DateTime<Utc>,
}

/// The persisted Xero authentication state. `Debug` redacts the token.
#[derive(Clone, Serialize, Deserialize)]
pub struct XeroState {
    /// The current (single-use) refresh token.
    pub refresh_token: String,

    /// The selected tenant (organisation) id used for every API call.
    pub tenant_id: String,

    /// When this refresh token was obtained.
    pub obtained_at: DateTime<Utc>,
}

impl std::fmt::Debug for XeroState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XeroState")
            .field("refresh_token", &"<redacted>")
            .field("tenant_id", &self.tenant_id)
            .field("obtained_at", &self.obtained_at)
            .finish()
    }
}

impl State {
    /// Loads the state from `path`, returning [`State::default`] if the file
    /// does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Read`] if the file exists but cannot be read, and
    /// [`Error::Parse`] if its contents are not valid JSON.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context(ParseSnafu {
                path: path.to_path_buf(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ReadSnafu {
                path: path.to_path_buf(),
            }
            .into_error(source)),
        }
    }

    /// Persists the state to `path` atomically, creating parent directories as
    /// needed. The file is created with mode `0600`; the write goes to a
    /// temporary file that is then renamed over the target.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CreateDir`], [`Error::Serialise`], [`Error::WriteTemp`],
    /// or [`Error::Persist`] on the corresponding failure.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context(CreateDirSnafu {
                path: parent.to_path_buf(),
            })?;
        }

        let json = serde_json::to_vec_pretty(self).context(SerialiseSnafu)?;

        let tmp = path.with_extension("json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .context(WriteTempSnafu { path: tmp.clone() })?;
        file.write_all(&json)
            .context(WriteTempSnafu { path: tmp.clone() })?;
        file.sync_all()
            .context(WriteTempSnafu { path: tmp.clone() })?;

        std::fs::rename(&tmp, path).context(PersistSnafu {
            path: path.to_path_buf(),
        })?;
        Ok(())
    }

    /// Re-reads the state at `path`, applies `change`, and saves it.
    ///
    /// Used for every write during a run, so a change made by someone else in
    /// the meantime (notably a rotated refresh token) is never overwritten with
    /// a stale in-memory copy.
    ///
    /// # Errors
    ///
    /// See [`Self::load`] and [`Self::save`].
    pub fn update(path: impl AsRef<Path>, change: impl FnOnce(&mut Self)) -> Result<()> {
        let path = path.as_ref();
        let mut state = Self::load(path)?;
        change(&mut state);
        state.save(path)
    }

    /// Returns the stored Xero login or an error pointing the user at
    /// `xero login`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAuthenticated`] if no login has been stored.
    pub fn require_xero(&self, path: impl AsRef<Path>) -> Result<&XeroState> {
        self.xero.as_ref().context(NotAuthenticatedSnafu {
            path: path.as_ref().to_path_buf(),
        })
    }
}

/// Resolves the effective state-file path.
///
/// If `override_path` is given it is returned as-is. Otherwise the path is
/// `$XDG_STATE_HOME/easybill-xero/state.json`, falling back to
/// `~/.local/state/easybill-xero/state.json`.
///
/// # Errors
///
/// Returns [`Error::NoStateDir`] if neither `XDG_STATE_HOME` nor `HOME` is set.
pub fn resolve_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }

    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            let home = std::env::var_os("HOME").context(NoStateDirSnafu)?;
            PathBuf::from(home).join(".local").join("state")
        }
    };

    Ok(base.join("easybill-xero").join("state.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "easybill-xero-test-{}-{tag}/state.json",
            std::process::id()
        ))
    }

    #[test]
    fn load_missing_returns_default() {
        let path = temp_path("missing");
        let state = State::load(&path).expect("missing file loads as default");
        assert!(state.xero.is_none());
    }

    #[test]
    fn save_then_load_roundtrips_xero() {
        let path = temp_path("roundtrip");
        let state = State {
            xero: Some(XeroState {
                refresh_token: "rt-123".to_owned(),
                tenant_id: "tenant-abc".to_owned(),
                obtained_at: "2026-09-16T10:00:00Z".parse().expect("valid timestamp"),
            }),
            ..Default::default()
        };
        state.save(&path).expect("save succeeds");

        let loaded = State::load(&path).expect("load succeeds");
        let xero = loaded.xero.expect("xero present");
        assert_eq!(xero.refresh_token, "rt-123");
        assert_eq!(xero.tenant_id, "tenant-abc");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn update_keeps_changes_made_since_load() {
        let path = temp_path("update");
        let stale = State::load(&path).expect("load succeeds");
        State {
            xero: Some(XeroState {
                refresh_token: "rotated".to_owned(),
                tenant_id: "tenant".to_owned(),
                obtained_at: "2026-10-27T10:00:00Z".parse().expect("valid timestamp"),
            }),
            ..Default::default()
        }
        .save(&path)
        .expect("save succeeds");

        // A writer holding the stale copy only changes its own part.
        drop(stale);
        State::update(&path, |s| {
            s.documents.insert(
                "1".to_owned(),
                DocumentMapping {
                    xero_id: "x".to_owned(),
                    kind: DocumentKind::Invoice,
                    pdf_uploaded: false,
                    synced_at: Utc::now(),
                },
            );
        })
        .expect("update succeeds");

        let loaded = State::load(&path).expect("load succeeds");
        assert_eq!(loaded.xero.expect("xero kept").refresh_token, "rotated");
        assert!(loaded.documents.contains_key("1"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn require_xero_errors_when_absent() {
        let state = State::default();
        assert!(state.require_xero("/nonexistent/state.json").is_err());
    }
}
