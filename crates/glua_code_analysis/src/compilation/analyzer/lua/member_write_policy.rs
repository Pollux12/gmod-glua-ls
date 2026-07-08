use crate::{DbIndex, LuaTypeCache, TypeOps, db_index::LuaType};

#[derive(Debug, Clone)]
pub(super) struct MemberAssignmentWideningState {
    pub(super) no_table_literal_widen_type: LuaType,
    pub(super) table_literal_widen_type: LuaType,
    pub(super) doc_type: Option<LuaType>,
    pub(super) all_table_assignment_merge_types: bool,
    class_bootstrap_type: Option<LuaType>,
    class_bootstrap_compatible: bool,
}

pub(super) enum MemberAssignmentWideningDecision {
    Widened(LuaType),
    ClassBootstrapRejected,
    NoPreviousAssignments,
}

impl MemberAssignmentWideningState {
    pub(super) fn from_assigned_type(assigned_type: &LuaType, doc_type: Option<LuaType>) -> Self {
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

    pub(super) fn from_type_cache(cache: &LuaTypeCache) -> Self {
        Self::from_assigned_type(
            cache.as_type(),
            cache.is_doc().then(|| cache.as_type().clone()),
        )
    }
}

pub(super) fn merge_member_assignment_widening_state(
    db: &DbIndex,
    state: &mut MemberAssignmentWideningState,
    new_state: MemberAssignmentWideningState,
    assigned_type: &LuaType,
) {
    state.no_table_literal_widen_type = TypeOps::Union.apply(
        db,
        &state.no_table_literal_widen_type,
        &new_state.no_table_literal_widen_type,
    );
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
    state.all_table_assignment_merge_types &= new_state.all_table_assignment_merge_types;
    merge_class_bootstrap_cache_state(
        state,
        assigned_type,
        new_state.class_bootstrap_type,
        new_state.class_bootstrap_compatible,
    );
}

pub(super) fn decide_member_assignment_widening<'a>(
    db: &DbIndex,
    incoming_type: &LuaType,
    allow_table_literal_widening: bool,
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

    let should_widen_table_literals = allow_table_literal_widening
        && is_table_assignment_merge_type(incoming_type)
        && previous_states
            .iter()
            .all(|state| state.all_table_assignment_merge_types);
    let previous_type = merge_assignment_types(
        db,
        previous_states.iter().map(|state| {
            if should_widen_table_literals {
                &state.table_literal_widen_type
            } else {
                &state.no_table_literal_widen_type
            }
        }),
    )
    .expect("previous states are non-empty");
    let incoming_type = widen_related_assignment_type(incoming_type, should_widen_table_literals);

    MemberAssignmentWideningDecision::Widened(TypeOps::Union.apply(
        db,
        &previous_type,
        &incoming_type,
    ))
}

