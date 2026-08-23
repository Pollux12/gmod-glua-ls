use std::ops::Deref;

use crate::db_index::r#type::types::lua_type_sort_key;
use crate::{DbIndex, LuaMultiLineUnion, LuaType, LuaUnionType, get_real_type};

// Union member *order* is preserved here, but the member *set* is
// canonical.

pub fn union_type(db: &DbIndex, source: LuaType, target: LuaType) -> LuaType {
    let match_source = get_real_type(db, &source).unwrap_or(&source);
    union_type_impl(match_source, &source, &target)
}

/// `union_type` without the alias dereference, for callers that have no `DbIndex`.
pub(crate) fn union_type_shallow(source: &LuaType, target: &LuaType) -> LuaType {
    union_type_impl(source, source, target)
}

/// Normalise a whole member set the same way repeated `union_type` would.
pub(crate) fn union_type_all(types: Vec<LuaType>) -> LuaType {
    // `any` is the one member that deliberately does NOT absorb its siblings
    // here, unlike the pairwise rule and upstream. A declared
    // `---@type any|string` has to keep its `string` arm — the arms are the
    // author's text, and dropping them stops param checking flagging the ones
    // that do not fit.
    //
    // Absorbing was measured twice and rejected both times: on every path it
    // takes the arm that carries a callable's real signature with it, which
    // cost 9 false `redundant-parameter` reports on StarfallEx (a method
    // resolving to a 0-parameter arm), and it buys no determinism — the
    // re-index gates already pass without it.
    //
    // `DocStringConst`/`DocIntegerConst` are held for the same reason: they only
    // exist where the author typed a literal, so `---@type string|"a"|"b"` has
    // to keep all three arms — the literals are what completion offers and what
    // hover shows. The pairwise rule still absorbs them, because joining two
    // types is a different question from listing the arms of a declared one.
    if types.iter().any(|typ| {
        matches!(
            typ,
            LuaType::Any | LuaType::DocStringConst(_) | LuaType::DocIntegerConst(_)
        )
    }) || can_use_structural_union(&types)
    {
        return LuaType::from_vec_structural(types);
    }

    if visiting_order_is_observable(&types) {
        return types.into_iter().fold(LuaType::Never, |result, typ| {
            union_type_shallow(&result, &typ)
        });
    }
    union_all_absorbed(types)
}

/// `union_type_all`'s fold without the per-step canonicalisation.
///
/// The pairwise fold rebuilds, de-duplicates and re-sorts the whole accumulated
/// union on every step, so joining n members costs O(n² log n) — and GMod
/// workspaces routinely produce unions with thousands of members (a `pairs()`
/// key type over a large config table, for one). Absorbing into a single member
/// list and canonicalising once gives the same answer for the same reason
/// `from_vec_structural` is safe to call last: the intermediate sorting cannot
/// change which members survive, only the order they are visited in, and the
/// final order comes from that last call either way.
///
/// Only valid where that visiting order is not observable, which
/// [`visiting_order_is_observable`] decides for the caller.
fn union_all_absorbed(types: Vec<LuaType>) -> LuaType {
    let mut members: Vec<LuaType> = Vec::with_capacity(types.len());
    for typ in types {
        match typ {
            // `never` is absorbed by any sibling, so it only survives when it is
            // all there is — and then the answer is `never`, not the empty union
            // `from_vec_structural` would turn into `nil`.
            LuaType::Never => {}
            LuaType::Union(union) => {
                for member in union.into_vec() {
                    if !matches!(member, LuaType::Never) {
                        absorb(&mut members, member);
                    }
                }
            }
            other => absorb(&mut members, other),
        }
    }

    if members.is_empty() {
        return LuaType::Never;
    }
    LuaType::from_vec_structural(members)
}

