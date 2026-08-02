use crate::app::MetricsSnapshot;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetricKey {
    TotalTransactions,
    ReadOnlyTransactions,
    CommitLatency,
    OccConflicts,
    QueryTimeouts,
    TotalDpu,
    ReadDpu,
    WriteDpu,
    ComputeDpu,
    MultiRegionWriteDpu,
    BytesRead,
    BytesWritten,
    ComputeTime,
    ClusterStorageSize,
    ActiveConnections,
    AdminConnectionAttempts,
    CustomRoleConnectionAttempts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetricUnit {
    Count,
    Percent,
    Milliseconds,
    Dpu,
    Bytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MetricSpec {
    pub(crate) key: MetricKey,
    pub(crate) provider_id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) unit: MetricUnit,
}

pub(crate) const METRIC_SPECS: [MetricSpec; 17] = [
    MetricSpec {
        key: MetricKey::TotalTransactions,
        provider_id: "total_transactions",
        label: "Total transactions",
        unit: MetricUnit::Count,
    },
    MetricSpec {
        key: MetricKey::ReadOnlyTransactions,
        provider_id: "read_only_transactions",
        label: "Read-only transactions",
        unit: MetricUnit::Count,
    },
    MetricSpec {
        key: MetricKey::CommitLatency,
        provider_id: "commit_latency",
        label: "Commit latency",
        unit: MetricUnit::Milliseconds,
    },
    MetricSpec {
        key: MetricKey::OccConflicts,
        provider_id: "occ_conflicts",
        label: "OCC conflicts",
        unit: MetricUnit::Count,
    },
    MetricSpec {
        key: MetricKey::QueryTimeouts,
        provider_id: "query_timeouts",
        label: "Query timeouts",
        unit: MetricUnit::Count,
    },
    MetricSpec {
        key: MetricKey::TotalDpu,
        provider_id: "total_dpu",
        label: "Total DPU",
        unit: MetricUnit::Dpu,
    },
    MetricSpec {
        key: MetricKey::ReadDpu,
        provider_id: "read_dpu",
        label: "Read DPU",
        unit: MetricUnit::Dpu,
    },
    MetricSpec {
        key: MetricKey::WriteDpu,
        provider_id: "write_dpu",
        label: "Write DPU",
        unit: MetricUnit::Dpu,
    },
    MetricSpec {
        key: MetricKey::ComputeDpu,
        provider_id: "compute_dpu",
        label: "Compute DPU",
        unit: MetricUnit::Dpu,
    },
    MetricSpec {
        key: MetricKey::MultiRegionWriteDpu,
        provider_id: "multi_region_write_dpu",
        label: "Multi-Region write DPU",
        unit: MetricUnit::Dpu,
    },
    MetricSpec {
        key: MetricKey::BytesRead,
        provider_id: "bytes_read",
        label: "Bytes read",
        unit: MetricUnit::Bytes,
    },
    MetricSpec {
        key: MetricKey::BytesWritten,
        provider_id: "bytes_written",
        label: "Bytes written",
        unit: MetricUnit::Bytes,
    },
    MetricSpec {
        key: MetricKey::ComputeTime,
        provider_id: "compute_time",
        label: "Compute time",
        unit: MetricUnit::Milliseconds,
    },
    MetricSpec {
        key: MetricKey::ClusterStorageSize,
        provider_id: "cluster_storage_size",
        label: "Cluster storage",
        unit: MetricUnit::Bytes,
    },
    MetricSpec {
        key: MetricKey::ActiveConnections,
        provider_id: "active_connections",
        label: "Active connections",
        unit: MetricUnit::Count,
    },
    MetricSpec {
        key: MetricKey::AdminConnectionAttempts,
        provider_id: "admin_connection_attempts",
        label: "Admin connection attempts",
        unit: MetricUnit::Count,
    },
    MetricSpec {
        key: MetricKey::CustomRoleConnectionAttempts,
        provider_id: "custom_role_connection_attempts",
        label: "Custom-role connection attempts",
        unit: MetricUnit::Count,
    },
];

pub(crate) struct DashboardModel<'a> {
    snapshot: &'a MetricsSnapshot,
}

impl<'a> DashboardModel<'a> {
    pub(crate) const fn new(snapshot: &'a MetricsSnapshot) -> Self {
        Self { snapshot }
    }

