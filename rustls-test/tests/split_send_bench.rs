//! Split-mode send cost per application data record.
//!
//! Measures `SendTraffic::write_into()` on the client side of a TLS 1.3
//! connection, encrypting each record straight into a destination buffer.
//! That buffer stands in for the reserved transmit buffer trrs-net-lib
//! encrypts into; this is the path the caller-provided encryption buffers
//! exist for (see `FORK.md`).
//!
//! Ignored by default. Run serialized, in release mode:
//!
//! ```text
//! cargo test --release -p rustls-test --test split_send_bench -- \
//!     --ignored --nocapture
//! ```
//!
//! Environment knobs:
//! - `PROVIDER=ring` selects the ring provider (default: aws-lc-rs).
//! - `SEND_LEN=<bytes>` measures one payload size (default: 64, 220 and 1400).
//! - `RECORDS=<n>` sets the number of timed records (default: 400000).

use core::hint::black_box;
use std::time::Instant;

use rustls::VecInput;
use rustls::crypto::CryptoProvider;
use rustls::split::SplitConnection;
use rustls_test::{KeyType, do_handshake, make_pair};

const WARMUP_RECORDS: u32 = 20_000;

fn env_or<T: core::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore]
fn split_send_bench() {
    let (provider_name, provider): (&str, &CryptoProvider) =
        match std::env::var("PROVIDER").as_deref() {
            Ok("ring") => ("ring", &rustls_ring::DEFAULT_PROVIDER),
            _ => ("aws-lc-rs", &rustls_aws_lc_rs::DEFAULT_PROVIDER),
        };
    let lens = match std::env::var("SEND_LEN") {
        Ok(_) => vec![env_or("SEND_LEN", 64usize)],
        Err(_) => vec![64, 220, 1400],
    };
    let records = env_or("RECORDS", 400_000u32);

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
        let SplitConnection { mut send, .. } = client.split().unwrap();

        let payload = vec![0xAB; len];
        let mut destination = vec![0u8; 2048];
        let mut written = 0;
        for _ in 0..WARMUP_RECORDS {
            written += send
                .write_into(payload.as_slice().into(), &mut destination)
                .unwrap()
                .tls_written;
        }

        let start = Instant::now();
        for _ in 0..records {
            written += send
                .write_into(payload.as_slice().into(), &mut destination)
                .unwrap()
                .tls_written;
            black_box(&destination);
        }
        let per_record = start.elapsed().as_nanos() / u128::from(records);
        black_box(written);

        println!(
            "split send: {len:5} B records, {per_record:4} ns/record \
             ({provider_name}, {suite:?})"
        );
    }
}
