use crate::LuaType;

pub trait TypeVisitTrait {
    fn visit_type<F>(&self, f: &mut F)
    where
        F: FnMut(&LuaType);
}

impl LuaType {
    /// Includes identity-bearing bases, keys and constraints that inference's visitor skips.
    pub(crate) fn visit_type_identities(&self, f: &mut impl FnMut(&LuaType)) {
        self.visit_type(&mut |typ| {
            f(typ);
            match typ {
                LuaType::Instance(instance) => instance.get_base().visit_type_identities(f),
                LuaType::DocAttribute(attribute) => {
                    for (_, typ) in attribute.get_params() {
                        if let Some(typ) = typ {
                            typ.visit_type_identities(f);
                        }
                    }
                }
                LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => {
                    if let Some(constraint) = tpl.get_constraint() {
                        constraint.visit_type_identities(f);
                    }
                }
                LuaType::StrTplRef(tpl) => {
                    if let Some(constraint) = tpl.get_constraint() {
                        constraint.visit_type_identities(f);
                    }
                }
                LuaType::Mapped(mapped) => {
                    if let Some(constraint) = &mapped.param.1.type_constraint {
                        constraint.visit_type_identities(f);
                    }
                    mapped.value.visit_type_identities(f);
                }
                LuaType::Object(object) => {
                    for key in object.get_fields().keys() {
                        if let crate::LuaMemberKey::ExprType(typ) = key {
                            typ.visit_type_identities(f);
                        }
                    }
                }
                LuaType::Conditional(conditional) => {
                    for param in conditional.get_infer_params() {
                        if let Some(constraint) = &param.type_constraint {
                            constraint.visit_type_identities(f);
                        }
                    }
                }
                LuaType::Unknown
                | LuaType::Any
                | LuaType::Nil
                | LuaType::Table
                | LuaType::Userdata
                | LuaType::Function
                | LuaType::Thread
                | LuaType::Boolean
                | LuaType::String
                | LuaType::Integer
                | LuaType::Number
                | LuaType::Io
                | LuaType::SelfInfer
                | LuaType::Global
                | LuaType::Never
                | LuaType::BooleanConst(_)
                | LuaType::StringConst(_)
                | LuaType::IntegerConst(_)
                | LuaType::FloatConst(_)
                | LuaType::TableConst(_)
                | LuaType::Ref(_)
                | LuaType::Def(_)
                | LuaType::Signature(_)
                | LuaType::DocStringConst(_)
                | LuaType::DocIntegerConst(_)
                | LuaType::DocBooleanConst(_)
                | LuaType::Namespace(_)
                | LuaType::Language(_)
                | LuaType::ModuleRef(_)
                | LuaType::ConditionalInfer(_)
                | LuaType::Array(_)
                | LuaType::Tuple(_)
                | LuaType::DocFunction(_)
                | LuaType::Union(_)
                | LuaType::Intersection(_)
                | LuaType::MergedTable(_)
                | LuaType::Generic(_)
                | LuaType::TableGeneric(_)
                | LuaType::Variadic(_)
                | LuaType::Call(_)
                | LuaType::MultiLineUnion(_)
                | LuaType::TypeGuard(_)
                | LuaType::TableOf(_) => {}
            }
        });
    }
}
