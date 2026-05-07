//! NBM (National Blend of Models) probabilistic-forecast client.
//!
//! Different from [`crate::nws_forecast`]:
//! - that's a deterministic point forecast (single number per hour);
//! - this is the **quantile** model data carrying a 21-step CDF
//!   (0%, 5%, …, 100%) per parameter per forecast hour. The CDF
//!   lets us answer `P(T_2m > X)` for any Kalshi-side threshold X
//!   via linear interpolation between adjacent quantiles.
//!
//! See `docs/WX_STAT_NBM_PHASE2.md` for the full design.
//!
//! ## API surface
//!
//! ```text
//! let client = NbmClient::new("(myapp.com, contact@example.com)")?;
//! let cycle = NbmCycle { date: ..., hour: 12 };
//! let idx = client.fetch_index(cycle, 24, "co", "qmd").await?;
//! let msgs = locate_quantile_messages(&idx, "TMP", "2 m above ground");
//! //   → [(0, range), (5, range), ..., (100, range)]
//! let bytes = client.fetch_message(cycle, 24, "co", "qmd", &msgs[0].1).await?;
//! ```
//!
//! ## Endpoint
//!
//! NOAA hosts NBM as a public-read S3 bucket; no auth required.
//! See [`DEFAULT_BASE`] and [`docs/WX_STAT_NBM_PHASE2.md`] for the
//! canonical URL shape. Range requests are validated as working —
//! the full file is ~600 MB but each quantile message is ~5 MB,
//! so we always fetch by byte range to stay within reasonable
//! bandwidth.

use crate::error::Error;
use std::time::Duration;
use tracing::{debug, info};

/// Public-read S3 endpoint hosting NBM GRIB2 + idx files. Set with
/// [`NbmClient::with_base`] for tests against a local mock.
pub const DEFAULT_BASE: &str = "https://noaa-nbm-grib2-pds.s3.amazonaws.com";

#[derive(Debug, Clone)]
pub struct NbmClient {
    http: reqwest::Client,
    base: String,
}

impl NbmClient {
    /// Build a client. `user_agent` is required — NWS / NOAA assets
    /// don't reject anonymous traffic on the S3 bucket, but it's
    /// best practice to identify the caller. Format:
    /// `"(myapp.com, contact@example.com)"`.
    pub fn new(user_agent: &str) -> Result<Self, Error> {
        if user_agent.trim().is_empty() {
            return Err(Error::Invalid(
                "NBM client requires a non-empty User-Agent".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .user_agent(user_agent.to_string())
            .timeout(Duration::from_mins(1))
            .build()?;
        Ok(Self {
            http,
            base: DEFAULT_BASE.into(),
        })
    }

    #[must_use]
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// Fetch and parse the `.grib2.idx` sidecar for one cycle ×
    /// forecast hour × region × product. Returns one entry per GRIB
    /// message. Cheap (~10 KB).
    pub async fn fetch_index(
        &self,
        cycle: NbmCycle,
        fcst_hour: u16,
        region: &str,
        product: &str,
    ) -> Result<Vec<IdxEntry>, Error> {
        let url = idx_url(&self.base, cycle, fcst_hour, region, product);
        debug!(%url, "nbm: fetch_index");
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                body,
            });
        }
        let text = resp.text().await?;
        parse_idx(&text)
    }

    /// Fetch the bytes of one GRIB message via HTTP range request.
    /// `range` is the byte half-open range; pass it through from
    /// [`IdxEntry::range_with`] which works out the ending byte
    /// from the next message's offset (or `None` for the file's
    /// last message — in that case we issue an open-ended range).
    pub async fn fetch_message(
        &self,
        cycle: NbmCycle,
        fcst_hour: u16,
        region: &str,
        product: &str,
        range: &MessageRange,
    ) -> Result<Vec<u8>, Error> {
        let url = grib_url(&self.base, cycle, fcst_hour, region, product);
        let header_value = match range.end_exclusive {
            Some(end) => format!("bytes={}-{}", range.start, end - 1),
            None => format!("bytes={}-", range.start),
        };
        debug!(%url, header = %header_value, "nbm: fetch_message");
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::RANGE, header_value)
            .send()
            .await?;
        let status = resp.status();
        // Allow both 206 Partial Content and 200 (some proxies
        // collapse range requests to whole-file responses).
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?.to_vec();
        info!(
            len = bytes.len(),
            range = ?range,
            "nbm: fetched message"
        );
        Ok(bytes)
    }
}

