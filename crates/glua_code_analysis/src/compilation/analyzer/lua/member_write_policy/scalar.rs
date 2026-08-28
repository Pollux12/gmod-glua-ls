use crate::{
    DbIndex, LuaTypeCache, TypeOps, db_index::LuaType, is_class_bootstrap_compatible_type,
    is_class_neutral_bootstrap_type, is_same_class_type, is_table_assignment_merge_type,
    merge_table_assignment_types, prefer_class_assignment_type, widen_related_assignment_type,
};

#[derive(Debug, Clone)]
pub(in crate::compilation::analyzer::lua) struct MemberAssignmentWideningState {
    pub(in crate::compilation::analyzer::lua) no_table_literal_widen_type: LuaType,
    pub(in crate::compilation::analyzer::lua) table_literal_widen_type: LuaType,
    pub(in crate::compilation::analyzer::lua) doc_type: Option<LuaType>,
    pub(in crate::compilation::analyzer::lua) all_table_assignment_merge_types: bool,
    class_bootstrap_type: Option<LuaType>,
    class_bootstrap_compatible: bool,
}

pub(in crate::compilation::analyzer::lua) enum MemberAssignmentWideningDecision {
    Widened(LuaType),
    ClassBootstrapRejected,
    NoPreviousAssignments,
}

impl MemberAssignmentWideningState {
    pub(in crate::compilation::analyzer::lua) fn from_assigned_type(
        assigned_type: &LuaType,
        doc_type: Option<LuaType>,
    ) -> Self {
        let (class_bootstrap_type, class_bootstrap_compatible) =
            class_bootstrap_cache_state(assigned_type);

        Self {
            no_table_literal_widen_type: widen_related_assignment_type(assigned_type, false),
            table_literal_widen_type: widen_related_assignment_type(assigned_type, true),
            doc_type,
            all_table_assignment_merge_types: is_table_assignment_merge_type(assigned_type),
            class_bootstrap_type,
            class_bootstrap_compatible,
        }
    }

    pub(in crate::compilation::analyzer::lua) fn from_type_cache(cache: &LuaTypeCache) -> Self {
        Self::from_assigned_type(
            cache.as_type(),
            cache.is_doc().then(|| cache.as_type().clone()),
        )
    }
}

pub(in crate::compilation::analyzer::lua) fn merge_member_assignment_widening_state(
    db: &DbIndex,
    state: &mut MemberAssignmentWideningState,
    new_state: MemberAssignmentWideningState,
    assigned_type: &LuaType,
) {
    state.all_table_assignment_merge_types &= new_state.all_table_assignment_merge_types;
    if state.all_table_assignment_merge_types {
        state.no_table_literal_widen_type = merge_table_assignment_types(
            db,
            vec![
                state.no_table_literal_widen_type.clone(),
                new_state.no_table_literal_widen_type,
            ],
        );
    } else {
        state.no_table_literal_widen_type = TypeOps::Union.apply(
            db,
            &state.no_table_literal_widen_type,
            &new_state.no_table_literal_widen_type,
        );
    }
    state.table_literal_widen_type = TypeOps::Union.apply(
        db,
        &state.table_literal_widen_type,
        &new_state.table_literal_widen_type,
    );
    if let Some(doc_type) = new_state.doc_type {
        state.doc_type = Some(match state.doc_type.take() {
            Some(current) => TypeOps::Union.apply(db, &current, &doc_type),
            None => doc_type,
        });
    }
    merge_class_bootstrap_cache_state(
        state,
        assigned_type,
        new_state.class_bootstrap_type,
        new_state.class_bootstrap_compatible,
    );
}

pub(in crate::compilation::analyzer::lua) fn decide_member_assignment_widening<'a>(
    db: &DbIndex,
    incoming_type: &LuaType,
    _allow_table_literal_widening: bool,
    previous_states: impl IntoIterator<Item = &'a MemberAssignmentWideningState>,
) -> MemberAssignmentWideningDecision {
    let previous_states = previous_states.into_iter().collect::<Vec<_>>();
    if previous_states.is_empty() {
        return MemberAssignmentWideningDecision::NoPreviousAssignments;
    }

    if let Some(doc_type) = merge_assignment_types(
        db,
        previous_states
            .iter()
            .filter_map(|state| state.doc_type.as_ref()),
    ) {
        return MemberAssignmentWideningDecision::Widened(doc_type);
    }

    if !matches!(
        incoming_type,
        LuaType::Union(_) | LuaType::Intersection(_) | LuaType::MultiLineUnion(_)
    ) && let Some(class_type) = prefer_class_assignment_type(incoming_type)
    {
        if !is_class_bootstrap_compatible_type(incoming_type, &class_type) {
            return MemberAssignmentWideningDecision::ClassBootstrapRejected;
        }

        let class_bootstrap_compatible = previous_states.iter().all(|state| {
            state.class_bootstrap_compatible
                && state
                    .class_bootstrap_type
                    .as_ref()
                    .is_none_or(|cached_class| is_same_class_type(cached_class, &class_type))
        });
        if class_bootstrap_compatible {
            return MemberAssignmentWideningDecision::Widened(class_type);
        }

        return MemberAssignmentWideningDecision::ClassBootstrapRejected;
    }

    if should_merge_table_literals(incoming_type, &previous_states) {
        return MemberAssignmentWideningDecision::Widened(merged_table_assignment_type(
            db,
            incoming_type,
            &previous_states,
        ));
    }

    let previous_type = merge_assignment_types(
        db,
        previous_states
            .iter()
            .map(|state| &state.no_table_literal_widen_type),
    )
    .expect("previous states are non-empty");
    let incoming_type = widen_related_assignment_type(incoming_type, false);

    MemberAssignmentWideningDecision::Widened(TypeOps::Union.apply(
        db,
        &previous_type,
        &incoming_type,
    ))
}

