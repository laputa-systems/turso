//! Writes to a WITHOUT ROWID table, whose b-tree is the index of its PRIMARY
//! KEY. Each entry of one of its other indexes ends with the primary key of
//! its row, which reaches the row as a rowid does in a rowid table.

use super::update::{emit_notnull_constraint_check, notnull_conflict_resolution};
use super::{
    check_expr_references_columns, emit_cdc_autocommit_commit, emit_check_constraints,
    emit_index_column_value_new_image, emit_index_column_value_old_image, emit_make_record,
    emit_partial_index_where_old_image, OperationMode, Resolver, TranslateCtx,
};
use crate::ast::{self, ResolveType, TableInternalId, TriggerEvent};
use crate::emit_explain;
use crate::error::SQLITE_CONSTRAINT_PRIMARYKEY;
use crate::schema::{BTreeTable, CheckConstraint, ColumnLayout, Index, IndexColumn};
use crate::sync::Arc;
use crate::translate::emitter::has_triggers_including_temp;
use crate::translate::eqp::eqp_detail_for_table_op;
use crate::translate::expr::{
    emit_constraint_values_decode, emit_dml_expr_index_value, emit_table_column, translate_expr,
};
use crate::translate::insert::{emit_column_type_conversion, format_unique_violation_desc};
use crate::translate::main_loop::{CloseLoop, InitLoop, OpenLoop};
use crate::translate::plan::{
    DeletePlan, EvalAt, IterationDirection, JoinOrderMember, JoinedTable, Operation, Scan,
    SubqueryEvalPhase, TableReferences, UpdatePlan,
};
use crate::translate::subquery::{
    emit_non_from_clause_subqueries_for_eval_at, emit_non_from_clause_subquery,
};
use crate::util::{normalize_ident, PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX};
use crate::vdbe::builder::{CursorKey, ProgramBuilder};
use crate::vdbe::insn::{to_u32, CmpInsFlags, ConstraintInfo, IdxInsertFlags, InsertFlags, Insn};
use crate::vdbe::BranchOffset;
use crate::{Connection, HashSet, Result};
use std::cmp::Reverse;

/// Emits a DELETE from a WITHOUT ROWID table as SQLite does when it cannot
/// delete while it scans: the loop collects the primary key of each row that
/// the WHERE clause selects in an ephemeral index, then each row is found by
/// its primary key and deleted with its index entries.
pub(crate) fn emit_program_for_delete(
    connection: &Arc<Connection>,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
    plan: &mut DeletePlan,
    table: &Arc<BTreeTable>,
) -> Result<()> {
    let target = plan
        .table_references
        .joined_tables()
        .first()
        .expect("DELETE has one target table");
    let namespace = target.namespace;
    let internal_id = target.internal_id;
    check_writes_are_supported(
        connection,
        resolver,
        program,
        target,
        table,
        TriggerEvent::Delete,
    )?;
    if !plan.result_columns.is_empty() {
        crate::bail_parse_error!("RETURNING on WITHOUT ROWID tables is not supported");
    }

    let mut t_ctx = TranslateCtx::new(
        program,
        resolver.fork(),
        plan.table_references.joined_tables().len(),
        connection.db.opts.unsafe_testing,
    );
    let after_main_loop_label = program.allocate_label();
    t_ctx.label_main_loop_end = Some(after_main_loop_label);

    let key_index = primary_key_ephemeral_index(table);
    let key_cursor_id = program.alloc_cursor_index(None, &key_index)?;
    program.emit_insn(Insn::OpenEphemeral {
        cursor_id: key_cursor_id,
        is_table: false,
    });
    if plan.contains_constant_false_condition {
        program.emit_insn(Insn::Goto {
            target_pc: after_main_loop_label,
        });
    }
    let join_order = [JoinOrderMember {
        table_id: internal_id,
        original_idx: 0,
        is_outer: false,
    }];
    emit_non_from_clause_subqueries_for_eval_at(
        program,
        &t_ctx.resolver,
        &mut plan.non_from_clause_subqueries,
        &join_order,
        Some(&plan.table_references),
        EvalAt::BeforeLoop,
        |_| true,
    )?;
    InitLoop::emit(
        program,
        &mut t_ctx,
        &plan.table_references,
        &mut [],
        &OperationMode::DELETE,
        &plan.where_clause,
        &join_order,
        &mut plan.non_from_clause_subqueries,
    )?;
    emit_explain!(
        program,
        true,
        eqp_detail_for_table_op(&plan.table_references.joined_tables()[0], None, None)
    );
    OpenLoop::emit(
        program,
        &mut t_ctx,
        &plan.table_references,
        &join_order,
        &plan.where_clause,
        None,
        OperationMode::DELETE,
        &mut plan.non_from_clause_subqueries,
    )?;
    emit_primary_key_insert(
        program,
        &t_ctx.resolver,
        &plan.table_references,
        internal_id,
        table,
        key_cursor_id,
    )?;
    CloseLoop::emit(
        program,
        &mut t_ctx,
        &plan.table_references,
        &join_order,
        OperationMode::DELETE,
        None,
    )?;
    program.preassign_label_to_next_insn(after_main_loop_label);

    let table_cursor_id = program.resolve_cursor_id(&CursorKey::table(internal_id));
    let indexes = resolver
        .with_schema(namespace.database_id, |s| {
            s.get_indices(namespace.id, &table.name)
                .cloned()
                .collect::<Vec<_>>()
        })
        .into_iter()
        .map(|index| {
            let cursor_id = program.resolve_cursor_id(&CursorKey::index(internal_id, index.clone()));
            (index, cursor_id)
        })
        .collect::<Vec<_>>();
    let loop_end = program.allocate_label();
    let next_key = program.allocate_label();
    let loop_start = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: key_cursor_id,
        pc_if_empty: loop_end,
    });
    program.preassign_label_to_next_insn(loop_start);
    let key_reg = emit_primary_key_from_ephemeral(program, table, key_cursor_id);
    emit_seek_primary_key(program, table, table_cursor_id, key_reg, next_key);
    emit_row_delete(
        program,
        &t_ctx.resolver,
        &mut plan.table_references,
        internal_id,
        table,
        table_cursor_id,
        &indexes,
        DeletedRow::Change,
    )?;
    program.preassign_label_to_next_insn(next_key);
    program.emit_insn(Insn::Next {
        cursor_id: key_cursor_id,
        pc_if_next: loop_start,
        fullscan: false,
        is_index: true,
    });
    program.preassign_label_to_next_insn(loop_end);
    emit_cdc_autocommit_commit(program, resolver, t_ctx.cdc_cursor_id)?;
    Ok(())
}

