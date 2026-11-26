//! The b-tree that SQLite scans for a full scan of a WITHOUT ROWID table.
//!
//! SQLite's whereLoopAddBtree adds a full scan of every index of a WITHOUT
//! ROWID table, the PRIMARY KEY index that is the table among them. A scan of
//! an index that holds every column the query reads costs
//! `rSize + 1 + 15*szIdxRow/szTabRow`, so the narrowest such index wins, and
//! of equally narrow ones, the first in the table's list of indexes. A scan
//! of another index also pays for a lookup of each row. Rows come out in the
//! order of the index, which a SELECT without ORDER BY shows.

use crate::schema::{BTreeTable, Column, Index};
use crate::sync::Arc;
use crate::translate::emitter::without_rowid::constraint_check_rank;
use crate::translate::optimizer::constraints::RangeConstraintRef;
use crate::translate::plan::{ColumnUsedMask, IterationDirection, JoinedTable};
use crate::util::PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX;

/// The statement that reads a WITHOUT ROWID table.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Reader<'a> {
    Select,
    /// An UPDATE whose WHERE clause reads these columns of the table. SQLite
    /// counts only them when it costs a full scan of an index, and never
    /// takes an index for covering when it costs a seek, because the loop
    /// has no SELECT.
    Update(&'a ColumnUsedMask),
}

/// Whether a scan is a full scan or a seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    FullScan,
    Seek,
}

/// A full scan of a WITHOUT ROWID table through its table b-tree, for None,
/// or one of its indexes.
#[derive(Debug, Clone)]
pub(crate) struct FullScan {
    pub(crate) index: Option<Arc<Index>>,
    /// Whether the scan gives the rows in the order of the ORDER BY.
    pub(crate) ordered: bool,
    pub(crate) iter_dir: IterationDirection,
}

/// The full scan of `scans` that SQLite chooses. Sorting costs SQLite's
/// solver more than any difference between two full scans, so a scan in the
/// order of the ORDER BY wins over the others when there is one.
pub(crate) fn sqlite_full_scan<'a>(
    joined_table: &JoinedTable,
    table: &BTreeTable,
    scans: &'a [FullScan],
    reader: Reader<'_>,
) -> Option<&'a FullScan> {
    let table_width = table
        .columns()
        .iter()
        .map(column_size_estimate)
        .sum::<u64>()
        + 1;
    let table_row = log_est(table_width * 4);
    let cost = |width: u64, partial: bool| -> i64 {
        let rows: i64 = if partial { 190 } else { 200 };
        rows + 1 + (15 * log_est(width * 4)) / table_row
    };
    let any_ordered = scans.iter().any(|scan| scan.ordered);
    let mut costed: Vec<(i64, &FullScan)> = Vec::new();
    for scan in scans.iter().filter(|scan| scan.ordered || !any_ordered) {
        let scan_cost = match scan.index.as_deref() {
            None => {
                let width = table
                    .columns()
                    .iter()
                    .filter(|column| !column.is_virtual_generated())
                    .map(column_size_estimate)
                    .sum();
                cost(width, false)
            }
            Some(index) if index.index_method.is_some() => continue,
            Some(index) => {
                let width = index
                    .columns
                    .iter()
                    .map(|column| match column.expression() {
                        Some(_) => 1,
                        None => column_size_estimate(&table.columns()[column.pos_in_table]),
                    })
                    .sum();
                let partial = index.where_clause.is_some();
                if covers(joined_table, index, reader, Access::FullScan) {
                    cost(width, partial)
                } else {
                    let rows: i64 = if partial { 190 } else { 200 };
                    log_est_add(cost(width, partial), rows + 16)
                }
            }
        };
        costed.push((scan_cost, scan));
    }
    costed.sort_by_key(|(_, scan)| constraint_check_rank(table, scan.index.as_deref()));
    costed
        .iter()
        .min_by_key(|(cost, _)| *cost)
        .map(|(_, scan)| *scan)
}

/// Whether SQLite finds the rows that `index` finds through `constraint_refs`
/// through the PRIMARY KEY of `table` instead. When the constraints are on
/// the leading PRIMARY KEY columns with the same collations, the PRIMARY
/// KEY finds the same rows without a lookup of each row, so it wins unless
/// `index` covers the query and costs less by its width, or as much and
/// comes first in the table's list of indexes.
pub(crate) fn primary_key_serves_seek(
    joined_table: &JoinedTable,
    table: &BTreeTable,
    index: &Index,
    constraint_refs: &[RangeConstraintRef],
    reader: Reader<'_>,
) -> bool {
    for (position, constraint) in constraint_refs.iter().enumerate() {
        let Some((pk_name, _)) = table.primary_key_columns.get(position) else {
            return false;
        };
        let Some((pk_position, pk_column)) = table.get_column(pk_name) else {
            return false;
        };
        let Some(index_column) = index.columns.get(constraint.index_col_pos) else {
            return false;
        };
        if constraint.index_col_pos != position
            || index_column.expression().is_some()
            || index_column.pos_in_table != pk_position
            || index_column.collation != table.primary_key_collation(pk_name, pk_column)
        {
            return false;
        }
    }
    if !covers(joined_table, index, reader, Access::Seek) {
        return true;
    }
    let table_width = table
        .columns()
        .iter()
        .map(column_size_estimate)
        .sum::<u64>()
        + 1;
    let table_row = log_est(table_width * 4);
    let primary_key_width: u64 = table
        .columns()
        .iter()
        .filter(|column| !column.is_virtual_generated())
        .map(column_size_estimate)
        .sum();
    let index_width: u64 = index
        .columns
        .iter()
        .map(|column| match column.expression() {
            Some(_) => 1,
            None => column_size_estimate(&table.columns()[column.pos_in_table]),
        })
        .sum();
    let primary_key_cost = (15 * log_est(primary_key_width * 4)) / table_row;
    let index_cost = (15 * log_est(index_width * 4)) / table_row;
    primary_key_cost < index_cost
        || (primary_key_cost == index_cost
            && constraint_check_rank(table, None) < constraint_check_rank(table, Some(index)))
}

