#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr};

    #[test]
    fn test_issue_586() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            --- @generic T
            --- @param cb fun(...: T...)
            --- @param ... T...
            function invoke1(cb, ...)
                cb(...)
            end

            invoke1(
                function(a, b, c)
                    _a = a
                    _b = b
                    _c = c
                end,
                1, "2", "3"
            )
            "#,
        );

        let a_ty = ws.expr_ty("_a");
        let b_ty = ws.expr_ty("_b");
        let c_ty = ws.expr_ty("_c");

        assert_eq!(a_ty, ws.ty("integer"));
        assert_eq!(b_ty, ws.ty("string"));
        assert_eq!(c_ty, ws.ty("string"));
    }

    #[test]
    fn test_issue_658() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            --- @generic T1, T2, R
            --- @param fn fun(_:T1..., _:T2...): R...
            --- @param ... T1...
            --- @return fun(_:T2...): R...
            local function curry(fn, ...)
            local nargs, args = select('#', ...), { ... }
            return function(...)
                local nargs2 = select('#', ...)
                for i = 1, nargs2 do
                args[nargs + i] = select(i, ...)
                end
                return fn(unpack(args, 1, nargs + nargs2))
            end
            end

            --- @param a string
            --- @param b string
            --- @param c table
            local function foo(a, b, c) end

            bar = curry(foo, 'a')
            "#,
        );

        let bar_ty = ws.expr_ty("bar");
        let expected = ws.ty("fun(b:string, c:table)");
        assert_eq!(bar_ty, expected);
    }

    #[test]
    fn test_generic_callback_variadic_middle_keeps_fixed_suffix() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@generic T
            ---@param cb fun(head: string, ...: T..., tail: boolean)
            ---@return fun(...: T...)
            function drop_edges(cb) end

            ---@param head string
            ---@param amount integer
            ---@param label string
            ---@param tail boolean
            local function source(head, amount, label, tail) end

            callback = drop_edges(source)
            "#,
        );

        let callback_ty = ws.expr_ty("callback");
        let expected = ws.ty("fun(amount: integer, label: string)");
        assert_eq!(callback_ty, expected);
    }

    #[test]
    fn test_generic_call_variadic_middle_keeps_fixed_suffix() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class CollectMiddle
            ---@overload fun<T>(head: string, ...: T..., tail: boolean): T...
            local collect_middle = {}

            first, second, third = collect_middle("head", 1, "two", true)
            "#,
        );

        assert_eq!(ws.expr_ty("first"), ws.ty("integer"));
        assert_eq!(ws.expr_ty("second"), ws.ty("string"));
        assert_eq!(ws.expr_ty("third"), ws.ty("nil"));
    }

    #[test]
    fn test_generic_call_variadic_middle_continues_into_generic_suffix() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class CollectTail
            ---@overload fun<T, U>(head: string, ...: T..., tail: U): U
            local collect_tail = {}

            tail = collect_tail("head", 1, "two", true)
            "#,
        );

        assert_eq!(ws.expr_ty("tail"), ws.ty("boolean"));
    }

    #[test]
    fn test_generic_params() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Observable<T>
            ---@class Subject<T>: Observable<T>

            ---@generic T
            ---@param ... Observable<T>
            ---@return Observable<T>
            function concat(...)
            end
            "#,
        );

        ws.def(
            r#"
            ---@type Subject<number>
            local s1
            A = concat(s1)
            "#,
        );

        let a_ty = ws.expr_ty("A");
        let expected = ws.ty("Observable<number>");
        assert_eq!(a_ty, expected);
    }

    #[test]
    fn test_generic_inference_combines_multiple_candidates() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@generic T
            ---@param first T
            ---@param second T
            ---@return T
            function choose(first, second) end

            value = choose("name", 1)
            other = choose("name", "other")
            "#,
        );

        let value_ty = ws.expr_ty("value");
        let expected = ws.ty("string|integer");
        assert_eq!(value_ty, expected);

        let other_ty = ws.expr_ty("other");
        let expected = ws.ty("string");
        assert_eq!(other_ty, expected);
    }

    #[test]
    fn test_generic_literal_widening_keeps_raw_type_for_conditional() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias IsName<T> T extends "name" and true or false

            ---@generic T
            ---@param value T
            ---@return T
            function identity(value) end

            ---@generic T
            ---@param value T
            ---@return IsName<T>
            function is_name(value) end

            widened = identity("name")
            matched = is_name("name")
            "#,
        );

        let widened_ty = ws.expr_ty("widened");
        let matched_ty = ws.expr_ty("matched");
        assert_eq!(ws.humanize_type(widened_ty), "string");
        assert_eq!(ws.humanize_type(matched_ty), "true");
    }

    #[test]
    fn test_generic_direct_candidates_use_union() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Animal
            ---@class Dog: Animal
            ---@class Cat: Animal

            ---@generic T
            ---@param first T
            ---@param second T
            ---@return T
            function choose(first, second) end

            ---@type Dog
            local dog
            ---@type Cat
            local cat
            animal = choose(dog, cat)
            "#,
        );

        let animal_ty = ws.expr_ty("animal");
        let expected = ws.ty("Dog|Cat");
        assert_eq!(animal_ty, expected);
    }

    #[test]
    fn test_generic_direct_candidates_use_union_for_generic_subclasses() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Box<T>
            ---@class StringBox: Box<string>
            ---@class NumberBox: Box<number>

            ---@generic T
            ---@param first T
            ---@param second T
            ---@return T
            function choose(first, second) end

            ---@type StringBox
            local string_box
            ---@type NumberBox
            local number_box
            box = choose(string_box, number_box)
            "#,
        );

        let box_ty = ws.expr_ty("box");
        let expected = ws.ty("StringBox|NumberBox");
        assert_eq!(box_ty, expected);
    }

    #[test]
    fn test_generic_function_parameter_candidates_use_common_subtype() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Animal
            ---@class Dog: Animal

            ---@generic T
            ---@param first fun(value: T)
            ---@param second fun(value: T)
            ---@return T
            function choose_callback_value(first, second) end

            ---@param value Animal
            local function accepts_animal(value) end

            ---@param value Dog
            local function accepts_dog(value) end

            dog = choose_callback_value(accepts_animal, accepts_dog)
            "#,
        );

        let dog_ty = ws.expr_ty("dog");
        let expected = ws.ty("Dog");
        assert_eq!(dog_ty, expected);
    }

    #[test]
    fn test_generic_function_return_candidates_use_union() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Animal
            ---@class Dog: Animal
            ---@class Cat: Animal

            ---@generic T
            ---@param first fun(): T
            ---@param second fun(): T
            ---@return T
            function choose_producer_value(first, second) end

            ---@return Dog
            local function make_dog() end

            ---@return Cat
            local function make_cat() end

            animal = choose_producer_value(make_dog, make_cat)
            "#,
        );

        let animal_ty = ws.expr_ty("animal");
        let expected = ws.ty("Dog|Cat");
        assert_eq!(animal_ty, expected);
    }

    #[test]
    fn test_generic_mixed_function_candidates_prefer_covariant_subtype() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Animal
            ---@class Dog: Animal

            ---@generic T
            ---@param value T
            ---@param callback fun(value: T)
            ---@return T
            function use_callback(value, callback) end

            ---@type Dog
            local dog

            ---@param value Animal
            local function accepts_animal(value) end

            result = use_callback(dog, accepts_animal)
            "#,
        );

        let result_ty = ws.expr_ty("result");
        let expected = ws.ty("Dog");
        assert_eq!(result_ty, expected);
    }

    #[test]
    fn test_generic_mixed_function_candidates_prefer_contravariant_subtype() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Animal
            ---@class Dog: Animal

            ---@generic T
            ---@param value T
            ---@param callback fun(value: T)
            ---@return T
            function use_callback(value, callback) end

            ---@type Animal
            local animal

            ---@param value Dog
            local function accepts_dog(value) end

            result = use_callback(animal, accepts_dog)
            "#,
        );

        let result_ty = ws.expr_ty("result");
        let expected = ws.ty("Dog");
        assert_eq!(result_ty, expected);
    }

    #[test]
    fn test_generic_mixed_function_candidates_ignore_any_covariant() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Dog

            ---@generic T
            ---@param value T
            ---@param callback fun(value: T)
            ---@return T
            function use_callback(value, callback) end

            ---@type any
            local value

            ---@param value Dog
            local function accepts_dog(value) end

            result = use_callback(value, accepts_dog)
            "#,
        );

        let result_ty = ws.expr_ty("result");
        let expected = ws.ty("Dog");
        assert_eq!(result_ty, expected);
    }

    #[test]
    fn test_generic_return_infers_from_local_doc_context() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@generic T
            ---@return T
            function make() end

            ---@type string
            local value = make()
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("string");
        assert_eq!(call_ty, expected);
    }

    #[test]
    fn test_generic_return_infers_from_assignment_doc_context() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@generic T
            ---@return T
            function make() end

            ---@type string
            local value
            value = make()
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("string");
        assert_eq!(call_ty, expected);
    }

    #[test]
    fn test_generic_return_infers_from_member_doc_context() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Holder
            ---@field value string

            ---@generic T
            ---@return T
            function make() end

            ---@type Holder
            local holder = {}
            holder.value = make()
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("string");
        assert_eq!(call_ty, expected);
    }

    #[test]
    fn test_generic_return_infers_from_table_field_doc_context() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Holder
            ---@field value string

            ---@generic T
            ---@return T
            function make() end

            ---@type Holder
            local holder = {
                value = make()
            }
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("string");
        assert_eq!(call_ty, expected);
    }

    #[test]
    fn test_generic_return_context_combines_repeated_candidates() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Dog
            ---@class Cat
            ---@class Pair<A, B>

            ---@generic T
            ---@return Pair<T, T>
            function make_pair() end

            ---@type Pair<Dog, Cat>
            local pair = make_pair()
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("Pair<Dog|Cat, Dog|Cat>");
        assert_eq!(call_ty, expected);
    }

    #[test]
    fn test_generic_return_context_flows_into_callback_param() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@generic T, U
            ---@param cb fun(value: T): U
            ---@return fun(value: T): U
            function wrap(cb) end

            ---@type fun(value: string): number
            local mapped = wrap(function(value)
                seen = value
                return 1
            end)
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("fun(value: string): number");
        assert_eq!(call_ty, expected);

        let seen_ty = ws.expr_ty("seen");
        assert_eq!(seen_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_return_context_does_not_override_arg_inference() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@generic T
            ---@param value T
            ---@return T
            function id(value) end

            ---@type string
            local value = id(1)
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("integer");
        assert_eq!(call_ty, expected);
    }

    #[test]
    fn test_generic_overload_return_context_flows_into_callback_param() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Wrap
            ---@overload fun<T, U>(cb: fun(value: T): U): fun(value: T): U
            local wrap = {}

            ---@type fun(value: string): number
            local mapped = wrap(function(value)
                overloaded_seen = value
                return 1
            end)
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("fun(value: string): number");
        assert_eq!(call_ty, expected);

        let seen_ty = ws.expr_ty("overloaded_seen");
        assert_eq!(seen_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_union_return_context_flows_into_callback_param() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Mapper
            ---@field map (fun<T, U>(cb: fun(value: T): U): fun(value: T): U) | (fun(label: string): string)
            local mapper = {}

            ---@type fun(value: string): number
            local mapped = mapper.map(function(value)
                union_seen = value
                return 1
            end)
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("fun(value: string): number");
        assert_eq!(call_ty, expected);

        let seen_ty = ws.expr_ty("union_seen");
        assert_eq!(seen_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_metatable_call_return_context_flows_into_callback_param() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@generic T, U
            ---@param cb fun(value: T): U
            ---@return fun(value: T): U
            function meta(cb)
            end

            local wrapper = setmetatable({}, { __call = meta })

            ---@type fun(value: string): number
            local mapped = wrapper(function(value)
                metatable_seen = value
                return 1
            end)
            "#,
        );

        let tree = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("Tree must exist");
        let call_expr = tree
            .get_chunk_node()
            .descendants::<LuaCallExpr>()
            .last()
            .expect("Call expression must exist");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("fun(value: string): number");
        assert_eq!(call_ty, expected);

        let seen_ty = ws.expr_ty("metatable_seen");
        assert_eq!(seen_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_callable_overload_return_context_flows_into_callback_param() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class WrappedArray<T>
            ---@overload fun<U>(iterator: fun(value: T): U): U[]

            ---@type WrappedArray<string>
            local array = {}

            ---@type number[]
            local mapped = array(function(value)
                callable_overload_seen = value
                return 1
            end)
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        assert_eq!(call_ty, ws.ty("number[]"));

        let seen_ty = ws.expr_ty("callable_overload_seen");
        assert_eq!(seen_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_receiver_method_return_context_flows_into_callback_param() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class WrappedArray<T>
            ---@field map fun<U>(self: WrappedArray<T>, cb: fun(value: T): U): WrappedArray<U>

            ---@type WrappedArray<string>
            local array = {}

            ---@type WrappedArray<number>
            local mapped = array:map(function(value)
                receiver_seen = value
                return 1
            end)
            "#,
        );

        let call_expr = ws.get_node::<LuaCallExpr>(file_id);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        let expected = ws.ty("WrappedArray<number>");
        assert_eq!(call_ty, expected);

        let seen_ty = ws.expr_ty("receiver_seen");
        assert_eq!(seen_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_combinator_uses_first_arg_to_type_callback_params() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Collection<T, U>
            local Collection = {}

            ---@generic T, U, V
            ---@param c Collection<T, U>
            ---@param cb fun(x: T, y: U): V
            ---@return Collection<T, V>
            function map(c, cb)
            end

            ---@type Collection<number, string>
            local collection = {}

            local result = map(collection, function(x, y)
                combinator_x = x
                combinator_y = y
                return y
            end)
            "#,
        );

        let tree = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("Tree must exist");
        let call_expr = tree
            .get_chunk_node()
            .descendants::<LuaCallExpr>()
            .last()
            .expect("call expression must exist");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        assert_eq!(call_ty, ws.ty("Collection<number, string>"));

        let x_ty = ws.expr_ty("combinator_x");
        let y_ty = ws.expr_ty("combinator_y");
        assert_eq!(x_ty, ws.ty("number"));
        assert_eq!(y_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_field_combinator_uses_first_arg_to_type_callback_params() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Collection<T, U>
            local Collection = {}

            ---@class Combinators
            ---@field map fun<T, U, V>(c: Collection<T, U>, cb: fun(x: T, y: U): V): Collection<T, V>

            ---@type Combinators
            local combinators = {}

            ---@type Collection<number, string>
            local collection = {}

            local result = combinators.map(collection, function(x, y)
                field_combinator_x = x
                field_combinator_y = y
                return y
            end)
            "#,
        );

        let tree = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("Tree must exist");
        let call_expr = tree
            .get_chunk_node()
            .descendants::<LuaCallExpr>()
            .last()
            .expect("call expression must exist");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        assert_eq!(call_ty, ws.ty("Collection<number, string>"));

        let x_ty = ws.expr_ty("field_combinator_x");
        let y_ty = ws.expr_ty("field_combinator_y");
        assert_eq!(x_ty, ws.ty("number"));
        assert_eq!(y_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_field_combinator_prefers_specific_overload() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Collection<T, U>
            local Collection = {}

            ---@class Combinators
            ---@field map (fun<T, U, V>(c: Collection<T, U>, cb: fun(x: T, y: U): V): Collection<T, V>) | (fun<T, U>(c: Collection<T, U>, cb: fun(x: T, y: U): any): Collection<any, any>)

            ---@type Combinators
            local combinators = {}

            ---@type Collection<number, string>
            local collection = {}

            local result = combinators.map(collection, function(x, y)
                overloaded_combinator_x = x
                overloaded_combinator_y = y
                return y
            end)
            "#,
        );

        let tree = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("Tree must exist");
        let call_expr = tree
            .get_chunk_node()
            .descendants::<LuaCallExpr>()
            .last()
            .expect("call expression must exist");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        assert_eq!(call_ty, ws.ty("Collection<number, string>"));

        let x_ty = ws.expr_ty("overloaded_combinator_x");
        let y_ty = ws.expr_ty("overloaded_combinator_y");
        assert_eq!(x_ty, ws.ty("number"));
        assert_eq!(y_ty, ws.ty("string"));
    }

    #[test]
    fn test_generic_field_combinator_infers_from_named_callback() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Collection<T>
            local Collection = {}

            ---@class Combinators
            ---@field map (fun<T, U>(c: Collection<T>, cb: fun(x: T): U): Collection<U>) | (fun<T>(c: Collection<T>, cb: fun(x: T): any): Collection<any>)

            ---@type Combinators
            local combinators = {}

            ---@type Collection<number>
            local collection = {}

            ---@param value number
            ---@return string
            local function stringify(value)
                named_callback_seen = value
                return ""
            end

            local result = combinators.map(collection, stringify)
            "#,
        );

        let tree = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("Tree must exist");
        let call_expr = tree
            .get_chunk_node()
            .descendants::<LuaCallExpr>()
            .last()
            .expect("call expression must exist");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        assert_eq!(call_ty, ws.ty("Collection<string>"));

        let seen_ty = ws.expr_ty("named_callback_seen");
        assert_eq!(seen_ty, ws.ty("number"));
    }

    #[test]
    fn test_generic_field_combinator_uses_explicit_type_args_for_callback() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class Collection<T>
            local Collection = {}

            ---@class Combinators
            ---@field map (fun<T, U>(c: Collection<T>, cb: fun(x: T): U): Collection<U>) | (fun<T>(c: Collection<T>, cb: fun(x: T): any): Collection<any>)

            ---@type Combinators
            local combinators = {}

            ---@type Collection<number>
            local collection = {}

            local result = combinators.map--[[@<number, string>]](collection, function(value)
                explicit_callback_seen = value
                return ""
            end)
            "#,
        );

        let tree = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("Tree must exist");
        let call_expr = tree
            .get_chunk_node()
            .descendants::<LuaCallExpr>()
            .last()
            .expect("call expression must exist");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let call_ty = semantic_model
            .infer_expr(LuaExpr::CallExpr(call_expr))
            .expect("Call type must resolve");
        assert_eq!(call_ty, ws.ty("Collection<string>"));

        let seen_ty = ws.expr_ty("explicit_callback_seen");
        assert_eq!(seen_ty, ws.ty("number"));
    }

    #[test]
    fn test_issue_646() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Base
            ---@field a string
            "#,
        );
        ws.def(
            r#"
            ---@generic T: Base
            ---@param file T
            function dirname(file)
                A = file.a
            end
            "#,
        );

        let a_ty = ws.expr_ty("A");
        let expected = ws.ty("string");
        assert_eq!(a_ty, expected);
    }

    #[test]
    fn test_local_generics_in_global_scope() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                --- @generic T
                --- @param x T
                function foo(x)
                    a = x
                end
            "#,
        );
        let a_ty = ws.expr_ty("a");
        assert_eq!(a_ty, ws.ty("unknown"));
    }

    // Currently fails:
    /*
    #[test]
    fn test_local_generics_in_global_scope_member() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                t = {}

                --- @generic T
                --- @param x T
                function foo(x)
                    t.a = x
                end
                local b = t.a
            "#,
        );
        let a_ty = ws.expr_ty("t.a");
        assert_eq!(a_ty, LuaType::Unknown);
    }
    */

    #[test]
    fn test_issue_738() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias Predicate<A> fun(...: A...): boolean
            ---@type Predicate<[string, integer, table]>
            pred = function() end
            "#,
        );
        assert!(ws.check_code_for(DiagnosticCode::ParamTypeMismatch, r#"pred('hello', 1, {})"#));
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"pred('hello',"1", {})"#
        ));
    }

    #[test]
    fn test_infer_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias A01<T> T extends infer P and P or unknown

            ---@param v number
            function f(v)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type A01<number>
            local a
            f(a)
            "#,
        ));
    }

    #[test]
    fn test_infer_type_params() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias A02<T> T extends (fun(v1: infer P)) and P or string

            ---@param v fun(v1: number)
            function f(v)
            end
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type A02<number>
            local a
            f(a)
            "#,
        ));
    }

    #[test]
    fn test_infer_type_params_extract() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias A02<T> T extends (fun(v0: number, v1: infer P)) and P or string

            ---@param v number
            function accept(v)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type A02<fun(v0: number, v1: number)>
            local a
            accept(a)
            "#,
        ));
    }

    #[test]
    fn test_return_generic() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias A01<T> T

            ---@param v number
            function f(v)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type A01<number>
            local a
            f(a)
            "#,
        ));
    }

    #[test]
    fn test_infer_parameters() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias Parameters<T> T extends (fun(...: infer P): any) and P or unknown

            ---@generic T
            ---@param fn T
            ---@param ... Parameters<T>...
            function f(fn, ...)
            end
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type fun(name: string, age: number)
            local greet
            f(greet, "a", "b")
            "#,
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type fun(name: string, age: number)
            local greet
            f(greet, "a", 1)
            "#,
        ));
    }

    #[test]
    fn test_infer_parameters_2() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias A01<T> T extends (fun(a: any, b: infer P): any) and P or number

            ---@alias A02 number

            ---@param v number
            function f(v)
            end
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type A01<fun(a: A02, b: string)>
            local a
            f(a)
            "#,
        ));
    }

    #[test]
    fn test_infer_return_parameters() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias ReturnType<T> T extends (fun(...: any): infer R) and R or unknown

            ---@generic T
            ---@param fn T
            ---@return ReturnType<T>
            function f(fn, ...)
            end

            ---@param v string
            function accept(v)
            end
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type fun(): number
            local greet
            local m = f(greet)
            accept(m)
            "#,
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type fun(): string
            local greet
            local m = f(greet)
            accept(m)
            "#,
        ));
    }

    #[test]
    fn test_type_mapped_pick() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias Pick<T, K extends keyof T> { [P in K]: T[P]; }

            ---@param v {name: string, age: number}
            function accept(v)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Pick<{name: string, age: number, email: string}, "name" | "age">
            local m
            accept(m)
            "#,
        ));
    }

    #[test]
    fn test_type_partial() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias Partial<T> { [P in keyof T]?: T[P]; }

            ---@param v {name?: string, age?: number}
            function accept(v)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Partial<{name: string, age: number}>
            local m
            accept(m)
            "#,
        ));
    }

    #[test]
    fn test_issue_787() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Wrapper<T>

            ---@alias UnwrapUnion<T> { [K in keyof T]: T[K] extends Wrapper<infer U> and U or unknown; }

            ---@generic T
            ---@param ... T...
            ---@return UnwrapUnion<T>...
            function unwrap(...) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Wrapper<int>, Wrapper<int>, Wrapper<string>
            local a, b, c

            D, E, F = unwrap(a, b, c)
            "#,
        ));
        assert_eq!(ws.expr_ty("D"), ws.ty("int"));
        assert_eq!(ws.expr_ty("E"), ws.ty("int"));
        assert_eq!(ws.expr_ty("F"), ws.ty("string"));
    }

    #[test]
    fn test_mapped_infer_from_generic_function_table_literal() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Schema<T>

            ---@alias InferShape<T> { [K in keyof T]: T[K] extends Schema<infer U> and U or unknown; }

            ---@return Schema<string>
            function mk_string() end

            ---@return Schema<number>
            function mk_number() end

            ---@generic T
            ---@param schema T
            ---@return InferShape<T>
            function object(schema) end

            result = object({
                name = mk_string(),
                age = mk_number(),
            })
            "#,
        );

        let name_ty = ws.expr_ty("result.name");
        let age_ty = ws.expr_ty("result.age");
        assert_eq!(ws.humanize_type(name_ty), "string");
        assert_eq!(ws.humanize_type(age_ty), "number");
    }

    #[test]
    fn test_mapped_infer_from_generic_function_table_variable() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Schema<T>

            ---@alias InferShape<T> { [K in keyof T]: T[K] extends Schema<infer U> and U or unknown; }

            ---@return Schema<string>
            function mk_string() end

            ---@return Schema<number>
            function mk_number() end

            ---@generic T
            ---@param schema T
            ---@return InferShape<T>
            function object(schema) end

            local shape = {
                name = mk_string(),
                age = mk_number(),
            }
            result = object(shape)
            "#,
        );

        let name_ty = ws.expr_ty("result.name");
        let age_ty = ws.expr_ty("result.age");
        assert_eq!(ws.humanize_type(name_ty), "string");
        assert_eq!(ws.humanize_type(age_ty), "number");
    }

    #[test]
    fn test_reverse_mapped_infer_from_reducer_table_literal() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Action
            ---@field type string

            ---@alias Reducer<S> fun(state: S, action: Action): S
            ---@alias Reducers<S> { [K in keyof S]: Reducer<S[K]>; }

            ---@generic S
            ---@param reducers Reducers<S>
            ---@return Reducer<S>
            function combine_reducers(reducers) end

            ---@param state string
            ---@param action Action
            ---@return string
            local function test_inner(state, action)
                return "dummy"
            end

            local test = combine_reducers({
                test_inner = test_inner,
            })

            local test_outer = combine_reducers({
                test = test,
            })

            local state = test_outer(nil, nil)
            result = state.test.test_inner
            "#,
        );

        let result_ty = ws.expr_ty("result");
        assert_eq!(ws.humanize_type(result_ty), "string");
    }

    #[test]
    fn test_reverse_mapped_infer_from_reducer_table_variable() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Action
            ---@field type string

            ---@alias Reducer<S> fun(state: S, action: Action): S
            ---@alias Reducers<S> { [K in keyof S]: Reducer<S[K]>; }

            ---@generic S
            ---@param reducers Reducers<S>
            ---@return Reducer<S>
            function combine_reducers(reducers) end

            ---@param state string
            ---@param action Action
            ---@return string
            local function name(state, action)
                return "dummy"
            end

            local reducers = {
                name = name,
            }

            local reducer = combine_reducers(reducers)
            result = reducer(nil, nil).name
            "#,
        );

        let result_ty = ws.expr_ty("result");
        assert_eq!(ws.humanize_type(result_ty), "string");
    }

    #[test]
    fn test_reverse_mapped_infer_with_rawget_alias() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias std.RawGet<T, K> unknown
            ---@alias RawMirror<T> { [K in keyof T]: std.RawGet<T, K>; }

            ---@generic T
            ---@param values RawMirror<T>
            ---@return T
            function raw_unmirror(values) end

            result = raw_unmirror({
                name = "Ada",
                age = 42,
            })
            "#,
        );

        let name_ty = ws.expr_ty("result.name");
        let age_ty = ws.expr_ty("result.age");
        assert_eq!(ws.humanize_type(name_ty), "\"Ada\"");
        assert_eq!(ws.humanize_type(age_ty), "42");
    }

    #[test]
    fn test_reverse_mapped_infer_through_pick_key_constraint() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias Pick<T, K extends keyof T> { [P in K]: T[P]; }

            ---@generic T, K extends keyof T
            ---@param values Pick<T, K>
            ---@return T
            function restore_pick(values) end

            result = restore_pick({
                name = "Ada",
            })
            "#,
        );

        let name_ty = ws.expr_ty("result.name");
        assert_eq!(ws.humanize_type(name_ty), "\"Ada\"");
    }

    #[test]
    fn test_reverse_mapped_infer_pick_key_constraint_key_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias Pick<T, K extends keyof T> { [P in K]: T[P]; }

            ---@generic T, K extends keyof T
            ---@param values Pick<T, K>
            ---@return K
            function picked_key(values) end

            key = picked_key({
                name = "Ada",
            })
            "#,
        );

        let key_ty = ws.expr_ty("key");
        assert_eq!(ws.humanize_type(key_ty), "\"name\"");
    }

    #[test]
    fn test_mapped_type_parameter_constraint_infers_value_union() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias ValueRecord<K extends string, V> { [P in K]: V; }

            ---@generic K extends string, V
            ---@param values ValueRecord<K, V>
            ---@return V
            function record_value(values) end

            result = record_value({
                name = "Ada",
                age = 42,
            })
            "#,
        );

        let value_ty = ws.expr_ty("result");
        let value_ty = ws.humanize_type(value_ty);
        assert!(
            matches!(value_ty.as_str(), "(integer|string)" | "(string|integer)"),
            "{value_ty}"
        );
    }

    #[test]
    fn test_mapped_type_parameter_constraint_infers_key_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias ValueRecord<K extends string, V> { [P in K]: V; }

            ---@generic K extends string, V
            ---@param values ValueRecord<K, V>
            ---@return K
            function record_key(values) end

            key = record_key({
                name = "Ada",
            })
            "#,
        );

        let key_ty = ws.expr_ty("key");
        assert_eq!(ws.humanize_type(key_ty), "\"name\"");
    }

    #[test]
    fn test_table_generic_index_infers_from_structural_fields() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@generic V
            ---@param values table<string, V>
            ---@return V
            function table_value(values) end

            result = table_value({
                name = "Ada",
                age = 42,
            })
            "#,
        );

        let value_ty = ws.expr_ty("result");
        let value_ty = ws.humanize_type(value_ty);
        assert!(
            matches!(value_ty.as_str(), "(integer|string)" | "(string|integer)"),
            "{value_ty}"
        );
    }

    #[test]
    fn test_reverse_mapped_partial_strips_optional_from_source_field() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias Partial<T> { [P in keyof T]?: T[P]; }

            ---@generic T
            ---@param values Partial<T>
            ---@return T
            function restore_partial(values) end

            ---@type {name?: string}
            local values

            result = restore_partial(values)
            "#,
        );

        let name_ty = ws.expr_ty("result.name");
        assert_eq!(ws.humanize_type(name_ty), "string");
    }

    #[test]
    fn test_reverse_mapped_infer_through_generic_wrapper() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@field value T

            ---@alias Boxified<T> { [K in keyof T]: Box<T[K]>; }

            ---@generic T
            ---@param value T
            ---@return Box<T>
            function box(value) end

            ---@generic T
            ---@param obj Boxified<T>
            ---@return T
            function unboxify(obj) end

            result = unboxify({
                is_perfect = box(true),
                weight = box(42),
            })
            "#,
        );

        let is_perfect_ty = ws.expr_ty("result.is_perfect");
        let weight_ty = ws.expr_ty("result.weight");
        assert_eq!(ws.humanize_type(is_perfect_ty), "boolean");
        assert_eq!(ws.humanize_type(weight_ty), "integer");
    }

    #[test]
    fn test_reverse_mapped_infer_through_tuple_source() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@field value T

            ---@alias Boxified<T> { [K in keyof T]: Box<T[K]>; }

            ---@generic T
            ---@param value T
            ---@return Box<T>
            function box(value) end

            ---@generic T
            ---@param values Boxified<T>
            ---@return T
            function unboxify(values) end

            result = unboxify({
                box(1),
                box("two"),
            })
            "#,
        );

        let first_ty = ws.expr_ty("result[1]");
        let second_ty = ws.expr_ty("result[2]");
        assert_eq!(ws.humanize_type(first_ty), "integer");
        assert_eq!(ws.humanize_type(second_ty), "string");
    }

    #[test]
    fn test_reverse_mapped_infer_through_array_source() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@field value T

            ---@alias Boxified<T> { [K in keyof T]: Box<T[K]>; }

            ---@generic T
            ---@param values Boxified<T>
            ---@return T
            function unboxify(values) end

            ---@type Box<number>[]
            local values

            result = unboxify(values)
            "#,
        );

        let value_ty = ws.expr_ty("result[1]");
        assert_eq!(ws.humanize_type(value_ty), "number");
    }

    #[test]
    fn test_full_reverse_mapped_inference_beats_partial_candidate() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@field value T

            ---@alias Boxified<T> { [K in keyof T]: Box<T[K]>; }

            ---@generic T
            ---@param value T
            ---@return Box<T>
            function box(value) end

            ---@generic T
            ---@param partial Boxified<T>
            ---@param full Boxified<T>
            ---@return T
            function prefer_full(partial, full) end

            local partial = {
                stale = box("partial"),
                skipped = true,
            }

            local full = {
                fresh = box(42),
            }

            result = prefer_full(partial, full)
            "#,
        );

        let stale_ty = ws.expr_ty("result.stale");
        let fresh_ty = ws.expr_ty("result.fresh");
        assert_eq!(ws.humanize_type(stale_ty), "nil");
        assert_eq!(ws.humanize_type(fresh_ty), "integer");
    }

    #[test]
    fn test_reverse_mapped_collects_full_candidates_after_first_mapped_candidate() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@field value T

            ---@alias Boxified<T> { [K in keyof T]: Box<T[K]>; }

            ---@generic T
            ---@param value T
            ---@return Box<T>
            function box(value) end

            ---@generic T
            ---@param first Boxified<T>
            ---@param second Boxified<T>
            ---@return T
            function merge_boxified(first, second) end

            local first = {
                value = box("first"),
            }

            local second = {
                value = box(42),
            }

            result = merge_boxified(first, second)
            "#,
        );

        let value_ty = ws.expr_ty("result.value");
        assert_eq!(ws.humanize_type(value_ty), "(string|integer)");
    }

    #[test]
    fn test_inline_reverse_mapped_partial_does_not_block_later_full_candidate() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@field value T

            ---@alias Boxified<T> { [K in keyof T]: Box<T[K]>; }

            ---@generic T
            ---@param value T
            ---@return Box<T>
            function box(value) end

            ---@generic T
            ---@param partial Boxified<T>
            ---@param full Boxified<T>
            ---@return T
            function prefer_full(partial, full) end

            result = prefer_full({
                stale = box("partial"),
                skipped = true,
            }, {
                fresh = box(42),
            })
            "#,
        );

        let stale_ty = ws.expr_ty("result.stale");
        let fresh_ty = ws.expr_ty("result.fresh");
        assert_eq!(ws.humanize_type(stale_ty), "nil");
        assert_eq!(ws.humanize_type(fresh_ty), "integer");
    }

    #[test]
    fn test_mapped_inference_is_lower_priority_than_direct_inference() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias Mirror<T> { [K in keyof T]: T[K]; }

            ---@generic T
            ---@param secondary Mirror<T>
            ---@param primary T
            ---@return T
            function mapped_then_direct(secondary, primary) end

            result = mapped_then_direct({ x = 1 }, { x = "direct", y = "name" })
            "#,
        );

        let x_ty = ws.expr_ty("result.x");
        let y_ty = ws.expr_ty("result.y");
        assert_eq!(ws.humanize_type(x_ty), "\"direct\"");
        assert_eq!(ws.humanize_type(y_ty), "\"name\"");
    }

    #[test]
    fn test_conditional_infer_from_concrete_class_super_generic() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Schema<T>
            ---@class StringSchema: Schema<string>
            ---@class NumberSchema: Schema<number>

            ---@alias Infer<T> T extends Schema<infer U> and U or unknown
            ---@alias InferShape<T> { [K in keyof T]: Infer<T[K]>; }

            ---@return StringSchema
            function mk_string() end

            ---@return NumberSchema
            function mk_number() end

            ---@generic T
            ---@param schema T
            ---@return InferShape<T>
            function object(schema) end

            result = object({
                name = mk_string(),
                age = mk_number(),
            })
            "#,
        );

        let name_ty = ws.expr_ty("result.name");
        let age_ty = ws.expr_ty("result.age");
        assert_eq!(ws.humanize_type(name_ty), "string");
        assert_eq!(ws.humanize_type(age_ty), "number");
    }

    #[test]
    fn test_conditional_infer_from_generic_alias_source() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Schema<T>
            ---@alias Wrapped<T> Schema<T>
            ---@alias Infer<T> T extends Schema<infer U> and U or unknown

            ---@generic T
            ---@param schema T
            ---@return Infer<T>
            function infer(schema) end

            ---@type Wrapped<string>
            local wrapped

            value = infer(wrapped)
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "string");
    }

    #[test]
    fn test_conditional_infer_combines_repeated_candidates() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Pair<A, B>
            ---@alias InferPair<T> T extends Pair<infer U, infer U> and U or unknown

            ---@generic T
            ---@param value T
            ---@return InferPair<T>
            function infer_pair(value) end

            ---@type Pair<string, number>
            local pair

            value = infer_pair(pair)
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "(string|number)");
    }

    #[test]
    fn test_conditional_infer_generic_params_use_independent_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@class Source<T>: Box<T>
            ---@class Pair<A, B>

            ---@alias ExtractFirst<T> T extends Pair<Box<infer A>, Box<infer B>> and A or unknown
            ---@alias ExtractSecond<T> T extends Pair<Box<infer A>, Box<infer B>> and B or unknown

            ---@generic T
            ---@param value T
            ---@return ExtractFirst<T>
            function extract_first(value) end

            ---@generic T
            ---@param value T
            ---@return ExtractSecond<T>
            function extract_second(value) end

            ---@type Pair<Source<string>, Source<number>>
            local pair

            first = extract_first(pair)
            second = extract_second(pair)
            "#,
        );

        let first_ty = ws.expr_ty("first");
        let second_ty = ws.expr_ty("second");
        assert_eq!(ws.humanize_type(first_ty), "string");
        assert_eq!(ws.humanize_type(second_ty), "number");
    }

    #[test]
    fn test_conditional_infer_collects_from_each_source_union_member() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Wrapper<T>
            ---@alias Unwrap<T> T extends Wrapper<infer U> and U or unknown

            ---@generic T
            ---@param value T
            ---@return Unwrap<T>
            function unwrap(value) end

            ---@type Wrapper<string>|Wrapper<number>
            local wrapped

            value = unwrap(wrapped)
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "(string|number)");
    }

    #[test]
    fn test_conditional_infer_collects_from_matching_pattern_union_members() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Left<T>
            ---@class Right<T>
            ---@alias Extract<T> T extends (Left<infer U>|Right<infer U>) and U or unknown

            ---@generic T
            ---@param value T
            ---@return Extract<T>
            function extract(value) end

            ---@type Left<string>|Right<number>
            local source

            value = extract(source)
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "(string|number)");
    }

    #[test]
    fn test_conditional_infer_pattern_union_prefers_structural_match_over_naked_infer() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@alias Extract<T> T extends (Box<infer U>|infer U) and U or unknown

            ---@generic T
            ---@param value T
            ---@return Extract<T>
            function extract(value) end

            ---@type Box<string>
            local source

            value = extract(source)
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "string");
    }

    #[test]
    fn test_conditional_infer_pattern_union_uses_naked_infer_for_unmatched_source_member() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@alias Extract<T> T extends (Box<infer U>|infer U) and U or unknown

            ---@generic T
            ---@param value T
            ---@return Extract<T>
            function extract(value) end

            ---@type Box<string>|number
            local source

            value = extract(source)
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "(string|number)");
    }

    #[test]
    fn test_conditional_infer_pattern_union_does_not_use_naked_fallback_after_structural_match() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Box<T>
            ---@alias ExtractA<T> T extends (Box<infer A>|infer B) and A or unknown
            ---@alias ExtractB<T> T extends (Box<infer A>|infer B) and B or unknown

            ---@generic T
            ---@param value T
            ---@return ExtractA<T>
            function extract_a(value) end

            ---@generic T
            ---@param value T
            ---@return ExtractB<T>
            function extract_b(value) end

            ---@type Box<string>
            local source

            a = extract_a(source)
            b = extract_b(source)
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let b_ty = ws.expr_ty("b");
        assert_eq!(ws.humanize_type(a_ty), "string");
        assert_eq!(ws.humanize_type(b_ty), "unknown");
    }

    #[test]
    fn test_generic_identity_table_literal_still_allows_later_field_inference() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@generic T
            ---@param value T
            ---@return T
            function identity(value) end

            local result = identity({})
            result.name = "abc"
            value = result.name
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "\"abc\"");
    }

    #[test]
    fn test_generic_identity_object_literal_still_allows_later_field_inference() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@generic T
            ---@param value T
            ---@return T
            function identity(value) end

            result = identity({ name = "abc" })
            "#,
        );
        ws.def(
            r#"
            function add_field()
                result.age = 1
            end
            value = result.age
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "1");
    }

    #[test]
    fn test_generic_identity_object_literal_preserves_raw_table_when_structural_field_fails() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@generic T
            ---@param value T
            ---@return T
            function identity(value) end

            ---@type unknown
            local source

            result = identity({ name = source.missing })
            "#,
        );
        ws.def(
            r#"
            function add_field()
                result.age = 1
            end
            value = result.age
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "1");
    }

    #[test]
    fn test_generic_identity_object_literal_preserves_raw_table_when_structural_key_fails() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@generic T
            ---@param value T
            ---@return T
            function identity(value) end

            result = identity({ [missing_key] = "abc" })
            "#,
        );
        ws.def(
            r#"
            function add_field()
                result.age = 1
            end
            value = result.age
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "1");
    }

    #[test]
    fn test_generic_nullable_identity_object_literal_still_allows_later_field_inference() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@generic T
            ---@param value T
            ---@return T?
            function identity(value) end

            result = identity({ name = "abc" })
            "#,
        );
        ws.def(
            r#"
            function add_field()
                result.age = 1
            end
            value = result.age
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "1");
    }

    #[test]
    fn test_generic_nullable_param_identity_object_literal_still_allows_later_field_inference() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@generic T
            ---@param value T?
            ---@return T?
            function identity(value) end

            result = identity({ name = "abc" })
            "#,
        );
        ws.def(
            r#"
            function add_field()
                result.age = 1
            end
            value = result.age
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "1");
    }

    #[test]
    fn test_alias_generic_does_not_replace_shadowed_mapped_key() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias Shadow<T> { [T in "name"]: T; }

            ---@type Shadow<number>
            local shadow

            shadowName = shadow.name
            "#,
        );

        let shadow_name_ty = ws.expr_ty("shadowName");
        assert_eq!(ws.humanize_type(shadow_name_ty), "string");
    }

    #[test]
    fn test_conditional_infer_requires_generic_base_match() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Schema<T>
            ---@field parse fun(self: Schema<T>, value: unknown): T
            ---@class ObjectSchema<T>: Schema<T>
            ---@class ArraySchema<T>: Schema<T[]>
            ---@class OptionalSchema<T>: Schema<T>

            ---@alias InferShape<T> { [K in keyof T]: T[K] extends ArraySchema<infer U> and U[] or T[K] extends ObjectSchema<infer U> and U or T[K] extends OptionalSchema<infer U> and U or T[K] extends Schema<infer U> and U or unknown; }

            ---@return Schema<string>
            function mk_string() end

            ---@return Schema<number>
            function mk_number() end

            ---@return Schema<boolean>
            function mk_boolean() end

            ---@generic T
            ---@param schema T
            ---@return ObjectSchema<InferShape<T>>
            function object(schema) end

            ---@generic T
            ---@param schema Schema<T>
            ---@return ArraySchema<T>
            function array(schema) end

            ---@generic T
            ---@param schema Schema<T>
            ---@return OptionalSchema<T|nil>
            function optional(schema) end

            ---@generic T
            ---@param schema T
            ---@return InferShape<T>
            function infer_shape(schema) end

            schema = object({
                name = mk_string(),
                age = mk_number(),
                admin = mk_boolean(),
                tags = array(mk_string()),
                profile = object({
                    id = mk_string(),
                    score = optional(mk_number()),
                }),
            })

            parsed = schema:parse({})
            "#,
        );

        let name_ty = ws.expr_ty("parsed.name");
        let age_ty = ws.expr_ty("parsed.age");
        let admin_ty = ws.expr_ty("parsed.admin");
        let profile_ty = ws.expr_ty("parsed.profile");
        let id_ty = ws.expr_ty("parsed.profile.id");
        let score_ty = ws.expr_ty("parsed.profile.score");
        let tags_ty = ws.expr_ty("parsed.tags");
        assert_eq!(ws.humanize_type(name_ty), "string");
        assert_eq!(ws.humanize_type(age_ty), "number");
        assert_eq!(ws.humanize_type(admin_ty), "boolean");
        assert_eq!(
            ws.humanize_type(profile_ty),
            "{ id: string, score: number? }"
        );
        assert_eq!(ws.humanize_type(id_ty), "string");
        assert_eq!(ws.humanize_type(score_ty), "number?");
        assert_eq!(ws.humanize_type(tags_ty), "string[]");
    }

    #[test]
    fn test_builder_namespace_method_chain_and_variable_shape_infer_parse_result() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Schema<T>
            ---@field parse fun(self: Schema<T>, value: unknown): T
            ---@field optional fun(self: Schema<T>): OptionalSchema<T|nil>
            ---@class StringSchema: Schema<string>
            ---@class NumberSchema: Schema<number>
            ---@class BooleanSchema: Schema<boolean>
            ---@class ObjectSchema<T>: Schema<T>
            ---@class ArraySchema<T>: Schema<T[]>
            ---@class OptionalSchema<T>: Schema<T>

            ---@alias InferShape<T> { [K in keyof T]: T[K] extends ArraySchema<infer U> and U[] or T[K] extends ObjectSchema<infer U> and U or T[K] extends OptionalSchema<infer U> and U or T[K] extends Schema<infer U> and U or unknown; }

            ---@class Z
            ---@field string fun(): StringSchema
            ---@field number fun(): NumberSchema
            ---@field boolean fun(): BooleanSchema
            ---@field array fun<T>(schema: Schema<T>): ArraySchema<T>
            ---@field object fun<T>(shape: T): ObjectSchema<InferShape<T>>

            ---@type Z
            local z

            local inline_schema = z.object({
                name = z.string(),
                age = z.number(),
                admin = z.boolean(),
                tags = z.array(z.string()),
                profile = z.object({
                    id = z.string(),
                    score = z.number():optional(),
                }),
            })
            parsed_inline = inline_schema:parse({})

            local shape = {
                profile = z.object({
                    id = z.string(),
                    score = z.number():optional(),
                }),
            }
            local variable_schema = z.object(shape)
            parsed_variable = variable_schema:parse({})
            "#,
        );

        let inline_name_ty = ws.expr_ty("parsed_inline.name");
        let inline_age_ty = ws.expr_ty("parsed_inline.age");
        let inline_admin_ty = ws.expr_ty("parsed_inline.admin");
        let inline_tags_ty = ws.expr_ty("parsed_inline.tags");
        let inline_profile_ty = ws.expr_ty("parsed_inline.profile");
        let inline_id_ty = ws.expr_ty("parsed_inline.profile.id");
        let inline_score_ty = ws.expr_ty("parsed_inline.profile.score");
        let variable_id_ty = ws.expr_ty("parsed_variable.profile.id");
        let variable_score_ty = ws.expr_ty("parsed_variable.profile.score");
        assert_eq!(ws.humanize_type(inline_name_ty), "string");
        assert_eq!(ws.humanize_type(inline_age_ty), "number");
        assert_eq!(ws.humanize_type(inline_admin_ty), "boolean");
        assert_eq!(ws.humanize_type(inline_tags_ty), "string[]");
        assert_eq!(
            ws.humanize_type(inline_profile_ty),
            "{ id: string, score: number? }"
        );
        assert_eq!(ws.humanize_type(inline_id_ty), "string");
        assert_eq!(ws.humanize_type(inline_score_ty), "number?");
        assert_eq!(ws.humanize_type(variable_id_ty), "string");
        assert_eq!(ws.humanize_type(variable_score_ty), "number?");
    }

    #[test]
    fn test_conditional_infer_handles_cyclic_concrete_supers() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Target<T>
            ---@class A: B
            ---@class B: A

            ---@alias Extract<T> T extends Target<infer U> and U or unknown

            ---@generic T
            ---@param value T
            ---@return Extract<T>
            function extract(value) end

            ---@type A
            local source

            value = extract(source)
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "unknown");
    }

    #[test]
    fn test_conditional_infer_handles_cyclic_generic_supers() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Target<T>
            ---@class A<T>: B<T>
            ---@class B<T>: A<T>

            ---@alias Extract<T> T extends Target<infer U> and U or unknown

            ---@generic T
            ---@param value T
            ---@return Extract<T>
            function extract(value) end

            ---@type A<string>
            local source

            value = extract(source)
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "unknown");
    }

    #[test]
    fn test_conditional_infer_function_return_uses_independent_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Target<T>
            ---@class Source<T>: Target<T>

            ---@alias ExtractArg<F> F extends (fun(...: Target<infer A>): Target<infer B>) and A or unknown
            ---@alias ExtractRet<F> F extends (fun(...: Target<infer A>): Target<infer B>) and B or unknown

            ---@generic F
            ---@param f F
            ---@return ExtractArg<F>
            function extract_arg(f) end

            ---@generic F
            ---@param f F
            ---@return ExtractRet<F>
            function extract_ret(f) end

            ---@param ... Source<string>
            ---@return Source<number>
            function fn(...) end

            arg = extract_arg(fn)
            ret = extract_ret(fn)
            "#,
        );

        let arg_ty = ws.expr_ty("arg");
        let ret_ty = ws.expr_ty("ret");
        assert_eq!(ws.humanize_type(arg_ty), "string");
        assert_eq!(ws.humanize_type(ret_ty), "number");
    }

    #[test]
    fn test_conditional_infer_instantiates_nested_generic_super_with_decl_tpl_id() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Pair<A, B>

            ---@generic Outer
            function setup()
                ---@class Nested<Inner>: Pair<Outer, Inner>
            end

            ---@alias ExtractInner<T> T extends Pair<any, infer U> and U or unknown

            ---@type ExtractInner<Nested<string>>
            value = nil
            "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param value number
            function take_number(value) end

            take_number(value)
            "#,
        ));
    }

    #[test]
    fn test_nested_generic_super_member_instantiates_decl_tpl_id() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Base<T>
            ---@field value T

            ---@generic Outer
            function setup()
                ---@class Nested<Inner>: Base<Inner>
            end

            ---@type Nested<string>
            local nested

            value = nested.value
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert_eq!(ws.humanize_type(value_ty), "string");
    }

    #[test]
    fn test_infer_new_constructor() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias ConstructorParameters<T> T extends new (fun(...: infer P): any) and P or never

            ---@generic T
            ---@param name `T`|T
            ---@param ... ConstructorParameters<T>...
            function f(name, ...)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class A
            ---@overload fun(name: string, age: number)
            local A = {}

            f(A, "b", 1)
            f("A", "b", 1)

            "#,
        ));
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            f("A", "b", "1")
            "#,
        ));
    }

    #[test]
    fn test_variadic_base() {
        let mut ws = VirtualWorkspace::new();
        {
            ws.def(
                r#"
            ---@generic T
            ---@param ... T... # 所有传入参数合并为一个`可变序列`, 即(T1, T2, ...)
            ---@return T # 返回可变序列
            function f1(...) end
            "#,
            );
            assert!(ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
              A, B, C =  f1(1, "2", true)
            "#,
            ));
            assert_eq!(ws.expr_ty("A"), ws.ty("integer"));
            assert_eq!(ws.expr_ty("B"), ws.ty("string"));
            assert_eq!(ws.expr_ty("C"), ws.ty("boolean"));
        }
        {
            ws.def(
                r#"
                ---@generic T
                ---@param ... T...
                ---@return T... # `...`的作用是转换类型为序列, 此时 T 为序列, 那么 T... = T
                function f2(...) end
            "#,
            );
            assert!(ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
              D, E, F =  f2(1, "2", true)
            "#,
            ));
            assert_eq!(ws.expr_ty("D"), ws.ty("integer"));
            assert_eq!(ws.expr_ty("E"), ws.ty("string"));
            assert_eq!(ws.expr_ty("F"), ws.ty("boolean"));
        }

        {
            ws.def(
                r#"
            ---@generic T
            ---@param ... T # T为单类型, `@param ... T`在语义上等同于 TS 的 T[]
            ---@return T # 返回一个单类型
            function f3(...) end
            "#,
            );
            assert!(!ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
              G, H =  f3(1, "2")
            "#,
            ));
            assert_eq!(ws.expr_ty("G"), ws.ty("integer"));
            assert_eq!(ws.expr_ty("H"), ws.ty("any"));
        }

        {
            ws.def(
                r#"
            ---@generic T
            ---@param ... T # T为单类型
            ---@return T... # 将单类型转为可变序列返回, 即返回了(T, T, T, ...)
            function f4(...) end
            "#,
            );
            assert!(!ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
              I, J, K =  f4(1, "2")
            "#,
            ));
            assert_eq!(ws.expr_ty("I"), ws.ty("integer"));
            assert_eq!(ws.expr_ty("J"), ws.ty("integer"));
            assert_eq!(ws.expr_ty("K"), ws.ty("integer"));
        }
    }

    #[test]
    fn test_long_extends_1() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias IsTypeGuard<T>
            --- T extends "nil"
            ---     and nil
            ---     or T extends "number"
            ---         and number
            ---         or T

            ---@param v number
            function f(v)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type IsTypeGuard<"number">
            local a
            f(a)
            "#,
        ));
    }

    #[test]
    fn test_long_extends_2() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias std.type
            ---| "nil"
            ---| "number"
            ---| "string"
            ---| "boolean"
            ---| "table"
            ---| "function"
            ---| "thread"
            ---| "userdata"

            ---@alias TypeGuard<T> boolean
        "#,
        );

        ws.def(
            r#"
            ---@alias IsTypeGuard<T>
            --- T extends "nil"
            ---     and nil
            ---     or T extends "number"
            ---         and number
            ---         or T

            ---@param v number
            function f(v)
            end

            ---@generic TP: std.type
            ---@param obj any
            ---@param tp std.ConstTpl<TP>
            ---@return TypeGuard<IsTypeGuard<TP>>
            function is_type(obj, tp)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local a
            if is_type(a, "number") then
                f(a)
            end
            "#,
        ));
    }

    #[test]
    fn test_issue_846() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@alias Parameters<T extends function> T extends (fun(...: infer P): any) and P or never

            ---@param x number
            ---@param y number
            ---@return number
            function pow(x, y) end

            ---@generic F
            ---@param f F
            ---@return Parameters<F>
            function return_params(f) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            result = return_params(pow)
            "#,
        ));
        let result_ty = ws.expr_ty("result");
        assert_eq!(ws.humanize_type(result_ty), "(number,number)");
    }

    #[test]
    fn test_overload() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Expect
            ---@overload fun<T>(actual: T): T
            local expect = {}

            result = expect("")
            "#,
        ));
        let result_ty = ws.expr_ty("result");
        assert_eq!(ws.humanize_type(result_ty), "string");
    }

    #[test]
    fn test_generic_default_constraint_used() {
        let mut ws = VirtualWorkspace::new();
        {
            ws.def(
                r#"
            ---@generic T: number
            ---@return T
            local function use()
            end

            result = use()
            "#,
            );

            let result_ty = ws.expr_ty("result");
            assert_eq!(result_ty, ws.ty("number"));
        }
        // 类的默认泛型约束暂时不支持
        // {
        //     ws.def(
        //         r#"
        //     ---@class A<T: number>
        //     local A = {}

        //     ---@return T
        //     function A:use()
        //     end

        //     ---@type A<number>
        //     local a

        //     resultA = a:use()
        //     "#,
        //     );

        //     let result_ty = ws.expr_ty("resultA");
        //     assert_eq!(result_ty, ws.ty("number"));
        // }
    }

    #[test]
    fn test_generic_extends_function_params() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias ConstructorParameters<T> T extends new (fun(...: infer P): any) and P or never

            ---@alias Parameters<T extends function> T extends (fun(...: infer P): any) and P or never

            ---@alias ReturnType<T extends function> T extends (fun(...: any): infer R) and R or any

            ---@alias Procedure fun(...: any[]): any

            ---@alias MockParameters<T> T extends Procedure and Parameters<T> or never

            ---@alias MockReturnType<T> T extends Procedure and ReturnType<T> or never

            ---@class Mock<T>
            ---@field calls MockParameters<T>[]
            ---@overload fun(...: MockParameters<T>...): MockReturnType<T>
            "#,
        );
        {
            ws.def(
                r#"
                ---@generic T: Procedure
                ---@param a T
                ---@return Mock<T>
                local function fn(a)
                end

                local sum = fn(function(a, b)
                    return a + b
                end)
                A = sum
            "#,
            );

            let result_ty = ws.expr_ty("A");
            assert_eq!(
                ws.humanize_type_detailed(result_ty),
                "Mock<fun(a, b) -> any>"
            );
        }

        {
            ws.def(
                r#"
                ---@generic T: Procedure
                ---@param a T?
                ---@return Mock<T>
                local function fn(a)
                end

                result = fn().calls
            "#,
            );

            let result_ty = ws.expr_ty("result");
            assert_eq!(ws.humanize_type(result_ty), "any[][]");
        }
    }

    #[test]
    fn test_constant_decay() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias std.RawGet<T, K> unknown

            ---@alias std.ConstTpl<T> unknown

            ---@generic T, K extends keyof T
            ---@param object T
            ---@param key K
            ---@return std.RawGet<T, K>
            function pick(object, key)
            end

            ---@class Person
            ---@field age integer
        "#,
        );

        ws.def(
            r#"
            ---@type Person
            local person

            result = pick(person, "age")
        "#,
        );

        let result_ty = ws.expr_ty("result");
        assert_eq!(ws.humanize_type(result_ty), "integer");
    }

    #[test]
    fn test_extends_true() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::TypeNotFound,
            r#"
            ---@alias TestA<T> T extends "test" and number or string
            ---@alias TestB<T> T extends true and number or string
            ---@alias TestC<T> T extends 111 and number or string
            "#,
        ));
    }
}