/// Fails for the writes to a WITHOUT ROWID table that Turso does not
/// support: with triggers, foreign keys, change data capture or materialized
/// views.
pub(crate) fn check_writes_are_supported(
    connection: &Arc<Connection>,
    resolver: &Resolver,
    program: &ProgramBuilder,
    target: &JoinedTable,
    table: &Arc<BTreeTable>,
    event: TriggerEvent,
) -> Result<()> {
    let namespace = target.namespace;
    if has_triggers_including_temp(resolver, namespace, event, None, table, connection) {
        crate::bail_parse_error!("triggers on WITHOUT ROWID tables are not supported");
    }
    if connection.foreign_keys_enabled()
        && resolver.with_schema(namespace.database_id, |s| {
            s.has_child_fks(namespace.id, &table.name)
                || s.any_resolved_fks_referencing(namespace.id, &table.name)
        })
    {
        crate::bail_parse_error!("foreign keys on WITHOUT ROWID tables are not supported");
    }
    if program.capture_data_changes_info().is_some() {
        crate::bail_parse_error!("CDC on WITHOUT ROWID tables is not supported");
    }
    if resolver.with_schema(namespace.database_id, |s| {
        !s.get_dependent_materialized_views(namespace.id, &table.name)
            .is_empty()
    }) {
        crate::bail_parse_error!("materialized views on WITHOUT ROWID tables are not supported");
    }
    Ok(())
}

/// An ephemeral index of primary keys of `table`, ordered as the table is.
pub(crate) fn primary_key_ephemeral_index(table: &BTreeTable) -> Arc<Index> {
    let columns = table
        .primary_key_columns
        .iter()
        .enumerate()
        .map(|(position, (pk_name, order))| {
            let (_, column) = table
                .get_column(pk_name)
                .expect("a PRIMARY KEY column is a column of its table");
            let mut key_column =
                IndexColumn::new(pk_name, position, table.primary_key_collation(pk_name, column));
            key_column.order = *order;
            key_column
        })
        .collect();
    Arc::new(Index {
        name: format!("{}_primary_keys", table.name),
        table_name: table.name.clone(),
        root_page: 0,
        columns,
        unique: false,
        ephemeral: true,
        has_rowid: false,
        where_clause: None,
        index_method: None,
        on_conflict: None,
        constraint_name: None,
        pk_suffix_len: 0,
    })
}

