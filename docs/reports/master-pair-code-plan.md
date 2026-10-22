# Plan: `gateway.master_pair_code` (static recovery credential) + CLI box fix

## Context

Two independent asks from the operator:

1. **CLI output bug.** `zeroclaw gateway get-paircode --new --port 42617` prints a
   decorative box whose border is hard-coded to 14 inner columns, but the generated
   code is **30 characters** (`0dz6K8GTvBRPo6mMGZsEn2TqxGizramf`). The code overflows
   the box, so the right border is misaligned:

   ```
   ┌──────────────┐
   │  0dz6K8GTvBRPo6mMGZsEn2TqxGizramf  │
   └──────────────┘
   ```

2. **New config key `gateway.master_pair_code`.** A static, reusable "master" code.
   When set, submitting this value on the pairing page (or to the `/api/pair` /
   `/pair` endpoints) logs the device in — without consuming the one-time
   startup/rotation code. Intended as a recovery/backdoor credential for cases where
   the dynamic one-time code is unavailable (remote/Docker origins can't read it).

Scope is deliberately narrow: only the gateway pairing endpoints change; the page
needs **no** change (it already POSTs whatever the operator types in the code field
to `/api/pair`).

---

## Part A — Fix the CLI box (standalone bug)

**File:** `src/main.rs` (the `GetPaircode` arm, ~line 5731-5733).

The border is built with three fixed-width literals. Replace them with a small helper
that sizes the border to the code:

```rust
/// Draw a box around the pairing code, sized to its width.
fn print_pairing_code_box(code: &str) {
    // matches the original inner padding of "  │  {code}  │" (2 spaces each side)
    let inner = code.chars().count() + 4;
    let bar = "─".repeat(inner);
    println!("  ┌{bar}┐");
    println!("  │  {code}  │");
    println!("  └{bar}┘");
}
```

- No i18n impact (the box is printed with raw `println!`, not through `t(...)`).
- **Width assertion test added** (`pairing_code_box_sized_to_code_width` in
  `src/main.rs`) — asserts the top/bottom border and the code row share one outer
  width and the 30-char code is fully enclosed (regression guard for the overflow).

---

## Part B — `gateway.master_pair_code`

### B1. Config schema

**File:** `crates/zeroclaw-config/src/schema.rs`, in `GatewayConfig`
(~line 7178, alongside `require_pairing` / `paired_tokens`).

```rust
/// Optional static "master" pairing code. When set, submitting this exact value
/// on the pairing page (or to POST /api/pair or POST /pair) logs the device in
/// and mints a bearer token, WITHOUT consuming the one-time startup/rotation
/// code. Treat it as a recovery credential: a reusable static secret. Prefer a
/// high-entropy value (e.g. 32 chars); it is never printed to the banner or
/// logs. Defaults to unset (None).
#[serde(default)]
#[secret]
#[credential_class = "encrypted_secret"]
#[cfg_attr(feature = "schema-export", schemars(extend("x-secret" = true)))]
pub master_pair_code: Option<String>,
```

- `Option<String>` + `#[serde(default)]` → fully backward compatible (existing
  configs without the key parse as `None`).
- `#[secret]` + `#[credential_class = "encrypted_secret"]` mirrors `webhook_secret`
  so it is redacted from config dumps / schema export, never logged.
- No migration needed (new optional key). Additive to the v2 schema.

### B2. Carry the value into the pairing guard

**File:** `crates/zeroclaw-config/src/pairing.rs` (`PairingGuard`).

Add a plain (post-construction) field and a builder so the **huge** number of
existing `PairingGuard::new(...)` call sites (including all tests) stay untouched:

```rust
pub struct PairingGuard {
    require_pairing: bool,
    pairing_code: Arc<Mutex<Option<PendingCode>>>,
    paired_tokens: Arc<Mutex<HashSet<String>>>,
    failed_attempts: Arc<Mutex<(HashMap<String, FailedAttemptState>, Instant)>>,
    /// Configured static master code (None = feature off).
    master_pair_code: Option<String>,
}
```

- `PairingGuard::new` sets `master_pair_code: None` (signature unchanged → tests
  compile).
- Add builder:
  ```rust
  pub fn with_master_code(mut self, code: Option<String>) -> Self {
      // Normalize: ignore blank values so an empty config string = "off".
      self.master_pair_code = code.filter(|c| !c.trim().is_empty());
      self
  }
  ```
- Add the validation/issuance method (constant-time, reusable, rate-limit is applied
  by the caller, not here):
  ```rust
  /// Returns a freshly minted bearer token if `code` matches the configured
  /// master code (constant-time). Reusable: does NOT consume the one-time code
  /// slot and is not rate-limited inside this method — the pairing endpoint's
  /// existing brute-force limiter still applies to misses.
  pub fn try_master(&self, code: &str) -> Option<String> {
      let Some(master) = self.master_pair_code.as_deref() else { return None; };
      if constant_time_eq(code.trim(), master.trim()) {
          let token = generate_token();
          self.paired_tokens.lock().insert(hash_token(&token));
          Some(token)
      } else {
          None
      }
  }

  /// Used by the gateway startup banner / UI hints (optional).
  pub fn has_master_code(&self) -> bool {
      self.master_pair_code.as_deref().is_some_and(|c| !c.trim().is_empty())
  }
  ```

### B3. Thread it into the live gateway state

The production gateway state builder is `crates/zeroclaw-gateway/src/lib.rs`
(~line 1526):

```rust
let pairing = Arc::new(
    PairingGuard::new(
        config.gateway.require_pairing,
        &config.gateway.paired_tokens,
        config.gateway.pairing_code,
    )
    .with_master_code(config.gateway.master_pair_code.clone()),
);
```

- Any `POST /admin/reload` (or AppState rebuild) that reuses this builder picks up a
  changed master code automatically — verify at implementation that the reload path
  flows through this constructor.
- Test-only `PairingGuard::new(...)` / `AppState { pairing: ... }` sites need **no**
  change (master stays `None`).

### B4. Accept the master code in both pairing endpoints

Extract the shared "finalize a successful pair" block (device registry insert +
`persist_pairing_tokens` + rollback-on-failure + success JSON) into one helper so
the one-time path and the master path don't diverge:

```rust
/// Shared success finalizer for both the one-time-code and master-code paths.
/// Registers the device, persists the token, and rolls back the in-process
/// token on any failure. The exact JSON/HTTP contract is unchanged.
async fn finalize_pairing(
    state: &AppState,
    rate_key: &str,
    device_name: Option<String>,
    device_type: Option<String>,
    token: String,
) -> impl IntoResponse { /* existing body of the Ok(Some(token)) arm, refactored */ }
```

Apply in **both** handlers (guard before the existing `try_pair` call):

- **`crates/zeroclaw-gateway/src/api_pairing.rs` → `submit_pairing_enhanced`** (the
  web login path, `POST /api/pair`, used by `web/src/lib/api.ts::pair`):
  ```rust
  // Master code short-circuit (configured recovery credential).
  if let Some(token) = state.pairing.try_master(code) {
      return finalize_pairing(&state, &client_id, device_name, device_type, token).await;
  }
  // ... existing `match state.pairing.try_pair(code, &client_id)` path ...
  ```
- **`crates/zeroclaw-gateway/src/lib.rs` → `handle_pair`** (legacy `POST /pair`,
  `X-Pairing-Code` header — desktop/Tauri + ACP bridge): both the master short-circuit
  **and** the one-time `Ok(Some(token))` arm route through `finalize_pairing` — the
  one-time inline body was removed so the legacy endpoint returns the **same** JSON
  contract (`paired`/`persisted`/`token`/`message: "Pairing successful"`) as the web
  path. This closes the contract split where the legacy one-time path used to return a
  divergent `"Save this token — use it as Authorization: Bearer <token>"` body.

Behavior on a *wrong* master code: `try_master` returns `None`, falls through to
`try_pair`, and a miss there still feeds the shared auth limiter
(`state.auth_limiter.record_attempt`) — so brute-force protection is preserved. The
master code is never rate-limited *on a correct match* (it just issues a token).

Note on semantics: using the master code mints a **fresh bearer token per use** and
registers a **device** (so it shows in `Pairing.tsx` and is individually revocable).
The master code itself is **not** consumed and stays valid for future use. Revoking a
master-paired device revokes that device's token only — the master code keeps working.

### B5. Tests (new)

`crates/zeroclaw-config/src/pairing.rs`:
- `try_master` returns `Some(token)` on exact match; `None` otherwise; reusable
  (two calls both succeed); returned token authenticates via `is_authenticated`.
- `with_master_code` ignores blank/`None`.

`crates/zeroclaw-gateway/src/api_pairing.rs` (mirror existing
`submit_pairing_enhanced_*` tests):
- With `master_pair_code` set, `submit_pairing_enhanced` returns `paired: true` and a
  token when the master code is submitted.
- Wrong code still rejected (shared auth limiter still engages).
- 5xx body on registry/persist failure still contains **no** plaintext token
  (master path reuses `finalize_pairing`, so the existing assertion holds).

### B6. Docs

- `docs/book/src/gateway/web-dashboard.md` (and the pairing setup section under
  `docs/book/src/setup/`): add a note that `gateway.master_pair_code` is a reusable
  static recovery credential, recommended length, and that it bypasses the one-time
  code without consuming it.
- `dev/config.template.toml` (if it lists `gateway.*` pairing keys): add a commented
  example.

---

## Files touched

| File | Change |
|------|--------|
| `src/main.rs` | Add `print_pairing_code_box` + `pairing_code_box_lines`, use it in the `GetPaircode` arm (Part A); width-assertion test (AC#1) |
| `crates/zeroclaw-config/src/schema.rs` | New `master_pair_code: Option<String>` on `GatewayConfig` (B1) |
| `crates/zeroclaw-config/src/pairing.rs` | `master_pair_code` field, `with_master_code`, `try_master`, `has_master_code`, `require_pairing` gate (B2) |
| `crates/zeroclaw-gateway/src/lib.rs` | `.with_master_code(...)` at state build (B3); `try_master` short-circuit AND one-time `Ok(Some(token))` arm both route through `finalize_pairing` in `handle_pair` (B4) |
| `crates/zeroclaw-gateway/src/api_pairing.rs` | Extract `finalize_pairing`; `try_master` short-circuit in `submit_pairing_enhanced` (B4); new tests (B5) |
| `docs/book/src/gateway/web-dashboard.md`, `dev/config.template.toml` | Document the new key (B6) |
| `docs/book/src/setup/container.md`, `docs/book/src/setup/windows.md` | Document `master_pair_code` as a recovery credential in setup docs (B6) |

## Acceptance criteria

1. `zeroclaw gateway get-paircode --new` prints a box whose borders align with the
   30-char code (verified visually + a width assertion test).
2. With `gateway.master_pair_code = "<value>"` set, the pairing page accepts `<value>`
   and logs in (mints a token, registers a device). The one-time code still works
   independently.
3. The master code is NOT printed anywhere (banner, `get-paircode` output, logs) and
   is redacted from config dumps (`#[secret]`).
4. Brute-force protection still applies: wrong master attempts are rate-limited via
   the shared auth limiter.
5. Master-paired devices appear in the device list and can be individually revoked;
   revoking does not disable the master code.
6. Backward compatible: configs without the key parse as `None`; all existing tests
   pass (`cargo test -p zeroclaw-config -p zeroclaw-gateway`).
7. Every **production** guard constructor carries the configured master code. A guard
   built by hand without it makes the pairing page reject a correct master code with
   no hint that it was configured. Regression pinned by
   `daemon::tests::the_published_pairing_authority_pairs_with_the_configured_master_code`
   (the daemon-hosted gateway regressed in the v0.8.6 merge — see the regression note
   above).

## Open questions for review

- **Naming:** `master_pair_code` vs `recovery_pair_code` vs `static_pair_code`.
  I lean `master_pair_code` (matches the operator's wording).
- **Should the master code also work when `require_pairing = false`?** Today it would
  be redundant (everyone is already authenticated). Plan leaves it inert in that mode.
- **Should a master-code login be flagged in the device list** (e.g. device_type
  hint) so an operator can tell recovery logins apart from normal ones? Optional; not
  in scope unless you want it.

## Resolved decisions (implementation)

Operator answers to the open questions:

1. **Naming:** `gateway.master_pair_code` (as written).
2. **`require_pairing = false` ⇒ master code inert.** Implemented by gating
   `PairingGuard::try_master` on `self.require_pairing()` — a disabled lock has no
   recovery path, and minting a token would be a side effect behind no auth gate.
   Gating lives in `try_master` itself, so **both** pairing endpoints
   (`/api/pair` and `/pair`) honor it from one place. Unit test
   `try_master_is_inert_when_require_pairing_false` pins this.
3. **No device-list flagging.** A master-paired device is indistinguishable from a
   normal one-time-paired device (same `DeviceInfo`, individually revocable, and
   revoking it does not disable the master code).

### Tests added beyond the plan

- `crates/zeroclaw-config/src/pairing.rs`:
  `try_master_is_inert_when_require_pairing_false` (decision #2).
- `crates/zeroclaw-gateway/src/api_pairing.rs` (mirror existing `submit_pairing_enhanced_*`):
  - `submit_pairing_enhanced_accepts_master_code_and_mints_token` — master code
    returns `paired: true` + a `zc_` token that authenticates immediately.
  - `submit_pairing_enhanced_wrong_master_code_feeds_shared_auth_limiter` — a wrong
    master value falls through to the one-time-code path and feeds the shared auth
    limiter exactly like a wrong one-time code (preload-then-lockout pattern).

### Reload path note

The only **production** `PairingGuard::new` is the gateway state builder
(`crates/zeroclaw-gateway/src/lib.rs:~1527`), which calls `.with_master_code(...)`.
`POST /admin/reload` reuses that builder, so a changed/added `master_pair_code` is
picked up on reload with no restart. (The `PairingGuard::new` calls without
`.with_master_code` are all `#[cfg(test)]` fixtures, not production code.)

### Regression: the daemon-hosted gateway dropped the master code (v0.8.6 merge)

The note above stopped being true when the upstream v0.8.6 merge
(`1f379c6de`, 2026-10-19) added a **second** production guard constructor so a
pairing change could reach both the RPC surface and the gateway at once:

```rust
// crates/zeroclaw-runtime/src/daemon/mod.rs  (before this fix)
let pairing_guard = Arc::new(PairingGuard::new(
    config.gateway.require_pairing,
    &config.gateway.paired_tokens,
    config.gateway.pairing_code,
)); // ← no .with_master_code(..): master_pair_code is hardcoded None
```

`PairingGuard::new` sets `master_pair_code: None` unconditionally, and the gateway
adopts the daemon's guard verbatim
(`shared_pairing.unwrap_or_else(|| PairingGuard::from_gateway_config(..))`), so
`from_gateway_config` — the constructor that *does* carry the code — never ran in a
supervised run. `try_master` then hit its `?` and fell through to the one-time-code
path, so the pairing page answered `Invalid or expired pairing code` for a
configured, correct master code.

**Why it looked like "the gateway path is fine":** a standalone
`zeroclaw gateway start` has no daemon, so `shared_pairing` is `None` and
`from_gateway_config` runs — the master code works there. Only a
`zeroclaw service start` / `daemon` run regressed. The gateway-side tests all built
their guard with `from_gateway_config`, so they kept passing while the daemon path
was broken — a test/production drift, the same shape as the SSE test that passed on
a hand-written fixture.

**Fix:** the daemon builds its guard with `PairingGuard::from_gateway_config` (one
production constructor again), and the gateway's choice is extracted as
`resolve_pairing_guard(shared, gateway_config)` so both starts are pinned by tests.

**Verified end to end** (docker, same config in both runs):

| daemon guard constructor | banner | `POST /api/pair` with the master code |
| --- | --- | --- |
| `PairingGuard::new(..)` (before) | no master line | `400 Invalid or expired pairing code` |
| `from_gateway_config(..)` (after) | `🔑 Master recovery code: configured` | `200 {"paired":true,"token":"zc_…"}` |

Tests: `zeroclaw-runtime` `daemon::tests::the_published_pairing_authority_pairs_with_the_configured_master_code`
(asserts the daemon-built authority actually mints a token) and `zeroclaw-gateway`
`tests::a_standalone_gateway_adopts_the_configured_master_code` /
`tests::a_supervised_gateway_serves_the_daemons_authority_verbatim`.

### B4 contract unification (post-review correction)

Initial implementation only routed the **master** branch of `handle_pair` through
`finalize_pairing`; the legacy one-time `Ok(Some(token))` arm stayed inline and
returned a divergent `"Save this token — use it as Authorization: Bearer <token>"`
body, contradicting "two paths don't diverge". Corrected so **both** the master and
one-time arms of `handle_pair` call `finalize_pairing`, matching
`submit_pairing_enhanced`. All four pairing paths (2 endpoints × 2 code types) now
return one JSON contract: `{paired, persisted, token, message:"Pairing successful"}`.
The only behavior change is the legacy `/pair` success `message` text (informational;
no client parses it for the token, which is a sibling field).