/// Cycle = (run_date, run_hour). NBM runs hourly UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NbmCycle {
    /// Year (e.g. 2026).
    pub year: u16,
    /// Month 1..=12.
    pub month: u8,
    /// Day-of-month 1..=31.
    pub day: u8,
    /// Cycle hour 0..=23 UTC.
    pub hour: u8,
}

impl NbmCycle {
    /// Format as the bucket-prefix string `blend.YYYYMMDD/CC`.
    /// Doesn't include the trailing slash.
    pub fn prefix(self) -> String {
        format!(
            "blend.{:04}{:02}{:02}/{:02}",
            self.year, self.month, self.day, self.hour
        )
    }
}

/// One entry from a parsed `.grib2.idx` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdxEntry {
    /// 1-indexed message number within the file.
    pub msg_num: usize,
    /// Byte offset where this message starts.
    pub offset: u64,
    /// Cycle datetime token (e.g. `"d=2026050612"`).
    pub cycle_token: String,
    /// Parameter name (e.g. `"TMP"`, `"APTMP"`, `"DPT"`).
    pub param: String,
    /// Level description (e.g. `"2 m above ground"`, `"surface"`).
    pub level: String,
    /// Forecast-hour label (e.g. `"24 hour fcst"`).
    pub fcst_label: String,
    /// Anything trailing the standard fields. For probabilistic
    /// quantile messages this carries the quantile label
    /// (`"50% level"`) or threshold (`"prob >305.372"`).
    pub extra: String,
}

impl IdxEntry {
    /// Look up the quantile percentage if this entry is a quantile-
    /// level message. Returns `None` for non-quantile messages.
    /// The label format is `"NN% level"` where `NN` is 0..=100 in
    /// 5-step increments.
    pub fn quantile_pct(&self) -> Option<u8> {
        let s = self.extra.trim();
        let pct = s.strip_suffix("% level")?;
        let n: u8 = pct.parse().ok()?;
        Some(n)
    }

    /// Half-open byte range for fetching this message, given the
    /// next message's offset (`None` if this is the last message).
    /// Use [`build_message_ranges`] to compute these in one pass
    /// across a full idx.
    pub fn range_with(&self, next_offset: Option<u64>) -> MessageRange {
        MessageRange {
            start: self.offset,
            end_exclusive: next_offset,
        }
    }
}

/// Half-open byte range describing one GRIB message inside the
/// concatenated `.grib2` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRange {
    pub start: u64,
    pub end_exclusive: Option<u64>,
}

impl MessageRange {
    pub fn len_bytes(&self) -> Option<u64> {
        self.end_exclusive.map(|e| e.saturating_sub(self.start))
    }
}

/// Find every quantile-level message in `idx` for a given parameter
/// + level filter (e.g. `("TMP", "2 m above ground")`). Returns
///   `(quantile_pct, range)` pairs sorted by quantile.
pub fn locate_quantile_messages(
    idx: &[IdxEntry],
    param: &str,
    level: &str,
) -> Vec<(u8, MessageRange)> {
    let ranges = build_message_ranges(idx);
    let mut out: Vec<(u8, MessageRange)> = idx
        .iter()
        .zip(ranges.iter())
        .filter_map(|(e, r)| {
            if e.param == param && e.level == level {
                e.quantile_pct().map(|pct| (pct, r.clone()))
            } else {
                None
            }
        })
        .collect();
    out.sort_by_key(|(pct, _)| *pct);
    out
}

/// Find one threshold-probability message: parameter + level +
/// `prob >X` or `prob <X`. Used as a sanity-check at integration
/// time — the deterministic threshold-prob fields exist alongside
/// the quantiles and we can cross-check our CDF interpolation
/// against them.
pub fn locate_threshold_message(
    idx: &[IdxEntry],
    param: &str,
    level: &str,
    extra_prefix: &str,
) -> Option<MessageRange> {
    let ranges = build_message_ranges(idx);
    idx.iter().zip(ranges.iter()).find_map(|(e, r)| {
        if e.param == param && e.level == level && e.extra.starts_with(extra_prefix) {
            Some(r.clone())
        } else {
            None
        }
    })
}

/// Build the per-message byte range list from a parsed idx.
/// The last message's `end_exclusive` is `None` (open-ended).
pub fn build_message_ranges(idx: &[IdxEntry]) -> Vec<MessageRange> {
    let mut ranges = Vec::with_capacity(idx.len());
    for (i, entry) in idx.iter().enumerate() {
        let next = idx.get(i + 1).map(|e| e.offset);
        ranges.push(entry.range_with(next));
    }
    ranges
}