/// Emits the insertion of the primary key of the current row of the loop over
/// the table `internal_id` into the ephemeral index under `key_cursor_id`.
pub(crate) fn emit_primary_key_insert(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table_references: &TableReferences,
    internal_id: TableInternalId,
    table: &BTreeTable,
    key_cursor_id: usize,
) -> Result<()> {
    let key_len = table.primary_key_columns.len();
    let key_reg = program.alloc_registers(key_len);
    for (offset, (pk_name, _)) in table.primary_key_columns.iter().enumerate() {
        let (column, _) = table
            .get_column(pk_name)
            .expect("a PRIMARY KEY column is a column of its table");
        let expr = ast::Expr::Column {
            database: None,
            table: internal_id,
            column,
            is_rowid_alias: false,
        };
        translate_expr(
            program,
            Some(table_references),
            &expr,
            key_reg + offset,
            resolver,
        )?;
    }
    let record_reg = program.alloc_register();
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(key_reg),
        count: to_u32(key_len),
        dest_reg: to_u32(record_reg),
        index_name: None,
        affinity_str: None,
    });
    program.emit_insn(Insn::IdxInsert {
        cursor_id: key_cursor_id,
        record_reg,
        unpacked_start: Some(key_reg),
        unpacked_count: Some(to_u32(key_len)),
        flags: IdxInsertFlags::new(),
    });
    Ok(())
}

/// Emits the reads of the primary key under `key_cursor_id`, a cursor on an
/// index of [primary_key_ephemeral_index], and returns its first register.
pub(crate) fn emit_primary_key_from_ephemeral(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    key_cursor_id: usize,
) -> usize {
    let key_len = table.primary_key_columns.len();
    let key_reg = program.alloc_registers(key_len);
    for offset in 0..key_len {
        program.emit_insn(Insn::Column {
            cursor_id: key_cursor_id,
            column: offset,
            dest: key_reg + offset,
            default: None,
        });
    }
    key_reg
}

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
        None => (
            table.rowid_alias_conflict_clause == Some(ResolveType::Replace),
            Some(Reverse(table.without_rowid_primary_key_number())),
        ),
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

/// The order in which the second pass of an UPDATE visits the rows that the
/// first pass collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowOrder {
    /// The order of the scan, as SQLite's one-pass UPDATE changes each row
    /// when its scan reaches it.
    Scan,
    /// The order of the primary key, in which SQLite's two-pass UPDATE reads
    /// its ephemeral index of primary keys.
    PrimaryKey,
}

/// A uniqueness constraint that an UPDATE checks.
#[derive(Debug, Clone, Copy)]
enum UniqueCheck<'a> {
    PrimaryKey,
    Index(&'a Index, usize),
}