/// Whether every writer of this slot assigns a table, so the answer is their
/// merge rather than a union of widened forms.
fn should_merge_table_literals(
    incoming_type: &LuaType,
    previous_states: &[&MemberAssignmentWideningState],
) -> bool {
    is_table_assignment_merge_type(incoming_type)
        && previous_states
            .iter()
            .all(|state| state.all_table_assignment_merge_types)
}

fn merged_table_assignment_type(
    db: &DbIndex,
    incoming_type: &LuaType,
    previous_states: &[&MemberAssignmentWideningState],
) -> LuaType {
    let mut components = Vec::with_capacity(previous_states.len() + 1);
    components.push(widen_related_assignment_type(incoming_type, false));
    for state in previous_states {
        let component = &state.no_table_literal_widen_type;
        if !components.contains(component) {
            components.push(component.clone());
        }
    }

    merge_table_assignment_types(db, components)
}

pub(in crate::compilation::analyzer::lua) fn union_member_assignment_widening<'a>(
    db: &DbIndex,
    incoming_type: &LuaType,
    _allow_table_literal_widening: bool,
    previous_states: impl IntoIterator<Item = &'a MemberAssignmentWideningState>,
) -> LuaType {
    let previous_states = previous_states.into_iter().collect::<Vec<_>>();
    if !previous_states.is_empty() && should_merge_table_literals(incoming_type, &previous_states) {
        return merged_table_assignment_type(db, incoming_type, &previous_states);
    }

    let incoming_type = widen_related_assignment_type(incoming_type, false);
    let Some(previous_type) = merge_assignment_types(
        db,
        previous_states
            .iter()
            .map(|state| &state.no_table_literal_widen_type),
    ) else {
        return incoming_type;
    };

    TypeOps::Union.apply(db, &previous_type, &incoming_type)
}

fn class_bootstrap_cache_state(typ: &LuaType) -> (Option<LuaType>, bool) {
    if let Some(class_type) = prefer_class_assignment_type(typ) {
        let compatible = is_class_bootstrap_compatible_type(typ, &class_type);
        return (Some(class_type), compatible);
    }

    (None, is_class_neutral_bootstrap_type(typ))
}

fn merge_class_bootstrap_cache_state(
    state: &mut MemberAssignmentWideningState,
    assigned_type: &LuaType,
    assigned_class_type: Option<LuaType>,
    assigned_class_compatible: bool,
) {
    if !state.class_bootstrap_compatible {
        return;
    }

    match (&state.class_bootstrap_type, assigned_class_type) {
        (_, Some(class_type)) => {
            state.class_bootstrap_compatible = assigned_class_compatible
                && state
                    .class_bootstrap_type
                    .as_ref()
                    .is_none_or(|current_class| is_same_class_type(current_class, &class_type));
            if state.class_bootstrap_compatible && state.class_bootstrap_type.is_none() {
                state.class_bootstrap_type = Some(class_type);
            }
        }
        (Some(class_type), None) => {
            state.class_bootstrap_compatible =
                is_class_bootstrap_compatible_type(assigned_type, class_type);
        }
        (None, None) => {
            state.class_bootstrap_compatible = is_class_neutral_bootstrap_type(assigned_type);
        }
    }
}

fn merge_assignment_types<'a>(
    db: &DbIndex,
    types: impl Iterator<Item = &'a LuaType>,
) -> Option<LuaType> {
    let mut result = None;
    for typ in types {
        result = Some(match result {
            Some(current) => TypeOps::Union.apply(db, &current, typ),
            None => typ.clone(),
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use rowan::{TextRange, TextSize};

    use crate::{FileId, InFiled, db_index::LuaType};

    use super::*;

    fn table_const(start: u32, end: u32) -> LuaType {
        LuaType::TableConst(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(start), TextSize::new(end)),
        ))
    }

    #[test]
    fn assignment_table_literal_widening_recurses_into_union_members() {
        let typ = LuaType::from_vec(vec![table_const(1, 2), LuaType::String]);

        let widened = widen_related_assignment_type(&typ, true);

        let LuaType::Union(union) = widened else {
            panic!("expected widened union");
        };
        assert!(union.types().any(|typ| matches!(typ, LuaType::Table)));
        assert!(union.types().any(|typ| matches!(typ, LuaType::String)));
        assert!(
            !union
                .types()
                .any(|typ| matches!(typ, LuaType::TableConst(_)))
        );
    }
}