fn idx_url(base: &str, cycle: NbmCycle, fcst_hour: u16, region: &str, product: &str) -> String {
    format!(
        "{base}/{prefix}/{product}/blend.t{cc:02}z.{product}.f{hhh:03}.{region}.grib2.idx",
        prefix = cycle.prefix(),
        cc = cycle.hour,
        hhh = fcst_hour,
    )
}

fn grib_url(base: &str, cycle: NbmCycle, fcst_hour: u16, region: &str, product: &str) -> String {
    format!(
        "{base}/{prefix}/{product}/blend.t{cc:02}z.{product}.f{hhh:03}.{region}.grib2",
        prefix = cycle.prefix(),
        cc = cycle.hour,
        hhh = fcst_hour,
    )
}

/// Pure-text idx parser. Format per line:
/// `<msg_num>:<offset>:d=<cycle>:<param>:<level>:<fcst_label>:[<extra>]`
///
/// The trailing `extra` field is everything after the 6th colon
/// (joined back together) — it can itself contain colons (e.g.
/// `"prob fcst 255/255:probability forecast"`).
pub fn parse_idx(text: &str) -> Result<Vec<IdxEntry>, Error> {
    let mut out = Vec::new();
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let entry = parse_idx_line(line).ok_or_else(|| {
            Error::Invalid(format!(
                "nbm idx line {} malformed: {line:?}",
                line_no + 1
            ))
        })?;
        out.push(entry);
    }
    Ok(out)
}

