//! SEC-5: every decoder must reject garbage without panicking.

use proptest::prelude::*;
use rim_core::identity::{decode_invite, decode_link, recovery_key_from_words, safety_number};
use rim_core::mailbox::{open_for, parse_seed};
use rim_core::proto::{Body, Envelope, WireReq};
use rim_core::store::Store;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn invites_and_link_codes(s in ".*") {
        let _ = decode_invite(&s);
        let _ = decode_invite(&format!("rim2:{s}"));
        let _ = decode_link(&s);
        let _ = decode_link(&format!("rimlink2:{s}"));
    }

    #[test]
    fn base64_invite_payloads(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
        use base64::Engine as _;
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
        let _ = decode_invite(&format!("rim2:{b}"));
        let _ = decode_link(&format!("rimlink2:{b}"));
    }

    #[test]
    fn wire_and_envelopes(bytes in proptest::collection::vec(any::<u8>(), 0..2000)) {
        let _ = serde_json::from_slice::<WireReq>(&bytes);
        let _ = serde_json::from_slice::<Envelope>(&bytes);
        let _ = serde_json::from_slice::<Body>(&bytes);
    }

    #[test]
    fn mailbox_items(seed in any::<[u8; 32]>(), s in ".*") {
        let _ = open_for(&seed, &s);
        let _ = parse_seed(&s);
    }

    #[test]
    fn recovery_words(s in ".*") {
        let _ = recovery_key_from_words(&s);
    }

    #[test]
    fn safety_numbers(a in ".*", b in ".*") {
        let _ = safety_number(&a, &b);
    }
}

#[test]
fn damaged_state_files_are_rejected() {
    let dir = std::env::temp_dir().join(format!("rim-robust-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for data in [vec![], b"RIM1".to_vec(), vec![0u8; 100], b"RIM1\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0garbage".to_vec()] {
        std::fs::write(Store::path_in(&dir), &data).unwrap();
        assert!(Store::open(&dir, "pass").is_err());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
