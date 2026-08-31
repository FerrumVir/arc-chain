use arc_crypto::Hash256;
use arc_types::transaction::Transaction;

#[test]
fn canonical_typescript_transfer_hash_fixture_is_stable() {
    let from = Hash256::from_hex(&"11".repeat(32)).expect("from fixture");
    let to = Hash256::from_hex(&"22".repeat(32)).expect("to fixture");
    let domain = Hash256::from_hex(&"33".repeat(32)).expect("domain fixture");
    let mut transaction = Transaction::new_transfer(from, to, 7, 4);
    transaction.fee = 1;

    assert_eq!(
        transaction.compute_hash().to_hex(),
        "267d4e0c25020d50ae17ce254a28f9556cc086814304902a499916993cb8f05b"
    );
    assert_eq!(
        transaction.compute_hash_in_domain(&domain).to_hex(),
        "5accadc1f889e29e95d2fdac38b3b0db2f76e727c8593bdddb6598c04172f522"
    );
}

#[test]
fn typescript_bigint_u64_boundary_matches_rust() {
    let from = Hash256::from_hex(&"11".repeat(32)).expect("from fixture");
    let to = Hash256::from_hex(&"22".repeat(32)).expect("to fixture");

    let mut max_safe = Transaction::new_transfer(from, to, 9_007_199_254_740_991, 4);
    max_safe.fee = 1;
    assert_eq!(
        max_safe.compute_hash().to_hex(),
        "89243800e36a725c72c633a4955e86938e0a94b9321e2a1349f3d24ccd165e35"
    );

    let mut first_unrepresentable = Transaction::new_transfer(from, to, 9_007_199_254_740_993, 4);
    first_unrepresentable.fee = 1;
    assert_eq!(
        first_unrepresentable.compute_hash().to_hex(),
        "caef4f5305f968dd0b5b8f23a12d85644ba1b80328a0907e764aa59723e8146c"
    );

    let decoded: u64 = serde_json::from_str("9007199254740993")
        .expect("serde_json accepts an exact unquoted u64 token");
    assert_eq!(decoded, 9_007_199_254_740_993);

    let mut all_transfer_fields =
        Transaction::new_transfer(from, to, 9_007_199_254_740_993, 9_007_199_254_740_995);
    all_transfer_fields.fee = 9_007_199_254_740_997;
    assert_eq!(
        all_transfer_fields.compute_hash().to_hex(),
        "6218ebf83c6e14e18216c33cdfaa70ad189ffbb8cb78ffb35a187efef9e51261"
    );

    let deploy = Transaction::new_deploy(
        from,
        vec![0, 97, 115, 109],
        vec![1, 2],
        9_007_199_254_740_993,
        9_007_199_254_740_997,
        9_007_199_254_740_999,
        9_007_199_254_740_995,
    );
    assert_eq!(
        deploy.compute_hash().to_hex(),
        "3f3e08f8525892e8564bb488bb7867637444af9c926b950a7ef3b9f4d02253da"
    );
}
