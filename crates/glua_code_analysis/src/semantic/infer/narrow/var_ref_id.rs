use std::{
    hash::{Hash, Hasher},
    ops::Deref,
};

use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr, LuaLiteralToken, PathTrait};
use internment::ArcIntern;
use rowan::TextSize;
use smol_str::SmolStr;

use crate::{
    DbIndex, LuaAliasCallKind, LuaDeclId, LuaDeclOrMemberId, LuaInferCache, LuaMemberId, LuaType,
    infer_expr,
    semantic::infer::{
        infer_index::get_index_expr_var_ref_id, infer_name::get_name_expr_var_ref_id,
    },
};

/// Returns true when a successfully-indexed `Unknown` prefix is authoritative
/// and may widen to `Any`.
pub fn unknown_prefix_should_widen_to_any(db: &DbIndex, var_ref_id: &VarRefId) -> bool {
    // Only authoritative unknowns should be promoted after successful indexing:
    // unresolved globals and locals explicitly documented as `unknown` are truly
    // opaque to the analyzer. Inferred unknown aliases can still have concrete
    // dynamic member origins (for example `local sounds = self.sounds`), so
    // widening those aliases to `any` would erase the guarded member type.
    if matches!(var_ref_id, VarRefId::GlobalName(_, _)) {
        return true;
    }

    var_ref_id
        .get_decl_id_ref()
        .and_then(|decl_id| db.get_type_index().get_type_cache(&decl_id.into()))
        .is_some_and(|type_cache| type_cache.is_doc())
}

/// Identifies member refs rooted in unannotated parameters, where nil cleanup
/// writes are not authoritative typed member facts.
pub fn is_untyped_param_rooted_index(db: &DbIndex, var_ref_id: &VarRefId) -> bool {
    let VarRefId::IndexRef(root, _) = var_ref_id else {
        return false;
    };
    let Some(decl_id) = root.as_decl_id() else {
        return false;
    };

    db.get_decl_index()
        .get_decl(&decl_id)
        .is_some_and(|decl| decl.is_param())
        && db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .is_none_or(|type_cache| !type_cache.is_doc())
}

/// Identity for an implicit `self` reference inside a colon method.
///
/// `self_decl_id` is the method's implicit `self` declaration, which is unique
/// per method body. It is used as the flow-cache / `VarRefId` identity so that
/// two methods of the *same* reused local (e.g. `local PANEL` reassigned and
/// redefined per region) do NOT share a `SelfRef` key and poison each other's
/// flow narrowing.
///
/// `receiver` is the colon-method prefix owner (decl or member) and is used
/// only for base/member type lookup, never for identity.
#[derive(Debug, Clone)]
pub struct SelfRefId {
    pub self_decl_id: LuaDeclId,
    pub receiver: LuaDeclOrMemberId,
}

impl PartialEq for SelfRefId {
    fn eq(&self, other: &Self) -> bool {
        // Identity is keyed on the unique implicit-self decl, NOT the receiver.
        self.self_decl_id == other.self_decl_id
    }
}

impl Eq for SelfRefId {}

impl Hash for SelfRefId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.self_decl_id.hash(state);
    }
}

/// Root identity for [`VarRefId::IndexRef`].
///
/// An index expression like `self.value` or `tbl.field` has two parts: the root
/// (what is being indexed) and the access path (`"value"`). This enum captures
/// the root identity with enough precision so that two index expressions from
/// *different* regions of a reused local (e.g. `local PANEL` reassigned and
/// redefined per `vgui.Register` region) do NOT share a flow-cache key.
///
/// - `Decl` / `Member` preserve the old behaviour for ordinary table/variable
///   index refs.
/// - `SelfRef` carries the full [`SelfRefId`] (method-aware identity) so that
///   `self.field` inside different colon-methods of the *same* reused local
///   keeps distinct var-ref identity.  This prevents flow narrowing from one
///   region poisoning another.
#[derive(Debug, Clone)]
pub enum VarRefRootId {
    Decl(LuaDeclId),
    Member(LuaMemberId),
    SelfRef(SelfRefId),
}