/// Whether the order `union_type_all` visits members in can change its answer.
///
/// A `MultiLineUnion` always matters: it matches an incoming literal against its
/// own arms rather than going through the absorption rules, so which side of the
/// join it lands on decides the result.
///
/// Otherwise the two paths can only disagree about member *order*, and only
/// when both of these hold. Sorting settles it if every member is
/// order-insensitive, since the final `from_vec_structural` orders them anyway.
/// And splicing only happens for a nested union: the pairwise rule joining a
/// plain accumulator to a union puts that union's members *first* and the
/// accumulator last, where absorbing in sequence keeps the accumulator first.
/// With no nested union to splice, the two visit members identically.
fn visiting_order_is_observable(types: &[LuaType]) -> bool {
    fn is_multi_line_union(typ: &LuaType) -> bool {
        matches!(typ, LuaType::MultiLineUnion(_))
    }

    let union_members = |typ: &LuaType, predicate: &dyn Fn(&LuaType) -> bool| match typ {
        LuaType::Union(union) => match union.as_ref() {
            LuaUnionType::Nullable(inner) => predicate(inner),
            LuaUnionType::Multi(members) => members.iter().any(predicate),
        },
        other => predicate(other),
    };

    if types
        .iter()
        .any(|typ| union_members(typ, &is_multi_line_union))
    {
        return true;
    }

    let order_sensitive = types.iter().any(|typ| {
        union_members(typ, &|member| {
            !LuaUnionType::is_order_insensitive_member(member)
        })
    });

    order_sensitive && types.iter().any(LuaType::is_union)
}

/// Whether `LuaType::from_vec_structural` alone matches the pairwise fold.
///
/// Skipping the fold is worth its own rule table: on a large workspace the
/// pairwise path costs ~65% more indexing time, and this decides the cases
/// where the two agree.
///
/// The flags mirror [`try_collapse`] arm for arm.
fn can_use_structural_union(types: &[LuaType]) -> bool {
    let (mut num, mut num_variant) = (false, false);
    let (mut int, mut int_const) = (false, false);
    let (mut string, mut string_const) = (false, false);
    let (mut boolean, mut bool_consts) = (false, 0u32);
    let (mut table, mut table_const) = (false, false);
    let (mut function, mut callable) = (false, false);

    for typ in types {
        match typ {
            LuaType::Never | LuaType::Union(_) | LuaType::MultiLineUnion(_) => return false,
            LuaType::Function => function = true,
            LuaType::DocFunction(_) | LuaType::Signature(_) => callable = true,
            LuaType::Number => num = true,
            LuaType::Integer => {
                num_variant = true;
                int = true;
            }
            LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) => {
                num_variant = true;
                int_const = true;
            }
            LuaType::FloatConst(_) => num_variant = true,
            LuaType::String => string = true,
            LuaType::StringConst(_) | LuaType::DocStringConst(_) => string_const = true,
            LuaType::Boolean => boolean = true,
            LuaType::BooleanConst(_) => bool_consts += 1,
            LuaType::Table => table = true,
            LuaType::TableConst(_) => table_const = true,
            _ => {}
        }

        if num && num_variant
            || int && int_const
            || string && string_const
            || boolean && bool_consts > 0
            || bool_consts > 1
            || table && table_const
            || function && callable
        {
            return false;
        }
    }

    true
}

fn union_type_impl(match_source: &LuaType, source: &LuaType, target: &LuaType) -> LuaType {
    // `any` absorbs everything, except that an explicit nil on the other side
    // survives: bare `any` is not treated as nullable by diagnostics like
    // NeedCheckNil, so collapsing `any | nil` to `any` loses information.
    match (match_source, target) {
        (LuaType::Any, right) if right.is_nullable() => return nullable_any_type(),
        (left, LuaType::Any) if left.is_nullable() => return nullable_any_type(),
        (LuaType::Any, _) | (_, LuaType::Any) => return LuaType::Any,
        _ => {}
    }

    if let Some(merged) = try_collapse(match_source, source, target) {
        return merged;
    }

    match (match_source, target) {
        (LuaType::MultiLineUnion(left), right) => {
            if multi_line_union_contains(left, right) {
                return source.clone();
            }
            LuaType::from_vec_structural(vec![source.clone(), target.clone()])
        }
        (left, LuaType::MultiLineUnion(right)) if multi_line_union_contains(right, left) => {
            target.clone()
        }
        // union
        (LuaType::Union(left), right) if !right.is_union() => {
            if let Some(merged) = union_sorted_insert(left, source, right) {
                return merged;
            }
            let mut members = left.deref().clone().into_vec();
            absorb(&mut members, right.clone());
            LuaType::from_vec_structural(members)
        }
        // The *source* joins the union, not the dereferenced view of it:
        // the alias is what the caller passed and what has to survive into
        // the rendered type, exactly as the sibling arms keep their
        // operands. The dereference is only ever a matching aid (see
        // `try_collapse`), and `absorb` matches an alias by identity, which
        // is the same answer the other two union arms give.
        (left, LuaType::Union(right)) if !left.is_union() => {
            let mut members = right.deref().clone().into_vec();
            absorb(&mut members, source.clone());
            LuaType::from_vec_structural(members)
        }
        // two union
        (LuaType::Union(left), LuaType::Union(right)) => {
            let mut members = left.into_vec();
            for member in right.into_vec() {
                absorb(&mut members, member);
            }
            LuaType::from_vec_structural(members)
        }

        _ => LuaType::from_vec_structural(vec![source.clone(), target.clone()]),
    }
}