    pub(crate) fn series(&self, key: MetricKey) -> Option<&'a [Option<f64>]> {
        let provider_id = metric_spec(key).provider_id;
        self.snapshot
            .series
            .iter()
            .find(|series| series.metric == provider_id)
            .map(|series| series.samples.as_slice())
    }

    pub(crate) fn latest(&self, key: MetricKey) -> Option<f64> {
        self.series(key)
            .and_then(|samples| samples.last())
            .copied()
            .flatten()
            .filter(|value| value.is_finite())
    }

    pub(crate) fn read_only_ratio(&self) -> Vec<Option<f64>> {
        let total = self
            .series(MetricKey::TotalTransactions)
            .unwrap_or_default();
        let read_only = self
            .series(MetricKey::ReadOnlyTransactions)
            .unwrap_or_default();
        (0..total.len().max(read_only.len()))
            .map(|index| match (total.get(index), read_only.get(index)) {
                (Some(Some(total)), Some(Some(read_only)))
                    if total.is_finite() && read_only.is_finite() && *total > 0.0 =>
                {
                    Some(read_only / total * 100.0)
                }
                _ => None,
            })
            .collect()
    }

    pub(crate) fn latest_read_only_ratio(&self) -> Option<f64> {
        self.read_only_ratio().last().copied().flatten()
    }

    pub(crate) fn chart_segments(&self, key: MetricKey) -> Vec<Vec<(f64, f64)>> {
        let mut segments = Vec::new();
        let mut segment = Vec::new();
        for (index, sample) in self.series(key).unwrap_or_default().iter().enumerate() {
            match sample.filter(|value| value.is_finite()) {
                Some(value) => segment.push((index as f64, value)),
                None if !segment.is_empty() => segments.push(std::mem::take(&mut segment)),
                None => {}
            }
        }
        if !segment.is_empty() {
            segments.push(segment);
        }
        segments
    }

    pub(crate) fn sparkline_data(&self, key: MetricKey) -> Vec<Option<u64>> {
        let samples = self.series(key).unwrap_or_default();
        let max = samples
            .iter()
            .flatten()
            .copied()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .fold(0.0_f64, f64::max);
        samples
            .iter()
            .map(|sample| {
                sample.and_then(|value| {
                    if !value.is_finite() || value < 0.0 {
                        None
                    } else if max == 0.0 {
                        Some(0)
                    } else {
                        Some((value / max * 100.0).round() as u64)
                    }
                })
            })
            .collect()
    }
}

pub(crate) fn metric_spec(key: MetricKey) -> &'static MetricSpec {
    METRIC_SPECS
        .iter()
        .find(|spec| spec.key == key)
        .expect("every MetricKey must have a MetricSpec")
}

pub(crate) fn format_value(unit: MetricUnit, value: Option<f64>) -> String {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return "No data".to_owned();
    };
    match unit {
        MetricUnit::Count if value.fract().abs() < f64::EPSILON => format!("{value:.0}"),
        MetricUnit::Count => format!("{:.1}", round_one_decimal(value)),
        MetricUnit::Percent => format!("{:.1}%", round_one_decimal(value)),
        MetricUnit::Milliseconds => format!("{:.1} ms", round_one_decimal(value)),
        MetricUnit::Dpu => format!("{:.1} DPU", round_one_decimal(value)),
        MetricUnit::Bytes => format_bytes(value),
    }
}

