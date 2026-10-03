# TerankTech rustls fork

This branch (`terank/in-place-encrypter`) carries a small patch series on top
of a snapshot of upstream rustls `main`. It exists for
[trrs-net-lib](https://github.com/TerankTech/trrs-net-lib), which pins one
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

Reference results for this branch: 219 rustls unit tests and 516 API tests pass.
BoGo with aws-lc-rs gives 1385 passed, 0 failed and 723 unimplemented, the same
result set as `ff52d536` without the fast path.

Then check trrs-net-lib against the new commit: point its three rustls
dependencies at it and run
`cargo test --release --features tls-split --lib` and
`cargo test --release --features tls-split --lib -- --ignored bench_tls_send_path --nocapture`.

## Upstream hardening not in this fork

Upstream `main` has landed hardening since the base that this fork does not
carry. Only patch 2 had an advisory. The client-relevant ones:

- `035b26f7`: check the server's chosen cipher suite against the offer
- `c566ba12`: reject a ServerHello that does not echo `legacy_session_id`
- `99f2358c`: check the compatibility session id against TLS 1.2 resumption
- `370b1336`: reject protected change_cipher_spec records
- `015713a3`: reject non-empty `renegotiation_info` in initial handshakes
- `5fae3042`: TLS 1.2 requires a known signature algorithm
- `2cd5cc10`: allow only one outstanding KeyUpdate request
- `453374fa`: return an error when encryption limits are exhausted
- `1e894afa`: refuse further encryption after a fatal alert
- `86e06619`: account for encryption overhead in `max_fragment_size`

Watch rustls security advisories
(<https://github.com/rustls/rustls/security/advisories>) and RUSTSEC. Backport
fixes onto this branch, record them under "Carried patches", and bump the pin in
trrs-net-lib.

## Updating the pin in trrs-net-lib

1. Change the three `rev = "…"` entries in trrs-net-lib's `Cargo.toml` to the
   new commit, and update the pin comment above them (base, carried patches,
   retire note).
2. Run `cargo update -p rustls -p rustls-aws-lc-rs -p rustls-ring`.
3. Release trrs-net-lib. trrs then bumps its `trrs-net-lib` tag and runs
   `cargo update -p trrs-net-lib`. Neither repository needs source changes for
   patches that keep the rustls API unchanged, like patches 2 and 3.