/// Emits an UPDATE of a WITHOUT ROWID table with SQLite's sqlite3Update
/// steps: the first pass collects the primary keys of the rows that the
/// WHERE clause selects, the second finds each row by its primary key,
/// computes the new row, checks NOT NULL, CHECK and the uniqueness of the
/// PRIMARY KEY and the UNIQUE indexes that change, deletes the old entries of
/// the indexes that change, and writes the new ones and the new row.
pub(crate) fn emit_program_for_update(
    connection: &Arc<Connection>,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
    mut plan: UpdatePlan,
    table: &Arc<BTreeTable>,
) -> Result<()> {
    let namespace = plan.target_table.namespace;
    let internal_id = plan.target_table.internal_id;
    check_writes_are_supported(
        connection,
        resolver,
        program,
        &plan.target_table,
        table,
        TriggerEvent::Update,
    )?;
    if plan.returning.as_ref().is_some_and(|r| !r.is_empty()) {
        crate::bail_parse_error!("RETURNING on WITHOUT ROWID tables is not supported");
    }
    if plan.write_set_plan.is_some() || !plan.from_tables.joined_tables().is_empty() {
        crate::bail_parse_error!("UPDATE FROM on WITHOUT ROWID tables is not supported");
    }
    if !plan.row_checks.is_empty() {
        crate::bail_parse_error!("row security on WITHOUT ROWID tables is not supported");
    }
    if table.has_generated_columns()
        || table
            .columns()
            .iter()
            .any(|column| resolver.schema().column_has_custom_type(column, table.is_strict))
    {
        crate::bail_parse_error!(
            "UPDATE of WITHOUT ROWID tables with generated or custom type columns is not supported"
        );
    }
    let statement_conflict = plan.or_conflict;
    let on_conflict = statement_conflict.unwrap_or(ResolveType::Abort);
    program.set_resolve_type(on_conflict);
    program
        .flags
        .set_has_statement_conflict(statement_conflict.is_some());

    let updated: Vec<usize> = plan.set_clauses.iter().map(|set| set.column_index).collect();
    let updated_names: HashSet<String> = updated
        .iter()
        .filter_map(|&position| table.columns()[position].name.as_deref())
        .map(normalize_ident)
        .collect();
    let primary_key_changes = table.primary_key_columns.iter().any(|(pk_name, _)| {
        table
            .get_column(pk_name)
            .is_some_and(|(position, _)| updated.contains(&position))
    });
    let all_indexes: Vec<Arc<Index>> = resolver.with_schema(namespace.database_id, |s| {
        s.get_indices(namespace.id, &table.name).cloned().collect()
    });
    let changes = |index: &Index| {
        primary_key_changes
            || index
                .where_clause
                .as_deref()
                .is_some_and(|where_clause| check_expr_references_columns(where_clause, &updated_names))
            || index.key_columns().iter().any(|column| match column.expression() {
                Some(expr) => check_expr_references_columns(expr, &updated_names),
                None => updated.contains(&column.pos_in_table),
            })
    };
    let changed_indexes: Vec<Arc<Index>> = all_indexes
        .iter()
        .filter(|index| changes(index))
        .cloned()
        .collect();
    // sqlite3Update: REPLACE resolution may run when the statement says so,
    // or when an index whose key changes says so, but not for an index that
    // changes only because its WHERE clause reads a column that changes.
    let may_replace = statement_conflict == Some(ResolveType::Replace)
        || (statement_conflict.is_none()
            && changed_indexes.iter().any(|index| {
                index.on_conflict == Some(ResolveType::Replace)
                    && (primary_key_changes
                        || index.key_columns().iter().any(|column| match column.expression() {
                            Some(expr) => check_expr_references_columns(expr, &updated_names),
                            None => updated.contains(&column.pos_in_table),
                        }))
            }))
        || (primary_key_changes && table.rowid_alias_conflict_clause == Some(ResolveType::Replace));
    let scan_index_changes = plan
        .target_table
        .op
        .index()
        .is_some_and(|index| changed_indexes.iter().any(|changed| changed.name == index.name));
    let where_has_subquery = !plan.non_from_clause_subqueries.is_empty()
        && plan.where_clause.iter().any(|term| {
            let mut found = false;
            let _ = crate::translate::expr::walk_expr(&term.expr, &mut |e| {
                if matches!(
                    e,
                    ast::Expr::Subquery(_)
                        | ast::Expr::InSelect { .. }
                        | ast::Expr::Exists(_)
                        | ast::Expr::SubqueryResult { .. }
                ) {
                    found = true;
                }
                Ok(crate::translate::expr::WalkControl::Continue)
            });
            found
        });
    let row_order = if primary_key_changes
        || may_replace
        || scan_index_changes
        || where_has_subquery
        || matches!(plan.target_table.op, Operation::MultiIndexScan(_))
    {
        RowOrder::PrimaryKey
    } else {
        RowOrder::Scan
    };

    let mut table_references =
        TableReferences::for_update_target(plan.target_table.clone(), &plan.from_tables);
    let mut t_ctx = TranslateCtx::new(
        program,
        resolver.fork(),
        table_references.joined_tables().len(),
        connection.db.opts.unsafe_testing,
    );
    let after_main_loop_label = program.allocate_label();
    t_ctx.label_main_loop_end = Some(after_main_loop_label);

    let key_index = match row_order {
        RowOrder::PrimaryKey => primary_key_ephemeral_index(table),
        RowOrder::Scan => scan_order_ephemeral_index(table),
    };
    let key_cursor_id = program.alloc_cursor_index(None, &key_index)?;
    program.emit_insn(Insn::OpenEphemeral {
        cursor_id: key_cursor_id,
        is_table: false,
    });
    let sequence_reg = (row_order == RowOrder::Scan).then(|| {
        let reg = program.alloc_register();
        program.emit_int(0, reg);
        reg
    });
    if plan.contains_constant_false_condition {
        program.emit_insn(Insn::Goto {
            target_pc: after_main_loop_label,
        });
    }
    let join_order = [JoinOrderMember {
        table_id: internal_id,
        original_idx: 0,
        is_outer: false,
    }];
    emit_non_from_clause_subqueries_for_eval_at(
        program,
        &t_ctx.resolver,
        &mut plan.non_from_clause_subqueries,
        &join_order,
        Some(&table_references),
        EvalAt::BeforeLoop,
        |_| true,
    )?;
    let mut set_subqueries = Vec::new();
    let mut i = 0;
    while i < plan.non_from_clause_subqueries.len() {
        if matches!(
            plan.non_from_clause_subqueries[i].eval_phase,
            SubqueryEvalPhase::PreWrite | SubqueryEvalPhase::PostWriteReturning
        ) {
            set_subqueries.push(plan.non_from_clause_subqueries.remove(i));
        } else {
            i += 1;
        }
    }
    InitLoop::emit(
        program,
        &mut t_ctx,
        &table_references,
        &mut [],
        &OperationMode::DELETE,
        &plan.where_clause,
        &join_order,
        &mut plan.non_from_clause_subqueries,
    )?;
    emit_explain!(
        program,
        true,
        eqp_detail_for_table_op(&table_references.joined_tables()[0], None, None)
    );
    OpenLoop::emit(
        program,
        &mut t_ctx,
        &table_references,
        &join_order,
        &plan.where_clause,
        None,
        OperationMode::DELETE,
        &mut plan.non_from_clause_subqueries,
    )?;
    match sequence_reg {
        Some(sequence_reg) => {
            program.emit_insn(Insn::AddImm {
                register: sequence_reg,
                value: 1,
            });
            emit_sequenced_primary_key_insert(
                program,
                &t_ctx.resolver,
                &table_references,
                internal_id,
                table,
                key_cursor_id,
                sequence_reg,
            )?;
        }
        None => emit_primary_key_insert(
            program,
            &t_ctx.resolver,
            &table_references,
            internal_id,
            table,
            key_cursor_id,
        )?,
    }
    CloseLoop::emit(
        program,
        &mut t_ctx,
        &table_references,
        &join_order,
        OperationMode::DELETE,
        None,
    )?;
    program.preassign_label_to_next_insn(after_main_loop_label);

    table_references.joined_tables_mut()[0].op = Operation::Scan(Scan::BTreeTable {
        iter_dir: IterationDirection::Forwards,
        index: None,
    });
    let table_cursor_id = program.resolve_cursor_id(&CursorKey::table(internal_id));
    let index_cursors: Vec<(Arc<Index>, usize)> = all_indexes
        .iter()
        .map(|index| {
            let cursor_id = program.resolve_cursor_id(&CursorKey::index(internal_id, index.clone()));
            (index.clone(), cursor_id)
        })
        .collect();
    let changed_index_cursors: Vec<(Arc<Index>, usize)> = index_cursors
        .iter()
        .filter(|(index, _)| changed_indexes.iter().any(|changed| changed.name == index.name))
        .cloned()
        .collect();

    let loop_end = program.allocate_label();
    let next_row = program.allocate_label();
    let loop_start = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: key_cursor_id,
        pc_if_empty: loop_end,
    });
    program.preassign_label_to_next_insn(loop_start);
    let key_reg = match row_order {
        RowOrder::PrimaryKey => emit_primary_key_from_ephemeral(program, table, key_cursor_id),
        RowOrder::Scan => {
            let key_len = table.primary_key_columns.len();
            let key_reg = program.alloc_registers(key_len);
            for offset in 0..key_len {
                program.emit_insn(Insn::Column {
                    cursor_id: key_cursor_id,
                    column: offset + 1,
                    dest: key_reg + offset,
                    default: None,
                });
            }
            key_reg
        }
    };
    emit_seek_primary_key(program, table, table_cursor_id, key_reg, next_row);

    for subquery in set_subqueries
        .iter_mut()
        .filter(|s| !s.has_been_evaluated() && !s.is_post_write_returning())
    {
        let subquery_plan = subquery.consume_plan(EvalAt::Loop(0));
        emit_non_from_clause_subquery(
            program,
            &t_ctx.resolver,
            *subquery_plan,
            &subquery.query_type,
            subquery.correlated,
        )?;
    }

    let layout = ColumnLayout::from_btree(table)?;
    let columns = table.columns();
    let new_reg = program.alloc_registers(columns.len());
    for (position, column) in columns.iter().enumerate() {
        let reg = layout.to_register(new_reg, position);
        match plan
            .set_clauses
            .iter()
            .find(|set| set.column_index == position)
        {
            Some(set) => {
                translate_expr(
                    program,
                    Some(&table_references),
                    set.emitted_expr(),
                    reg,
                    &t_ctx.resolver,
                )?;
            }
            None => emit_table_column(
                program,
                table_cursor_id,
                internal_id,
                &table_references,
                column,
                position,
                reg,
                &t_ctx.resolver,
            )?,
        };
    }
    for &position in &updated {
        let column = &columns[position];
        if column.notnull() {
            emit_notnull_constraint_check(
                program,
                &table_references,
                layout.to_register(new_reg, position),
                column,
                &table.name,
                notnull_conflict_resolution(program, column),
                next_row,
                &t_ctx.resolver,
            )?;
        }
    }
    emit_column_type_conversion(
        program,
        &t_ctx.resolver,
        table,
        new_reg,
        layout.num_non_virtual_cols(),
    )?;
    let relevant_checks: Vec<CheckConstraint> = table
        .check_constraints
        .iter()
        .filter(|check| check_expr_references_columns(&check.expr, &updated_names))
        .cloned()
        .collect();
    if !relevant_checks.is_empty() {
        let check_halt_on_error = program.constraint_halt_on_error(on_conflict);
        let null_rowid_reg = program.alloc_register();
        program.emit_null(null_rowid_reg, None);
        emit_check_constraints(
            program,
            &relevant_checks,
            &mut t_ctx.resolver,
            table,
            null_rowid_reg,
            columns
                .iter()
                .enumerate()
                .map(|(position, column)| (column, layout.to_register(new_reg, position))),
            connection,
            on_conflict,
            check_halt_on_error,
            next_row,
            Some(&table_references),
        )?;
    }

    let mut checks: Vec<UniqueCheck> = changed_index_cursors
        .iter()
        .map(|(index, cursor_id)| UniqueCheck::Index(index, *cursor_id))
        .collect();
    if primary_key_changes {
        checks.push(UniqueCheck::PrimaryKey);
    }
    checks.sort_by_key(|check| match check {
        UniqueCheck::PrimaryKey => constraint_check_rank(table, None),
        UniqueCheck::Index(index, _) => constraint_check_rank(table, Some(index)),
    });
    let mut new_entries = Vec::new();
    for check in checks {
        match check {
            UniqueCheck::PrimaryKey => emit_update_primary_key_check(
                program,
                &mut t_ctx.resolver,
                &mut table_references,
                internal_id,
                table,
                table_cursor_id,
                &index_cursors,
                &layout,
                new_reg,
                key_reg,
                statement_conflict,
                next_row,
            )?,
            UniqueCheck::Index(index, index_cursor_id) => {
                let entry = emit_new_index_entry(
                    program,
                    &t_ctx.resolver,
                    table,
                    index,
                    &layout,
                    new_reg,
                )?;
                if index.unique {
                    emit_update_unique_check(
                        program,
                        &mut t_ctx.resolver,
                        &mut table_references,
                        internal_id,
                        table,
                        table_cursor_id,
                        &index_cursors,
                        index,
                        index_cursor_id,
                        &entry,
                        key_reg,
                        statement_conflict,
                        next_row,
                    )?;
                }
                new_entries.push((index_cursor_id, entry));
            }
        }
    }
    if may_replace || primary_key_changes {
        emit_seek_primary_key(program, table, table_cursor_id, key_reg, next_row);
    }
    emit_index_entries_delete(
        program,
        &t_ctx.resolver,
        &mut table_references,
        internal_id,
        table_cursor_id,
        &changed_index_cursors,
    )?;
    if primary_key_changes {
        program.emit_insn(Insn::Delete {
            cursor_id: table_cursor_id,
            table_name: table.name.clone(),
            is_part_of_update: true,
        });
    }
    for (index_cursor_id, entry) in &new_entries {
        if let Some(where_reg) = entry.where_reg {
            let skip = program.allocate_label();
            program.emit_insn(Insn::IfNot {
                reg: where_reg,
                jump_if_null: true,
                target_pc: skip,
            });
            emit_index_entry_insert(program, *index_cursor_id, entry);
            program.preassign_label_to_next_insn(skip);
        } else {
            emit_index_entry_insert(program, *index_cursor_id, entry);
        }
    }
    let record_reg = program.alloc_register();
    emit_make_record(program, table.record_columns(), new_reg, record_reg);
    let unused_key_reg = program.alloc_register();
    program.emit_insn(Insn::Insert {
        cursor: table_cursor_id,
        key_reg: unused_key_reg,
        record_reg,
        flag: InsertFlags::new().require_seek().skip_last_rowid(),
        table_name: table.name.clone(),
    });

    program.preassign_label_to_next_insn(next_row);
    program.emit_insn(Insn::Next {
        cursor_id: key_cursor_id,
        pc_if_next: loop_start,
        fullscan: false,
        is_index: true,
    });
    program.preassign_label_to_next_insn(loop_end);
    emit_cdc_autocommit_commit(program, resolver, t_ctx.cdc_cursor_id)?;
    program.result_columns = Vec::new();
    program.table_references.extend(table_references);
    Ok(())
}

