//! ADR 022: the per-account virtual-model documents in the store.

use lumen_auth::store::{AccountModelsCas, KeyStore};

const A: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b61";
const B: &str = "0192f3c1-7c2e-7b1a-9f00-3c9d2e4a5b62";
const EMPTY: &str = "hash-of-empty";

#[tokio::test]
async fn writes_are_compare_and_swap_per_account() {
    let store = KeyStore::in_memory().await.unwrap();
    assert!(store
        .load_account_virtual_models(A)
        .await
        .unwrap()
        .is_none());
    assert_eq!(store.account_virtual_models_generation().await.unwrap(), 0);

    // First write against the empty hash.
    let out = store
        .save_account_virtual_models(A, "[1]", "h1", EMPTY, EMPTY)
        .await
        .unwrap();
    assert_eq!(out, AccountModelsCas::Applied { generation: 1 });
    // A stale write is refused and changes nothing.
    let out = store
        .save_account_virtual_models(A, "[2]", "h2", EMPTY, EMPTY)
        .await
        .unwrap();
    assert_eq!(
        out,
        AccountModelsCas::Stale {
            current_hash: "h1".to_owned()
        }
    );
    // Account B has its own version: the empty hash still applies there.
    let out = store
        .save_account_virtual_models(B, "[9]", "h9", EMPTY, EMPTY)
        .await
        .unwrap();
    assert_eq!(out, AccountModelsCas::Applied { generation: 2 });
    let out = store
        .save_account_virtual_models(A, "[]", "h0", "h1", EMPTY)
        .await
        .unwrap();
    assert_eq!(out, AccountModelsCas::Applied { generation: 3 });

    let a = store.load_account_virtual_models(A).await.unwrap().unwrap();
    assert_eq!(
        (a.document.as_str(), a.hash.as_str(), a.generation),
        ("[]", "h0", 3)
    );
    let b = store.load_account_virtual_models(B).await.unwrap().unwrap();
    assert_eq!((b.document.as_str(), b.generation), ("[9]", 2));
    // Debug never prints the document.
    assert!(!format!("{b:?}").contains("[9]"));

    assert_eq!(store.account_virtual_models_generation().await.unwrap(), 3);
    let since: Vec<String> = store
        .account_virtual_models_since(1)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.account_ref)
        .collect();
    assert_eq!(since, [B, A]);
    assert_eq!(
        store.account_virtual_models_since(0).await.unwrap().len(),
        2
    );
    assert_eq!(
        store.account_virtual_models_since(3).await.unwrap().len(),
        0
    );
}