impl PartialEq for VarRefRootId {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Decl(l), Self::Decl(r)) => l == r,
            (Self::Member(l), Self::Member(r)) => l == r,
            (Self::SelfRef(l), Self::SelfRef(r)) => l == r,
            _ => false,
        }
    }
}

impl Eq for VarRefRootId {}

impl Hash for VarRefRootId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Decl(d) => d.hash(state),
            Self::Member(m) => m.hash(state),
            Self::SelfRef(s) => s.hash(state),
        }
    }
}

impl VarRefRootId {
    /// Returns the underlying decl id, if any.
    ///
    /// For `SelfRef` roots this resolves through the receiver.
    pub fn as_decl_id(&self) -> Option<LuaDeclId> {
        match self {
            Self::Decl(d) => Some(*d),
            Self::SelfRef(s) => s.receiver.as_decl_id(),
            Self::Member(_) => None,
        }
    }

    /// Returns the underlying member id, if any.
    pub fn as_member_id(&self) -> Option<LuaMemberId> {
        match self {
            Self::Member(m) => Some(*m),
            Self::SelfRef(s) => s.receiver.as_member_id(),
            Self::Decl(_) => None,
        }
    }

    /// Source position used for realm resolution.
    pub fn get_position(&self) -> TextSize {
        match self {
            Self::Decl(d) => d.position,
            Self::Member(m) => m.get_position(),
            // Use the implicit-self decl position so the flow query resolves the
            // realm at the method body, consistent with the self identity.
            Self::SelfRef(s) => s.self_decl_id.position,
        }
    }

    /// Returns true when this root represents the same *receiver object* as the
    /// given [`LuaDeclOrMemberId`].
    ///
    /// For `Decl` / `Member` roots the comparison is direct.  For `SelfRef`
    /// roots the comparison goes through `SelfRefId::receiver`, so that effects
    /// targeting `self` (as a `SelfRef`) correctly match index refs rooted in
    /// the same receiver even across different method bodies.
    pub fn receiver_eq(&self, other: &LuaDeclOrMemberId) -> bool {
        match self {
            Self::Decl(d) => LuaDeclOrMemberId::Decl(*d) == *other,
            Self::Member(m) => LuaDeclOrMemberId::Member(*m) == *other,
            Self::SelfRef(s) => s.receiver == *other,
        }
    }
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum VarRefId {
    VarRef(LuaDeclId),
    SelfRef(SelfRefId),
    IndexRef(VarRefRootId, ArcIntern<SmolStr>),
    GlobalName(ArcIntern<SmolStr>, TextSize),
}

impl PartialEq for VarRefId {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (VarRefId::VarRef(left), VarRefId::VarRef(right)) => left == right,
            (VarRefId::SelfRef(left), VarRefId::SelfRef(right)) => left == right,
            (
                VarRefId::IndexRef(left_root, left_path),
                VarRefId::IndexRef(right_root, right_path),
            ) => left_root == right_root && left_path == right_path,
            (VarRefId::GlobalName(left_name, _), VarRefId::GlobalName(right_name, _)) => {
                left_name == right_name
            }
            _ => false,
        }
    }
}

impl Eq for VarRefId {}

impl Hash for VarRefId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            VarRefId::VarRef(decl_id) => decl_id.hash(state),
            VarRefId::SelfRef(self_ref_id) => self_ref_id.hash(state),
            VarRefId::IndexRef(root, path) => {
                root.hash(state);
                path.hash(state);
            }
            VarRefId::GlobalName(name, _) => name.hash(state),
        }
    }
}

impl VarRefId {
    pub fn get_decl_id_ref(&self) -> Option<LuaDeclId> {
        match self {
            VarRefId::VarRef(decl_id) => Some(*decl_id),
            VarRefId::SelfRef(self_ref_id) => self_ref_id.receiver.as_decl_id(),
            _ => None,
        }
    }

    pub fn get_member_id_ref(&self) -> Option<LuaMemberId> {
        match self {
            VarRefId::SelfRef(self_ref_id) => self_ref_id.receiver.as_member_id(),
            _ => None,
        }
    }

