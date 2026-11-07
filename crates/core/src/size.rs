/// Parses a byte size such as `10MiB`, `512KiB`, `1GiB` or `1048576`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let number: u64 = number.parse().map_err(|_| format!("invalid size `{s}`"))?;
    let factor: u64 = match unit.trim() {
        "" | "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => {
            return Err(format!(
                "invalid size unit in `{s}` (use B, KiB, MiB or GiB)"
            ));
        }
    };
    number
        .checked_mul(factor)
        .ok_or_else(|| format!("size `{s}` is too large"))
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1048576"), Ok(1 << 20));
        assert_eq!(parse_size("10MiB"), Ok(10 << 20));
        assert_eq!(parse_size("512 KiB"), Ok(512 << 10));
        assert_eq!(parse_size("1GiB"), Ok(1 << 30));
        for bad in ["", "MiB", "10MB", "-1", "99999999999999999999GiB"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }
}
