//! Natural ("human") ordering of file names: `img2.jpg` < `img10.jpg`, case-insensitive, like
//! Windows Explorer.

use std::cmp::Ordering;

#[derive(Debug, PartialEq, Eq)]
enum Chunk<'a> {
    Text(&'a str),
    Number(&'a str),
}

fn chunks(s: &str) -> impl Iterator<Item = Chunk<'_>> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let first = rest.chars().next()?;
        let is_digit = first.is_ascii_digit();
        let end = rest.find(|c: char| c.is_ascii_digit() != is_digit).unwrap_or(rest.len());
        let (chunk, tail) = rest.split_at(end);
        rest = tail;
        Some(if is_digit { Chunk::Number(chunk) } else { Chunk::Text(chunk) })
    })
}

fn cmp_numbers(a: &str, b: &str) -> Ordering {
    let ta = a.trim_start_matches('0');
    let tb = b.trim_start_matches('0');
    // longer (without leading zeros) is larger; same length compares digit by digit;
    // equal values: fewer leading zeros first ("1" < "01")
    ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| a.len().cmp(&b.len()))
}

fn cmp_text(a: &str, b: &str) -> Ordering {
    let la = a.chars().flat_map(char::to_lowercase);
    let lb = b.chars().flat_map(char::to_lowercase);
    la.cmp(lb)
}

/// Natural comparison. Equal-looking names that differ only in case are ordered by their raw
/// bytes, so the order is total and stable.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ca = chunks(a);
    let mut cb = chunks(b);
    loop {
        let ord = match (ca.next(), cb.next()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(Chunk::Number(x)), Some(Chunk::Number(y))) => cmp_numbers(x, y),
            (Some(Chunk::Text(x)), Some(Chunk::Text(y))) => cmp_text(x, y),
            // digits sort before letters, as in Explorer
            (Some(Chunk::Number(_)), Some(Chunk::Text(_))) => Ordering::Less,
            (Some(Chunk::Text(_)), Some(Chunk::Number(_))) => Ordering::Greater,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    #[test]
    fn numbers_compare_by_value() {
        assert_eq!(
            sorted(vec!["img10.jpg", "img2.jpg", "img1.jpg", "img100.jpg", "img20.jpg"]),
            vec!["img1.jpg", "img2.jpg", "img10.jpg", "img20.jpg", "img100.jpg"]
        );
    }

    #[test]
    fn case_insensitive_with_stable_ties() {
        assert_eq!(sorted(vec!["b.png", "A.png", "a.png", "C.png"]), vec!["A.png", "a.png", "b.png", "C.png"]);
    }

    #[test]
    fn leading_zeros_and_long_numbers() {
        assert_eq!(sorted(vec!["p010", "p9", "p01", "p1"]), vec!["p1", "p01", "p9", "p010"]);
        // longer than u64 must not overflow
        assert_eq!(natural_cmp("x123456789012345678901234567890", "x123456789012345678901234567891"), Ordering::Less);
    }

    #[test]
    fn chapter_style_names() {
        assert_eq!(
            sorted(vec!["ch 2 p 10.jpg", "ch 10 p 1.jpg", "ch 2 p 9.jpg", "ch 1.jpg"]),
            vec!["ch 1.jpg", "ch 2 p 9.jpg", "ch 2 p 10.jpg", "ch 10 p 1.jpg"]
        );
    }

    #[test]
    fn non_ascii_text_is_compared_case_insensitively() {
        assert_eq!(natural_cmp("Ärger", "ärger2"), Ordering::Less);
        assert_eq!(sorted(vec!["사진10", "사진2"]), vec!["사진2", "사진10"]);
    }
}
