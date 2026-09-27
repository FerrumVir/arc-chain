//! Offline export/proof bounds, not production placement capabilities.
use arc_inference::tensor_parallel::MAX_ROW_WORKERS_PER_STAGE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowPartition {
    pub rank: usize,
    pub count: usize,
}

impl RowPartition {
    pub fn parse(value: &str) -> Result<Self, String> {
        let (rank, count) = value.split_once('/').ok_or("expected rank/count")?;
        let rank = rank.parse().map_err(|_| "invalid partition rank")?;
        let count = count.parse().map_err(|_| "invalid partition count")?;
        let partition = Self { rank, count };
        partition.validate()?;
        Ok(partition)
    }

    fn validate(self) -> Result<(), String> {
        if self.count == 0 || self.count > MAX_ROW_WORKERS_PER_STAGE || self.rank >= self.count {
            return Err(format!(
                "partition requires 0 <= rank < count <= {MAX_ROW_WORKERS_PER_STAGE}"
            ));
        }
        Ok(())
    }

    /// Floor boundaries in canonical output-row order, including Q/K after
    /// permutation. u128 intermediates avoid overflow on supported targets.
    pub fn range(self, rows: usize) -> Result<(usize, usize), String> {
        self.validate()?;
        if rows < self.count {
            return Err("partition would leave an empty row interval".into());
        }
        let boundary = |rank: usize| ((rows as u128 * rank as u128) / self.count as u128) as usize;
        Ok((boundary(self.rank), boundary(self.rank + 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_cover_uneven_rows_and_large_boundaries_exactly() {
        for rows in [1, 5, 17, 257, usize::MAX] {
            for count in 1..=MAX_ROW_WORKERS_PER_STAGE.min(rows) {
                let mut cursor = 0;
                for rank in 0..count {
                    let (start, end) = RowPartition { rank, count }.range(rows).unwrap();
                    assert_eq!(start, cursor);
                    assert!(start < end && end <= rows);
                    cursor = end;
                }
                assert_eq!(cursor, rows);
            }
        }
    }

    #[test]
    fn malformed_out_of_bounds_and_empty_partitions_refuse() {
        for text in ["", "2", "-1/2", "0/0", "2/2", "0/33", "0/2/3", "x/2"] {
            assert!(RowPartition::parse(text).is_err(), "{text}");
        }
        assert!(RowPartition::parse("0/2").unwrap().range(1).is_err());
        assert!(RowPartition::parse("0/1").unwrap().range(0).is_err());
        assert_eq!(
            RowPartition::parse("1/3").unwrap().range(5).unwrap(),
            (1, 3)
        );
    }
}