/// The pairwise rules that collapse two union members into a single type.
///
/// Joining two types subsumes a literal into its primitive, author-written or
/// not. Listing the arms of a *declared* union is the other question, and
/// [`union_type_all`] answers it without these rules.
fn try_collapse(match_source: &LuaType, source: &LuaType, target: &LuaType) -> Option<LuaType> {
    Some(match (match_source, target) {
        (LuaType::Never, _) => target.clone(),
        (_, LuaType::Never) => source.clone(),
        // int | int const
        (LuaType::Integer, LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_)) => {
            LuaType::Integer
        }
        (LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_), LuaType::Integer) => {
            LuaType::Integer
        }
        // float | float const
        (LuaType::Number, right) if right.is_number() => LuaType::Number,
        (left, LuaType::Number) if left.is_number() => LuaType::Number,
        // string | string const
        (LuaType::String, LuaType::StringConst(_) | LuaType::DocStringConst(_)) => LuaType::String,
        (LuaType::StringConst(_) | LuaType::DocStringConst(_), LuaType::String) => LuaType::String,
        // boolean | boolean const
        (LuaType::Boolean, LuaType::BooleanConst(_)) => LuaType::Boolean,
        (LuaType::BooleanConst(_), LuaType::Boolean) => LuaType::Boolean,
        (LuaType::BooleanConst(left), LuaType::BooleanConst(right)) => {
            if left == right {
                LuaType::BooleanConst(*left)
            } else {
                LuaType::Boolean
            }
        }
        // table | table const
        (LuaType::Table, LuaType::TableConst(_)) => LuaType::Table,
        (LuaType::TableConst(_), LuaType::Table) => LuaType::Table,
        // function | function const
        (LuaType::Function, LuaType::DocFunction(_) | LuaType::Signature(_)) => LuaType::Function,
        (LuaType::DocFunction(_) | LuaType::Signature(_), LuaType::Function) => LuaType::Function,
        // class references
        (LuaType::Ref(id1), LuaType::Ref(id2)) if id1 == id2 => source.clone(),
        // same type
        (left, right) if *left == *right => source.clone(),
        _ => return None,
    })
}

/// Whether `other` is already one of `union`'s arms.
///
/// A multi-line union's arms are author-written literals, so an inferred
/// literal of the same value is the same member.
fn multi_line_union_contains(union: &LuaMultiLineUnion, other: &LuaType) -> bool {
    union
        .get_unions()
        .iter()
        .any(|(member, _)| match (member, other) {
            (LuaType::DocStringConst(a), LuaType::StringConst(b)) => a == b,
            (LuaType::DocIntegerConst(a), LuaType::IntegerConst(b)) => a == b,
            _ => false,
        })
}

/// Add `ty` to an existing union's member list, applying absorption rules.
fn absorb(members: &mut Vec<LuaType>, ty: LuaType) {
    let mut ty = ty;
    // A merged member keeps the slot of the member it merged with, so absorbing
    // never reorders the survivors. Member order is semantic for overloads and
    // template dispatch, which the structural constructor deliberately leaves
    // alone.
    let mut slot = members.len();

    'restart: loop {
        for i in 0..members.len() {
            if let Some(merged) = try_collapse(&members[i], &members[i], &ty) {
                members.remove(i);
                ty = merged;
                slot = slot.min(i);
                continue 'restart;
            }
        }
        break;
    }

    members.insert(slot.min(members.len()), ty);
}

fn nullable_any_type() -> LuaType {
    LuaType::Union(LuaUnionType::Nullable(LuaType::Any).into())
}

