pub(crate) fn rounded_ratio(part: f64, whole: f64) -> f64 {
    if whole == 0.0 {
        return 0.0;
    }

    let exact_ratio = part / whole;

    (exact_ratio * 10_000.0).round() / 10_000.0
}

pub(crate) fn format_percentage(part: f64, whole: f64) -> String {
    let percentage = if whole == 0.0 {
        0.0
    } else {
        part * 100.0 / whole
    };

    format!("{percentage:.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratios_are_rounded_to_four_decimals_and_zero_without_a_whole() {
        assert_eq!(rounded_ratio(246.0, 19_104.0), 0.0129);
        assert_eq!(rounded_ratio(0.0, 0.0), 0.0);
    }

    #[test]
    fn percentages_have_one_decimal() {
        assert_eq!(format_percentage(1.0, 3.0), "33.3%");
        assert_eq!(format_percentage(0.0, 0.0), "0.0%");
    }
}
