pub const PRICES: &str = include_str!("../../data/prices.json");

#[cfg(test)]
mod tests {
    #[test]
    fn an_apple_costs_3() {
        assert!(super::PRICES.contains("\"apple\": 3"));
    }
}
