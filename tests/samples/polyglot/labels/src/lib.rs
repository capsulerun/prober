pub fn label(name: &str) -> String {
    name.to_uppercase()
}

#[cfg(test)]
mod tests {
    #[test]
    fn uppercases_the_name() {
        assert_eq!(super::label("pear"), "PEAR");
    }
}