fn parse_idx_line(line: &str) -> Option<IdxEntry> {
    // Split on ':' but keep at most 6 splits — anything past the
    // first 6 colons stays part of `extra`.
    let mut parts = line.splitn(7, ':');
    let msg_num: usize = parts.next()?.parse().ok()?;
    let offset: u64 = parts.next()?.parse().ok()?;
    let cycle_token = parts.next()?.to_string();
    let param = parts.next()?.to_string();
    let level = parts.next()?.to_string();
    let fcst_label = parts.next()?.to_string();
    // The trailing extra may be empty (no field after the 6th colon).
    let extra = parts.next().unwrap_or("").trim_end_matches(':').to_string();
    Some(IdxEntry {
        msg_num,
        offset,
        cycle_token,
        param,
        level,
        fcst_label,
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_prefix_zero_pads() {
        let c = NbmCycle {
            year: 2026,
            month: 5,
            day: 6,
            hour: 12,
        };
        assert_eq!(c.prefix(), "blend.20260506/12");
    }

    #[test]
    fn cycle_prefix_zero_pads_single_digit() {
        let c = NbmCycle {
            year: 2026,
            month: 1,
            day: 7,
            hour: 0,
        };
        assert_eq!(c.prefix(), "blend.20260107/00");
    }

    #[test]
    fn parse_idx_handles_threshold_prob_lines() {
        // The trickiest format — `prob >X` and `prob fcst N/M` and
        // `probability forecast` all packed into the trailing
        // colon-separated tail.
        let line = "239:389547015:d=2026050612:TMP:2 m above ground:24 hour fcst:prob >305.372:prob fcst 255/255";
        let e = parse_idx_line(line).unwrap();
        assert_eq!(e.msg_num, 239);
        assert_eq!(e.offset, 389_547_015);
        assert_eq!(e.cycle_token, "d=2026050612");
        assert_eq!(e.param, "TMP");
        assert_eq!(e.level, "2 m above ground");
        assert_eq!(e.fcst_label, "24 hour fcst");
        assert_eq!(e.extra, "prob >305.372:prob fcst 255/255");
    }

    #[test]
    fn parse_idx_handles_quantile_lines() {
        let line = "243:391420435:d=2026050612:TMP:2 m above ground:24 hour fcst:0% level";
        let e = parse_idx_line(line).unwrap();
        assert_eq!(e.extra, "0% level");
        assert_eq!(e.quantile_pct(), Some(0));
    }

    #[test]
    fn parse_idx_handles_simple_lines_with_no_extra() {
        // Some messages have nothing past the 6th colon (or just a
        // trailing colon) — non-quantile, non-prob deterministic
        // forecasts.
        let line = "194:162353299:d=2026050612:TMP:surface:24 hour fcst:";
        let e = parse_idx_line(line).unwrap();
        assert_eq!(e.extra, "");
        assert_eq!(e.quantile_pct(), None);
    }

    #[test]
    fn parse_idx_rejects_bad_format() {
        assert!(parse_idx_line("not-an-idx-line").is_none());
        assert!(parse_idx_line("1:not-a-number:d=...:TMP:s:f:").is_none());
    }

    #[test]
    fn build_message_ranges_caps_with_next_offset() {
        let entries = vec![
            IdxEntry {
                msg_num: 1,
                offset: 0,
                cycle_token: "d=...".into(),
                param: "TMP".into(),
                level: "surface".into(),
                fcst_label: "24 hour fcst".into(),
                extra: String::new(),
            },
            IdxEntry {
                msg_num: 2,
                offset: 1000,
                cycle_token: "d=...".into(),
                param: "DPT".into(),
                level: "surface".into(),
                fcst_label: "24 hour fcst".into(),
                extra: String::new(),
            },
            IdxEntry {
                msg_num: 3,
                offset: 2500,
                cycle_token: "d=...".into(),
                param: "WIND".into(),
                level: "surface".into(),
                fcst_label: "24 hour fcst".into(),
                extra: String::new(),
            },
        ];
        let ranges = build_message_ranges(&entries);
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[0].end_exclusive, Some(1000));
        assert_eq!(ranges[0].len_bytes(), Some(1000));
        assert_eq!(ranges[1].start, 1000);
        assert_eq!(ranges[1].end_exclusive, Some(2500));
        // Last is open-ended.
        assert_eq!(ranges[2].start, 2500);
        assert_eq!(ranges[2].end_exclusive, None);
        assert_eq!(ranges[2].len_bytes(), None);
    }

    #[test]
    fn locate_quantile_messages_returns_only_quantile_entries_sorted() {
        let entries = vec![
            // Non-matching param.
            IdxEntry {
                msg_num: 1,
                offset: 0,
                cycle_token: "d=...".into(),
                param: "DPT".into(),
                level: "2 m above ground".into(),
                fcst_label: "24 hour fcst".into(),
                extra: "50% level".into(),
            },
            // Matching param + level + 80% level.
            IdxEntry {
                msg_num: 2,
                offset: 1000,
                cycle_token: "d=...".into(),
                param: "TMP".into(),
                level: "2 m above ground".into(),
                fcst_label: "24 hour fcst".into(),
                extra: "80% level".into(),
            },
            // Threshold-probability — should NOT be included.
            IdxEntry {
                msg_num: 3,
                offset: 2000,
                cycle_token: "d=...".into(),
                param: "TMP".into(),
                level: "2 m above ground".into(),
                fcst_label: "24 hour fcst".into(),
                extra: "prob >305.372:prob fcst 255/255".into(),
            },
            // Matching param + level + 5% level (smaller pct → first).
            IdxEntry {
                msg_num: 4,
                offset: 3000,
                cycle_token: "d=...".into(),
                param: "TMP".into(),
                level: "2 m above ground".into(),
                fcst_label: "24 hour fcst".into(),
                extra: "5% level".into(),
            },
        ];
        let q = locate_quantile_messages(&entries, "TMP", "2 m above ground");
        assert_eq!(q.len(), 2);
        // Sorted by pct ascending.
        assert_eq!(q[0].0, 5);
        assert_eq!(q[1].0, 80);
    }

    #[test]
    fn idx_url_format_matches_bucket_layout() {
        let cycle = NbmCycle {
            year: 2026,
            month: 5,
            day: 6,
            hour: 12,
        };
        let url = idx_url("https://x.example", cycle, 24, "co", "qmd");
        assert_eq!(
            url,
            "https://x.example/blend.20260506/12/qmd/blend.t12z.qmd.f024.co.grib2.idx"
        );
    }

    #[test]
    fn grib_url_format_matches_bucket_layout() {
        let cycle = NbmCycle {
            year: 2026,
            month: 5,
            day: 6,
            hour: 12,
        };
        let url = grib_url("https://x.example", cycle, 24, "co", "qmd");
        assert_eq!(
            url,
            "https://x.example/blend.20260506/12/qmd/blend.t12z.qmd.f024.co.grib2"
        );
    }

    #[test]
    fn rejects_empty_user_agent() {
        assert!(NbmClient::new("").is_err());
        assert!(NbmClient::new("   ").is_err());
    }
}
