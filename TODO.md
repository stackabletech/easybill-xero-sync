# TODO

Open items for `easybill-xero`. Remove an item when it's done; link the PR or
commit if it isn't obvious.

## Waiting on the accountants

- [ ] **Cancelled easybill documents.** Cancellations aren't reflected in Xero:
  the sync only fetches documents with `cancel_id = null`, and the original
  invoice probably stays in Xero as a draft. Decide how a cancellation should
  look in Xero (void the invoice, or push the cancellation as a credit note).
  See `easybill/src/models.rs` (`cancel_id`).
- [ ] **`DRAFT` vs `AUTHORISED`.** All documents are pushed as `DRAFT`. Switch
  to `AUTHORISED` once confirmed; `STATUS_DRAFT` in `src/xero/document.rs` is
  the single switch point.

## Monitoring

- [ ] **Deploy the collector.** Decide between a native sidecar in the CronJob
  (`initContainer` with `restartPolicy: Always`, Kubernetes ≥ 1.29) and a
  central collector, and write its config (OTLP in, Prometheus remote write
  and traces out). Needs: which backend (Prometheus version or Mimir, Tempo,
  Loki).
- [ ] **Try the dashboard in Grafana.** The queries in
  `grafana/easybill-xero-sync.json` were tested against Prometheus 3.16, but
  the panels themselves weren't rendered: check the table layout, the easybill
  link and the colour thresholds.
- [ ] **Match the dashboard to the schedule.** Set the `lookback` variable
  (default `25h`) and the "Last successful sync" thresholds (orange 2 h, red
  26 h) to the real schedule.
- [ ] **Link the tables to the logs.** Add a data link to the log backend,
  filtered by `document_id`, once it's known (Loki?).
- [ ] **Alert on a stale sync.** At least on "last successful sync older than
  the schedule allows"; the dashboard alone is only seen when someone looks.
- [ ] **Clean up statuses of vanished documents.** A failing document that is
  deleted in easybill is never fetched again and stays on the dashboard. Add a
  command to drop such status entries, or check them against easybill.
- [ ] **Recheck blocked customers on `--since` runs.** Unmodified customers
  aren't planned again, so a customer blocked by a Xero-side conflict stays
  `blocked` after the conflict is fixed in Xero.
- [ ] **Backfill statuses.** Statuses exist only for documents attempted since
  the monitoring was added. Run one `sync all` with an early `--from` after
  deploying.

## Possible further statuses

- [ ] **Cancelled in easybill, still in Xero.** Depends on the cancellation
  decision above; first check how easybill marks the original invoice.
- [ ] **Changed in easybill after approval in Xero.** An edit that keeps the
  total isn't noticed today. Needs a last-modified date for documents from the
  easybill API; not verified that one exists.

## Testing

- [ ] **`xero relink --apply` with open invoices.** Tested end to end in the
  dev org on a contact without invoices: clearing `ContactNumber` with an empty
  value, archiving and linking the new contact all worked. Not tried: archiving
  an old contact that still has open invoices, which Xero may refuse (the
  command then stops after clearing `ContactNumber`; see `apply_switch` in
  `src/xero/relink.rs`).
- [ ] **End-to-end run with real credentials.** The `document`, `customer` and
  HTTP spans and the recorded statuses have only been checked with a dry run
  and hand-made state files, not in a real sync.
- [ ] **Container build.** The OpenTelemetry dependencies haven't been built
  with the static Dockerfile yet (no C dependencies were added, so no problems
  are expected). The `Build/Publish Image` check on the first pull request
  will show whether it builds. Also run the image once against Xero and
  easybill: TLS roots now come from the image's CA bundle
  (`rustls-native-certs`), and that hasn't been tried in `hi/static` yet.
- [ ] **Dev shell with Nix and direnv.** The credential loading in
  `shell.nix` was only tested as plain bash; `nix-shell` and `direnv allow`
  with the committed `.envrc` haven't been run.

## CI

- [ ] **Make the checks required.** `prek`, `Run cargo-udeps` and
  `Build/Publish Image` only block merges once they are required in the
  branch protection (or a ruleset) for `main`. That's a repository setting,
  not part of the code.
- [ ] **Dev shell tooling.** `shell.nix` takes Rust from nixpkgs (not the
  1.99.1 in `rust-toolchain.toml`) and has no rustup, so the
  `cargo +nightly-…` rustfmt hook fails in the Nix shell. prek, cargo-deny,
  cargo-udeps and hadolint aren't in it either. Without them, `prek run`
  doesn't work locally and CI is the first place the lints run.
- [ ] **Keep the copied configs in sync.** `deny.toml`, `rustfmt.toml`,
  `.yamllint.yaml`, `.hadolint.yaml`, `.markdownlint.yaml` and the hook
  versions in `.pre-commit-config.yaml` are copies from
  stackabletech/operator-templating. Nothing updates them automatically;
  without that, new advisory ignores or licence decisions there won't reach
  this repo.
