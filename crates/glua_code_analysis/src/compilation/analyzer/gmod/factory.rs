//! A factory registers a table it returns, so each call of it makes a class.
//! Its body runs once per call: what it writes under keys built from its
//! parameters, and the parent its `__index` walks to, belong to the class a
//! given call makes.

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaFuncStat,
    LuaIndexExpr, LuaIndexKey, LuaLiteralToken, LuaReturnStat, LuaSyntaxId, LuaVarExpr,
};
use rowan::TextRange;
use smol_str::SmolStr;

use super::super::lua::resolve_index_expr_member_owner_for_file;
use super::{
    find_registered_table_expr_at_write_position, find_table_expr_for_arg_source,
    registered_metatable, resolve_local_registration_region,
};
use crate::semantic::{
    decl_value_expr, find_members_with_key, get_prefix_expr_signature_id, proxy_metatable,
};
use crate::{
    AsyncState, DbIndex, FileId, GlobalId, GmodClassCallLiteral, GmodScriptedClassCallMetadata,
    InFiled, LuaDeclExtra, LuaDeclId, LuaFunctionType, LuaInferCache, LuaMember, LuaMemberFeature,
    LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaSignatureId, LuaType, LuaTypeDeclId, TypeOps,
    infer_expr,
};

/// The factory a registering call calls, as that call runs it.
pub(super) struct FactoryCall {
    file_id: FileId,
    body: LuaClosureExpr,
    signature_id: LuaSignatureId,
    /// The table the body registers.
    pub(super) table: InFiled<TextRange>,
    /// The local holding the registered table, when the body registers one
    /// itself rather than through another factory.
    table_decl: Option<LuaDeclId>,
    /// The constant value of each argument of the call.
    args: Vec<Value>,
}

/// A value the factory body computes from the call's arguments.
#[derive(Clone, PartialEq)]
enum Value {
    String(String),
    /// `nil` or `false`.
    Falsy,
    Type(LuaType),
    /// Not known from the call alone.
    Unknown,
}

impl FactoryCall {
    /// The factory `call` calls, if it registers a table in its own body or
    /// through another factory call.
    pub(super) fn resolve(
        db: &DbIndex,
        file_id: FileId,
        call: &GmodScriptedClassCallMetadata,
    ) -> Option<Self> {
        Self::resolve_visiting(db, file_id, call, &mut Vec::new())
    }

    fn resolve_visiting(
        db: &DbIndex,
        file_id: FileId,
        call: &GmodScriptedClassCallMetadata,
        visited: &mut Vec<InFiled<TextRange>>,
    ) -> Option<Self> {
        let call_range = InFiled::new(file_id, call.syntax_id.get_range());
        if visited.contains(&call_range) {
            return None;
        }
        visited.push(call_range);
        let root = db.get_vfs().get_syntax_tree(&file_id)?.get_red_root();
        let call_expr = call
            .syntax_id
            .to_node_from_root(&root)
            .and_then(LuaCallExpr::cast)?;
        let mut infer_cache = LuaInferCache::new(file_id, super::super::call_target::pass_view());
        let signature_id = get_prefix_expr_signature_id(db, &mut infer_cache, &call_expr)?;
        let factory_file_id = signature_id.get_file_id();
        let factory_root = db
            .get_vfs()
            .get_syntax_tree(&factory_file_id)?
            .get_red_root();
        let body = factory_root
            .token_at_offset(signature_id.get_position())
            .right_biased()?
            .parent_ancestors()
            .find_map(LuaClosureExpr::cast)?;
        let metadata = db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&factory_file_id)?;
        let mut registrations = [
            (&metadata.vgui_register_calls, 1),
            (&metadata.metatable_register_calls, 1),
            (&metadata.derma_define_control_calls, 2),
        ]
        .into_iter()
        .flat_map(|(calls, table_arg_idx)| calls.iter().map(move |call| (call, table_arg_idx)))
        .filter(|(inner, _)| {
            LuaSyntaxId::to_node_from_root(&inner.syntax_id, &factory_root)
                .and_then(|node| node.ancestors().find_map(LuaClosureExpr::cast))
                .as_ref()
                == Some(&body)
        })
        .collect::<Vec<_>>();
        registrations.sort_by_key(|(inner, _)| inner.syntax_id.get_range().start());

