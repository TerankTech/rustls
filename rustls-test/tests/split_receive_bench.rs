//! Split-mode receive cost per application data record.
//!
//! Measures `ReceiveTraffic::read()` plus `ReceivedApplicationData::into_next()`
//! on the client side of a TLS 1.3 connection, with the server encrypting
//! `PER_FLIGHT` records per flight into a reused input buffer. Only the receive
//! side is timed; record generation is not. This is the code path the
//! post-handshake application data fast path targets (see `FORK.md`).
//!
//! Ignored by default. Run serialized, in release mode:
//!
//! ```text
//! cargo test --release -p rustls-test --test split_receive_bench -- \
//!     --ignored --nocapture
//! ```
//!
//! Environment knobs:
//! - `PROVIDER=ring` selects the ring provider (default: aws-lc-rs).
//! - `RECV_LEN=<bytes>` measures one payload size (default: 64, 220 and 1400).
//! - `FLIGHTS=<n>` sets the number of timed flights (default: 25000).

use core::hint::black_box;
use core::time::Duration;
use std::time::Instant;

use rustls::crypto::CryptoProvider;
use rustls::split::{ReceiveTrafficState, SplitConnection};
use rustls::{TlsInputBuffer, VecInput};
use rustls_test::{KeyType, do_handshake, make_pair};

const PER_FLIGHT: usize = 8;
const WARMUP_FLIGHTS: u32 = 500;

/// Input buffer with a discard cursor, like an application-owned receive buffer.
struct Input {
    bytes: Vec<u8>,
    start: usize,
}

impl TlsInputBuffer for Input {
    fn slice_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[self.start..]
    }

    fn discard(&mut self, num_bytes: usize) {
        self.start += num_bytes;
    }

    fn received_close_notify(&mut self) {}

    fn has_seen_eof(&self) -> bool {
        false
    }
}

fn env_or<T: core::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore]
fn split_receive_bench() {
    let (provider_name, provider): (&str, &CryptoProvider) =
        match std::env::var("PROVIDER").as_deref() {
            Ok("ring") => ("ring", &rustls_ring::DEFAULT_PROVIDER),
            _ => ("aws-lc-rs", &rustls_aws_lc_rs::DEFAULT_PROVIDER),
        };
    let lens = match std::env::var("RECV_LEN") {
        Ok(_) => vec![env_or("RECV_LEN", 64usize)],
        Err(_) => vec![64, 220, 1400],
    };
    let flights = env_or("FLIGHTS", 25_000u32);

    for len in lens {
        let (mut client, mut server) = make_pair(KeyType::Rsa2048, provider);
        let (mut client_input, mut server_input) = (VecInput::default(), VecInput::default());
        do_handshake(
            &mut client_input,
            &mut client,
            &mut server_input,
            &mut server,
        );
        let suite = client.negotiated_cipher_suite();
        let SplitConnection { receive, .. } = client.split().unwrap();
        let SplitConnection {
            send: mut server_send,
            ..
        } = server.split().unwrap();

        let payload = vec![0xAB; len];
        let mut input = Input {
            bytes: Vec::with_capacity(64 * 1024),
            start: 0,
        };
        let mut receive = receive;
        let mut timed = Duration::ZERO;

        for flight in 0..WARMUP_FLIGHTS + flights {
            input.bytes.clear();
            input.start = 0;
            for _ in 0..PER_FLIGHT {
                for record in server_send.write(payload.as_slice().into()) {
                    input.bytes.extend_from_slice(&record);
                }
            }

            let start = Instant::now();
            let mut received = 0;
            loop {
                receive = match receive
                    .read(&mut input)
                    .map_err(|err| err.error)
                    .unwrap()
                {
                    ReceiveTrafficState::Available(mut data) => {
                        received += black_box(data.data()).len();
                        match data.into_next() {
                            ReceiveTrafficState::ReadMore(next) => next,
                            _ => panic!("unexpected state after application data"),
                        }
                    }
                    ReceiveTrafficState::ReadMore(next) => {
                        receive = next;
                        break;
                    }
                    _ => panic!("unexpected receive state"),
                };
            }
            if flight >= WARMUP_FLIGHTS {
                timed += start.elapsed();
            }
            assert_eq!(received, len * PER_FLIGHT);
        }

        let per_record = timed.as_nanos() / u128::from(flights) / PER_FLIGHT as u128;
        println!(
            "split receive: {len:5} B records, {per_record:4} ns/record \
             ({PER_FLIGHT} records/flight, {provider_name}, {suite:?})"
        );
    }
}
