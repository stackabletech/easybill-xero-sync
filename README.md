# easybill → Xero exporter

A CLI that pushes accounting data one-way from easybill into a Xero organisation:
easybill customers become Xero contacts, and easybill invoices and credit notes
become Xero invoices and credit notes (as drafts, with the easybill PDF
attached). It never writes to easybill.

**Status:** Xero authentication (OAuth for development, custom connection for
production), config validation (`xero check`), customer→contact sync, and
invoice/credit-note sync with total verification and PDF attachments. Cancelled
easybill documents are **not** reflected in Xero (out of scope pending an
accountant decision).

## Build

```bash
cargo build --release
```

Or build the container image (see [Container image](#container-image)):

```bash
docker buildx build -t easybill-xero:dev --load .
```

## Configuration

Secrets come from the environment, never from the state file:

| Variable              | Used by                                         |
|-----------------------|-------------------------------------------------|
| `XERO_CLIENT_ID`      | every `xero` and `sync` command                 |
| `XERO_CLIENT_SECRET`  | every `xero` and `sync` command                 |
| `EASYBILL_API_TOKEN`  | `sync`, `check --from`                          |

Global options can also come from the environment (handy in a container):

| Variable                     | Option          | Default                        |
|------------------------------|-----------------|--------------------------------|
| `XERO_AUTH`                  | `--xero-auth`   | `oauth`                        |
| `EASYBILL_XERO_CONFIG`       | `--config`      | `./config.toml`                |
| `EASYBILL_XERO_STATE_FILE`   | `--state-file`  | see [State file](#state-file)  |
| `EASYBILL_XERO_LOG_FORMAT`   | `--log-format`  | `text` (`json` for collectors) |

### Xero authentication

Two modes, selected with the global `--xero-auth` option or `XERO_AUTH`:

- `oauth` (default, development): a Xero **Web app** with the redirect URI
  `http://localhost:8080/callback` registered byte-for-byte (adjust the port with
  `--port`). Run `xero login` once; the single-use refresh token is stored in the
  state file and rotated on every run. Run something at least every 60 days or
  the token expires.
- `client-credentials` (production): a Xero **custom connection**, authorised
  for exactly one organisation. Each run requests a fresh access token with the
  client id/secret alone; nothing is stored, and `xero login`/`xero refresh`
  refuse to run.

Each mode needs its own Xero app, so its own `XERO_CLIENT_ID`/`XERO_CLIENT_SECRET`.
Scopes (the Web app requests them at login; for a custom connection select them
in the developer portal): `accounting.contacts`, `accounting.invoices`,
`accounting.attachments`, `accounting.settings.read`.

`xero login` prints an authorize URL, opens the browser, catches the redirect on
`localhost`, exchanges the code, picks the tenant (or lists them if ambiguous —
use `--tenant`), and verifies connectivity with `GET /Organisation`. Tokens are
never printed. The browser and the callback listener must share a host; on a
headless box, forward the port and open the URL locally:

```bash
ssh -L 8080:localhost:8080 user@host
```

### Tax / account config

`xero check` and the `sync` commands read a TOML file (`--config`, default
`./config.toml`) mapping easybill tax combinations and booking accounts onto the
org-specific Xero `TaxType` and `AccountCode` values. See
[`config.example.toml`](./config.example.toml); `xero check` validates the values
against the org.

Line items are booked to the easybill `booking_account` as the Xero
`AccountCode` directly (keep the Xero codes in easybill). `[accounts]` entries
override individual booking accounts; `default` covers lines without one.
Documents listed in `[sync] skip_documents` (by number or easybill id) are never
synced; they are logged, counted in the summary and left out of
`xero check --from`.

### State file

Default: `$XDG_STATE_HOME/easybill-xero/state.json`, falling back to
`~/.local/state/easybill-xero/state.json`; override with `--state-file`. It holds
the easybill→Xero document mapping and, with `oauth`, the login. Created with
mode `0600` and written atomically; every write re-reads the file and changes
only its own part. With `oauth`, the rotated refresh token is persisted
**before** any further API call. Mappings are per Xero organisation: start a
fresh state file when pointing the tool at another org.

## Commands

```bash
# Global options: --config <path>, --state-file <path>,
#                 --xero-auth oauth|client-credentials, --log-format text|json.

# oauth only: one-time browser login; stores the refresh token + tenant.
easybill-xero xero login [--port 8080] [--tenant <name|id>]

# oauth only: keep the (single-use) refresh token alive.
easybill-xero xero refresh

# Validate the config against the connected org.
easybill-xero xero check [--from <YYYY-MM-DD> [--to <YYYY-MM-DD>] [--verbose]]

# Upsert easybill customers as Xero contacts.
# --since: YYYY-MM-DD or YYYY-MM-DD HH:MM:SS, easybill local time.
easybill-xero sync customers [--since <DATETIME>] [--dry-run | --preview]

# Push easybill invoices + credit notes into Xero as DRAFT.
easybill-xero sync documents --from <YYYY-MM-DD> [--to <YYYY-MM-DD>] [--dry-run] [--no-pdf]

# Run customers, then documents, in one pass.
easybill-xero sync all --from <YYYY-MM-DD> [--to <YYYY-MM-DD>] [--since <DATETIME>] [--dry-run] [--no-pdf]
```

### `xero check`

Fails fast, listing every configured `TaxType` / `AccountCode` that is missing in
the org, not `ACTIVE`, or (for tax types) not usable on sales. It also lists
every easybill tax key (`null/20`, `null/0`, each non-taxable `vat_option`, the
`[tax]` keys) with its mapping. `--from` adds the keys actually used in easybill
(documents in the window plus all customers) and the booking accounts used on
document lines with the `AccountCode` each resolves to; an unmapped key or an
unusable account on a document fails the check. `--verbose` additionally lists
every document line (document number, line, VAT, tax key → `TaxType`, booking
account → `AccountCode`, status).

### `sync customers`

Fetches all easybill customers (or those modified since `--since`), maps each to
a Xero contact, and upserts it. Each customer is linked to an existing contact
at runtime (nothing about customers is stored): a contact whose `ContactNumber`
is the easybill id, or — for contacts that existed in Xero before the first
sync — a contact with no `ContactNumber` whose `AccountNumber` equals the
easybill customer number. A linked contact is updated in place and gets
`ContactNumber` set, so later runs link by that. Otherwise a new contact is
created.

Colliding display names get a ` (<number>)` suffix (Xero requires unique names).
A customer is **blocked** instead when its `AccountNumber` matches several
unlinked contacts, or when it would get a new contact whose name matches an
existing contact that isn't linked to any easybill customer (most likely the
same company, imported before the first sync — set that contact's
`AccountNumber` to link it). Blocked and failed customers are logged and the run
continues; the exit code is non-zero if any customer failed.

`--dry-run` prints the payloads without calling Xero, so it cannot show links to
existing contacts. `--preview` reads the existing Xero contacts and prints one
line per customer (update / create / blocked) without writing to Xero.

### `sync documents`

Fetches non-draft, non-cancelled `INVOICE`/`CREDIT` documents in the
`document_date` window (`--to` is optional), maps each to a Xero invoice
(`ACCREC`) or credit note (`ACCRECCREDIT`) with **DRAFT** status, creates or
updates it, and verifies the Xero `Total` against the easybill `amount` (a
mismatch marks that document failed). Only `POSITION` rows become line items.
Money is handled as decimals, never floats. Line discounts of any easybill type
are taken from easybill's own line total: invoices carry them as
`DiscountAmount`; credit notes (which Xero can't discount) get one unit at the
discounted line total.

Before creating, a document is looked up by number, so a lost state file never
causes duplicates; supplier bills with the same number are never matched. A
document that already exists in Xero and is no longer `DRAFT`/`SUBMITTED`
(approved, paid — e.g. entered before the first sync, or approved after an
earlier run) is left unchanged; only its total is verified. One deleted or
voided in Xero is left alone and reported.

After each document is pushed, its easybill PDF is attached to the Xero document
(idempotent; already-uploaded PDFs are skipped). `--no-pdf` skips attachments.
`--dry-run` prints the payloads without calling Xero and exits non-zero if any
document would fail to map.

### `sync all`

Runs the customer sync first (so invoices can resolve their contact), then the
document sync, with the same options. Failed or blocked customers don't stop the
document phase; the exit code is non-zero if either phase had failures.

### Logging

Diagnostics go to stderr (`--log-format json` for log collectors), results to
stdout. Error logs carry Xero's and easybill's error messages only, never the
response bodies (which echo invoice and contact data). HTTP requests time out,
and an expiring access token is renewed during long runs.

## Container image

The [`Dockerfile`](./Dockerfile) builds a fully static binary on the Red Hat
Hummingbird Rust builder and ships it on Hummingbird's `static` image: no shell,
no package manager, no C library. The image defaults to production settings,
each overridable via its environment variable:

| Variable                   | Image default            |
|----------------------------|--------------------------|
| `XERO_AUTH`                | `client-credentials`     |
| `EASYBILL_XERO_CONFIG`     | `/config/config.toml`    |
| `EASYBILL_XERO_STATE_FILE` | `/state/state.json`      |
| `EASYBILL_XERO_LOG_FORMAT` | `json`                   |

The entrypoint is the binary; pass the command as arguments. It runs as uid
65532, which needs write access to `/state`.

## CI

[`.github/workflows/build.yaml`](./.github/workflows/build.yaml) runs
`cargo fmt`, `clippy` and the tests, then builds the image:

| Trigger         | Published tags                                         |
|-----------------|--------------------------------------------------------|
| tag `vX.Y.Z`    | `X.Y.Z` (the tag must match the `Cargo.toml` version)  |
| push to `main`  | `0.0.0-dev` (floating) and `0.0.0-dev-<sha>`           |
| pull request    | none (checks and a test build only)                    |

Images are published for `linux/amd64` to
`oci.stackable.tech/<namespace>/easybill-xero`, where `<namespace>` is the Harbor
project set in the repository variable `REGISTRY_NAMESPACE`; the robot account's
secret is the repository secret `HARBOR_ROBOT_SECRET`. Published images are
signed with cosign (keyless, via the workflow's GitHub identity):

```bash
cosign verify \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github.com/<org>/<repo>/\.github/workflows/build\.yaml@' \
  oci.stackable.tech/<namespace>/easybill-xero:0.1.0
```

Run a published image locally:

```bash
docker run --rm \
  -v "$PWD/config.toml:/config/config.toml:ro" \
  -v "$PWD/state:/state" \
  -e XERO_CLIENT_ID -e XERO_CLIENT_SECRET -e EASYBILL_API_TOKEN \
  oci.stackable.tech/<namespace>/easybill-xero:0.1.0 xero check
```

In Kubernetes, run it as a CronJob that never overlaps itself, with the config
from a ConfigMap, the credentials from a Secret and the state on a persistent
volume:

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: easybill-xero
spec:
  schedule: "0 6 * * *"
  timeZone: Europe/Berlin
  concurrencyPolicy: Forbid
  jobTemplate:
    spec:
      backoffLimit: 0
      template:
        spec:
          restartPolicy: Never
          securityContext:
            fsGroup: 65532
          containers:
            - name: easybill-xero
              image: oci.stackable.tech/<namespace>/easybill-xero:0.1.0
              args: ["sync", "all", "--from", "2026-01-01"]
              envFrom:
                # XERO_CLIENT_ID, XERO_CLIENT_SECRET, EASYBILL_API_TOKEN
                - secretRef:
                    name: easybill-xero
              volumeMounts:
                - { name: config, mountPath: /config, readOnly: true }
                - { name: state, mountPath: /state }
          volumes:
            - name: config
              configMap:
                name: easybill-xero
            - name: state
              persistentVolumeClaim:
                claimName: easybill-xero-state
```

The image has no shell, so `--from` is a fixed date: move it forward from time
to time to keep the window (and the number of Xero calls per run) small.
