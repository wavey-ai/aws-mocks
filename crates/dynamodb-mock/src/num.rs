//! Exact decimal numbers for the `N` type.
//!
//! DynamoDB numbers carry up to 38 significant digits, which `f64` cannot hold (epoch
//! milliseconds and large counters lose precision). Values are kept as a digit string
//! and a power-of-ten exponent, so comparison and `+`/`-` are exact.

use std::cmp::Ordering;

/// `digits × 10^exp`, normalised: no leading or trailing zero digits; zero has no digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Num {
    neg: bool,
    digits: Vec<u8>,
    exp: i64,
}

const MAX_DIGITS: usize = 38;

impl Num {
    pub fn zero() -> Self {
        Num {
            neg: false,
            digits: Vec::new(),
            exp: 0,
        }
    }

    /// Parse a DynamoDB number literal (`-12`, `1.50`, `3e5`, `.5`).
    pub fn parse(text: &str) -> Option<Num> {
        let bytes = text.as_bytes();
        let mut index = 0;
        let mut neg = false;
        if index < bytes.len() && (bytes[index] == b'-' || bytes[index] == b'+') {
            neg = bytes[index] == b'-';
            index += 1;
        }
        let mut digits = Vec::new();
        let mut exp: i64 = 0;
        let mut seen_digit = false;
        let mut seen_point = false;
        while index < bytes.len() {
            match bytes[index] {
                c @ b'0'..=b'9' => {
                    seen_digit = true;
                    digits.push(c - b'0');
                    if seen_point {
                        exp -= 1;
                    }
                }
                b'.' if !seen_point => seen_point = true,
                b'e' | b'E' => break,
                _ => return None,
            }
            index += 1;
        }
        if !seen_digit {
            return None;
        }
        if index < bytes.len() {
            // Exponent part.
            let rest = std::str::from_utf8(&bytes[index + 1..]).ok()?;
            if rest.is_empty() || rest.len() > 12 {
                return None;
            }
            let shift: i64 = rest.parse().ok()?;
            exp = exp.checked_add(shift)?;
        }
        Some(Num::normalised(neg, digits, exp))
    }

    fn normalised(neg: bool, mut digits: Vec<u8>, mut exp: i64) -> Num {
        let lead = digits.iter().take_while(|d| **d == 0).count();
        digits.drain(..lead);
        while digits.last() == Some(&0) {
            digits.pop();
            exp += 1;
        }
        if digits.is_empty() {
            return Num::zero();
        }
        Num { neg, digits, exp }
    }

    pub fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    /// The DynamoDB range check: at most 38 significant digits, magnitude within
    /// 1E-130 ..= 9.99…E+125.
    pub fn check_range(&self) -> Result<(), &'static str> {
        if self.is_zero() {
            return Ok(());
        }
        if self.digits.len() > MAX_DIGITS {
            return Err("Attempting to store more than 38 significant digits in a Number");
        }
        let magnitude = self.digits.len() as i64 + self.exp - 1;
        if magnitude > 125 {
            return Err(
                "Number overflow. Attempting to store a number with magnitude larger than supported range",
            );
        }
        if magnitude < -130 {
            return Err(
                "Number underflow. Attempting to store a number with magnitude smaller than supported range",
            );
        }
        Ok(())
    }

    fn cmp_magnitude(&self, other: &Num) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let lead_a = self.digits.len() as i64 + self.exp;
        let lead_b = other.digits.len() as i64 + other.exp;
        lead_a
            .cmp(&lead_b)
            .then_with(|| self.digits.cmp(&other.digits))
    }

    /// Digits scaled to exponent `exp` (which must not exceed `self.exp`), least
    /// significant first.
    fn scaled_le(&self, exp: i64) -> Vec<u8> {
        let mut out = vec![0u8; (self.exp - exp) as usize];
        out.extend(self.digits.iter().rev());
        out
    }

    fn add_signed(&self, other: &Num, other_neg: bool) -> Num {
        if self.is_zero() {
            let mut result = other.clone();
            result.neg = other_neg && !other.is_zero();
            return result;
        }
        if other.is_zero() {
            return self.clone();
        }
        let exp = self.exp.min(other.exp);
        let a = self.scaled_le(exp);
        let b = other.scaled_le(exp);
        if self.neg == other_neg {
            let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
            let mut carry = 0u8;
            for i in 0..a.len().max(b.len()) {
                let sum = a.get(i).copied().unwrap_or(0) + b.get(i).copied().unwrap_or(0) + carry;
                out.push(sum % 10);
                carry = sum / 10;
            }
            if carry > 0 {
                out.push(carry);
            }
            out.reverse();
            return Num::normalised(self.neg, out, exp);
        }
        let (big, small, neg) = match self.cmp_magnitude(other) {
            Ordering::Equal => return Num::zero(),
            Ordering::Greater => (a, b, self.neg),
            Ordering::Less => (b, a, other_neg),
        };
        let mut out = Vec::with_capacity(big.len());
        let mut borrow = 0i8;
        for (i, digit) in big.iter().enumerate() {
            let mut diff = *digit as i8 - small.get(i).copied().unwrap_or(0) as i8 - borrow;
            borrow = 0;
            if diff < 0 {
                diff += 10;
                borrow = 1;
            }
            out.push(diff as u8);
        }
        out.reverse();
        Num::normalised(neg, out, exp)
    }

    pub fn add(&self, other: &Num) -> Num {
        self.add_signed(other, other.neg)
    }

    pub fn sub(&self, other: &Num) -> Num {
        self.add_signed(other, !other.neg)
    }
}

