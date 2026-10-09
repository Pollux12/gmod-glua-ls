use rustc_hash::FxHashMap;

use glua_parser::{LuaAstNode, LuaIndexKey, LuaTableExpr, NumberResult};

use crate::{DiagnosticCode, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct DuplicateIndexChecker;

impl Checker for DuplicateIndexChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::DuplicateIndex];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let Some(buckets) = context.file_buckets() else {
            return;
        };
        for table in buckets.table_exprs.iter().cloned() {
            if context.is_cancelled() {
                return;
            }
            check_table_duplicate_index(context, semantic_model, table);
        }
    }
}

fn check_table_duplicate_index(
    context: &mut DiagnosticContext,
    _: &SemanticModel,
    table: LuaTableExpr,
) -> Option<()> {
    if context.is_cancelled() {
        return Some(());
    }

    let fields = table.get_fields().collect::<Vec<_>>();
    if fields.len() > 50 {
        // Skip checking if there are too many fields to avoid performance issues
        return Some(());
    }

    let mut index_map: FxHashMap<DuplicateKey, Vec<LuaIndexKey>> = FxHashMap::default();

    for field in fields {
        if context.is_cancelled() {
            return Some(());
        }
        let key = field.get_field_key();
        if let Some(key) = key {
            index_map
                .entry(DuplicateKey::from_index_key(&key))
                .or_default()
                .push(key);
        }
    }

    for keys in index_map.into_values() {
        if context.is_cancelled() {
            return Some(());
        }
        if keys.len() > 1 {
            for key in keys {
                if context.is_cancelled() {
                    return Some(());
                }
                let range = if let Some(range) = key.get_range() {
                    range
                } else {
                    continue;
                };
                context.add_diagnostic(
                    DiagnosticCode::DuplicateIndex,
                    range,
                    format!("Duplicate index `{name}`.", name = key.get_path_part()),
                    None,
                );
            }
        }
    }

    Some(())
}

/// Two table keys are the same Lua key only when their kind and value agree;
/// integers, integral floats and positional entries share one numeric key.
#[derive(Debug, PartialEq, Eq, Hash)]
enum DuplicateKey {
    Name(String),
    Integer(i64),
    Unsigned(u64),
    Float(u64),
    Expr(String),
}

impl DuplicateKey {
    fn from_index_key(key: &LuaIndexKey) -> Self {
        match key {
            LuaIndexKey::Name(name) => Self::Name(name.get_name_text().to_string()),
            LuaIndexKey::String(string) => Self::Name(string.get_value()),
            LuaIndexKey::Integer(number) => match number.get_number_value() {
                NumberResult::Int(value) => Self::Integer(value),
                NumberResult::Uint(value) => Self::Unsigned(value),
                NumberResult::Float(value) => Self::from_float(value),
            },
            LuaIndexKey::Expr(expr) => Self::Expr(expr.syntax().text().to_string()),
            LuaIndexKey::Idx(index) => Self::Integer(*index as i64),
        }
    }

    fn from_float(value: f64) -> Self {
        // `i64::MIN` is exactly -2^63 and `-i64::MIN as f64` is 2^63, the first float out of range.
        if value.fract() == 0.0 && value >= i64::MIN as f64 && value < -(i64::MIN as f64) {
            Self::Integer(value as i64)
        } else {
            Self::Float(value.to_bits())
        }
    }
}
