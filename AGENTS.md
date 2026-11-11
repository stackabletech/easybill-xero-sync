# AGENTS.md

Guidance for AI coding agents (and humans) working on this repository.
`CLAUDE.md` points here. Open work is tracked in [`TODO.md`](TODO.md);
user-facing behaviour is documented in [`README.md`](README.md).

## What this is

`easybill-xero` is a one-way sync CLI: easybill customers become Xero contacts,
easybill invoices and credit notes become Xero invoices and credit notes (as
`DRAFT`, with the easybill PDF attached). It never writes to easybill. It runs
as a static binary in a minimal container, typically as a scheduled job.

```text
easybill/          reusable read-only easybill REST v1 client (own crate)
src/main.rs        CLI entry point, run span, final error handling
src/cli.rs         clap definitions
src/config.rs      TOML config (tax and account tables, skip list)
src/state.rs       state file: login, document mapping, sync statuses, runs
src/telemetry.rs   logging, OTLP traces and metrics setup
src/sync/          the sync commands (customers, documents, all) and the
                   monitoring snapshot (report.rs)
src/xero/          Xero auth, API client, mappings, `xero check`
grafana/           dashboard for the monitoring snapshot
```

## Before you finish a change

CI runs exactly these, all must pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Update `README.md` when behaviour, options, environment variables or metrics
change, and `TODO.md` when you finish or discover open work.

## Ground rules of the domain

- **Never write to easybill.** The easybill client is read-only by design.
- **Safe by default in Xero.** Documents go in as `DRAFT`; finalised (approved,
  paid) Xero documents are never modified, only their total is verified;
  deleted or voided ones are left alone.
- **Per-item failures don't abort a run.** They are logged, recorded as a
  status, counted, and turn the exit code non-zero (`Error::ItemsFailed`).
  Run-level failures (auth, API unreachable, state file) abort.
- **State writes go through `State::update`**, which re-reads the file and
  changes only its own part, because the OAuth refresh token may be rotated
  in the same file during a run. The rotated token is persisted before any
  further API call.
- **No business data in logs or metric labels.** Error logs carry only Xero's
  and easybill's error messages, never response bodies (they echo invoice
  and contact data). Metric labels carry ids, numbers, status categories and
  currency, never customer names or error text.
- `--dry-run` and `--preview` make no writes: no Xero writes, no state
  changes, no status snapshot.

## Observability design

- **Logs:** `tracing` to stderr, `--log-format text|json`. Results for humans go
  to stdout with `println!`. Every log line carries its spans: `run` (`run_id`,
  `command`) → `sync_customers` / `sync_documents` → `customer` / `document`
  (`document_id`, `number`, `xero_id`, `outcome`) → `xero_request` /
  `easybill_request`. Don't repeat a span's fields on events inside it.
- **Errors in spans:** record `otel.status_code = "error"` on the item span
  when an item fails, so the trace shows it.
- **Traces:** exported over OTLP/HTTP when `OTEL_EXPORTER_OTLP_ENDPOINT` (or the
  `…_TRACES_ENDPOINT`) is set. One run is one trace; `run_id` is the trace id.
- **Metrics:** the monitoring snapshot (`src/sync/report.rs`). Each real sync
  records per-document and per-customer statuses in the state file, then
  exports **all** recorded statuses as OTLP gauges once, on exit. Because a
  batch job's samples go stale between runs, every issue series carries a
  `…_reported_at…` gauge equal to the run's `last_run` timestamp; the
  dashboard keeps only issues of the latest run by comparing the two. Don't
  add per-process resource attributes (such as `service.instance.id`): runs
  must overwrite each other's series.
- **OTLP HTTP client:** the exporters use a small custom client
  (`OtlpClient` in `src/telemetry.rs`) on the app's own `reqwest` 0.12. The
  stock `opentelemetry-otlp` clients pull in `reqwest` 0.13 and `aws-lc-rs`
  (C code, harder for the static build). The batch processors export from
  their own threads, so the client spawns each request onto the Tokio runtime,
  and `Telemetry::shutdown` runs on a blocking thread.

When adding a status: extend `DocumentStatus` / `CustomerStatus` (including
`ALL`, `label` and `is_issue`), the classification in `src/sync/documents.rs`,
the README table and the dashboard legend panel.

## Rust conventions

This repo follows the Stackable Rust style guide; the points that matter
most, and where this repo differs:

**Errors**
- `snafu` only (no `thiserror`, no `anyhow`). One `Error` enum per module,
  explicit `.context(…Snafu)` at every call site; never
  `#[snafu(context(false))]`.
- Variant names are a verb or noun phrase: `ReadConfig`, `HttpClient`. No
  `FailedTo…` prefix, no `…Error` suffix.
- **Repo deviation:** display messages here are sentence case without a
  trailing full stop (`"Failed to load configuration"`), not the guide's
  lowercase. Stay consistent with the surrounding module.
- Use `crate::sync::error_chain` to log an error with its full source chain
  on one line.

**Panics**
- No `unwrap()`, also not in tests. `unwrap_or*` are fine.
- `expect("…")` only where failure is impossible; the message states the
  invariant (`must` in production code, `should` in tests).

**Types and signatures**
- No sentinel values (empty string, `-1`) for "nothing": use `Option`.
- Take owned values when you need ownership, instead of cloning a reference
  inside.
- `Option` variables carry no `maybe_` / `opt_` prefix. Use full words for
  names, except in closures.

**Formatting and docs**
- `format!`/`println!` with three or more substitutions use named arguments.
  Some older code still uses positional ones; fix them when you touch it.
- Every item gets a doc comment (summary sentence ending in a full stop);
  public fallible functions get an `# Errors` section. Module docs (`//!`)
  explain the flow and the why. Comments explain why, not what.
- British spelling, as in the existing code (`initialise`, `serialise`).
- Enum variants and struct fields with doc comments or attributes are
  separated by blank lines.

**Tests**
- Unit tests live in a `#[cfg(test)] mod tests` in the same file.
- Names describe what is verified, without a `test_` prefix.
- Prefer `assert_eq!` over `assert!`. For several inputs the guide asks for
  `rstest` with `#[case]` rows; this repo doesn't depend on `rstest` yet, so
  add it rather than writing one function per input.
- No network in tests. Keep mapping and classification logic pure so it can
  be tested without the APIs (see `snapshot` in `src/sync/report.rs`).

**Dependencies**
- Check the licence before adding a crate: it must be compatible with
  Apache-2.0, the project's licence.
- Keep the binary statically linkable: avoid crates with C dependencies, and
  check `cargo tree` for duplicate `reqwest`, `rustls` providers or
  `aws-lc-rs` after adding anything HTTP- or TLS-related.
- `Cargo.toml`: `[package]` first, `name`, `version`, other keys sorted,
  `description` last; dependencies grouped as they are now.