/// An ephemeral index of primary keys of `table` in the order they are added:
/// each key follows a sequence number.
fn scan_order_ephemeral_index(table: &BTreeTable) -> Arc<Index> {
    let primary_keys = primary_key_ephemeral_index(table);
    let mut columns = vec![IndexColumn::new(
        "sequence",
        0,
        crate::translate::collate::CollationSeq::BINARY,
    )];
    for column in &primary_keys.columns {
        let mut column = column.clone();
        column.pos_in_table += 1;
        columns.push(column);
    }
    Arc::new(Index {
        columns,
        ..Index::clone(&primary_keys)
    })
}

/// Like [emit_primary_key_insert], with the number in `sequence_reg` before
/// the key.
fn emit_sequenced_primary_key_insert(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table_references: &TableReferences,
    internal_id: TableInternalId,
    table: &BTreeTable,
    key_cursor_id: usize,
    sequence_reg: usize,
) -> Result<()> {
    let key_len = table.primary_key_columns.len();
    let start_reg = program.alloc_registers(key_len + 1);
    program.emit_insn(Insn::Copy {
        src_reg: sequence_reg,
        dst_reg: start_reg,
        extra_amount: 0,
    });
    for (offset, (pk_name, _)) in table.primary_key_columns.iter().enumerate() {
        let (column, _) = table
            .get_column(pk_name)
            .expect("a PRIMARY KEY column is a column of its table");
        let expr = ast::Expr::Column {
            database: None,
            table: internal_id,
            column,
            is_rowid_alias: false,
        };
        translate_expr(
            program,
            Some(table_references),
            &expr,
            start_reg + 1 + offset,
            resolver,
        )?;
    }
    let record_reg = program.alloc_register();
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(start_reg),
        count: to_u32(key_len + 1),
        dest_reg: to_u32(record_reg),
        index_name: None,
        affinity_str: None,
    });
    program.emit_insn(Insn::IdxInsert {
        cursor_id: key_cursor_id,
        record_reg,
        unpacked_start: Some(start_reg),
        unpacked_count: Some(to_u32(key_len + 1)),
        flags: IdxInsertFlags::new(),
    });
    Ok(())
}