fn round_one_decimal(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn format_bytes(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut scaled = value.max(0.0);
    let mut unit = 0;
    while scaled >= 1024.0 && unit < UNITS.len() - 1 {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{scaled:.0} {}", UNITS[unit])
    } else {
        format!("{scaled:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::app::{MetricSeries, MetricsFetchStatus, MetricsRange, MetricsSnapshot};

    use super::{DashboardModel, METRIC_SPECS, MetricKey, MetricUnit, format_value};

    fn snapshot(series: Vec<MetricSeries>) -> MetricsSnapshot {
        MetricsSnapshot {
            range: MetricsRange::OneHour,
            fetched_at: Some(SystemTime::UNIX_EPOCH),
            series,
            status: MetricsFetchStatus::Fresh,
        }
    }

    fn series(metric: &str, samples: &[Option<f64>]) -> MetricSeries {
        MetricSeries {
            metric: metric.to_owned(),
            samples: samples.to_vec(),
        }
    }

    #[test]
    fn catalog_covers_every_required_source_metric_and_display_unit() {
        let actual = METRIC_SPECS
            .iter()
            .map(|spec| (spec.key, spec.provider_id, spec.unit))
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![
                (
                    MetricKey::TotalTransactions,
                    "total_transactions",
                    MetricUnit::Count
                ),
                (
                    MetricKey::ReadOnlyTransactions,
                    "read_only_transactions",
                    MetricUnit::Count
                ),
                (
                    MetricKey::CommitLatency,
                    "commit_latency",
                    MetricUnit::Milliseconds
                ),
                (MetricKey::OccConflicts, "occ_conflicts", MetricUnit::Count),
                (
                    MetricKey::QueryTimeouts,
                    "query_timeouts",
                    MetricUnit::Count
                ),
                (MetricKey::TotalDpu, "total_dpu", MetricUnit::Dpu),
                (MetricKey::ReadDpu, "read_dpu", MetricUnit::Dpu),
                (MetricKey::WriteDpu, "write_dpu", MetricUnit::Dpu),
                (MetricKey::ComputeDpu, "compute_dpu", MetricUnit::Dpu),
                (
                    MetricKey::MultiRegionWriteDpu,
                    "multi_region_write_dpu",
                    MetricUnit::Dpu,
                ),
                (MetricKey::BytesRead, "bytes_read", MetricUnit::Bytes),
                (MetricKey::BytesWritten, "bytes_written", MetricUnit::Bytes),
                (
                    MetricKey::ComputeTime,
                    "compute_time",
                    MetricUnit::Milliseconds
                ),
                (
                    MetricKey::ClusterStorageSize,
                    "cluster_storage_size",
                    MetricUnit::Bytes,
                ),
                (
                    MetricKey::ActiveConnections,
                    "active_connections",
                    MetricUnit::Count,
                ),
                (
                    MetricKey::AdminConnectionAttempts,
                    "admin_connection_attempts",
                    MetricUnit::Count,
                ),
                (
                    MetricKey::CustomRoleConnectionAttempts,
                    "custom_role_connection_attempts",
                    MetricUnit::Count,
                ),
            ]
        );
    }

    #[test]
    fn read_only_ratio_preserves_missing_samples_and_zero_denominators() {
        let snapshot = snapshot(vec![
            series(
                "total_transactions",
                &[Some(10.0), None, Some(0.0), Some(20.0)],
            ),
            series(
                "read_only_transactions",
                &[Some(4.0), Some(2.0), Some(0.0), None],
            ),
        ]);

        assert_eq!(
            DashboardModel::new(&snapshot).read_only_ratio(),
            vec![Some(40.0), None, None, None]
        );
    }

    #[test]
    fn chart_segments_do_not_bridge_missing_samples() {
        let snapshot = snapshot(vec![series(
            "total_transactions",
            &[Some(2.0), Some(4.0), None, Some(8.0)],
        )]);

        assert_eq!(
            DashboardModel::new(&snapshot).chart_segments(MetricKey::TotalTransactions),
            vec![vec![(0.0, 2.0), (1.0, 4.0)], vec![(3.0, 8.0)]]
        );
    }

    #[test]
    fn sparkline_data_keeps_absent_buckets_distinct_from_zero() {
        let snapshot = snapshot(vec![series(
            "total_dpu",
            &[Some(0.0), None, Some(2.0), Some(4.0)],
        )]);

        assert_eq!(
            DashboardModel::new(&snapshot).sparkline_data(MetricKey::TotalDpu),
            vec![Some(0), None, Some(50), Some(100)]
        );
    }

    #[test]
    fn values_are_formatted_with_stable_units_and_no_data_text() {
        assert_eq!(format_value(MetricUnit::Count, Some(12.0)), "12");
        assert_eq!(format_value(MetricUnit::Percent, Some(42.25)), "42.3%");
        assert_eq!(
            format_value(MetricUnit::Milliseconds, Some(12.25)),
            "12.3 ms"
        );
        assert_eq!(format_value(MetricUnit::Dpu, Some(3.5)), "3.5 DPU");
        assert_eq!(format_value(MetricUnit::Bytes, Some(2048.0)), "2.0 KiB");
        assert_eq!(format_value(MetricUnit::Count, None), "No data");
    }
}
