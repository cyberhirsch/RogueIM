//! Live test against public Nostr relays. Needs internet: `cargo test -- --ignored`.

use rim_core::mailbox::{parse_seed, new_seed, Mailbox, DEFAULT_RELAYS};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mailbox_roundtrip_and_rendezvous() {
    let relays: Vec<String> = DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect();
    let mb = Mailbox::connect(&relays).await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    println!("relays: {:?}", mb.relay_status().await);
    let seed = parse_seed(&new_seed()).unwrap();
    mb.post(&seed, b"hello mailbox").await.expect("post");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let items = mb.fetch(&[seed]).await.expect("fetch");
    assert_eq!(items.len(), 1, "exactly one item");
    assert_eq!(items[0].payload, b"hello mailbox");
    mb.delete(&items[0]).await.expect("delete");

    let rdv = parse_seed(&new_seed()).unwrap();
    mb.publish_rdv(&rdv, &["/ip4/1.2.3.4/tcp/5".into()]).await.expect("rdv publish");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let got = mb.fetch_rdv(&rdv).await.expect("rdv fetch");
    assert_eq!(got, Some(vec!["/ip4/1.2.3.4/tcp/5".to_string()]));
    mb.shutdown().await;
}