/// The new entry of an index for the new row of an UPDATE.
struct NewIndexEntry {
    start_reg: usize,
    num_regs: usize,
    /// The register that says whether the new row satisfies the WHERE clause
    /// of a partial index.
    where_reg: Option<usize>,
}

/// Emits the values of the new entry of `index` for the new row in the
/// registers from `new_reg`.
fn emit_new_index_entry(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table: &Arc<BTreeTable>,
    index: &Index,
    layout: &ColumnLayout,
    new_reg: usize,
) -> Result<NewIndexEntry> {
    let columns = table.columns();
    let unused_rowid_reg = program.alloc_register();
    let where_reg = match index.where_clause.as_deref() {
        Some(where_clause) => {
            let column_regs: Vec<usize> = (0..columns.len())
                .map(|position| layout.to_register(new_reg, position))
                .collect();
            let reg = program.alloc_register();
            emit_dml_expr_index_value(
                program,
                resolver,
                where_clause.clone(),
                columns,
                &column_regs,
                table,
                reg,
            )?;
            Some(reg)
        }
        None => None,
    };
    let num_regs = index.columns.len();
    let start_reg = program.alloc_registers(num_regs);
    for (offset, column) in index.columns.iter().enumerate() {
        emit_index_column_value_new_image(
            program,
            resolver,
            columns,
            new_reg,
            unused_rowid_reg,
            column,
            start_reg + offset,
            layout,
            table,
        )?;
    }
    Ok(NewIndexEntry {
        start_reg,
        num_regs,
        where_reg,
    })
}

