pub fn slug(text: &str) -> String {
    text.to_lowercase().replace(' ', "-")
}

#[cfg(test)]
mod tests {
    #[test]
    fn joins_the_words_with_dashes() {
        assert_eq!(super::slug("Hello World"), "hello-world");
    }
}