pub(super) fn union_member_assignment_widening<'a>(
    db: &DbIndex,
    incoming_type: &LuaType,
    allow_table_literal_widening: bool,
    previous_states: impl IntoIterator<Item = &'a MemberAssignmentWideningState>,
) -> LuaType {
    let previous_states = previous_states.into_iter().collect::<Vec<_>>();
    let should_widen_table_literals = allow_table_literal_widening
        && is_table_assignment_merge_type(incoming_type)
        && previous_states
            .iter()
            .all(|state| state.all_table_assignment_merge_types);
    let incoming_type = widen_related_assignment_type(incoming_type, should_widen_table_literals);
    let Some(previous_type) = merge_assignment_types(
        db,
        previous_states.iter().map(|state| {
            if should_widen_table_literals {
                &state.table_literal_widen_type
            } else {
                &state.no_table_literal_widen_type
            }
        }),
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

pub(super) fn widen_related_assignment_type(typ: &LuaType, widen_table_literals: bool) -> LuaType {
    if widen_table_literals {
        return widen_table_literals_for_assignment(typ);
    }

    crate::widen_literal_type_for_assignment(typ)
}

fn widen_table_literals_for_assignment(typ: &LuaType) -> LuaType {
    match typ {
        LuaType::TableConst(_) => LuaType::Table,
        LuaType::Union(union) => LuaType::from_vec(
            union
                .into_vec()
                .into_iter()
                .map(|sub_type| widen_table_literals_for_assignment(&sub_type))
                .collect(),
        ),
        _ => crate::widen_literal_type_for_assignment(typ),
    }
}

fn is_table_assignment_merge_type(typ: &LuaType) -> bool {
    matches!(
        typ,
        LuaType::Table
            | LuaType::TableConst(_)
            | LuaType::Object(_)
            | LuaType::MergedTable(_)
            | LuaType::TableOf(_)
    )
}

fn prefer_class_assignment_type(typ: &LuaType) -> Option<LuaType> {
    match typ {
        LuaType::Def(def_id) => Some(LuaType::Def(def_id.clone())),
        LuaType::Ref(ref_id) => Some(LuaType::Ref(ref_id.clone())),
        LuaType::Instance(instance) => prefer_class_assignment_type(instance.get_base()),
        LuaType::TypeGuard(inner) => prefer_class_assignment_type(inner),
        LuaType::Union(union) => prefer_class_assignment_type_from_iter(union.types()),
        LuaType::Intersection(intersection) => {
            prefer_class_assignment_type_from_iter(intersection.get_types().iter())
        }
        LuaType::MultiLineUnion(union) => {
            prefer_class_assignment_type_from_iter(union.get_unions().iter().map(|(typ, _)| typ))
        }
        _ => None,
    }
}

fn prefer_class_assignment_type_from_iter<'a>(
    types: impl Iterator<Item = &'a LuaType>,
) -> Option<LuaType> {
    for typ in types {
        if let Some(class_type) = prefer_class_assignment_type(typ) {
            return Some(class_type);
        }
    }

    None
}

fn is_class_bootstrap_compatible_type(typ: &LuaType, class_type: &LuaType) -> bool {
    if is_same_class_type(typ, class_type) {
        return true;
    }

    match typ {
        LuaType::TypeGuard(inner) => is_class_bootstrap_compatible_type(inner, class_type),
        LuaType::Instance(instance) => {
            is_class_bootstrap_compatible_type(instance.get_base(), class_type)
                || is_table_bootstrap_type(typ)
        }
        LuaType::Union(union) => union
            .types()
            .all(|sub_type| is_class_bootstrap_compatible_type(sub_type, class_type)),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .all(|sub_type| is_class_bootstrap_compatible_type(sub_type, class_type)),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(sub_type, _)| is_class_bootstrap_compatible_type(sub_type, class_type)),
        _ => is_table_bootstrap_type(typ),
    }
}

fn is_class_neutral_bootstrap_type(typ: &LuaType) -> bool {
    if is_table_bootstrap_type(typ) {
        return true;
    }

    match typ {
        LuaType::TypeGuard(inner) => is_class_neutral_bootstrap_type(inner),
        LuaType::Union(union) => union.types().all(is_class_neutral_bootstrap_type),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .all(is_class_neutral_bootstrap_type),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(sub_type, _)| is_class_neutral_bootstrap_type(sub_type)),
        _ => false,
    }
}

fn is_same_class_type(left: &LuaType, right: &LuaType) -> bool {
    match (
        class_decl_id_from_type(left),
        class_decl_id_from_type(right),
    ) {
        (Some(left_id), Some(right_id)) => left_id == right_id,
        _ => false,
    }
}

fn class_decl_id_from_type(typ: &LuaType) -> Option<crate::LuaTypeDeclId> {
    match typ {
        LuaType::Def(def_id) | LuaType::Ref(def_id) => Some(def_id.clone()),
        LuaType::Instance(instance) => class_decl_id_from_type(instance.get_base()),
        LuaType::TypeGuard(inner) => class_decl_id_from_type(inner),
        _ => None,
    }
}

fn is_table_bootstrap_type(typ: &LuaType) -> bool {
    typ.is_table() || matches!(typ, LuaType::Unknown | LuaType::Nil | LuaType::Never)
}
