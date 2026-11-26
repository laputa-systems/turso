//! Writes to a WITHOUT ROWID table, whose b-tree is the index of its PRIMARY
//! KEY. Each entry of one of its other indexes ends with the primary key of
//! its row, which reaches the row as a rowid does in a rowid table.

use super::{emit_index_column_value_old_image, emit_partial_index_where_old_image, Resolver};
use crate::ast::{ResolveType, TableInternalId};
use crate::schema::{BTreeTable, Index};
use crate::util::PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX;
use std::cmp::Reverse;
use crate::sync::Arc;
use crate::translate::plan::TableReferences;
use crate::vdbe::builder::ProgramBuilder;
use crate::vdbe::insn::Insn;
use crate::vdbe::BranchOffset;
use crate::Result;

/// Whether a deleted row is a change of the statement that deletes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeletedRow {
    /// A row that DELETE deletes.
    Change,
    /// A row that REPLACE deletes, or the old image of a row that UPDATE
    /// writes again: SQLite does not count it.
    NotAChange,
}

/// Emits the deletion of the row under `table_cursor_id`: its entry in each
/// index of `indexes`, built from the row, then the row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_row_delete(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table_references: &mut TableReferences,
    table_internal_id: TableInternalId,
    table: &BTreeTable,
    table_cursor_id: usize,
    indexes: &[(Arc<Index>, usize)],
    deleted_row: DeletedRow,
) -> Result<()> {
    emit_index_entries_delete(
        program,
        resolver,
        table_references,
        table_internal_id,
        table_cursor_id,
        indexes,
    )?;
    program.emit_insn(Insn::Delete {
        cursor_id: table_cursor_id,
        table_name: table.name.clone(),
        is_part_of_update: deleted_row == DeletedRow::NotAChange,
    });
    Ok(())
}

/// Emits the deletion of the entry of the row under `table_cursor_id` in each
/// index of `indexes`.
pub(crate) fn emit_index_entries_delete(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table_references: &mut TableReferences,
    table_internal_id: TableInternalId,
    table_cursor_id: usize,
    indexes: &[(Arc<Index>, usize)],
) -> Result<()> {
    for (index, index_cursor_id) in indexes {
        let skip_label = emit_partial_index_where_old_image(
            program,
            resolver,
            table_references,
            table_cursor_id,
            table_internal_id,
            index,
        )?
        .map(|where_reg| {
            let skip_label = program.allocate_label();
            program.emit_insn(Insn::IfNot {
                reg: where_reg,
                jump_if_null: true,
                target_pc: skip_label,
            });
            skip_label
        });
        let num_regs = index.columns.len();
        let start_reg = program.alloc_registers(num_regs);
        for (offset, column) in index.columns.iter().enumerate() {
            emit_index_column_value_old_image(
                program,
                resolver,
                table_references,
                table_cursor_id,
                table_internal_id,
                column,
                start_reg + offset,
            )?;
        }
        program.emit_insn(Insn::IdxDelete {
            start_reg,
            num_regs,
            cursor_id: *index_cursor_id,
            raise_error_if_no_matching_entry: true,
        });
        if let Some(label) = skip_label {
            program.preassign_label_to_next_insn(label);
        }
    }
    Ok(())
}

/// Emits the reads of the primary key of the entry under `index_cursor_id`,
/// an index of `table`, into the registers from `dest_reg`, in PRIMARY KEY
/// order. SQLite reads each from the first column of the index that holds it.
pub(crate) fn emit_primary_key_from_index(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    index: &Index,
    index_cursor_id: usize,
    dest_reg: usize,
) {
    for (offset, (pk_name, _)) in table.primary_key_columns.iter().enumerate() {
        let (pos_in_table, _) = table
            .get_column(pk_name)
            .expect("a PRIMARY KEY column is a column of its table");
        let column = index
            .column_table_pos_to_index_pos(pos_in_table)
            .expect("an index of a WITHOUT ROWID table holds its PRIMARY KEY");
        program.emit_insn(Insn::Column {
            cursor_id: index_cursor_id,
            column,
            dest: dest_reg + offset,
            default: None,
        });
    }
}

/// Emits the move of `table_cursor_id` to the row whose primary key is in the
/// registers from `key_reg`, jumping to `not_found` when there is none.
pub(crate) fn emit_seek_primary_key(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    table_cursor_id: usize,
    key_reg: usize,
    not_found: BranchOffset,
) {
    program.emit_insn(Insn::NotFound {
        cursor_id: table_cursor_id,
        target_pc: not_found,
        record_reg: key_reg,
        num_regs: table.primary_key_columns.len(),
    });
}

/// The place of a uniqueness check of `table` in SQLite's order: `index`, or
/// the PRIMARY KEY when it is None. SQLite checks the indexes of a table in
/// the order of its list of indexes, which holds the PRIMARY KEY of a WITHOUT
/// ROWID table too. The list has the indexes without REPLACE before those with
/// it and the newest index first in each group. An index of CREATE INDEX is
/// newer than the indexes of the constraints, whose names number them in the
/// order they were made.
pub(crate) fn constraint_check_rank(
    table: &BTreeTable,
    index: Option<&Index>,
) -> (bool, Option<Reverse<usize>>) {
    match index {
        None => {
            let number = table
                .unique_sets
                .iter()
                .position(|unique_set| table.is_without_rowid_primary_key(unique_set))
                .map_or(0, |position| position + 1);
            (
                table.rowid_alias_conflict_clause == Some(ResolveType::Replace),
                Some(Reverse(number)),
            )
        }
        Some(index) => {
            let number = index
                .name
                .strip_prefix(PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX)
                .and_then(|rest| rest.rsplit_once('_'))
                .and_then(|(_, number)| number.parse::<usize>().ok());
            (
                index.on_conflict == Some(ResolveType::Replace),
                number.map(Reverse),
            )
        }
    }
}
