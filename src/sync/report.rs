//! The sync status snapshot for monitoring.
//!
//! After every real sync run (not dry-run or preview), the run is recorded in
//! state and the status of every known document and customer is recorded as
//! OTLP gauges (exported on exit, see [`crate::telemetry`]). The snapshot is
//! built from state, not from the run, so documents outside the run's date
//! window keep their last status.
//!
//! Each run reports all issues again, with `…_issue_reported_at…` set to the
//! run's time, which equals `easybill_xero_last_run_timestamp_seconds` of the
//! same run. A dashboard keeps only the issues reported by the latest run by
//! comparing the two, so fixed documents drop off without a Pushgateway.
//!
//! Labels carry ids, numbers and status categories only, never customer names
//! or error messages; those are in the logs, searchable by `document_id`.

use std::path::Path;

use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use snafu::ResultExt;

use crate::state::{self, CustomerStatus, DocumentStatus, RunRecord, State};
use crate::sync::{Result, StateSnafu};

/// One gauge of the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metric {
    LastRun,
    LastRunSuccess,
    LastSuccess,
    Documents,
    Customers,
    DocumentIssueSince,
    DocumentIssueAmount,
    DocumentIssueReportedAt,
    CustomerIssueSince,
    CustomerIssueReportedAt,
}

impl Metric {
    /// Every gauge, in export order.
    const ALL: [Self; 10] = [
        Self::LastRun,
        Self::LastRunSuccess,
        Self::LastSuccess,
        Self::Documents,
        Self::Customers,
        Self::DocumentIssueSince,
        Self::DocumentIssueAmount,
        Self::DocumentIssueReportedAt,
        Self::CustomerIssueSince,
        Self::CustomerIssueReportedAt,
    ];

    /// The metric name. No OTLP unit is set, so Prometheus keeps it as is.
    fn name(self) -> &'static str {
        match self {
            Self::LastRun => "easybill_xero_last_run_timestamp_seconds",
            Self::LastRunSuccess => "easybill_xero_last_run_success",
            Self::LastSuccess => "easybill_xero_last_success_timestamp_seconds",
            Self::Documents => "easybill_xero_documents",
            Self::Customers => "easybill_xero_customers",
            Self::DocumentIssueSince => "easybill_xero_document_issue_since_timestamp_seconds",
            Self::DocumentIssueAmount => "easybill_xero_document_issue_amount",
            Self::DocumentIssueReportedAt => {
                "easybill_xero_document_issue_reported_at_timestamp_seconds"
            }
            Self::CustomerIssueSince => "easybill_xero_customer_issue_since_timestamp_seconds",
            Self::CustomerIssueReportedAt => {
                "easybill_xero_customer_issue_reported_at_timestamp_seconds"
            }
        }
    }

    /// The metric description.
    fn description(self) -> &'static str {
        match self {
            Self::LastRun => "Unix time a sync command last finished.",
            Self::LastRunSuccess => {
                "Whether a sync command's last run finished without a run-level error (1) or not (0)."
            }
            Self::LastSuccess => "Unix time a sync command last finished without a run-level error.",
            Self::Documents => "Known easybill documents by sync status.",
            Self::Customers => "Known easybill customers by sync status.",
            Self::DocumentIssueSince => {
                "Unix time a document entered its current problem status."
            }
            Self::DocumentIssueAmount => "Gross amount of a problem document, in its currency.",
            Self::DocumentIssueReportedAt => {
                "Unix time of the run that reported this document problem."
            }
            Self::CustomerIssueSince => {
                "Unix time a customer entered its current problem status."
            }
            Self::CustomerIssueReportedAt => {
                "Unix time of the run that reported this customer problem."
            }
        }
    }
}

