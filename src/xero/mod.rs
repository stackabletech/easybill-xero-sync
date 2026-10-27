//! Xero integration: authentication (OAuth2 login for development, client
//! credentials for a custom connection), a thin JSON API client, the pure
//! easybill → Xero mappings, and `xero check`.

pub mod attachment;
pub mod auth;
pub mod check;
pub mod client;
pub mod contact;
pub mod document;
pub mod models;
pub mod token;