        let args: Vec<Value> = call
            .literal_args
            .iter()
            .map(|literal| match literal {
                Some(GmodClassCallLiteral::String(value)) => Value::String(value.clone()),
                Some(GmodClassCallLiteral::Nil | GmodClassCallLiteral::Boolean(false)) => {
                    Value::Falsy
                }
                _ => Value::Unknown,
            })
            .collect();
        registrations
            .into_iter()
            .find_map(|(inner, table_arg_idx)| {
                if inner.class_registration_table_is_result() {
                    let nested = Self::resolve_visiting(db, factory_file_id, inner, visited)?;
                    return Some(Self {
                        table_decl: None,
                        ..nested
                    });
                }
                let table_source = inner.class_registration_table_arg_source(table_arg_idx)?;
                let (table, table_decl) = match inner.value_for_arg_source(&table_source) {
                    Some(GmodClassCallLiteral::NameRef(table_var)) => {
                        let (decl_id, region_start) = resolve_local_registration_region(
                            db,
                            factory_file_id,
                            table_var,
                            inner.syntax_id.get_range().start(),
                        )?;
                        let table = find_registered_table_expr_at_write_position(
                            db,
                            factory_file_id,
                            region_start,
                        )
                        .map(|table| InFiled::new(factory_file_id, table.get_range()))
                        .or_else(|| registered_metatable(db, factory_file_id, region_start))?;
                        (table, Some(decl_id))
                    }
                    _ => {
                        let table = find_table_expr_for_arg_source(
                            db,
                            factory_file_id,
                            inner,
                            &table_source,
                        )?;
                        (InFiled::new(factory_file_id, table.get_range()), None)
                    }
                };
                Some(Self {
                    file_id: factory_file_id,
                    body: body.clone(),
                    signature_id,
                    table,
                    table_decl,
                    args: args.clone(),
                })
            })
    }

    /// What the body, as this call runs it, writes under keys built from its
    /// parameters, as members of the tables it writes them to. A function that
    /// returns the registered table, or values made with it as their
    /// metatable, returns `class`.
    pub(super) fn keyed_members(
        &self,
        db: &DbIndex,
        call_file_id: FileId,
        call_syntax_id: LuaSyntaxId,
        class: &LuaTypeDeclId,
    ) -> Vec<(LuaMemberOwner, LuaMember, LuaType)> {
        if self.table_decl.is_none() {
            return Vec::new();
        }
        let class_type = LuaType::Ref(class.clone());
        let mut cache = self.cache();
        let mut members = Vec::new();
        for assign_stat in self.own_descendants::<LuaAssignStat>() {
            let (vars, values) = assign_stat.get_var_and_expr_list();
            for (var, value) in vars.iter().zip(values) {
                let LuaVarExpr::IndexExpr(target) = var else {
                    continue;
                };
                let Some(LuaIndexKey::Expr(key)) = target.get_index_key() else {
                    continue;
                };
                let Value::String(name) = self.eval(db, &mut cache, &key, &class_type) else {
                    continue;
                };
                let Some(owner) = self.write_owner(db, &mut cache, target) else {
                    continue;
                };
                let typ = match &value {
                    LuaExpr::ClosureExpr(constructor) => {
                        match self.constructor_type(db, &mut cache, constructor, &class_type) {
                            Some(typ) => typ,
                            None => continue,
                        }
                    }
                    _ => match self.eval(db, &mut cache, &value, &class_type) {
                        Value::Type(typ) => typ,
                        Value::String(value) => LuaType::StringConst(SmolStr::new(value).into()),
                        Value::Falsy | Value::Unknown => continue,
                    },
                };
                let member_id = LuaMemberId::new_in_slot(
                    call_syntax_id,
                    call_file_id,
                    members.len() as u32 + 1,
                );
                let member = LuaMember::new(
                    member_id,
                    LuaMemberKey::Name(name.as_str().into()),
                    LuaMemberFeature::FileFieldDecl,
                    None,
                );
                members.push((owner, member, typ));
            }
        }
        members
    }

    /// The table a body write `target` writes into: the owner the write's own
    /// member has, else the one its prefix names, which a write under a
    /// computed key may not have been given.
    fn write_owner(
        &self,
        db: &DbIndex,
        cache: &mut LuaInferCache,
        target: &LuaIndexExpr,
    ) -> Option<LuaMemberOwner> {
        if let Some(owner) = db
            .get_member_index()
            .get_member_owner(&LuaMemberId::new(target.get_syntax_id(), self.file_id))
        {
            return Some(owner.clone());
        }
        match infer_expr(db, cache, target.get_prefix_expr()?).ok()? {
            // A module's table, whose fields are looked up by global path.
            LuaType::Namespace(path) => Some(LuaMemberOwner::GlobalPath(GlobalId::new(&path))),
            prefix_type => {
                resolve_index_expr_member_owner_for_file(db, &prefix_type, Some(self.file_id))
                    .map(|(owner, _)| owner)
            }
        }
    }

    /// The parents the registered table's `__index` walks to, as this call
    /// stores them: a walk `m = rawget(m, "F")` or `m = m.F` inherits from what
    /// the body writes to the table's `F`.
    pub(super) fn walked_parents(&self, db: &DbIndex, class: &LuaTypeDeclId) -> Vec<LuaType> {
        let Some(table_decl) = self.table_decl else {
            return Vec::new();
        };
        let link_fields = self.index_walk_fields(db, table_decl);
        if link_fields.is_empty() {
            return Vec::new();
        }
        let class_type = LuaType::Ref(class.clone());
        let mut cache = self.cache();
        let mut parents = Vec::new();
        for assign_stat in self.own_descendants::<LuaAssignStat>() {
            let (vars, values) = assign_stat.get_var_and_expr_list();
            for (var, value) in vars.iter().zip(values) {
                let LuaVarExpr::IndexExpr(target) = var else {
                    continue;
                };
                if !self.is_table(db, &target.get_prefix_expr(), table_decl)
                    || !static_key(target).is_some_and(|key| link_fields.contains(&key))
                {
                    continue;
                }
                if let Value::Type(parent) = self.eval(db, &mut cache, &value, &class_type)
                    && parent != class_type
                    && !parents.contains(&parent)
                {
                    parents.push(parent);
                }
            }
        }
        parents
    }

    /// The fields the `__index` function of the registered table walks
    /// through, itself or in the local functions whose result it returns.
    fn index_walk_fields(&self, db: &DbIndex, table_decl: LuaDeclId) -> Vec<String> {
        let Some(index_function) = self.index_function(db, table_decl) else {
            return Vec::new();
        };
        let mut walkers = vec![index_function.clone()];
        for return_stat in index_function.descendants::<LuaReturnStat>() {
            let Some(LuaExpr::CallExpr(call)) = return_stat.get_expr_list().next() else {
                continue;
            };
            let Some(LuaExpr::NameExpr(callee)) = call.get_prefix_expr() else {
                continue;
            };
            if let Ok(LuaType::Signature(signature_id)) =
                infer_expr(db, &mut self.cache(), LuaExpr::NameExpr(callee))
                && signature_id.get_file_id() == self.file_id
                && let Some(walker) = self.body.syntax().ancestors().last().and_then(|root| {
                    root.token_at_offset(signature_id.get_position())
                        .right_biased()?
                        .parent_ancestors()
                        .find_map(LuaClosureExpr::cast)
                })
            {
                walkers.push(walker);
            }
        }

        let mut fields = Vec::new();
        for walker in walkers {
            for assign_stat in walker.descendants::<LuaAssignStat>() {
                let (vars, values) = assign_stat.get_var_and_expr_list();
                for (var, value) in vars.iter().zip(values) {
                    let LuaVarExpr::NameExpr(cursor) = var else {
                        continue;
                    };
                    let Some(cursor_name) = cursor.get_name_text() else {
                        continue;
                    };
                    let field = match &value {
                        LuaExpr::CallExpr(call) if is_named_call(call, "rawget") => {
                            let args = call
                                .get_args_list()
                                .map(|args| args.get_args().collect::<Vec<_>>())
                                .unwrap_or_default();
                            match args.as_slice() {
                                [LuaExpr::NameExpr(walked), field]
                                    if walked.get_name_text().as_deref()
                                        == Some(cursor_name.as_str()) =>
                                {
                                    string_literal(field)
                                }
                                _ => None,
                            }
                        }
                        LuaExpr::IndexExpr(step) => match step.get_prefix_expr() {
                            Some(LuaExpr::NameExpr(walked))
                                if walked.get_name_text().as_deref()
                                    == Some(cursor_name.as_str()) =>
                            {
                                static_key(step)
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(field) = field
                        && !fields.contains(&field)
                    {
                        fields.push(field);
                    }
                }
            }
        }
        fields
    }

    /// The function the body makes the registered table's `__index`.
    fn index_function(&self, db: &DbIndex, table_decl: LuaDeclId) -> Option<LuaClosureExpr> {
        let is_index = |target: &LuaIndexExpr| {
            self.is_table(db, &target.get_prefix_expr(), table_decl)
                && static_key(target).as_deref() == Some("__index")
        };
        self.own_descendants::<LuaFuncStat>()
            .find_map(|func_stat| match func_stat.get_func_name()? {
                LuaVarExpr::IndexExpr(target) if is_index(&target) => func_stat.get_closure(),
                _ => None,
            })
            .or_else(|| {
                self.own_descendants::<LuaAssignStat>()
                    .find_map(|assign_stat| {
                        let (vars, values) = assign_stat.get_var_and_expr_list();
                        vars.iter()
                            .zip(values)
                            .find_map(|(var, value)| match (var, value) {
                                (LuaVarExpr::IndexExpr(target), LuaExpr::ClosureExpr(function))
                                    if is_index(target) =>
                                {
                                    Some(function)
                                }
                                _ => None,
                            })
                    })
            })
    }

    /// The function type of a function the body writes, when every value it
    /// returns is `class`: the registered table or a value made with it as its
    /// metatable.
    fn constructor_type(
        &self,
        db: &DbIndex,
        cache: &mut LuaInferCache,
        constructor: &LuaClosureExpr,
        class_type: &LuaType,
    ) -> Option<LuaType> {
        let returned = constructor
            .descendants::<LuaReturnStat>()
            .filter(|return_stat| {
                return_stat.ancestors::<LuaClosureExpr>().next().as_ref() == Some(constructor)
            })
            .map(|return_stat| return_stat.get_expr_list().next())
            .collect::<Option<Vec<_>>>()?;
        if returned.is_empty()
            || !returned
                .into_iter()
                .all(|expr| self.instance_of_table(db, cache, expr, class_type))
        {
            return None;
        }
        let signature_id = LuaSignatureId::from_closure(self.file_id, constructor);
        let signature = db.get_signature_index().get(&signature_id)?;
        let params = signature
            .params
            .iter()
            .map(|param| (param.to_string(), None))
            .collect();
        Some(LuaType::DocFunction(
            LuaFunctionType::new(AsyncState::None, false, false, params, class_type.clone()).into(),
        ))
    }

    /// Whether `expr`, or the never reassigned local it names, is the
    /// registered table or a value made with it as its metatable.
    fn instance_of_table(
        &self,
        db: &DbIndex,
        cache: &mut LuaInferCache,
        expr: LuaExpr,
        class_type: &LuaType,
    ) -> bool {
        let expr = self.unaliased(db, expr);
        match &expr {
            LuaExpr::CallExpr(call) if call.is_setmetatable() => call
                .get_args_list()
                .and_then(|args| args.get_args().nth(1))
                .is_some_and(|metatable| {
                    self.eval(db, cache, &metatable, class_type) == Value::Type(class_type.clone())
                }),
            LuaExpr::CallExpr(call) if is_named_call(call, "newproxy") => {
                proxy_metatable(db, cache, &expr).as_ref() == Some(&self.table)
            }
            _ => self.eval(db, cache, &expr, class_type) == Value::Type(class_type.clone()),
        }
    }

    /// `expr` as this call computes it.
    fn eval(
        &self,
        db: &DbIndex,
        cache: &mut LuaInferCache,
        expr: &LuaExpr,
        class_type: &LuaType,
    ) -> Value {
        match expr {
            LuaExpr::LiteralExpr(literal) => match literal.get_literal() {
                Some(LuaLiteralToken::String(value)) => Value::String(value.get_value()),
                Some(LuaLiteralToken::Nil(_)) => Value::Falsy,
                Some(LuaLiteralToken::Bool(value)) if !value.is_true() => Value::Falsy,
                _ => infer_value(db, cache, expr),
            },
            LuaExpr::ParenExpr(paren_expr) => match paren_expr.get_expr() {
                Some(inner) => self.eval(db, cache, &inner, class_type),
                None => Value::Unknown,
            },
            LuaExpr::NameExpr(name) => {
                let Some(decl_id) = db
                    .get_reference_index()
                    .get_var_reference_decl(&self.file_id, name.get_range())
                else {
                    return infer_value(db, cache, expr);
                };
                if Some(decl_id) == self.table_decl {
                    return Value::Type(class_type.clone());
                }
                if let Some(param_idx) = self.param_idx(db, decl_id) {
                    return self.args.get(param_idx).cloned().unwrap_or(Value::Falsy);
                }
                match self.local_value(db, decl_id) {
                    Some(value) => self.eval(db, cache, &value, class_type),
                    None => infer_value(db, cache, expr),
                }
            }
            LuaExpr::BinaryExpr(binary) => {
                let (Some(op), Some((left, right))) = (binary.get_op_token(), binary.get_exprs())
                else {
                    return Value::Unknown;
                };
                let left = self.eval(db, cache, &left, class_type);
                match op.get_op() {
                    BinaryOperator::OpConcat => {
                        match (left, self.eval(db, cache, &right, class_type)) {
                            (Value::String(left), Value::String(right)) => {
                                Value::String(left + &right)
                            }
                            _ => Value::Unknown,
                        }
                    }
                    BinaryOperator::OpAnd => match truthiness(&left) {
                        Some(true) => self.eval(db, cache, &right, class_type),
                        Some(false) => left,
                        None => Value::Unknown,
                    },
                    BinaryOperator::OpOr => match truthiness(&left) {
                        Some(true) => left,
                        Some(false) => self.eval(db, cache, &right, class_type),
                        None => Value::Unknown,
                    },
                    _ => infer_value(db, cache, expr),
                }
            }
            LuaExpr::IndexExpr(index_expr) => {
                let key = match index_expr.get_index_key() {
                    Some(LuaIndexKey::Expr(key)) => match self.eval(db, cache, &key, class_type) {
                        Value::String(key) => key,
                        _ => return Value::Unknown,
                    },
                    _ => match static_key(index_expr) {
                        Some(key) => key,
                        None => return Value::Unknown,
                    },
                };
                let Some(prefix) = index_expr.get_prefix_expr() else {
                    return Value::Unknown;
                };
                let Value::Type(table) = self.eval(db, cache, &prefix, class_type) else {
                    return Value::Unknown;
                };
                find_members_with_key(db, &table, LuaMemberKey::Name(key.as_str().into()), true)
                    .into_iter()
                    .flatten()
                    .map(|member| member.typ)
                    .reduce(|left, right| TypeOps::Union.apply(db, &left, &right))
                    .map_or(Value::Unknown, Value::Type)
            }
            _ => infer_value(db, cache, expr),
        }
    }

    fn param_idx(&self, db: &DbIndex, decl_id: LuaDeclId) -> Option<usize> {
        let is_mutated = db
            .get_reference_index()
            .get_decl_references(&self.file_id, &decl_id)
            .is_some_and(|references| references.is_written_after_declaration(decl_id.position));
        if is_mutated {
            return None;
        }
        match &db.get_decl_index().get_decl(&decl_id)?.extra {
            LuaDeclExtra::Param {
                idx, signature_id, ..
            } if *signature_id == self.signature_id => Some(*idx),
            _ => None,
        }
    }

    /// The value of a never reassigned local the body declares.
    fn local_value(&self, db: &DbIndex, decl_id: LuaDeclId) -> Option<LuaExpr> {
        let decl = db.get_decl_index().get_decl(&decl_id)?;
        let is_mutated = db
            .get_reference_index()
            .get_decl_references(&self.file_id, &decl_id)
            .is_some_and(|references| references.mutable);
        if !decl.is_local() || is_mutated || !self.body.get_range().contains(decl_id.position) {
            return None;
        }
        decl_value_expr(db, decl_id)
    }

    fn unaliased(&self, db: &DbIndex, expr: LuaExpr) -> LuaExpr {
        let LuaExpr::NameExpr(name) = &expr else {
            return expr;
        };
        db.get_reference_index()
            .get_var_reference_decl(&self.file_id, name.get_range())
            .filter(|decl_id| Some(*decl_id) != self.table_decl)
            .and_then(|decl_id| {
                let value = decl_value_expr(db, decl_id)?;
                let is_mutated = db
                    .get_reference_index()
                    .get_decl_references(&self.file_id, &decl_id)
                    .is_some_and(|references| references.mutable);
                (!is_mutated).then_some(value)
            })
            .unwrap_or(expr)
    }

    fn is_table(&self, db: &DbIndex, expr: &Option<LuaExpr>, table_decl: LuaDeclId) -> bool {
        matches!(expr, Some(LuaExpr::NameExpr(name)) if db
            .get_reference_index()
            .get_var_reference_decl(&self.file_id, name.get_range())
            == Some(table_decl))
    }

    /// The nodes of type `N` the body runs itself, outside nested functions.
    fn own_descendants<N: LuaAstNode + 'static>(&self) -> impl Iterator<Item = N> + '_ {
        self.body.descendants::<N>().filter(|node| {
            node.syntax()
                .ancestors()
                .skip(1)
                .find_map(LuaClosureExpr::cast)
                .as_ref()
                == Some(&self.body)
        })
    }

    fn cache(&self) -> LuaInferCache {
        LuaInferCache::new(self.file_id, super::super::call_target::pass_view())
    }
}

fn infer_value(db: &DbIndex, cache: &mut LuaInferCache, expr: &LuaExpr) -> Value {
    match infer_expr(db, cache, expr.clone()) {
        Ok(LuaType::Nil) => Value::Falsy,
        Ok(typ) if !typ.is_unknown() && !typ.is_any() => Value::Type(typ),
        _ => Value::Unknown,
    }
}

/// Whether `value` is truthy: `None` when that is not known.
fn truthiness(value: &Value) -> Option<bool> {
    match value {
        Value::String(_) => Some(true),
        Value::Falsy => Some(false),
        Value::Type(typ) if !typ.is_nullable() && !typ.is_boolean() => Some(true),
        _ => None,
    }
}

fn static_key(index_expr: &LuaIndexExpr) -> Option<String> {
    match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => Some(name.get_name_text().to_string()),
        LuaIndexKey::String(value) => Some(value.get_value()),
        LuaIndexKey::Expr(expr) => string_literal(&expr),
        _ => None,
    }
}

fn string_literal(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(literal) => match literal.get_literal()? {
            LuaLiteralToken::String(value) => Some(value.get_value()),
            _ => None,
        },
        _ => None,
    }
}

fn is_named_call(call: &LuaCallExpr, name: &str) -> bool {
    matches!(call.get_prefix_expr(), Some(LuaExpr::NameExpr(callee)) if callee.get_name_text().as_deref() == Some(name))
}