/// One value of the snapshot.
#[derive(Debug, Clone, PartialEq)]
struct Point {
    metric: Metric,
    value: f64,
    labels: Vec<(&'static str, String)>,
}

/// Records the finished run in state and the status snapshot as gauges.
///
/// `run_ok` is whether the run finished without a run-level error; failed
/// items don't count. Failures are logged, not returned: monitoring must not
/// fail the sync.
pub fn publish(state_override: Option<&Path>, command: &'static str, run_ok: bool) {
    match record_run(state_override, command, run_ok) {
        Ok(state) => {
            let points = snapshot(&state, command);
            tracing::info!(points = points.len(), "status snapshot recorded");
            export(&points);
        }
        Err(e) => tracing::warn!(
            error = %super::error_chain(&e),
            "could not record the run; no status snapshot"
        ),
    }
}

/// Records the run in state and returns the updated state.
fn record_run(state_override: Option<&Path>, command: &str, run_ok: bool) -> Result<State> {
    let path = state::resolve_path(state_override).context(StateSnafu)?;
    let now = Utc::now();
    State::update(&path, |s| {
        let last_success = s.runs.get(command).and_then(|r| r.last_success);
        s.runs.insert(
            command.to_owned(),
            RunRecord {
                last_run: now,
                last_run_ok: run_ok,
                last_success: if run_ok { Some(now) } else { last_success },
            },
        );
    })
    .context(StateSnafu)?;
    State::load(&path).context(StateSnafu)
}

/// Builds the snapshot from state. `command` is the run being reported; its
/// `last_run` is the `reported_at` of every issue.
fn snapshot(state: &State, command: &str) -> Vec<Point> {
    let mut points = Vec::new();
    let point = |metric, value, labels| Point {
        metric,
        value,
        labels,
    };

    for (name, run) in &state.runs {
        let labels = vec![("command", name.clone())];
        points.push(point(
            Metric::LastRun,
            seconds(run.last_run),
            labels.clone(),
        ));
        points.push(point(
            Metric::LastRunSuccess,
            f64::from(u8::from(run.last_run_ok)),
            labels.clone(),
        ));
        if let Some(success) = run.last_success {
            points.push(point(Metric::LastSuccess, seconds(success), labels));
        }
    }
    let Some(reported_at) = state.runs.get(command).map(|r| seconds(r.last_run)) else {
        return points;
    };

    // Every status, including zero counts, so the dashboard shows "0".
    for status in DocumentStatus::ALL {
        let count = state
            .document_status
            .values()
            .filter(|r| r.status == status)
            .count();
        points.push(point(
            Metric::Documents,
            count as f64,
            vec![("status", status.label().to_owned())],
        ));
    }
    for status in CustomerStatus::ALL {
        let count = state
            .customer_status
            .values()
            .filter(|r| r.status == status)
            .count();
        points.push(point(
            Metric::Customers,
            count as f64,
            vec![("status", status.label().to_owned())],
        ));
    }

    for (id, record) in &state.document_status {
        if !record.status.is_issue() {
            continue;
        }
        let labels = vec![
            ("document_id", id.clone()),
            ("number", record.number.clone().unwrap_or_default()),
            ("status", record.status.label().to_owned()),
            (
                "customer_id",
                record
                    .customer_id
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            ),
            ("currency", record.currency.clone().unwrap_or_default()),
        ];
        points.push(point(
            Metric::DocumentIssueSince,
            seconds(record.since),
            labels.clone(),
        ));
        points.push(point(
            Metric::DocumentIssueAmount,
            record.amount_cents as f64 / 100.0,
            labels.clone(),
        ));
        points.push(point(Metric::DocumentIssueReportedAt, reported_at, labels));
    }
    for (id, record) in &state.customer_status {
        if !record.status.is_issue() {
            continue;
        }
        let labels = vec![
            ("customer_id", id.clone()),
            ("status", record.status.label().to_owned()),
        ];
        points.push(point(
            Metric::CustomerIssueSince,
            seconds(record.since),
            labels.clone(),
        ));
        points.push(point(Metric::CustomerIssueReportedAt, reported_at, labels));
    }
    points
}

/// Records the points on the global meter provider (a no-op without OTLP).
fn export(points: &[Point]) {
    let meter = opentelemetry::global::meter("easybill-xero");
    for metric in Metric::ALL {
        let gauge = meter
            .f64_gauge(metric.name())
            .with_description(metric.description())
            .build();
        for p in points.iter().filter(|p| p.metric == metric) {
            let attributes: Vec<KeyValue> = p
                .labels
                .iter()
                .map(|(key, value)| KeyValue::new(*key, value.clone()))
                .collect();
            gauge.record(p.value, &attributes);
        }
    }
}

/// A timestamp as Unix seconds.
fn seconds(at: DateTime<Utc>) -> f64 {
    at.timestamp() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CustomerRecord, DocumentRecord};

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid timestamp")
    }

    fn document(status: DocumentStatus, amount_cents: i64) -> DocumentRecord {
        DocumentRecord {
            status,
            since: at("2026-11-08T14:00:00Z"),
            number: Some("RE-2026-0815".to_owned()),
            customer_id: Some(1000417),
            amount_cents,
            currency: Some("EUR".to_owned()),
        }
    }

    fn state() -> State {
        let mut state = State::default();
        state.runs.insert(
            "sync all".to_owned(),
            RunRecord {
                last_run: at("2026-11-11T08:00:00Z"),
                last_run_ok: true,
                last_success: Some(at("2026-11-11T08:00:00Z")),
            },
        );
        state
            .document_status
            .insert("1".to_owned(), document(DocumentStatus::Synced, 100));
        state.document_status.insert(
            "2".to_owned(),
            document(DocumentStatus::FailedConfig, 1_190_000),
        );
        state
            .document_status
            .insert("3".to_owned(), document(DocumentStatus::Skipped, 100));
        state.customer_status.insert(
            "1000417".to_owned(),
            CustomerRecord {
                status: CustomerStatus::Blocked,
                since: at("2026-11-01T00:00:00Z"),
            },
        );
        state
    }

    fn values(points: &[Point], metric: Metric) -> Vec<&Point> {
        points.iter().filter(|p| p.metric == metric).collect()
    }

    #[test]
    fn counts_cover_every_status() {
        let points = snapshot(&state(), "sync all");
        let documents = values(&points, Metric::Documents);
        assert_eq!(documents.len(), DocumentStatus::ALL.len());
        let failed_config = documents
            .iter()
            .find(|p| p.labels == vec![("status", "failed_config".to_owned())])
            .expect("failed_config count");
        assert_eq!(failed_config.value, 1.0);
        assert_eq!(
            values(&points, Metric::Customers).len(),
            CustomerStatus::ALL.len()
        );
    }

    #[test]
    fn only_issues_are_listed_with_the_run_time() {
        let points = snapshot(&state(), "sync all");
        let since = values(&points, Metric::DocumentIssueSince);
        assert_eq!(since.len(), 1, "synced and skipped are not issues");
        assert_eq!(since[0].labels[0], ("document_id", "2".to_owned()));
        assert_eq!(since[0].value, 1_794_146_400.0);

        let amount = values(&points, Metric::DocumentIssueAmount);
        assert_eq!(amount[0].value, 11_900.0);

        let reported = values(&points, Metric::DocumentIssueReportedAt);
        let last_run = values(&points, Metric::LastRun);
        assert_eq!(reported[0].value, last_run[0].value);

        assert_eq!(values(&points, Metric::CustomerIssueSince).len(), 1);
    }

    #[test]
    fn unknown_command_reports_runs_only() {
        let points = snapshot(&state(), "sync customers");
        assert!(points.iter().all(|p| matches!(
            p.metric,
            Metric::LastRun | Metric::LastRunSuccess | Metric::LastSuccess
        )));
    }
}
