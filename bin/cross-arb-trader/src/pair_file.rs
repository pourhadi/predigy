//! Parse the `KALSHI_TICKER=POLY_ASSET_ID` pair file written by
//! `cross-arb-curator`. Comments (`#`) and blank lines are ignored.

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("line {line_no}: {reason}")]
    Malformed { line_no: usize, reason: String },
}

/// Read `path` and return the parsed pair map. Caller diffs this
/// against its current state to figure out add/remove deltas.
pub fn read(path: &Path) -> Result<HashMap<String, String>, ParseError> {
    let text = std::fs::read_to_string(path)?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<HashMap<String, String>, ParseError> {
    let mut out = HashMap::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (k, p) = line.split_once('=').ok_or_else(|| ParseError::Malformed {
            line_no,
            reason: format!("expected KALSHI=POLY, got {raw:?}"),
        })?;
        let k = k.trim();
        let p = p.trim();
        if k.is_empty() || p.is_empty() {
            return Err(ParseError::Malformed {
                line_no,
                reason: "empty side".into(),
            });
        }
        out.insert(k.to_string(), p.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_comments_and_blanks() {
        let s = "
            # comment
            FOO=bar

            BAZ=quux  # inline
        ";
        let m = parse(s).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(m.get("BAZ"), Some(&"quux".to_string()));
    }

    #[test]
    fn rejects_missing_equals() {
        assert!(parse("BADLINE\n").is_err());
    }

    #[test]
    fn rejects_empty_side() {
        assert!(parse("=poly\n").is_err());
        assert!(parse("kal=\n").is_err());
    }
}
