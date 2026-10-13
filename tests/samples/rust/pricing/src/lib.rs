pub fn total(prices: &[u32]) -> u32 {
    prices.iter().sum()
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds_the_prices() {
        assert_eq!(super::total(&[1, 2, 3]), 6);
    }
}
