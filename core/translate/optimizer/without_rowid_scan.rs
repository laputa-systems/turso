//! The index that SQLite scans for a full scan of a WITHOUT ROWID table.
//!
//! SQLite's whereLoopAddBtree adds a full scan of every index of a WITHOUT
//! ROWID table, the PRIMARY KEY index that is the table among them. A scan of
//! an index that holds every column the query reads costs
//! `rSize + 1 + 15*szIdxRow/szTabRow`, so the narrowest such index wins, and
//! of equally narrow ones, the first in the table's list of indexes. Rows come
//! out in the order of that index, which a SELECT without ORDER BY shows.

use crate::schema::{BTreeTable, Column, Index};
use crate::sync::Arc;
use crate::translate::emitter::without_rowid::constraint_check_rank;
use crate::translate::plan::JoinedTable;

/// The index of `table` that SQLite scans when the query reads the table in
/// full, of the table b-tree, for None, and the indexes of `candidates`.
/// Returns None for the table b-tree.
pub(crate) fn full_scan_index<'a>(
    joined_table: &JoinedTable,
    table: &BTreeTable,
    table_is_candidate: bool,
    candidates: impl Iterator<Item = &'a Arc<Index>>,
) -> Option<Arc<Index>> {
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
    let mut scans: Vec<(i64, Option<&Arc<Index>>)> = Vec::new();
    if table_is_candidate {
        let width = table
            .columns()
            .iter()
            .filter(|column| !column.is_virtual_generated())
            .map(column_size_estimate)
            .sum();
        scans.push((cost(width, false), None));
    }
    for index in candidates {
        if index.index_method.is_some() || !joined_table.index_is_covering(index) {
            continue;
        }
        let width = index
            .columns
            .iter()
            .map(|column| match column.expression() {
                Some(_) => 1,
                None => column_size_estimate(&table.columns()[column.pos_in_table]),
            })
            .sum();
        scans.push((cost(width, index.where_clause.is_some()), Some(index)));
    }
    scans.sort_by_key(|(_, index)| constraint_check_rank(table, index.map(Arc::as_ref)));
    let (_, index) = scans
        .iter()
        .min_by_key(|(cost, _)| *cost)
        .copied()
        .unwrap_or((0, None));
    index.cloned()
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