fn emit_index_entry_insert(program: &mut ProgramBuilder, index_cursor_id: usize, entry: &NewIndexEntry) {
    let record_reg = program.alloc_register();
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(entry.start_reg),
        count: to_u32(entry.num_regs),
        dest_reg: to_u32(record_reg),
        index_name: None,
        affinity_str: None,
    });
    program.emit_insn(Insn::IdxInsert {
        cursor_id: index_cursor_id,
        record_reg,
        unpacked_start: Some(entry.start_reg),
        unpacked_count: Some(to_u32(entry.num_regs)),
        flags: IdxInsertFlags::new().nchange(true),
    });
}

/// Emits the jump to `same_row` when the primary key in the registers from
/// `found_reg` is the old primary key of the row, in the registers from
/// `old_key_reg`, compared with the PRIMARY KEY collations.
fn emit_jump_if_old_primary_key(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    found_reg: usize,
    old_key_reg: usize,
    same_row: BranchOffset,
) {
    let different = program.allocate_label();
    let key_len = table.primary_key_columns.len();
    for (offset, (pk_name, _)) in table.primary_key_columns.iter().enumerate() {
        let (_, column) = table
            .get_column(pk_name)
            .expect("a PRIMARY KEY column is a column of its table");
        let collation = table.primary_key_collation(pk_name, column);
        if offset + 1 == key_len {
            program.emit_insn(Insn::Eq {
                lhs: old_key_reg + offset,
                rhs: found_reg + offset,
                target_pc: same_row,
                flags: CmpInsFlags::default(),
                collation,
            });
        } else {
            program.emit_insn(Insn::Ne {
                lhs: old_key_reg + offset,
                rhs: found_reg + offset,
                target_pc: different,
                flags: CmpInsFlags::default(),
                collation,
            });
        }
    }
    program.preassign_label_to_next_insn(different);
}

