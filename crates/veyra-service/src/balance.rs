//! Real account-balance observations and bounded chart sampling.
//!
//! These are broker-reported `AccountBalance()` values, not returns attributable
//! to Veyra. No point is inferred from trades or equity.

use serde::Serialize;

/// One broker-reported account balance at the service observation time.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BalancePoint {
    /// Unix milliseconds when the service received the observation.
    pub at_ms: u64,
    /// Balance in the account's (currently unreported) currency.
    pub balance: f64,
}

/// Identity of the account that supplied a balance history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BalanceAccount {
    /// Positive broker account number.
    pub login: u64,
    /// Validated broker server name.
    pub server: String,
}

/// Response for the read-only balance-history endpoint.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceHistory {
    /// `ok`, `disabled`, or `waiting_for_account`.
    pub status: &'static str,
    /// The only source used for this series.
    pub source: &'static str,
    /// Active account, if one has reported in this process.
    pub account: Option<BalanceAccount>,
    /// Requested lookback window.
    pub days: u16,
    /// Configured audit retention; zero means no automatic pruning.
    pub retention_days: u32,
    /// The EA does not currently report account currency.
    pub currency: Option<&'static str>,
    /// Chronological, actual observations only.
    pub points: Vec<BalancePoint>,
    /// First returned observation time.
    pub first_observed_at_ms: Option<u64>,
    /// Last returned observation time.
    pub last_observed_at_ms: Option<u64>,
    /// Whether the real observation set was reduced for the chart.
    pub sampled: bool,
    /// Whether the link and latest persisted observation are both recent.
    pub fresh: bool,
}

/// Largest number of real observations returned to a chart request.
pub const MAX_POINTS: usize = 1_000;

/// Reduces a chronological history while preserving true extrema and edges.
///
/// Four actual points at most are retained per time bucket: its first, last,
/// minimum, and maximum. No values or timestamps are interpolated.
pub fn sample_points(points: Vec<BalancePoint>) -> (Vec<BalancePoint>, bool) {
    if points.len() <= MAX_POINTS {
        return (points, false);
    }
    let first_at = points.first().map(|point| point.at_ms).unwrap_or(0);
    let last_at = points.last().map(|point| point.at_ms).unwrap_or(first_at);
    let span = last_at.saturating_sub(first_at).saturating_add(1);
    let mut selected = Vec::with_capacity(MAX_POINTS);
    let mut bucket_start = 0;
    while bucket_start < points.len() {
        let bucket = bucket_for(points[bucket_start].at_ms, first_at, span);
        let mut bucket_end = bucket_start + 1;
        while bucket_end < points.len()
            && bucket_for(points[bucket_end].at_ms, first_at, span) == bucket
        {
            bucket_end += 1;
        }
        let slice = &points[bucket_start..bucket_end];
        let mut indices = [0, slice.len() - 1, 0, 0];
        for (index, point) in slice.iter().enumerate() {
            if point.balance < slice[indices[2]].balance {
                indices[2] = index;
            }
            if point.balance > slice[indices[3]].balance {
                indices[3] = index;
            }
        }
        indices.sort_unstable();
        for (offset, index) in indices.into_iter().enumerate() {
            if offset == 0 || index != indices[offset - 1] {
                selected.push(slice[index].clone());
            }
        }
        bucket_start = bucket_end;
    }
    (selected, true)
}

fn bucket_for(at_ms: u64, first_at: u64, span: u64) -> u64 {
    let offset = at_ms.saturating_sub(first_at);
    ((u128::from(offset) * u128::from((MAX_POINTS / 4) as u64)) / u128::from(span)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_keeps_real_edges_and_extrema() {
        let points: Vec<_> = (0..20_000)
            .map(|index| BalancePoint {
                at_ms: index * 1_000,
                balance: if index == 9_999 {
                    1_000.0
                } else if index == 15_000 {
                    0.0
                } else {
                    20.0
                },
            })
            .collect();
        let (sampled, reduced) = sample_points(points.clone());
        assert!(reduced);
        assert!(sampled.len() <= MAX_POINTS);
        assert_eq!(sampled.first(), points.first());
        assert_eq!(sampled.last(), points.last());
        assert!(sampled.contains(&points[9_999]));
        assert!(sampled.contains(&points[15_000]));
        assert!(
            sampled
                .windows(2)
                .all(|pair| pair[0].at_ms <= pair[1].at_ms)
        );
    }

    #[test]
    fn short_series_is_not_changed() {
        let points = vec![BalancePoint {
            at_ms: 1,
            balance: 20.0,
        }];
        assert_eq!(sample_points(points.clone()), (points, false));
    }
}