impl Ord for Num {
    fn cmp(&self, other: &Num) -> Ordering {
        let sign = |n: &Num| -> i8 {
            if n.is_zero() {
                0
            } else if n.neg {
                -1
            } else {
                1
            }
        };
        match sign(self).cmp(&sign(other)) {
            Ordering::Equal => {
                let magnitude = self.cmp_magnitude(other);
                if self.neg {
                    magnitude.reverse()
                } else {
                    magnitude
                }
            }
            unequal => unequal,
        }
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Num) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Num {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_zero() {
            return f.write_str("0");
        }
        let mut out = String::new();
        if self.neg {
            out.push('-');
        }
        let digits: String = self.digits.iter().map(|d| (b'0' + d) as char).collect();
        if self.exp >= 0 {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', self.exp as usize));
        } else {
            let point = digits.len() as i64 + self.exp;
            if point > 0 {
                out.push_str(&digits[..point as usize]);
                out.push('.');
                out.push_str(&digits[point as usize..]);
            } else {
                out.push_str("0.");
                out.extend(std::iter::repeat_n('0', (-point) as usize));
                out.push_str(&digits);
            }
        }
        f.write_str(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::Num;

    fn n(text: &str) -> Num {
        Num::parse(text).unwrap()
    }

    #[test]
    fn formats_normalised() {
        assert_eq!(n("1.50").to_string(), "1.5");
        assert_eq!(n("007").to_string(), "7");
        assert_eq!(n("1e3").to_string(), "1000");
        assert_eq!(n("-0.0").to_string(), "0");
        assert_eq!(n(".5").to_string(), "0.5");
        assert_eq!(n("-1.25E-3").to_string(), "-0.00125");
        assert!(Num::parse("abc").is_none());
        assert!(Num::parse("1.2.3").is_none());
        assert!(Num::parse("").is_none());
        assert!(Num::parse("1e").is_none());
    }

    #[test]
    fn compares_exactly() {
        assert!(n("1700000000000001") > n("1700000000000000"));
        assert!(
            n("12345678901234567890123456789012345678")
                > n("12345678901234567890123456789012345677")
        );
        assert!(n("-2") < n("-1"));
        assert!(n("-1") < n("0"));
        assert!(n("0.1") < n("1"));
        assert!(n("10") > n("9.99"));
        assert_eq!(n("1.0"), n("1"));
    }

    #[test]
    fn arithmetic() {
        assert_eq!(n("1").add(&n("2")).to_string(), "3");
        assert_eq!(n("1.5").add(&n("1.5")).to_string(), "3");
        assert_eq!(n("1").sub(&n("3")).to_string(), "-2");
        assert_eq!(n("-1").sub(&n("-1")).to_string(), "0");
        assert_eq!(n("0.1").add(&n("0.2")).to_string(), "0.3");
        assert_eq!(n("1700000000000").add(&n("1")).to_string(), "1700000000001");
        assert_eq!(n("99").add(&n("1")).to_string(), "100");
        assert_eq!(n("100").sub(&n("0.01")).to_string(), "99.99");
        assert_eq!(n("-5").add(&n("3")).to_string(), "-2");
    }
}
