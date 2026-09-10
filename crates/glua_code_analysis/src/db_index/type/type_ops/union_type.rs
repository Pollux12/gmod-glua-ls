use crate::db_index::r#type::types::{lua_type_sort_key, lua_type_sort_ordinal};
use crate::{DbIndex, LuaMultiLineUnion, LuaType, LuaUnionType, get_real_type};
use rustc_hash::{FxHashMap, FxHashSet};

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
    union_type_all_mode(types, false)
}

/// Normalise inferred alternatives while deferring canonical ordering until
/// the final union is built.
pub(crate) fn union_type_all_inferred(types: Vec<LuaType>) -> LuaType {
    union_type_all_mode(types, true)
}

fn union_type_all_mode(types: Vec<LuaType>, inferred: bool) -> LuaType {
    if types.len() < 2 {
        return LuaType::from_vec_structural(types);
    }

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
        if inferred
            && !types.iter().any(contains_multi_line_union)
            && !types.iter().any(contains_preserved_union_member)
            && !contains_distinct_sort_key_collision(&types)
        {
            return union_all_absorbed_inferred(types);
        }
        return types.into_iter().fold(LuaType::Never, |result, typ| {
            union_type_shallow(&result, &typ)
        });
    }
    if inferred {
        union_all_absorbed_inferred(types)
    } else {
        union_all_absorbed(types)
    }
}

/// Batch equivalent of folding [`union_type`] from [`LuaType::Never`].
///
/// The result is exactly the ordered pairwise fold for every input. A
/// structural fast path runs only when proven equivalent; otherwise this
/// literally performs that fold.
pub fn union_all(db: &DbIndex, types: Vec<LuaType>) -> LuaType {
    match types.len() {
        0 => return LuaType::Never,
        // `Never | t` is `t` via the `Never` collapse arm, with no
        // dereference in play (`Never` is not a `Ref`).
        1 => return types.into_iter().next().unwrap_or(LuaType::Never),
        _ => {}
    }

    if can_use_db_aware_structural_union(db, &types) {
        return LuaType::from_vec_structural(types);
    }

    types
        .into_iter()
        .fold(LuaType::Never, |acc, ty| union_type(db, acc, ty))
}

/// Whether [`LuaType::from_vec_structural`] matches the DB-aware ordered fold.
///
/// Proof of equivalence when this returns true:
/// - No top-level `Any`: `Any` absorbs siblings (`Any | T` is `Any`,
///   `Any | nil` is `Any?`) while the structural constructor keeps both.
/// - No top-level `Never`: `Never` is the fold identity (`Never | T` is `T`)
///   while the structural constructor keeps it as an arm.
/// - No top-level `Union`/`MultiLineUnion`: joining a plain accumulator to a
///   union splices that union's members first, where sequential absorption
///   keeps the accumulator first; a multi-line union matches an incoming
///   literal by value, so which side it lands on decides the result.
/// - No top-level alias `Ref`: the fold dereferences the accumulator
///   (`get_real_type`) for matching only, so an alias to a primitive, to
///   `Any`, or to a union collapses or splices differently than the
///   structural path, which matches a `Ref` by identity. Non-alias `Ref`s
///   dereference to themselves and are safe.
/// - No absorption pair per [`can_use_structural_union`]: primitive absorbs
///   literal (`Integer` absorbs `IntegerConst`, `Number` absorbs numerics,
///   `String` absorbs string consts, `Boolean` absorbs `BooleanConst`, two
///   distinct `BooleanConst`s become `Boolean`, `Table` absorbs `TableConst`,
///   `Function` absorbs callables), while the structural constructor keeps
///   both arms.
/// - No observable visiting order per [`visiting_order_is_observable`]:
///   with the union cases above already rejected this only restates that no
///   nested multi-line union and no order-sensitive callable members mixed
///   with a spliced union can reorder survivors.
///
/// Anything not proven safe above falls back to the literal fold.
fn can_use_db_aware_structural_union(db: &DbIndex, types: &[LuaType]) -> bool {
    for typ in types {
        match typ {
            LuaType::Any | LuaType::Never | LuaType::Union(_) | LuaType::MultiLineUnion(_) => {
                return false;
            }
            LuaType::Ref(id) => {
                if db
                    .get_type_index()
                    .get_type_decl(id)
                    .is_some_and(|decl| decl.is_alias())
                {
                    return false;
                }
            }
            _ => {}
        }
    }

    if !can_use_structural_union(types) {
        return false;
    }

    if visiting_order_is_observable(types) {
        return false;
    }

    true
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
    let members = absorbed_members(types);
    if members.is_empty() {
        return LuaType::Never;
    }
    LuaType::from_vec_structural(members)
}