/// Whether SQLite reads the columns that the query uses from `index` alone.
/// SQLite records which columns the index of a UNIQUE constraint holds
/// before convertToWithoutRowidTable appends the PRIMARY KEY to it, so only
/// the declared key of such an index counts.
fn covers(joined_table: &JoinedTable, index: &Index, reader: Reader<'_>, access: Access) -> bool {
    let used_columns = match reader {
        Reader::Update(_) if access == Access::Seek => return false,
        Reader::Update(where_columns) => where_columns,
        Reader::Select if !index.name.starts_with(PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX) => {
            return joined_table.index_is_covering(index);
        }
        Reader::Select => &joined_table.col_used_mask,
    };
    let held_columns = if index.name.starts_with(PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX) {
        index.key_columns()
    } else {
        &index.columns
    };
    used_columns.iter().all(|position| {
        held_columns
            .iter()
            .any(|column| column.expression().is_none() && column.pos_in_table == position)
    })
}

/// SQLite's estimate of the size of a value of `column`, in units of 4 bytes,
/// which sqlite3AffinityType derives from the declared type.
fn column_size_estimate(column: &Column) -> u64 {
    let declared = if column.ty_params.is_empty() {
        column.ty_str.clone()
    } else {
        let params = column
            .ty_params
            .iter()
            .map(|param| param.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!("{}({params})", column.ty_str)
    };
    type_size_estimate(&declared)
}

/// The affinities in the order of SQLite's SQLITE_AFF_ codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Affinity {
    Blob,
    Text,
    Numeric,
    Integer,
    Real,
}

fn type_size_estimate(declared: &str) -> u64 {
    let bytes = declared.as_bytes();
    let mut hash: u32 = 0;
    let mut affinity = Affinity::Numeric;
    let mut size_text: Option<usize> = None;
    let mut position = 0;
    while position < bytes.len() {
        hash = (hash << 8).wrapping_add(u32::from(bytes[position].to_ascii_lowercase()));
        position += 1;
        if hash == u32::from_be_bytes(*b"char") {
            affinity = Affinity::Text;
            size_text = Some(position);
        } else if hash == u32::from_be_bytes(*b"clob") || hash == u32::from_be_bytes(*b"text") {
            affinity = Affinity::Text;
        } else if hash == u32::from_be_bytes(*b"blob")
            && matches!(affinity, Affinity::Numeric | Affinity::Real)
        {
            affinity = Affinity::Blob;
            if bytes.get(position) == Some(&b'(') {
                size_text = Some(position);
            }
        } else if (hash == u32::from_be_bytes(*b"real")
            || hash == u32::from_be_bytes(*b"floa")
            || hash == u32::from_be_bytes(*b"doub"))
            && affinity == Affinity::Numeric
        {
            affinity = Affinity::Real;
        } else if hash & 0x00FF_FFFF == u32::from_be_bytes(*b"\0int") {
            affinity = Affinity::Integer;
            break;
        }
    }
    let mut size: u64 = 0;
    if affinity < Affinity::Numeric {
        match size_text {
            Some(start) => {
                if let Some(first_digit) = bytes[start..].iter().position(u8::is_ascii_digit) {
                    size = bytes[start + first_digit..]
                        .iter()
                        .take_while(|byte| byte.is_ascii_digit())
                        .fold(0u64, |value, digit| {
                            (value * 10 + u64::from(digit - b'0')).min(u64::from(u32::MAX))
                        });
                }
            }
            None => size = 16,
        }
    }
    (size / 4 + 1).min(255)
}

/// SQLite's sqlite3LogEst: about `10*log2(x)`.
fn log_est(mut x: u64) -> i64 {
    const FRACTIONS: [i64; 8] = [0, 2, 3, 5, 6, 7, 8, 9];
    let mut y: i64 = 40;
    if x < 8 {
        if x < 2 {
            return 0;
        }
        while x < 8 {
            y -= 10;
            x <<= 1;
        }
    } else {
        let shift = 60 - i64::from(x.leading_zeros());
        y += shift * 10;
        x >>= shift;
    }
    FRACTIONS[(x & 7) as usize] + y - 10
}

/// SQLite's sqlite3LogEstAdd: the estimate of the sum of the values that `a`
/// and `b` estimate.
fn log_est_add(a: i64, b: i64) -> i64 {
    const EXTRA: [i64; 32] = [
        10, 10, 9, 9, 8, 8, 7, 7, 7, 6, 6, 6, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2,
        2, 2, 2,
    ];
    let (high, low) = if a >= b { (a, b) } else { (b, a) };
    if high > low + 49 {
        return high;
    }
    if high > low + 31 {
        return high + 1;
    }
    high + EXTRA[(high - low) as usize]
}
