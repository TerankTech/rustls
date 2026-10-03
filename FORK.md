# TerankTech rustls fork

This branch (`terank/in-place-encrypter`) carries a small patch series on top
of a snapshot of upstream rustls `main`. It exists for trrs-net-lib (the
private `TerankTech/trrs-net-lib` repository), which pins one
commit of this branch by `rev` for `rustls`, `rustls-aws-lc-rs` and
`rustls-ring`. All three crates must come from the same commit. trrs uses the
fork only through trrs-net-lib.

The fork is the vehicle for latency work on the TLS hot path. Upstream `main`
has since reworked the connection API for 0.24. Moving trrs-net-lib onto it
would need a port of its TLS layer and measured slightly slower receive. That
was decided against on 2026-10-03.

## Base

Upstream `rustls/rustls` `main` at `b1e2b870c510` (2026-07-12), version
`0.24.0-dev.0`. It already contains split mode (upstream PR #3080):
`ClientConnection::split()`, `SendTraffic` and `ReceiveTraffic`.

## Carried patches

In branch order:

1. **Caller-provided encryption buffers** (upstream PR #3127 by Daniel
   McCarney, rebased): `fa3a4254`, `73df6b92`, `cb64403a`, `ec783cc5`,
   `e1e65396`, `50be183a`, `fd854bee`, plus `eb14d524`, which adapts two of
   its tests to the `TlsInputBuffer` handshake helper. Adds
   `SendTraffic::write_into(&mut [u8])`, which trrs-net-lib uses to encrypt
   records in place inside reserved DPDK transmit buffers.
   - Upstream: merged 2026-07-22 and shipped as `write_tls_into` in
     `0.24.0-dev.1`. Upstream PR #3193 (2026-08-05) then replaced slice output
     with `&mut Vec<u8>`, so no upstream release will offer it.
2. **RUSTSEC-2026-0285 / GHSA-2mjx-qc3c-rqvc fix**: `ff52d536`, a cherry-pick
   of upstream `98456aea`. Rejects TLS 1.3 handshake messages that follow a
   key-changing message in the same record.
3. **Application data fast path on receive** (see below):
   - "perf: fast-path post-handshake application data delivery" (cherry-pick
     of `919a9c75` from `perf/rx-app-data-fast-path`, written 2026-07-16)
   - "Ignore application data after close_notify on the fast path"
   - "Share traffic-state application data bookkeeping with the fast path"
   - "Add an ignored split-mode receive benchmark"
4. **Upstream hardening backports**, cherry-picked with `-x` in this order. Each
   commit's "Backport notes" record its conflicts and test adaptations.
   - `035b26f7`: the server's chosen cipher suite must be one we offered.
   - `e3fab0e1`: bound the ticket age calculation.
   - `3da1f723`: saturating arithmetic for the PSK binder suffix.
   - `5fae3042`: TLS 1.2 requires a known signature algorithm.
   - `c566ba12`: reject a TLS 1.3 ServerHello that does not echo
     `legacy_session_id`.
   - `015713a3`: reject non-empty `renegotiation_info` in initial handshakes.
   - `99f2358c`: check the compatibility session id against TLS 1.2
     resumption.
   - `370b1336`: reject change_cipher_spec records that arrive encrypted.
   - `f5f7bf50`: keep session secrets out of `Debug` output.

   These add `PeerMisbehaved::UnmatchedSessionId` and
   `PeerMisbehaved::NonEmptyRenegotiationInfo`. `PeerMisbehaved` is
   `#[non_exhaustive]`, so this is not an API break.

## Application data fast path

`ReceivePath::process_new_packets()` (`rustls/src/conn/receive.rs`) delivers a
decrypted `ApplicationData` record directly when the state machine is in one of
the four post-handshake traffic states. Those are `ExpectTraffic` for TLS 1.2
and TLS 1.3, client and server. It skips `receive_message()`, the
`Message::try_from` parse and the boxed state's `handle()` round trip. Both the
buffered connection API and split mode use this loop.

### Why it is equivalent to the generic path

- **All record checks have already run.** The shortcut sits after `deframe()`.
  By then a record has passed:
  - decryption with tag verification, with the correct sequence number
  - the plaintext length limits, enforced by the provider's decrypter
  - the rule that application data must be encrypted
  - rejection of change_cipher_spec records that arrive encrypted (patch 4,
    `370b1336`)
  - rejection of records interleaved with a partial handshake message
    (`Deframer::aligned()`, tightened by patch 2)
  - the consecutive empty record limit
  - the read sequence soft limit, which queues our own `close_notify`
- **What it skips does nothing for application data.**
  - The branches of `receive_message()` only match change_cipher_spec, alert
    and renegotiation records.
  - `MessagePayload::new()` only wraps application data bytes
    (`Payload::Borrowed`).
  - `ClientState`, `ServerState`, `Tls12State` and `Tls13State` only forward
    to the current state.
- **The state effect is the same.** The `ApplicationData` arm of every traffic
  state's `handle()` does exactly what the shortcut does:
  - TLS 1.2: deliver the plaintext.
  - TLS 1.3: also call `ExpectTraffic::received_app_data()`, which resets the
    limit on consecutive post-handshake handshake messages. That helper is
    shared by `handle()` and the shortcut.

  The state value does not change. The plaintext reaches the same
  `CaptureAppData` sink, and the deferred input discard is the same.
- **Every other state takes the generic path**: handshake states, the server's
  early data states and QUIC.
- **Data after a peer's `close_notify` takes the generic path**, which ignores
  it (RFC 8446 §6.1). Without that guard, data fed in a later call through
  `SliceInput` or a custom `TlsInputBuffer` would have been delivered.
  `test_data_after_close_notify_in_later_call_is_ignored` covers this.

### Rebase and backport checklist

Re-check the equivalence above whenever a change touches any of these:

- `ReceivePath::receive_message()` or `MessagePayload::new()` for
  `ApplicationData`
- the `ApplicationData` arm of an `ExpectTraffic::handle()`, or
  `ExpectTraffic::received_app_data()`
- the checks in `ReceivePath::deframe()`
- the checks after `handle()` in `process_new_packets()`, such as the
  `close_notify` handling

Then run the full validation below. Changes to those arms now conflict with
this branch instead of applying silently.

### Measured effect

Pure rustls split-mode receive, eight records per flight,
`TLS13_AES_128_GCM_SHA256`, aws-lc-rs, AMD Ryzen 9 7950X3D. Three serialized
runs of `split_receive_bench` per side:

| record size | before (`ff52d536`) | with fast path |
|---|---|---|
| 64 B | 247–250 ns | 216–217 ns |
| 220 B | 250–256 ns | 219–220 ns |
| 1400 B | 360–367 ns | 332–333 ns |

callgrind counts about 12% fewer instructions per 64 B record. After the fast
path, about 70% of the remaining per-record instructions are AES-GCM in aws-lc,
mostly fixed per-record setup rather than data.

## Validation

Run all of these on every change to this branch:

```sh
cargo test --release -p rustls --lib
cargo test --release -p rustls-test --test api
cargo clippy --release -p rustls -p rustls-test --all-targets
cargo fmt --all -- --check
# BoGo needs Go on PATH; it fetches and builds the BoringSSL test runner.
(cd bogo && ./runme)                          # aws-lc-rs
(cd bogo && BOGO_SHIM_PROVIDER=ring ./runme)  # ring
cargo test --release -p rustls-test --test split_receive_bench -- --ignored --nocapture
```

Reference results for this branch:
- 226 rustls unit tests and 516 API tests pass.
- BoGo with aws-lc-rs: 1385 passed, 0 failed, 723 unimplemented.
- BoGo with ring: 1301 passed, 0 failed, 703 unimplemented.

Both BoGo result sets are the same as at `ff52d536`, before patches 3 and 4.

Then check trrs-net-lib against the new commit: point its three rustls
dependencies at it and run
`cargo test --release --features tls-split --lib` and
`cargo test --release --features tls-split --lib -- --ignored bench_tls_send_path --nocapture`.

## Upstream hardening not in this fork

Upstream `main` has landed hardening since the base that this fork does not
carry. Only patch 2 had an advisory. Patch 4 backported the client-relevant
fixes that apply with small adaptations. Still missing:

- **Upstream rewrote the send path, so these need reimplementing rather than
  cherry-picking:**
  - `2cd5cc10`: allow only one outstanding KeyUpdate request
  - `453374fa`: return an error when encryption limits are exhausted
  - `1e894afa`: refuse further encryption after a fatal alert. trrs-net-lib
    already poisons the send half after fatal errors.
  - `86e06619`: account for encryption overhead in `max_fragment_size`. Only
    matters if `max_fragment_size` is set.
- **Candidates for a further backport round:**
  - `3f2ef371`: reject TLS 1.3 records whose outer `opaque_type` is not
    `application_data`
  - `27b048a9`, `86205ca1`, `78c18be6`, `a1256b0e`: reject misplaced
    extensions in ServerHello, EncryptedExtensions, CertificateRequest and
    NewSessionTicket
  - `fbdeb82e`, `1d87e381`: reject trailing data in encoded TLS 1.2 and
    TLS 1.3 sessions
  - `37d51997`: do not send TLS 1.3-only signature schemes when no TLS 1.3
    suites are configured (low relevance; does not compile as-is on this base)
- **Not relevant to how trrs-net-lib uses rustls:**
  - ECH: `7a71ad82`, `7dcbe4c1`
  - certificate compression: `5c9a6502`, `402b7b78`
  - server ticketer: `2063323c`
  - provider private key zeroizing: `8fcffe1b`

### Known test gap at this base

The client RPK tests in `rustls/src/client/test.rs`
(`test_client_requiring_rpk_*`) return early: they call `x25519_provider()`,
which finds no X25519 group in the fake `TEST_PROVIDER`. Upstream later moved
them to the fake key exchange group. The TLS 1.3 tests added in patch 4 use
that fake group (`KEY_EXCHANGE_GROUP`). Each new test was checked to fail with
its fix removed, except the `Debug` test, which compares the exact output.

Watch rustls security advisories
(<https://github.com/rustls/rustls/security/advisories>) and RUSTSEC. Backport
fixes onto this branch, record them under "Carried patches", and bump the pin in
trrs-net-lib.

## Branches and tags

`terank/in-place-encrypter` is the default branch and the only long-lived one.
Work happens on short-lived branches merged into it through pull requests.
Delete those branches after merging.

trrs-net-lib and trrs releases lock exact fork commits. Cargo fetches a locked
commit directly, even if the branch named in an old `Cargo.toml` no longer
exists. A release therefore stays buildable as long as its commit is reachable
from some branch or tag. Before deleting a branch, or force-pushing over
commits, check whether any release locks one of its commits. If one does, tag
the branch head as `archive/<branch>` first.

Tags that keep released or referenced commits reachable:

| tag | commit | needed by |
|---|---|---|
| `archive/set-plaintext-buffer-limit` | `4504657a` | trrs-net-lib v0.1.x–v0.2.2rc2, locked at `f4635ae8`, `51d0b61f` and `4504657a`; trrs history |
| `archive/increase-buffer-limits-ac50752d` | `ac50752d` | trrs-net-lib v0.2.2rc3–v0.5.7; trrs history |
| `v0.24.0-terank.1` | `ea88e9b6` | trrs-net-lib v0.6.0; trrs history |
| `archive/state-api-fix` | `0d158b20` | trrs-net-lib v0.7.0–v0.8.2; trrs history |
| `archive/perf-rx-app-data-fast-path` | `919a9c75` | trrs-net-lib branch `feature/tx-send-latency` |
| `archive/feat-tls-large-burst-buffer` | `9fee78a9` | unpinned experiment, kept for reference |
| `archive/teranktech-jbp-new-api` | `8169be04` | unpinned split/state API experiment, superseded by upstream split mode |

On 2026-10-03 the branches these tags replace were deleted, and so were
`increase-buffer-limits` (its head is `v0.24.0-terank.1`) and the stale `main`
(an untouched upstream snapshot from 2025-01-22). Fetching trrs-net-lib v0.1.0,
v0.5.7 and v0.8.2 with an empty cargo git cache still resolves their locked
rustls commits.

## Updating the pin in trrs-net-lib

1. Change the three `rev = "…"` entries in trrs-net-lib's `Cargo.toml` to the
   new commit, and update the pin comment above them (base, carried patches,
   retire note).
2. Run `cargo update -p rustls -p rustls-aws-lc-rs -p rustls-ring`.
3. Release trrs-net-lib. trrs then bumps its `trrs-net-lib` tag and runs
   `cargo update -p trrs-net-lib`. Neither repository needs source changes for
   patches that keep the rustls API unchanged, like patches 2 to 4.
