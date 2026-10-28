# Local store API (offline till, DigitGlance Trade)

The native local store behind the offline POS till: an encrypted SQLite
database inside the Trade desktop app, exposed to the till as Tauri commands.
This file is the contract the TypeScript till and sync engine code against.
Command names, argument names and JSON field names here are exactly what the
Rust side accepts and returns (`src-tauri/src/store/commands.rs`); a test
(`command_lists_agree_with_build_rs_capability_lib_and_docs`) fails if a
command is missing from this file, `build.rs`, the capability file or the
handler list.

- [Conventions](#conventions)
- [Security model](#security-model)
- [Durability](#durability)
- [Commands](#commands)
- [Lifecycle on the till](#lifecycle-on-the-till)
- [The bundled till](#the-bundled-till)
- [Tests and timings](#tests-and-timings)
- [Known gaps](#known-gaps)

## Conventions

- Every command is async and is called with `invoke(name, args)` from
  `@tauri-apps/api/core`. Command names are snake_case; argument and result
  fields are camelCase. Tauri maps a top-level argument `ownerId` to the Rust
  parameter `owner_id`, so the argument object keys below are exactly what to pass.
- Every command resolves with its result or rejects with a `StoreError`:

  ```json
  { "code": "storage_critical", "message": "Storage is critical (98.5 MB free, 0.1%) and there is nothing safe to clean. ..." }
  ```

  | code | meaning |
  |---|---|
  | `not_open` | no store is open: call `store_open` (or `device_identity_get`) first |
  | `storage_critical` | the disk is at the critical threshold and nothing could be cleaned; **nothing was written** |
  | `not_found` | the record does not exist |
  | `invalid` | the input was refused (shape, ids, state such as a closed shift) |
  | `crypto` | wrong or missing DB key, credential store unavailable, file of another tenant |
  | `io` | file system or database failure |
  | `busy` | the store was busy for 10 s; retry |

- Times the store generates are RFC 3339 UTC with milliseconds
  (`2026-10-27T09:00:00.000Z`). `recordedAt` from the till must be RFC 3339.
- Lookups that can miss (`catalog_by_barcode`, `customer_get`, `settings_get`,
  `device_identity_get`, `shift_current`) resolve `null`; `sale_get` rejects
  with `not_found`.
- The commands exist only in the DigitGlance Trade shell (and the base dev
  config). The Books and School shells reject every call with `invalid`.

## Security model

- **Encryption at rest:** SQLCipher (whole file, AES-256, raw 256-bit key, no
  passphrase derivation). The key is 32 bytes from the OS CSPRNG, generated on
  the first `store_open` for an (ownerId, deviceId) and stored only in the
  Windows Credential Manager (`keyring` crate, current Windows user):
  service `DigitGlance Trade`, account `till-db:<ownerId>:<deviceId>`. It is
  never written to a file. The WAL file is encrypted too. A copied file is
  unreadable without that key; opening with the wrong key fails with `crypto`.
- **Device token and secret** (from pairing) are in the Credential Manager as
  `device-token:<ownerId>:<deviceId>` and `device-secret:<ownerId>:<deviceId>`.
  The DB holds only the non-secret identity fields.
- **One file per tenant and device:** `<LocalAppData>/com.digitglance.trade/offline-pos/till_<ownerId>_<deviceId>.db`
  (`app_local_data_dir()/offline-pos`). Each file has its own key, and the
  file records its owner and device: opening it under other ids fails with
  `crypto`. A device signed in to business A can never open business B's file.
- **Missing key is never papered over:** if the file exists but its key is gone
  from the Credential Manager, `store_open` fails with `crypto` and does NOT
  create a new key or a new file (that would hide the unsent sales).
- **Unencrypted builds refuse:** the `sqlcipher` Cargo feature is on by default.
  A build without it (`--no-default-features`, only for logic tests on a
  machine without native Perl) refuses `store_open` at runtime outside tests.
- **IPC scope:** the commands are declared in `src-tauri/build.rs` (app
  manifest), so each needs an `allow-<command>` permission. Only
  `src-tauri/capabilities/local-store.json` grants them, to the `till` window
  only, with **no `remote` section**: Tauri therefore allows them only to the
  bundled till's own pages (`tauri://localhost` / `http://tauri.localhost`) in
  that window. The web back office opens in a separate `backoffice` window
  (core permissions only), so `https://digitglance.com` can never call them.
  See [The bundled till](#the-bundled-till).
- **PIN hashes** (bcrypt, from the snapshot) are kept only in the `staff`
  table; `staff_list` never returns them and the JSON blob of each member has
  `pinHash` stripped. Verification happens in Rust (`pin_verify`).
- **What it does not protect against:** a signed-in Windows user on that
  machine (the key is readable by that user), memory dumps, a compromised
  bundled till page. Revoke a lost till and reset its branch's PINs.

## Durability

- WAL journal, `synchronous = FULL`, `foreign_keys = ON`,
  `auto_vacuum = INCREMENTAL`, busy timeout 5 s.
- One connection behind one mutex (single writer); the single-instance plugin
  guarantees one app process. Commands run on Tauri's blocking pool.
- `sale_commit`, `shift_open`, `shift_close`, `catalog_apply_snapshot`,
  `catalog_apply_changes`, `queue_mark` and `cleanup_run` are each ONE SQLite
  transaction. `sale_commit` uses `BEGIN IMMEDIATE`. The catalogue cursor is
  stored inside the same transaction as the page it belongs to.
- Crash safety is tested by aborting a child process at named points inside
  `sale_commit` (after the receipt number, after the lines, after the outbox
  row, just before COMMIT, and just after COMMIT): the reopened database holds
  either the whole sale or nothing, and `PRAGMA integrity_check` is `ok`.
- Print the receipt only after `sale_commit` resolves. Reprint with `sale_get`.
- Schema versions are migrated on `store_open`, each in its own transaction;
  a file from a newer app is refused untouched. Version 1 is the initial
  schema. Version 2 (`returns`) adds returns: the outbox is rebuilt so its
  `kind` may be `return` (every row, its `seq` and the AUTOINCREMENT counter
  are kept, so pending items upload unchanged and in order), `sales` gains
  `original_transaction_id` (indexed), and `sale_lines` gains
  `original_line_no` and `restock` (default 0).

## Commands

### `store_open`

Opens (creating and migrating if needed) the store for this tenant and device.
Opening another (ownerId, deviceId) closes the current store first.
Ids: 1 to 128 characters of letters, digits and `-` (UUIDs).

```js
await invoke('store_open', { ownerId: '0b7e5c1a-...', deviceId: '6f1c...' })
// -> { "opened": true, "schemaVersion": 2 }
```

### `store_close`

Checkpoints the WAL and closes. `invoke('store_close')` -> `null`.

### `device_identity_get`

The paired identity, or `null` when the device is not paired (or its token is
gone from the Credential Manager). **Call it first on launch:** when no store
is open it reopens the last paired store (named by `offline-pos/active-device.json`,
which holds the two ids and nothing secret), so a `null` means "show pairing".

```json
{
  "ownerId": "0b7e5c1a-...", "deviceId": "6f1c...", "deviceCode": "T01",
  "branchId": "b1...", "terminalId": "t1...",
  "deviceToken": "<device JWT>", "apiBase": "https://api.digitglance.com"
}
```

### `device_identity_save`

After `POST /pos/devices/pair` succeeds and `store_open` was called with the
same ids. Token and secret go to the Credential Manager, the rest to the DB.
`deviceCode` 1 to 16 letters/digits; `deviceSecret` 32 to 256 characters;
`apiBase` https (or `http://localhost` / `http://127.0.0.1` for development).

```js
await invoke('device_identity_save', {
  ownerId, deviceId, deviceCode: 'T01', branchId, terminalId,
  deviceToken: token, apiBase: 'https://api.digitglance.com', deviceSecret: secret,
})
// -> null
```

### `device_identity_clear`

Forgets the pairing (token, secret, identity row, pointer file). The DB and
every unsent sale in it are **kept**; `export_pending` still works.

### `device_secret_get`

*(Addition to the original contract.)* The device secret for the
`X-Device-Secret` header, or `null`. Kept out of `device_identity_get` so it is
fetched only where a request is built.

### `store_status`

`pendingCount` counts everything not yet on the server (pending, and transient
failures waiting to retry); `failedCount` counts permanent failures.
`lastSyncResult` is `ok` | `partial` | `failed`, from the last `queue_mark`.

```json
{
  "pendingCount": 3, "failedCount": 0, "syncedCount": 1240,
  "oldestPendingAt": "2026-10-27T08:01:12.034Z",
  "lastSyncAt": "2026-10-27T09:05:58.120Z", "lastSyncResult": "ok",
  "dbSizeMb": 18.4,
  "storage": { "freeMb": 18250.5, "totalMb": 237000.0, "freePct": 7.7, "state": "warning" },
  "retentionDays": 30
}
```

`storage` is measured on the volume holding the DB (Windows
`GetDiskFreeSpaceExW`, via `fs4`). `state` is `critical` at or below
`storageCriticalPct` percent free OR `storageCriticalMb`, `warning` at or below
the warning pair, else `ok`. Thresholds come from the `offlineSettings` setting
(`storageWarningPct`, `storageWarningMb`, `storageCriticalPct`,
`storageCriticalMb`); defaults 15% / 500 MB and 5% / 150 MB.

### `storage_check`

The `storage` object of `store_status`, plus whether a cleanup ran. At or
below warning it runs the retention cleanup and measures again. A change of
state writes one audit row (`storage_warning`, `storage_critical`,
`storage_ok`), not one per tick. Call it on a timer (every few minutes).

```json
{ "freeMb": 480.2, "totalMb": 237000.0, "freePct": 0.2, "state": "warning",
  "cleanupRemoved": 12, "cleanupRan": true }
```

### `settings_get` / `settings_set`

Free-form offline settings as JSON, keyed by 1 to 64 characters of letters,
digits, `_`, `.`, `-`. The snapshot fills `settings` (VAT rate, currency,
receipt footer), `offlineSettings` (retention, thresholds, tenders),
`approvalSettings`, `business`, `branch`, `terminal`, `device`, `loyalty`
(the business's loyalty programme settings). The store itself
reads `offlineSettings.retentionDays` and the four storage thresholds.

```js
await invoke('settings_set', { key: 'receiptFooter', valueJson: 'Thank you' })
await invoke('settings_get', { key: 'settings' })   // -> { "vatRate": 7.5, ... } or null
```

### `catalog_apply_snapshot`

Replaces products, categories, branch stock, customers and staff in ONE
transaction, stores the settings blocks listed above and the `cursor`. Takes
the `GET /pos/sync/snapshot` body as is. The arrays `products`, `categories`,
`stock`, `customers` and `staff` must all be present (an empty array is fine);
a missing one, or a row without an id, rejects with `invalid` and changes
nothing. Stock rows for another branch than the paired one are ignored.

```js
await invoke('catalog_apply_snapshot', { snapshot })
// -> { "products": 20000, "categories": 40, "stock": 20000, "customers": 812, "staff": 9, "cursor": 48211 }
```

### `catalog_apply_changes`

One `GET /pos/sync/changes` page, in ONE transaction with its cursor. Entities:
`product`, `price` (`sellingPrice` only), `category`, `stock`, `customer`,
`user` (staff; a delete matches the membership id or the user id), and the
settings entities `settings`, `vat`, `approval_settings` -> `approvalSettings`,
`offline_settings` -> `offlineSettings`, `loyalty_settings` -> `loyalty`.
Unknown entities are skipped and counted in `skipped`, never fatal (the rest
of the page and its cursor still apply).

```js
await invoke('catalog_apply_changes', { rows: page.changes, cursor: page.cursor })
// -> { "applied": 4, "skipped": 0, "cursor": 48215 }
```

**Stock rule.** `stock.onHand` is only ever the server's figure. Each sale line
writes a `local_stock_delta` row (`-qty`). Displayed quantity =
server on-hand + the deltas not yet reconciled. When the server accepts a sale
(`queue_mark`) its deltas are marked synced but still count; the next server
stock row for that product (delta page or snapshot) already includes the sale,
so the synced deltas are then reconciled and drop out. Pending deltas always
count, so an overwrite by the server never "gives back" a sale still in the
queue. Offline sales may take the display negative (D7).

### `catalog_get_cursor`

The stored cursor (whatever the server sent: number or string), or `null`.

### `catalog_search`

Case-insensitive prefix match on each word of the name, on the SKU and on the
barcode (indexed term table). Extra words narrow the result (each must occur
in the name, SKU or barcode). Exact barcode, then exact SKU matches sort
first, then by name. Active products only. `limit` default 30, at most 200.
An empty `q` lists the category (or everything) by name.

```js
await invoke('catalog_search', { q: 'gold rice', categoryId: null, limit: 30 })
```

Each product is the server's product object with `sellingPrice` kept current
and a `stock` block:

```json
{
  "id": "p1", "sku": "CK50", "barcode": "5449000000996", "name": "Coke 50cl",
  "categoryId": "cat-drinks", "sellingPrice": 300, "isActive": true, "vatClass": "standard",
  "stock": { "onHand": 7, "serverOnHand": 10, "localDelta": -3, "reserved": 0,
             "reorderLevel": null, "tracked": true }
}
```

### `catalog_by_barcode`

Exact barcode (case-insensitive), falling back to an exact SKU; `null` if none.
Includes inactive products (so the till can say "inactive" instead of "unknown").
`invoke('catalog_by_barcode', { code: '5449000000996' })`.

### `catalog_get_products`

Products by id in the order asked, unknown ids skipped, at most 1000.
`invoke('catalog_get_products', { ids: ['p1', 'p2'] })`.

### `categories_list`

All categories ordered by name (the server's category objects).

### `customers_search`

Name prefix, surname (word) prefix, phone prefix, or exact loyalty number.
Active customers only. `limit` default 20, at most 100.
`invoke('customers_search', { q: 'okaf', limit: 20 })` -> customer objects as in the snapshot.

### `customer_get`

`invoke('customer_get', { id })` -> the customer object or `null`.

### `staff_list`

Cached staff of this branch, **without** PIN hashes:

```json
[{ "id": "8a2e...", "membershipId": "c41d...", "name": "Ikeja Cashier", "role": "cashier",
   "permissions": ["pos.view", "pos.create"], "allowedTerminalIds": null, "active": true, "hasPin": true }]
```

### `pin_verify`

bcrypt check in Rust against the cached hash (D5 sign-in, D6 approvals).
3 wrong PINs in a row lock that user on this device for 15 minutes (stored in
the DB, so a restart does not reset it); a correct PIN resets the count. A
member whose `allowedTerminalIds` does not include this terminal is refused.

```js
await invoke('pin_verify', { userId: '8a2e...', pin: '1234' })
```

```json
{ "ok": true, "permissions": ["pos.view", "pos.create"], "name": "Ikeja Cashier", "role": "cashier" }
{ "ok": false, "permissions": [], "reason": "wrong_pin", "attemptsLeft": 2 }
{ "ok": false, "permissions": [], "reason": "locked", "lockedUntil": "2026-10-27T09:20:00.000Z", "attemptsLeft": 0 }
```

`reason`: `wrong_pin` | `locked` | `unknown_user` | `inactive` | `no_pin` |
`terminal_not_allowed`. The third wrong PIN answers `locked`. A lockout writes
a `pin_lockout` audit row. Writing `offline_sign_in` / approval audit rows is
the till's job (`audit_append`), since only it knows why the PIN was asked.

### `sale_commit`

Records a sale in ONE transaction: receipt number
`<deviceCode>-<6-digit local seq>` from the `receipt` sequence, the sale, its
lines and payments, one `local_stock_delta` per line with a `productId`, the
outbox row (status `pending`) and a `sale_recorded` audit row. Storage is
checked first: at critical it runs the retention cleanup, and if nothing could
be cleaned it rejects with `storage_critical` and **writes nothing** (not even
an audit row, and the receipt number is not used up).

Idempotent: committing a `transactionId` that is already stored returns its
original `{ receiptNumber, localSeq }` and writes nothing. A sale into a shift
that is closed on this device is refused; a `shiftLocalId` the device does not
know is accepted (a shift opened before the store existed).

```js
await invoke('sale_commit', { sale: {
  transactionId: '0192f0b6-6f6e-7c3a-9a51-3a0f1e2d4c5b',   // UUID v7 from the till
  kind: 'sale',
  recordedAt: '2026-10-27T09:00:00.000Z',
  cashierId: '8a2e...',
  approvals: [{ kind: 'discount', approverId: '91bc...', deviceTime: '2026-10-27T08:59:40.000Z' }],
  branchId: 'b1...', terminalId: 't1...', shiftLocalId: 'shift-0192f0a1',
  customerId: null,
  lines: [{ productId: 'p1', productSnapshot: { name: 'Coke 50cl', sku: 'CK50' },
            qty: 2, unitPrice: 300, discount: 0, vatRate: 7.5, vatAmount: 45, lineTotal: 645 }],
  payments: [{ method: 'cash', amount: 645, tendered: 1000, change: 355, confirmationStatus: 'confirmed' },
             { method: 'card', amount: 0, confirmationStatus: 'pending', reference: 'APPR 123456' }],
  totals: { subtotal: 600, discount: 0, vat: 45, total: 645 },
  loyalty: { earned: 6, redeemed: 0 },
  notes: null,
} })
// -> { "receiptNumber": "T01-000123", "localSeq": 123 }
```

Validation (`invalid`): `transactionId` a UUID; `kind` `'sale'` or `'return'`; `recordedAt`
RFC 3339; `cashierId`, `branchId`, `terminalId`, `shiftLocalId` non-empty; at
least one line; `qty > 0`; every number finite; `productSnapshot` and `totals`
objects; `method` and `confirmationStatus` 2 to 32 lower-case letters or `_`.
Suggested values: `method` `cash` | `card` | `transfer` | `store_credit` |
`loyalty` | `on_account`; `confirmationStatus` `confirmed` | `pending` |
`unconfirmed`. The store does not check the arithmetic: the till computes the
totals and the server re-checks them.

**Upload payload.** The outbox payload is the sale object exactly as passed,
plus `receiptNumber`, `localSeq` and `deviceCode`. That is what
`queue_next_batch` returns and what should be uploaded.

**Returns (refunds).** `sale_commit` with `kind: 'return'` records a refund of
a sale made on this till. Same shape and rules as a sale (validation, one
transaction, idempotency by `transactionId`, closed-shift refusal, critical
storage, failure points), and the same `receipt` sequence: returns and sales
share one numbering, so a return gets the next `<deviceCode>-<6 digits>`.
Extra fields:

- `originalTransactionId` (required, a UUID): the local sale being returned.
  It must be a sale (not a return) stored on this device, else `not_found`
  "The original sale is not on this till." (for example already cleaned by
  retention). If that sale's queue item is `failed` (the server refused it),
  `invalid` "The server refused the original sale, so it cannot be returned
  on this till."
- each line: `originalLineNo` (required, integer 1 or more: the 1-based line
  of the original sale) and `restock` (boolean, default `false`). A line not
  on the original is `invalid` "Line {i}: that line is not on the original
  sale.". The quantity may not exceed what was sold on that line less what
  earlier returns of the same sale already refunded on it (lines of one
  return naming the same original line count together; tolerance 1e-9), else
  `invalid` "Line {i}: only {remaining} of {sold} can still be returned."

The return is stored as `sales.kind = 'return'` with
`original_transaction_id`, its lines with `original_line_no` and `restock`,
an outbox item of kind `return` (payload = the object passed plus
`receiptNumber`, `localSeq`, `deviceCode`, unknown keys kept) and a
`return_recorded` audit row (`transactionId`, `receiptNumber`,
`originalTransactionId`, `total`, `offline: true`). Stock: a restocked line
with a `productId` writes a `+qty` local stock delta (the display goes back
up); a line not restocked (damaged, written off) writes none. Accepting the
return in `queue_mark` marks its deltas synced, as for a sale.

```js
await invoke('sale_commit', { sale: {
  transactionId: '0192f0c4-...', kind: 'return', originalTransactionId: '0192f0b6-6f6e-7c3a-9a51-3a0f1e2d4c5b',
  recordedAt: '2026-10-27T11:00:00.000Z', cashierId: '8a2e...', approvals: [...],
  branchId: 'b1...', terminalId: 't1...', shiftLocalId: 'shift-0192f0a1', customerId: null,
  lines: [{ originalLineNo: 1, restock: true, productId: 'p1', productSnapshot: { name: 'Coke 50cl', sku: 'CK50' },
            qty: 1, unitPrice: 300, discount: 0, vatRate: 7.5, vatAmount: 22.5, lineTotal: 322.5 }],
  payments: [{ method: 'cash', amount: 322.5, confirmationStatus: 'confirmed' }],
  totals: { subtotal: 300, discount: 0, vat: 22.5, total: 322.5 },
  notes: 'Wrong flavour',
} })
// -> { "receiptNumber": "T01-000124", "localSeq": 124 }
```

### `shift_open` / `shift_close`

Same pattern: one transaction, an outbox item (`kind` `shift_open` /
`shift_close`, payload = the object passed plus `kind`), an audit row
(`shift_opened` / `shift_closed`). Required fields: `transactionId` (UUID),
`shiftLocalId`, `recordedAt`, `cashierId`; `branchId`, `terminalId` and any
other fields (opening float, counted cash, Z figures) are carried in the payload.
One open shift per device (D4): opening a second rejects with `invalid`.
`shift_open` obeys critical storage like a sale; `shift_close` is always
allowed, so the drawer can be closed on a full disk. Both are idempotent by
`transactionId`.

```js
await invoke('shift_open', { shift: { transactionId, shiftLocalId: 'shift-0192f0a1',
  recordedAt: '2026-10-27T08:00:00.000Z', cashierId, branchId, terminalId, openingFloat: 5000 } })
// -> { "transactionId": "...", "shiftLocalId": "shift-0192f0a1", "status": "open" }
await invoke('shift_close', { shift: { transactionId, shiftLocalId: 'shift-0192f0a1',
  recordedAt: '2026-10-27T17:00:00.000Z', cashierId, countedCash: 120500 } })
// -> { ..., "status": "closed" }
```

### `shift_current`

*(Addition.)* The open shift's payload plus `shiftLocalId` and
`"status": "open"`, or `null`. Lets the till resume after a restart.

### `sales_recent`

Newest first, sales and returns together. `limit` default 50 (at most 500),
`offset` default 0. Optional filters, combined with AND: `kind` (`'sale'` or
`'return'`; anything else is `invalid`), `originalTransactionId` (the returns
of one sale) and `receiptNumber` (exact, case-insensitive). An empty string
for the last two means no filter. `kind` and `originalTransactionId` are in
every row (`originalTransactionId` is `null` for a sale).

```js
await invoke('sales_recent', { limit: 20, offset: 0 })
await invoke('sales_recent', { kind: 'return', originalTransactionId: '0192f0b6-...' })  // returns of a sale
await invoke('sales_recent', { receiptNumber: 't01-000123' })                            // look up a receipt
```

```json
[{ "transactionId": "...", "receiptNumber": "T01-000123", "localSeq": 123,
   "recordedAt": "2026-10-27T09:00:00.000Z", "cashierId": "8a2e...", "customerId": null,
   "shiftLocalId": "shift-0192f0a1", "total": 645, "syncStatus": "pending",
   "serverId": null, "syncedAt": null, "kind": "sale", "originalTransactionId": null }]
```

### `sale_get`

The sale (or return) exactly as committed (the upload payload, for reprint)
plus a `sync` block; `not_found` if it is not on this device (for example
already cleaned).

```json
{ "transactionId": "...", "receiptNumber": "T01-000123", "localSeq": 123, "deviceCode": "T01",
  "lines": [ ... ], "payments": [ ... ], "totals": { ... },
  "sync": { "status": "synced", "serverId": "9c1...", "serverReceiptNumber": "T01-000123",
            "syncedAt": "2026-10-27T09:06:01.002Z", "flags": ["vat_rate_mismatch"], "lastError": null } }
```

### `queue_next_batch`

Items with status `pending`, and `failed_transient` items whose
`nextAttemptAt` has passed, in creation order (so a shift open goes before its
sales, its close after them, and a return after the sale it refunds). `kind`
is `sale`, `return`, `shift_open` or `shift_close`. `limit` default 50, at
most 500.

```json
[{ "seq": 41, "transactionId": "...", "kind": "sale", "status": "pending",
   "payload": { "transactionId": "...", "kind": "sale", "receiptNumber": "T01-000123", "...": "..." },
   "attempts": 0, "nextAttemptAt": null, "lastError": null, "syncResult": null,
   "serverId": null, "serverReceiptNumber": null, "flags": null,
   "createdAt": "2026-10-27T09:00:00.120Z", "syncedAt": null }]
```

### `queue_mark`

Applies the server's per-item answers in one transaction.

| result `status` | effect |
|---|---|
| `accepted`, `duplicate` | `synced`, `syncedAt` set, `serverId` / `receiptNumber` / `flags` kept; the row stays until retention |
| `failed_permanent` | `failed`, `reason` kept as `lastError`; never retried, never deleted; `sync_failed_permanent` audit row |
| `failed_transient` | `attempts + 1`, `nextAttemptAt = now + backoff` |

Backoff (computed in Rust): step = min(10 min, 2 s x 2^(attempts-1)), then
jittered down by up to 20%: the wait is always in [0.8 x step, step] and never
above 10 minutes (attempt 1: 1.6 to 2 s; attempt 10 and later: 8 to 10 min).
A result for an item already `synced` is ignored (counted in `ignored`); an
unknown `transactionId` is listed in `unknown`; an unknown `status` rejects the
whole call with `invalid`. It also records `lastSyncAt` and `lastSyncResult`.

```js
await invoke('queue_mark', { results: [
  { transactionId: 'a...', status: 'accepted', serverId: '9c1...', receiptNumber: 'T01-000123', flags: ['clock_drift'] },
  { transactionId: 'b...', status: 'failed_transient', reason: 'timeout' },
  { transactionId: 'c...', status: 'failed_permanent', reason: 'date_too_old' },
] })
// -> { "synced": 1, "failed": 1, "retrying": 1, "unknown": [], "ignored": 0 }
```

On a network failure with no per-item answers, mark the whole batch
`failed_transient` so each item backs off.

### `queue_failed_list`

Items with status `failed` (permanent), oldest first, same shape as
`queue_next_batch`.

### `queue_retry_failed`

Admin: puts a `failed` (or waiting `failed_transient`) item back to `pending`
with `attempts = 0`; keeps `lastError` until the next answer. Audit row
`sync_retry_requested`. `invoke('queue_retry_failed', { id: transactionId })`
-> the item; `not_found` if it is not failed.

### `cleanup_run`

Deletes ONLY sales and returns whose upload was accepted (`synced`) more than
`retentionDays` ago (argument, else `offlineSettings.retentionDays`, else 30;
1 to 3650), with their lines and payments, their stock deltas once
reconciled, other synced queue items past retention, closed shifts with
nothing left, and audit rows sent before the cutoff. Never pending, waiting or
failed items, never the open shift, never unsent audit rows. Reclaims space
with `PRAGMA incremental_vacuum` when more than 64 MB (or a quarter of the
file) is free inside it; `force: true` runs a full `VACUUM`.

```js
await invoke('cleanup_run', { retentionDays: 7, force: false })
// -> { "removed": 130, "sales": 120, "queueItems": 4, "shifts": 2, "auditRows": 4,
//      "stockDeltas": 240, "retentionDays": 7, "vacuumed": false }
```

### `export_pending`

Writes every unsent queue item (pending, waiting and failed, with full
payloads) to a **plain JSON file** for support. Use it before uninstalling: an
MSIX uninstall deletes the app's data, and with it every unsent sale. **The
file contains sales data and is not encrypted**; say so to the owner when
offering it. `path` must be absolute and end in `.json`; the file is written
to `<path>.partial` and renamed, so a half-written export never appears.

```js
await invoke('export_pending', { path: 'C:\\Users\\Shop\\Desktop\\till-T01-unsent.json' })
// -> { "path": "C:\\Users\\Shop\\Desktop\\till-T01-unsent.json", "items": 3, "bytes": 5120 }
```

```json
{ "format": "digitglance-till-export", "version": 1,
  "warning": "This file contains sales data ... and is not encrypted. ...",
  "exportedAt": "...", "ownerId": "...", "deviceId": "...", "deviceCode": "T01",
  "branchId": "...", "terminalId": "...", "count": 3, "items": [ { "...queue item..." } ] }
```

### `audit_append`

Appends an event for `POST /pos/devices/events`. `kind` matches
`^[a-z0-9_.-]{2,64}$`; `detail` any JSON up to 16 KB (default `{}`);
optional `actingUserId`.

```js
await invoke('audit_append', { kind: 'offline_sign_in', detail: { method: 'pin' }, actingUserId: '8a2e...' })
// -> { "id": 812 }
```

The store itself writes: `sale_recorded`, `shift_opened`, `shift_closed`,
`pin_lockout`, `sync_failed_permanent`, `sync_retry_requested`,
`storage_warning` / `storage_critical` / `storage_ok`, `local_cleanup`,
`pending_exported`.

### `audit_take_unsent`

Oldest unsent first, `limit` default and maximum 200 (the server's batch size).

```json
[{ "id": 812, "kind": "offline_sign_in", "actingUserId": "8a2e...",
   "deviceTime": "2026-10-27T08:00:00.000Z", "detail": { "method": "pin" } }]
```

### `audit_mark_sent`

`invoke('audit_mark_sent', { ids: [812, 813] })` -> `{ "marked": 2 }`.

## Lifecycle on the till

1. Launch: `device_identity_get()`. `null` -> pairing screen.
2. Pairing: generate the device secret, `POST /pos/devices/pair`, then
   `store_open({ ownerId, deviceId })` with the ids from the response,
   `device_identity_save({...})`, `GET /pos/sync/snapshot` ->
   `catalog_apply_snapshot`.
3. Online loop (single flight): `queue_next_batch` -> upload -> `queue_mark`;
   `catalog_get_cursor` -> changes -> `catalog_apply_changes` until `hasMore`
   is false (upload first, then pull, so the stock rows pulled include the
   sales just accepted); `audit_take_unsent` -> events -> `audit_mark_sent`;
   heartbeat with `store_status`.
4. Timer: `storage_check` every few minutes; show warning and critical.
5. Sale: `pin_verify` as needed -> `sale_commit` -> print.
6. Before uninstall or hand-over: `export_pending`.

## The bundled till

DigitGlance Trade ships the till (`apps/till` in the web monorepo) inside the
app: `till-dist/` holds the built files, vendored by
`node scripts/sync-till.mjs <path to the digitglance repo>` (runs
`pnpm --filter till build` there, copies `apps/till/dist`, copies the Trade
launcher page to `till-dist/backoffice/`, and writes `till-dist/SOURCE.json`
with the source repo, branch, commit and build time). The files are committed,
so the release and MSIX workflows build them in with no access to the web repo.
Only `tauri.trade.conf.json` points `frontendDist` at `../till-dist`; Books
and School keep their launcher pages.

| Window | Loads | Capabilities |
|---|---|---|
| `till` (opens at launch) | `till-dist/index.html` | `default` (core), `local-store` (the 36 commands), `till`: `open_backoffice`, `dialog:allow-save`, `http:default` scoped to `https://api.digitglance.com/*` |
| `backoffice` (from the till's Back office button, `invoke('open_backoffice')`) | `till-dist/backoffice/index.html`, the connectivity gate, which navigates to `https://digitglance.com/app/dashboard?client=trade-desktop` | `default` (core) only |

- **Device HTTP.** The API's CORS does not list the Tauri origins, so the till
  sends every device call (pair, ping, snapshot, changes, transactions,
  heartbeat, events) through `@tauri-apps/plugin-http`, which makes the request
  from Rust. The scope allows the production API only. For a local API, build or
  run with `--config src-tauri/tauri.trade.dev.conf.json` as well, which enables
  `capabilities/till-dev-http.json` (`http://localhost:*`, `http://127.0.0.1:*`);
  the base config lists the shipped capabilities explicitly
  (`app.security.capabilities`), so release and MSIX builds never include it.
- **Closing with unsent sales.** Closing the till window ends the app. When the
  open store has `pendingCount + failedCount > 0`, the close is held and a
  native warning asks "Close anyway" / "Keep the till open" (`src/till.rs`,
  non-blocking dialog). Nothing is lost by closing: the sales stay in the
  database and upload on the next launch. The warning exists because an MSIX
  uninstall deletes that database; the till also offers **Export unsent sales**
  (`export_pending` to a path picked in the save dialog, outside the package).
- **Updates (D17: never during an open shift).** Direct-download builds check
  the feed at launch, download a newer build, and install it only when
  `shift_current` is null (reopening the last paired store if the till has not
  yet). With a shift open the install waits and re-checks every five minutes;
  an unreadable store also waits. Quitting before then drops the download and
  the next launch starts over. Microsoft Store builds (`--features store`) have
  no self-updater: the Store updates the app, and Windows waits for the app to
  exit before servicing it (docs/STORE.md), so a till is updated only when it
  is closed (with the warning above if anything is unsent).

## Tests and timings

Run from `src-tauri/`:

```bash
cargo test --lib store::                          # all store tests (SQLCipher build)
cargo test --lib store::tests::perf -- --ignored --nocapture   # timings
```

Building SQLCipher's vendored OpenSSL needs a **native** Perl (Strawberry Perl
on Windows; it is preinstalled on GitHub's Windows runners). Git Bash / MSYS
Perl cannot build it. On a machine without one, run the logic tests on plain
SQLite with `cargo test --lib --no-default-features store::`; the encryption
test is then reported as ignored with that reason, and CI
(`.github/workflows/local-store-tests.yml`, windows-latest) runs the full
SQLCipher suite and clippy.

Covered: migrations from empty, re-run and newer-DB refusal; v1 -> v2 with
pending, synced and failed queue rows (payloads, statuses and `seq` kept, the
counter continues past deleted rows); returns (shared numbering, restock and
write-off stock, over-refund after earlier returns, unknown and refused
originals, idempotency, atomicity, queue order and payload, `queue_mark`
deltas, `sales_recent` filters, cleanup); loyalty settings and unknown
entities in delta pages; WAL / FULL /
incremental auto-vacuum; tenant binding; wrong key, copied file, plain open and
no plaintext on disk (SQLCipher); key created once and never silently
replaced; Windows Credential Manager round trip (skips with a message if the
credential store is unavailable); identity save/get/clear and launch reopen;
sale_commit contents, numbering and idempotency; atomicity with a panic at
five points; atomicity with a real process abort at four points; storage
critical refuses with nothing written, and proceeds once cleanup frees rows;
thresholds and `storage_check`; shift ordering and one-open-shift; every queue
transition; backoff bounds; cleanup only of synced rows older than retention,
30 -> 7 days, pending/failed/waiting never cleaned; stock = server copy +
unsynced deltas through delta pages and snapshots; search, barcode, SKU,
customers; PIN lockout across a restart and its expiry; audit; export;
command lists in sync across code, ACL and this file.

Timings (Windows 11, this development machine, debug test build with SQLite
compiled at -O3; plain SQLite build, SQLCipher adds page encryption cost on
top), 20,000 products and 50,000 sales (100,000 lines and stock deltas):

| operation | average | worst |
|---|---|---|
| `catalog_search` "co" (2-letter prefix, about 1,250 hits, page of 30) | 18.6 ms | 48.8 ms |
| `catalog_search` "golden rice" (two words) | 11.6 ms | 26.2 ms |
| `catalog_search` "sku1234" (SKU prefix) | 1.1 ms | 2.9 ms |
| `catalog_search` "m" + category (1-letter prefix, page of 50) | 28.6 ms | 68.1 ms |
| `catalog_by_barcode` | 0.07 ms | 0.43 ms |
| `sale_commit` (2 lines, synchronous=FULL, fsync) | 3.9 ms | 23.3 ms |
| `queue_next_batch` (50 of 50,000 pending) | 5.6 ms | 10.6 ms |
| `store_status` | 10.9 ms | 19.8 ms |

Setup in the same run: applying a 20,000-product snapshot 9.5 s (one
transaction; debug build), seeding 50,000 sales through `sale_commit` 87 s,
database 138 MB. Every search and the barcode lookup average under the
50 ms target; a one-letter prefix is the slowest case (the till should start
searching at two characters). Reproduce with the perf command above.

The same test with **SQLCipher** on GitHub's `windows-latest` runner (CI run
40487286076, all 27 store tests passed including encryption, keyring, crash
and perf): `catalog_search` "co" 16.8 ms avg / 23.6 worst, "golden rice"
11.8 / 15.3, SKU prefix 0.35 / 1.7, "m" + category 31.0 / 88.1;
`catalog_by_barcode` 0.04 / 0.35; `sale_commit` (FULL) 14.9 / 434 (the
runner's disk flush); `queue_next_batch` 3.0 / 5.1; `store_status` 3.3 / 8.1;
snapshot 3.2 s; database 140 MB.

## Known gaps

- Re-pairing creates a new device id and therefore a new file; the old file's
  unsent sales stay in the old file. Export them first (`export_pending`), or
  sync before re-pairing.
- Credit-limit, store-credit and loyalty balances are read from the cached
  customer objects; the store does not keep a local running balance between
  syncs (the till should subtract the session's own uses).
- An MSIX uninstall deletes `LocalAppData` for the package, including the DB;
  the Credential Manager entries survive it (they are per Windows user).