/// Adding one member to an already-canonical union, without rebuilding it.
///
/// The general arm clones every member, rescans them all for something to
/// collapse with (its last rule is a full structural equality), then
/// de-duplicates through a hash set and re-sorts — and the sort key hashes a
/// type's *name*. That is O(n log n) with an expensive constant, paid for every
/// `or` in a chain, and GMod workspaces build unions thousands of members wide:
/// measured on a gamemode edit, this arm alone walked 12.5M members across 23k
/// calls for a single keystroke.
///
/// `LuaUnionType::from_vec` leaves an order-insensitive union sorted by
/// `lua_type_sort_key`, so for those the same answer is a binary search. Returns
/// `None` whenever that shortcut cannot be justified, leaving the general arm to
/// decide.
fn union_sorted_insert(left: &LuaUnionType, source: &LuaType, right: &LuaType) -> Option<LuaType> {
    let LuaUnionType::Multi(members) = left else {
        // A `Nullable` is not stored in sort order.
        return None;
    };
    // `any` and `never` have absorbing rules of their own, and a multi-line
    // union matches by value rather than by these rules.
    if matches!(
        right,
        LuaType::Never | LuaType::Any | LuaType::MultiLineUnion(_)
    ) || !LuaUnionType::is_order_insensitive_member(right)
    {
        return None;
    }
    if !members.iter().all(|member| {
        LuaUnionType::is_order_insensitive_member(member)
            && !matches!(member, LuaType::Never | LuaType::MultiLineUnion(_))
    }) {
        return None;
    }

    // Anything `right` could collapse with sorts under a known discriminant, so
    // its absence is a binary search rather than a scan. Finding one means a
    // merge is due, which the general arm performs.
    if collapse_partner_ordinals(right)
        .iter()
        .any(|ordinal| contains_ordinal(members, *ordinal))
    {
        return None;
    }

    let key = lua_type_sort_key(right);
    match members.binary_search_by(|member| lua_type_sort_key(member).cmp(&key)) {
        // Equal sort keys: usually the same member already present, leaving the
        // union unchanged. Otherwise two types collided on the key and the
        // general arm settles it.
        Ok(hit) => {
            let mut start = hit;
            while start > 0 && lua_type_sort_key(&members[start - 1]) == key {
                start -= 1;
            }
            members[start..]
                .iter()
                .take_while(|member| lua_type_sort_key(member) == key)
                .any(|member| member == right)
                .then(|| source.clone())
        }
        Err(at) => {
            let mut inserted = Vec::with_capacity(members.len() + 1);
            inserted.extend_from_slice(&members[..at]);
            inserted.push(right.clone());
            inserted.extend_from_slice(&members[at..]);
            // Already at least three members, so `from_vec`'s nullable collapse
            // cannot apply and this is the order it would have produced.
            Some(LuaType::Union(LuaUnionType::Multi(inserted).into()))
        }
    }
}

/// The `lua_type_sort_key` discriminants of everything [`try_collapse`] would
/// merge `typ` with, other than an equal member.
fn collapse_partner_ordinals(typ: &LuaType) -> &'static [u8] {
    match typ {
        LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) => &[4, 7],
        LuaType::FloatConst(_) => &[7],
        LuaType::StringConst(_) | LuaType::DocStringConst(_) => &[9],
        LuaType::BooleanConst(_) => &[1, 2],
        LuaType::TableConst(_) => &[12],
        LuaType::DocFunction(_) | LuaType::Signature(_) => &[16],
        LuaType::Integer => &[5, 6, 7],
        LuaType::Number => &[4, 5, 6, 8],
        LuaType::String => &[10, 11],
        LuaType::Boolean => &[2],
        LuaType::Table => &[13],
        LuaType::Function => &[17, 38],
        _ => &[],
    }
}

/// Whether a sorted member list holds any type with this sort discriminant.
fn contains_ordinal(members: &[LuaType], ordinal: u8) -> bool {
    let at = members.partition_point(|member| lua_type_sort_key(member).0 < ordinal);
    members
        .get(at)
        .is_some_and(|member| lua_type_sort_key(member).0 == ordinal)
}

#[cfg(test)]
mod union_shortcut_tests {
    use super::*;
    use crate::LuaTypeDeclId;
    use internment::ArcIntern;
    use smol_str::SmolStr;