/// The same absorbed member set, left unsorted for the inferred constructor's
/// one final canonicalisation pass.
fn union_all_absorbed_inferred(types: Vec<LuaType>) -> LuaType {
    let mut members = absorbed_members(types);
    match members.len() {
        0 => LuaType::Never,
        1 => members.pop().unwrap_or(LuaType::Never),
        _ => LuaType::Union(LuaUnionType::from_multi_unchecked(members).into()),
    }
}

fn absorbed_members(types: Vec<LuaType>) -> Vec<LuaType> {
    let mut members: Vec<LuaType> = Vec::with_capacity(types.len());
    let mut present = 0u64;
    let mut hashed: FxHashSet<LuaType> =
        FxHashSet::with_capacity_and_hasher(types.len(), Default::default());
    for typ in types {
        match typ {
            // `never` is absorbed by any sibling, so it only survives when it is
            // all there is — and then the answer is `never`, not the empty union
            // `from_vec_structural` would turn into `nil`.
            LuaType::Never => {}
            LuaType::Union(union) => {
                for member in union.into_vec() {
                    if !matches!(member, LuaType::Never) {
                        add_member(&mut members, &mut present, &mut hashed, member);
                    }
                }
            }
            other => add_member(&mut members, &mut present, &mut hashed, other),
        }
    }

    members
}

fn contains_multi_line_union(typ: &LuaType) -> bool {
    match typ {
        LuaType::MultiLineUnion(_) => true,
        LuaType::Union(union) => union.types().any(contains_multi_line_union),
        _ => false,
    }
}

fn contains_preserved_union_member(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any
        | LuaType::Never
        | LuaType::DocStringConst(_)
        | LuaType::DocIntegerConst(_) => true,
        LuaType::FloatConst(value) if *value == 0.0 => true,
        LuaType::Union(union) => union.types().any(contains_preserved_union_member),
        _ => false,
    }
}

fn contains_distinct_sort_key_collision(types: &[LuaType]) -> bool {
    fn visit<'a>(typ: &'a LuaType, keys: &mut FxHashMap<(u8, u64), &'a LuaType>) -> bool {
        if let LuaType::Union(union) = typ {
            return union.types().any(|member| visit(member, keys));
        }
        let key = lua_type_sort_key(typ);
        if let Some(existing) = keys.get(&key) {
            return *existing != typ;
        }
        keys.insert(key, typ);
        false
    }
    let mut keys = FxHashMap::default();
    types.iter().any(|typ| visit(typ, &mut keys))
}

