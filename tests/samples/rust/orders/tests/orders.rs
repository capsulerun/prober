#[test]
fn totals_the_order() {
    assert_eq!(orders::order_total(&[2, 2]), 4);
}