    /// The pairwise fold both shortcuts replace.
    fn fold(types: Vec<LuaType>) -> LuaType {
        types.into_iter().fold(LuaType::Never, |result, typ| {
            union_type_shallow(&result, &typ)
        })
    }

    fn sample(pick: u64) -> LuaType {
        match pick % 16 {
            0 => LuaType::Nil,
            1 => LuaType::Boolean,
            2 => LuaType::BooleanConst(pick % 32 < 16),
            3 => LuaType::Integer,
            4 => LuaType::IntegerConst((pick % 5) as i64),
            5 => LuaType::Number,
            6 => LuaType::FloatConst((pick % 3) as f64),
            7 => LuaType::String,
            8 => LuaType::StringConst(ArcIntern::new(SmolStr::new(match pick % 4 {
                0 => "a",
                1 => "b",
                2 => "c",
                _ => "d",
            }))),
            9 => LuaType::Table,
            10 => LuaType::Function,
            11 => LuaType::Userdata,
            12 => LuaType::Thread,
            13 => LuaType::Ref(LuaTypeDeclId::global(match pick % 3 {
                0 => "Alpha",
                1 => "Beta",
                _ => "Gamma",
            })),
            14 => LuaType::Unknown,
            _ => LuaType::Never,
        }
    }

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        }
    }

    /// `union_all_absorbed` exists only to be a faster spelling of the fold, so
    /// the thing worth testing is that it never disagrees with it — including
    /// the collapses that cascade (`1 | 2 | integer`) and the merges that move a
    /// member into an earlier slot.
    #[test]
    fn absorbing_in_one_pass_matches_the_pairwise_fold() {
        let mut next = rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..2000 {
            let count = (next() % 10) as usize + 1;
            let types = (0..count).map(|_| sample(next())).collect::<Vec<_>>();
            if visiting_order_is_observable(&types) {
                continue;
            }
            assert_eq!(
                union_all_absorbed(types.clone()),
                fold(types.clone()),
                "absorbed and folded unions disagree for {types:?}"
            );
        }
    }

    /// Likewise the sorted insert: it has to decline the collapses (a literal
    /// meeting its primitive), spot the duplicates, and reproduce the ordering.
    #[test]
    fn sorted_insert_matches_rebuilding_the_union() {
        let mut next = rng(0x9e37_79b9_7f4a_7c15);
        let mut exercised = 0;
        for _ in 0..4000 {
            let count = (next() % 8) as usize + 2;
            let members = (0..count).map(|_| sample(next())).collect::<Vec<_>>();
            let LuaType::Union(union) = LuaType::from_vec_structural(members) else {
                continue;
            };
            let incoming = sample(next());
            let source = LuaType::Union(union.clone());

            let general = {
                let mut rebuilt = union.deref().clone().into_vec();
                absorb(&mut rebuilt, incoming.clone());
                LuaType::from_vec_structural(rebuilt)
            };
            if let Some(fast) = union_sorted_insert(&union, &source, &incoming) {
                exercised += 1;
                assert_eq!(
                    fast, general,
                    "sorted insert disagreed for {union:?} | {incoming:?}"
                );
            }
        }
        assert!(
            exercised > 100,
            "fixture never exercised the fast path ({exercised} hits)"
        );
    }

    #[test]
    fn a_primitive_absorbs_every_literal_of_its_family_at_once() {
        let types = vec![
            LuaType::IntegerConst(1),
            LuaType::IntegerConst(2),
            LuaType::IntegerConst(3),
            LuaType::Integer,
        ];
        assert_eq!(union_all_absorbed(types.clone()), fold(types));
    }

    #[test]
    fn two_different_boolean_literals_collapse_to_boolean() {
        let types = vec![LuaType::BooleanConst(true), LuaType::BooleanConst(false)];
        assert_eq!(union_all_absorbed(types.clone()), fold(types));
    }

    #[test]
    fn distinct_class_references_are_not_confused_by_sharing_a_variant() {
        let types = vec![
            LuaType::Ref(LuaTypeDeclId::global("Alpha")),
            LuaType::Ref(LuaTypeDeclId::global("Beta")),
            LuaType::Ref(LuaTypeDeclId::global("Alpha")),
        ];
        assert_eq!(union_all_absorbed(types.clone()), fold(types));
    }
}