/// [`absorb`], with the common case answered by a hash set instead of a scan.
///
/// `absorb` scans every member sharing a discriminant with the incoming type,
/// and the rule that decides them is a full structural equality — so a union of
/// n distinct `Ref`s or `TableConst`s costs O(n²). When nothing already present
/// can collapse with `ty` across discriminants, and `ty`'s own discriminant has
/// no same-discriminant rule but equality, that scan is exactly a de-duplication
/// and `hashed` gives the same answer in one hash.
///
/// `hashed` holds every member this shortcut inserted, and only those: a member
/// `absorb` inserted or merged away is one whose partner discriminant is set in
/// `present`, and `present` is only ever widened, so an equal type arriving
/// later cannot reach the shortcut and find the set out of date.
fn add_member(
    members: &mut Vec<LuaType>,
    present: &mut u64,
    hashed: &mut FxHashSet<LuaType>,
    ty: LuaType,
) {
    if dedups_by_equality(&ty)
        && collapse_partner_ordinals(&ty)
            .iter()
            .all(|ordinal| *present & 1 << ordinal == 0)
    {
        if hashed.insert(ty.clone()) {
            *present |= 1 << lua_type_sort_ordinal(&ty);
            members.push(ty);
        }
        return;
    }
    absorb(members, present, ty);
}

