//! Cross-check the Rust transform against vectors produced by the verified
//! Python implementation (`python/xiaomi_auth.py`) over random challenges.

use redmi_buds::auth::{hex_decode, hex_encode, transform, ADDR6_PLACEHOLDER};

#[test]
fn matches_python_vectors() {
    let data = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors.txt"))
        .expect("tests/vectors.txt");
    let mut n = 0;
    for (i, line) in data.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let chal = hex_decode(it.next().expect("challenge")).unwrap();
        let want = it.next().expect("response");
        let chal: [u8; 16] = chal.try_into().unwrap_or_else(|_| panic!("line {}", i + 1));
        let got = hex_encode(&transform(&ADDR6_PLACEHOLDER, &chal));
        assert_eq!(got, want, "vector line {}", i + 1);
        n += 1;
    }
    assert!(n >= 1000, "expected >=1000 vectors, got {n}");
    println!("validated {n} cross-language vectors");
}
