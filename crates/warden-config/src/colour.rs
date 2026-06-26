use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Error, PartialEq)]
pub enum ColourError {
    #[error("colour must start with '#': {0:?}")]
    NoHash(String),
    #[error("colour must be #rgb or #rrggbb: {0:?}")]
    BadLength(String),
    #[error("colour has non-hex digits: {0:?}")]
    BadDigit(String),
}

impl Colour {
    pub fn parse(s: &str) -> Result<Colour, ColourError> {
        let rest = s
            .strip_prefix('#')
            .ok_or_else(|| ColourError::NoHash(s.to_string()))?;
        let expand = |h: &str| u8::from_str_radix(h, 16);
        match rest.len() {
            3 => {
                let mut it = rest.chars().map(|c| {
                    let d = c.to_string();
                    expand(&format!("{d}{d}"))
                });
                let mut next = || it.next().unwrap();
                let r = next().map_err(|_| ColourError::BadDigit(s.to_string()))?;
                let g = next().map_err(|_| ColourError::BadDigit(s.to_string()))?;
                let b = next().map_err(|_| ColourError::BadDigit(s.to_string()))?;
                Ok(Colour { r, g, b })
            }
            6 => {
                let r = expand(&rest[0..2]).map_err(|_| ColourError::BadDigit(s.to_string()))?;
                let g = expand(&rest[2..4]).map_err(|_| ColourError::BadDigit(s.to_string()))?;
                let b = expand(&rest[4..6]).map_err(|_| ColourError::BadDigit(s.to_string()))?;
                Ok(Colour { r, g, b })
            }
            _ => Err(ColourError::BadLength(s.to_string())),
        }
    }

    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_six_digit() {
        assert_eq!(Colour::parse("#0F8A8A").unwrap(), Colour { r: 15, g: 138, b: 138 });
    }

    #[test]
    fn parses_three_digit_shorthand() {
        assert_eq!(Colour::parse("#0a0").unwrap(), Colour { r: 0, g: 170, b: 0 });
    }

    #[test]
    fn round_trips_to_lowercase_hex() {
        assert_eq!(Colour::parse("#0F8A8A").unwrap().hex(), "#0f8a8a");
    }

    #[test]
    fn rejects_missing_hash() {
        assert_eq!(Colour::parse("0f8a8a"), Err(ColourError::NoHash("0f8a8a".into())));
    }

    #[test]
    fn rejects_bad_length() {
        assert!(matches!(Colour::parse("#ff"), Err(ColourError::BadLength(_))));
    }

    #[test]
    fn rejects_non_hex() {
        assert!(matches!(Colour::parse("#gggggg"), Err(ColourError::BadDigit(_))));
    }
}