/// Emits the uniqueness check of the new primary key of an UPDATE that
/// changes it.
#[allow(clippy::too_many_arguments)]
fn emit_update_primary_key_check(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table_references: &mut TableReferences,
    internal_id: TableInternalId,
    table: &Arc<BTreeTable>,
    table_cursor_id: usize,
    index_cursors: &[(Arc<Index>, usize)],
    layout: &ColumnLayout,
    new_reg: usize,
    old_key_reg: usize,
    statement_conflict: Option<ResolveType>,
    next_row: BranchOffset,
) -> Result<()> {
    let key_len = table.primary_key_columns.len();
    let new_key_reg = program.alloc_registers(key_len);
    let mut column_names = Vec::with_capacity(key_len);
    for (offset, (pk_name, _)) in table.primary_key_columns.iter().enumerate() {
        let (position, _) = table
            .get_column(pk_name)
            .expect("a PRIMARY KEY column is a column of its table");
        program.emit_insn(Insn::Copy {
            src_reg: layout.to_register(new_reg, position),
            dst_reg: new_key_reg + offset,
            extra_amount: 0,
        });
        column_names.push(pk_name.clone());
    }
    let unique = program.allocate_label();
    program.emit_insn(Insn::NoConflict {
        cursor_id: table_cursor_id,
        target_pc: unique,
        record_reg: new_key_reg,
        num_regs: key_len,
    });
    emit_jump_if_old_primary_key(program, table, new_key_reg, old_key_reg, unique);
    let on_conflict = statement_conflict
        .or(table.rowid_alias_conflict_clause)
        .unwrap_or(ResolveType::Abort);
    match on_conflict {
        ResolveType::Ignore => program.emit_insn(Insn::Goto { target_pc: next_row }),
        ResolveType::Replace => emit_row_delete(
            program,
            resolver,
            table_references,
            internal_id,
            table,
            table_cursor_id,
            index_cursors,
            DeletedRow::NotAChange,
        )?,
        _ => {
            let description = column_names
                .iter()
                .map(|name| format!("{}.{}", table.name, name))
                .collect::<Vec<_>>()
                .join(", ");
            let mut constraint_info = ConstraintInfo::primary_key(
                &table.name,
                table.primary_key_constraint_name.clone(),
                column_names,
                (0..key_len).map(|offset| new_key_reg + offset).collect(),
            );
            emit_constraint_values_decode(program, resolver, table, &mut constraint_info)?;
            program.emit_insn(Insn::Halt {
                constraint_info: Some(Box::new(constraint_info)),
                err_code: SQLITE_CONSTRAINT_PRIMARYKEY,
                description,
                on_error: program.constraint_halt_on_error(on_conflict),
                description_reg: None,
            });
        }
    }
    program.preassign_label_to_next_insn(unique);
    Ok(())
}

/// Emits the uniqueness check of the new entry of the UNIQUE index `index`.
#[allow(clippy::too_many_arguments)]
fn emit_update_unique_check(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table_references: &mut TableReferences,
    internal_id: TableInternalId,
    table: &Arc<BTreeTable>,
    table_cursor_id: usize,
    index_cursors: &[(Arc<Index>, usize)],
    index: &Index,
    index_cursor_id: usize,
    entry: &NewIndexEntry,
    old_key_reg: usize,
    statement_conflict: Option<ResolveType>,
    next_row: BranchOffset,
) -> Result<()> {
    let unique = program.allocate_label();
    if let Some(where_reg) = entry.where_reg {
        program.emit_insn(Insn::IfNot {
            reg: where_reg,
            jump_if_null: true,
            target_pc: unique,
        });
    }
    let key_len = index.key_columns().len();
    program.emit_insn(Insn::NoConflict {
        cursor_id: index_cursor_id,
        target_pc: unique,
        record_reg: entry.start_reg,
        num_regs: key_len,
    });
    let found_key_reg = program.alloc_registers(table.primary_key_columns.len());
    emit_primary_key_from_index(program, table, index, index_cursor_id, found_key_reg);
    emit_jump_if_old_primary_key(program, table, found_key_reg, old_key_reg, unique);
    let on_conflict = statement_conflict
        .or(index.on_conflict)
        .unwrap_or(ResolveType::Abort);
    match on_conflict {
        ResolveType::Ignore => program.emit_insn(Insn::Goto { target_pc: next_row }),
        ResolveType::Replace => {
            emit_seek_primary_key(program, table, table_cursor_id, found_key_reg, unique);
            emit_row_delete(
                program,
                resolver,
                table_references,
                internal_id,
                table,
                table_cursor_id,
                index_cursors,
                DeletedRow::NotAChange,
            )?;
        }
        _ => {
            let mut constraint_info = ConstraintInfo::unique_index(
                table,
                index,
                (0..key_len).map(|offset| entry.start_reg + offset).collect(),
            );
            emit_constraint_values_decode(program, resolver, table, &mut constraint_info)?;
            let err_code = constraint_info.kind.sqlite_extended_result_code();
            program.emit_insn(Insn::Halt {
                constraint_info: Some(Box::new(constraint_info)),
                err_code,
                description: format_unique_violation_desc(table, index),
                on_error: program.constraint_halt_on_error(on_conflict),
                description_reg: None,
            });
        }
    }
    program.preassign_label_to_next_insn(unique);
    Ok(())
}
