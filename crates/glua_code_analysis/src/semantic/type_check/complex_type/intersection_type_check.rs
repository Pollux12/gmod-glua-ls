use std::sync::Arc;

use crate::{
    LuaIntersectionType, LuaMemberOwner, LuaType, TypeCheckFailReason, TypeCheckResult,
    semantic::type_check::{
        check_general_type_compact, intersection_utils::intersection_to_object,
        type_check_context::TypeCheckContext, type_check_guard::TypeCheckGuard,
    },
};

pub fn check_intersection_type_compact(
    context: &mut TypeCheckContext,
    source_intersection: &LuaIntersectionType,
    compact_type: &LuaType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    match compact_type {
        LuaType::TableConst(range) => check_intersection_type_compact_table(
            context,
            source_intersection,
            LuaMemberOwner::Element(range.clone()),
            check_guard.next_level()?,
        ),
        LuaType::Def(_) | LuaType::Ref(_) => {
            if let Some(object_type) = intersection_to_object(context.db, source_intersection) {
                let object_type = LuaType::Object(Arc::new(object_type));
                check_general_type_compact(
                    context,
                    &object_type,
                    compact_type,
                    check_guard.next_level()?,
                )
            } else {
                Err(TypeCheckFailReason::TypeNotMatch)
            }
        }
        LuaType::Object(_) => {
            // Check whether the object satisfies all components of the intersection type
            for intersection_component in source_intersection.get_types() {
                // Keep expression-specific member facts while isolating mutable branch state.
                let mut component_context = context.clone();
                check_general_type_compact(
                    &mut component_context,
                    intersection_component,
                    compact_type,
                    check_guard.next_level()?,
                )?;
            }
            Ok(())
        }
        LuaType::Intersection(compact_intersection) => {
            // Intersection against intersection: check all components
            check_intersection_type_compact_intersection(
                context,
                source_intersection,
                compact_intersection,
                check_guard.next_level()?,
            )
        }
        LuaType::Table => Ok(()), // The generic table type matches any intersection type
        _ => {
            // For other types, check that at least one component is satisfied
            for intersection_component in source_intersection.get_types() {
                let mut component_context = context.clone();
                if check_general_type_compact(
                    &mut component_context,
                    intersection_component,
                    compact_type,
                    check_guard.next_level()?,
                )
                .is_ok()
                {
                    return Ok(());
                }
            }
            Err(TypeCheckFailReason::TypeNotMatch)
        }
    }
}

fn check_intersection_type_compact_table(
    context: &mut TypeCheckContext,
    source_intersection: &LuaIntersectionType,
    table_owner: LuaMemberOwner,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    // An intersection type requires TableConst to satisfy all components
    for intersection_component in source_intersection.get_types() {
        let mut component_context = context.clone();
        check_general_type_compact(
            &mut component_context,
            intersection_component,
            &LuaType::TableConst(
                table_owner
                    .get_element_range()
                    .ok_or(TypeCheckFailReason::TypeNotMatch)?
                    .clone(),
            ),
            check_guard.next_level()?,
        )?;
    }

    Ok(())
}

fn check_intersection_type_compact_intersection(
    context: &mut TypeCheckContext,
    source_intersection: &LuaIntersectionType,
    compact_intersection: &LuaIntersectionType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    // Check that every component of the source intersection finds a match in the target intersection
    for source_component in source_intersection.get_types() {
        let mut component_matched = false;

        for compact_component in compact_intersection.get_types() {
            if check_general_type_compact(
                context,
                source_component,
                compact_component,
                check_guard.next_level()?,
            )
            .is_ok()
            {
                component_matched = true;
                break;
            }
        }

        if !component_matched {
            return Err(TypeCheckFailReason::TypeNotMatch);
        }
    }

    Ok(())
}