/// Whether two members of this type's discriminant can only collapse by being
/// equal, and that equality agrees with `Hash`.
///
/// `BooleanConst` is the one discriminant with a rule of its own (two different
/// literals give `boolean`). `FloatConst` is left out because `0.0 == -0.0`
/// while their bit patterns — what `Hash` uses — differ. Every other type is
/// decided by [`try_collapse`]'s trailing equality rule, including `Ref`, whose
/// own arm compares the same ids that `PartialEq` does.
fn dedups_by_equality(ty: &LuaType) -> bool {
    matches!(
        ty,
        LuaType::Nil
            | LuaType::Boolean
            | LuaType::Integer
            | LuaType::Number
            | LuaType::String
            | LuaType::Table
            | LuaType::Userdata
            | LuaType::Function
            | LuaType::Thread
            | LuaType::SelfInfer
            | LuaType::Global
            | LuaType::Unknown
            | LuaType::Io
            | LuaType::IntegerConst(_)
            | LuaType::DocIntegerConst(_)
            | LuaType::StringConst(_)
            | LuaType::DocStringConst(_)
            | LuaType::TableConst(_)
            | LuaType::Ref(_)
            | LuaType::Def(_)
            | LuaType::Signature(_)
    )
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
            let mut members = left.into_vec();
            let mut present = ordinal_mask(&members);
            absorb(&mut members, &mut present, right.clone());
            LuaType::from_vec_structural(members)
        }
        // The *source* joins the union, not the dereferenced view of it:
        // the alias is what the caller passed and what has to survive into
        // the rendered type, exactly as the sibling arms keep their
        // operands. The dereference is only ever a matching aid (see
        // `try_collapse`), and `absorb` matches an alias by identity, which
        // is the same answer the other two union arms give.
        (left, LuaType::Union(right)) if !left.is_union() => {
            let mut members = right.into_vec();
            let mut present = ordinal_mask(&members);
            absorb(&mut members, &mut present, source.clone());
            LuaType::from_vec_structural(members)
        }
        // two union
        (LuaType::Union(left), LuaType::Union(right)) => {
            let mut members = left.into_vec();
            let mut present = ordinal_mask(&members);
            for member in right.into_vec() {
                absorb(&mut members, &mut present, member);
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

/// This type's bit in a discriminant mask.
///
/// The masks are `u64`, so every ordinal has to stay under 64; the largest
/// [`lua_type_sort_ordinal`] hands out today is 50. Going over would silently
/// produce a wrong mask in release, so it is checked here, the one place a bit
/// is taken.
fn ordinal_bit(ty: &LuaType) -> u64 {
    let ordinal = lua_type_sort_ordinal(ty);
    debug_assert!(
        ordinal < 64,
        "sort ordinal {ordinal} does not fit in a u64 discriminant mask"
    );
    1 << ordinal
}

/// Bitset of the sort discriminants present in a member list.
fn ordinal_mask(members: &[LuaType]) -> u64 {
    members
        .iter()
        .fold(0, |mask, member| mask | ordinal_bit(member))
}

/// The discriminants of every member `ty` could collapse with: its collapse
/// partners, plus its own for the equality rules, plus `never`.
fn collapse_candidate_mask(ty: &LuaType) -> u64 {
    collapse_partner_ordinals(ty).iter().fold(
        ordinal_bit(ty) | ordinal_bit(&LuaType::Never),
        |mask, ordinal| mask | 1 << ordinal,
    )
}

/// Add `ty` to an existing union's member list, applying absorption rules.
///
/// `present` is [`ordinal_mask`] over `members`, carried across calls so that a
/// member with no possible partner costs a mask test rather than a scan. It is
/// only ever widened, so a member removed by a merge leaves a stale bit behind:
/// that costs a scan that finds nothing, never a missed collapse.
fn absorb(members: &mut Vec<LuaType>, present: &mut u64, ty: LuaType) {
    let mut ty = ty;
    // A merged member keeps the slot of the member it merged with, so absorbing
    // never reorders the survivors. Member order is semantic for overloads and
    // template dispatch, which the structural constructor deliberately leaves
    // alone.
    let mut slot = members.len();

    'restart: loop {
        let candidates = collapse_candidate_mask(&ty);
        if candidates & *present != 0 {
            for i in 0..members.len() {
                // Anything that collapses sorts under a candidate discriminant,
                // so the rest are skipped without reaching `try_collapse`, whose
                // last rule is a full structural equality.
                if candidates & 1 << lua_type_sort_ordinal(&members[i]) == 0 {
                    continue;
                }
                if let Some(merged) = try_collapse(&members[i], &members[i], &ty) {
                    members.remove(i);
                    ty = merged;
                    slot = slot.min(i);
                    continue 'restart;
                }
            }
        }
        break;
    }

    *present |= 1 << lua_type_sort_ordinal(&ty);
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
    let at = members.partition_point(|member| lua_type_sort_ordinal(member) < ordinal);
    members
        .get(at)
        .is_some_and(|member| lua_type_sort_ordinal(member) == ordinal)
}

#[cfg(test)]
mod union_shortcut_tests {
    use super::*;
    use crate::{FileId, LuaIndexAccessKey, LuaObjectType, LuaSignatureId, LuaTypeDeclId};
    use internment::ArcIntern;
    use rowan::TextSize;
    use smol_str::SmolStr;
    use std::sync::Arc;

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
                let mut rebuilt = union.into_vec();
                let mut present = ordinal_mask(&rebuilt);
                absorb(&mut rebuilt, &mut present, incoming.clone());
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

    /// The shape the hash-set shortcut exists for: a wide union of members
    /// sharing one discriminant, mixed with the one same-discriminant rule that
    /// is not plain equality.
    #[test]
    fn many_references_beside_boolean_literals_match_the_fold() {
        let mut next = rng(0x1234_5678_9abc_def1);
        for _ in 0..200 {
            let mut types = (0..40)
                .map(|i| LuaType::Ref(LuaTypeDeclId::global(&format!("Class{}", i % 25))))
                .collect::<Vec<_>>();
            types.insert(
                (next() % 40) as usize,
                LuaType::BooleanConst(next() & 1 == 0),
            );
            types.insert(
                (next() % 40) as usize,
                LuaType::BooleanConst(next() & 1 == 0),
            );
            types.insert((next() % 40) as usize, LuaType::Boolean);
            if visiting_order_is_observable(&types) {
                continue;
            }
            assert_eq!(union_all_absorbed(types.clone()), fold(types.clone()));
        }
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

    fn inferred_before_optimisation(types: Vec<LuaType>) -> LuaType {
        match LuaType::from_vec(types) {
            LuaType::Union(union) => match Arc::try_unwrap(union) {
                Ok(LuaUnionType::Multi(members)) => {
                    LuaType::Union(LuaUnionType::from_inferred_vec(members).into())
                }
                Ok(union) => LuaType::Union(union.into()),
                Err(union) => match union.as_ref() {
                    LuaUnionType::Multi(members) => {
                        LuaType::Union(LuaUnionType::from_inferred_vec(members.clone()).into())
                    }
                    LuaUnionType::Nullable(_) => LuaType::Union(union),
                },
            },
            other => other,
        }
    }

    fn signature(file_id: u32, position: u32) -> LuaType {
        LuaType::Signature(LuaSignatureId::new(
            FileId::new(file_id),
            TextSize::new(position),
        ))
    }

    fn nested_callable_union() -> LuaType {
        LuaType::Union(
            LuaUnionType::from_multi_unchecked(vec![signature(1, 0), LuaType::String]).into(),
        )
    }

    fn nested_preserved_literal_union() -> LuaType {
        LuaType::Union(
            LuaUnionType::from_multi_unchecked(vec![
                LuaType::Any,
                LuaType::String,
                signature(1, 1),
            ])
            .into(),
        )
    }

    fn nested_never_union() -> LuaType {
        LuaType::Union(
            LuaUnionType::from_multi_unchecked(vec![signature(1, 3), LuaType::Never]).into(),
        )
    }

    fn nested_multiline_union() -> LuaType {
        LuaType::Union(
            LuaUnionType::from_multi_unchecked(vec![
                LuaType::MultiLineUnion(Arc::new(LuaMultiLineUnion::new(vec![(
                    LuaType::StringConst(ArcIntern::new(SmolStr::new("literal"))),
                    None,
                )]))),
                signature(1, 2),
            ])
            .into(),
        )
    }

    #[test]
    fn inferred_normalisation_matches_the_previous_fold_across_permutations() {
        let object = LuaType::Object(Arc::new(LuaObjectType::new(vec![(
            LuaIndexAccessKey::String(SmolStr::new("field")),
            LuaType::String,
        )])));
        let cases = vec![
            vec![
                nested_callable_union(),
                signature(2, 0),
                LuaType::Integer,
                object.clone(),
            ],
            vec![
                nested_preserved_literal_union(),
                object.clone(),
                LuaType::String,
            ],
            vec![nested_multiline_union(), signature(3, 0), LuaType::Integer],
            vec![nested_never_union(), signature(3, 1), LuaType::Integer],
            vec![LuaType::Any, signature(4, 0), LuaType::String],
            vec![
                LuaType::FloatConst(0.0),
                LuaType::Union(
                    LuaUnionType::from_multi_unchecked(vec![
                        LuaType::FloatConst(-0.0),
                        signature(5, 0),
                    ])
                    .into(),
                ),
                LuaType::Integer,
            ],
        ];
        let orders = [
            vec![0, 1, 2, 3],
            vec![3, 2, 1, 0],
            vec![1, 3, 0, 2],
            vec![2, 0, 3, 1],
        ];

        for case in cases {
            for order in &orders {
                let permutation = order
                    .iter()
                    .filter_map(|&idx| case.get(idx).cloned())
                    .collect::<Vec<_>>();
                let old = inferred_before_optimisation(permutation.clone());
                let actual = LuaType::from_inferred_vec(permutation);
                assert_eq!(actual, old, "inferred normalisation changed its member set");
                assert_eq!(format!("{actual:?}"), format!("{old:?}"));
            }
        }
    }
    #[test]
    fn inferred_sort_key_collision_checks_all_input_members() {
        let types = vec![
            LuaType::FloatConst(f64::NAN),
            LuaType::Union(
                LuaUnionType::from_multi_unchecked(vec![
                    LuaType::FloatConst(f64::NAN),
                    signature(1, 0),
                ])
                .into(),
            ),
        ];
        assert!(contains_distinct_sort_key_collision(&types));
        let old = inferred_before_optimisation(types.clone());
        let actual = LuaType::from_inferred_vec(types);
        assert_eq!(format!("{actual:?}"), format!("{old:?}"));
    }
}
