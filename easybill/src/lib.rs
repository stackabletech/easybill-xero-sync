//! Reusable read-only client for the easybill REST v1 API.
//!
//! A small, dependency-light client covering the paginated `GET` endpoints an
//! integration typically reads: customers, documents (invoices/credit notes) and
//! document PDFs. It is deliberately read-only — nothing here writes to easybill.
//! Auth is a static bearer token (see [`EasybillClient::from_env`]).
//!
//! The crate is independent of any particular downstream (e.g. a Xero exporter):
//! it exposes typed models and a thin HTTP client, and leaves mapping and
//! orchestration to the caller.

pub mod client;
pub mod models;

pub use client::{EasybillClient, Error, Result};
pub use models::{Customer, Document, DocumentItem, Page};