    pub fn get_position(&self) -> TextSize {
        match self {
            VarRefId::VarRef(decl_id) => decl_id.position,
            // Use the implicit-self decl position so the flow query resolves the
            // realm at the method body, consistent with the self identity.
            VarRefId::SelfRef(self_ref_id) => self_ref_id.self_decl_id.position,
            VarRefId::IndexRef(root, _) => root.get_position(),
            VarRefId::GlobalName(_, position) => *position,
        }
    }

    pub fn start_with(&self, prefix: &VarRefId) -> bool {
        let (root, path) = match self {
            VarRefId::IndexRef(root, path) => (root, path.clone()),
            _ => return false,
        };

        match prefix {
            VarRefId::VarRef(decl_id) => root.as_decl_id() == Some(*decl_id),
            VarRefId::SelfRef(self_ref_id) => root.receiver_eq(&self_ref_id.receiver),
            VarRefId::IndexRef(prefix_root, prefix_path) => {
                *prefix_root == *root
                    && (path == *prefix_path
                        || path
                            .strip_prefix(prefix_path.deref().as_str())
                            .is_some_and(|rest| rest.starts_with('.')))
            }
            VarRefId::GlobalName(_, _) => false,
        }
    }

    pub fn is_self_ref(&self) -> bool {
        matches!(self, VarRefId::SelfRef(_))
    }
}

fn get_call_expr_var_ref_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<VarRefId> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    let maybe_func = infer_expr(db, cache, prefix_expr.clone()).ok()?;

    let ret = match maybe_func {
        LuaType::DocFunction(f) => f.get_ret().clone(),
        LuaType::Signature(signature_id) => db
            .get_signature_index()
            .get(&signature_id)?
            .get_return_type(),
        _ => return None,
    };
    let LuaType::Call(alias_call_type) = ret else {
        return None;
    };

    match alias_call_type.get_call_kind() {
        LuaAliasCallKind::RawGet => {
            let args_list = call_expr.get_args_list()?;
            let mut args_iter = args_list.get_args();

            let obj_expr = args_iter.next()?;
            let root = match get_var_expr_var_ref_id(db, cache, obj_expr.clone()) {
                Some(VarRefId::SelfRef(self_ref_id)) => VarRefRootId::SelfRef(self_ref_id),
                Some(VarRefId::VarRef(decl_id)) => VarRefRootId::Decl(decl_id),
                _ => return None,
            };
            // 开始构建 access_path
            let mut access_path = String::new();
            access_path.push_str(obj_expr.syntax().text().to_string().as_str()); // 这里不需要精确的文本
            access_path.push('.');
            let key_expr = args_iter.next()?;
            match key_expr {
                LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal()? {
                    LuaLiteralToken::String(string_token) => {
                        access_path.push_str(string_token.get_value().as_str());
                    }
                    LuaLiteralToken::Number(number_token) => {
                        access_path.push_str(number_token.get_number_value().to_string().as_str());
                    }
                    _ => return None,
                },
                LuaExpr::NameExpr(name_expr) => {
                    access_path.push_str(name_expr.get_access_path()?.as_str());
                }
                LuaExpr::IndexExpr(index_expr) => {
                    access_path.push_str(index_expr.get_access_path()?.as_str());
                }
                _ => return None,
            }

            Some(VarRefId::IndexRef(
                root,
                ArcIntern::new(SmolStr::new(access_path)),
            ))
        }
        _ => None,
    }
}

pub fn get_var_expr_var_ref_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    var_expr: LuaExpr,
) -> Option<VarRefId> {
    if let Some(var_ref_id) = cache.expr_var_ref_id_cache.get(&var_expr.get_syntax_id()) {
        return Some(var_ref_id.clone());
    }

    let ref_id = match &var_expr {
        LuaExpr::NameExpr(name_expr) => get_name_expr_var_ref_id(db, cache, name_expr),
        LuaExpr::IndexExpr(index_expr) => get_index_expr_var_ref_id(db, cache, index_expr),
        LuaExpr::CallExpr(call_expr) => get_call_expr_var_ref_id(db, cache, call_expr),
        _ => None,
    }?;

    cache
        .expr_var_ref_id_cache
        .insert(var_expr.get_syntax_id(), ref_id.clone());
    Some(ref_id)
}
