const KEYWORD_DOCS: &[(&str, &str)] = &[
    (
        "for",
        r#"The `for` keyword is used to create a loop that can iterate over a range, collection, or iterator.

### Example Usage

```lua
-- Iterate over a range
for i = 1, 10 do
    print(i)
end

-- Iterate over a collection
local fruits = {"apple", "banana", "cherry"}
for index, fruit in ipairs(fruits) do
    print(index, fruit)
end
```"#,
    ),
    (
        "if",
        r#"The `if` keyword is used for conditional statements, executing different code blocks based on the truthiness of the condition.

### Example Usage

```lua
local x = 10
if x > 5 then
    print("x is greater than 5")
elseif x == 5 then
    print("x is equal to 5")
else
    print("x is less than 5")
end
```"#,
    ),
    (
        "while",
        r#"The `while` keyword is used to create a loop that repeats as long as the condition is true.

### Example Usage

```lua
local i = 1
while i <= 10 do
    print(i)
    i = i + 1
end
```"#,
    ),
    (
        "function",
        r#"The `function` keyword is used to define a function, which can contain a set of instructions and can be called.

### Example Usage

```lua
function greet(name)
    print("Hello, " .. name)
end

greet("world")
```"#,
    ),
    (
        "local",
        r#"The `local` keyword is used to declare local variables or functions, which are limited to the scope of the block.

### Example Usage

```lua
local x = 10
local function add(a, b)
    return a + b
end

print(add(x, 5))
```"#,
    ),
    (
        "return",
        r#"The `return` keyword is used to return values from a function and terminate the function's execution.

### Example Usage

```lua
function add(a, b)
    return a + b
end

local sum = add(5, 3)
print(sum)  -- Output 8
```"#,
    ),
    (
        "break",
        r#"The `break` keyword is used to exit the current loop.

### Example Usage

```lua
local i = 1
while i <= 10 do
    if i == 5 then
        break
    end
    print(i)
    i = i + 1
end
-- Output 1 to 4
```"#,
    ),
    (
        "do",
        r#"The `do` keyword is used to create a block, where the variables inside the block are local.

### Example Usage

```lua
local x = 10
do
    local x = 5
    print(x)  -- Output 5
end
print(x)  -- Output 10
```"#,
    ),
    (
        "end",
        r#"The `end` keyword is used to end a block, function, or control structure.

### Example Usage

```lua
if true then
    print("This is true")
end
```"#,
    ),
    (
        "repeat",
        r#"The `repeat` keyword is used to create a loop that ends when the condition is true.

### Example Usage

```lua
local i = 1
repeat
    print(i)
    i = i + 1
until i > 5
-- Output 1 to 5
```"#,
    ),
    (
        "until",
        r#"The `until` keyword is used in a `repeat` loop to indicate the end condition of the loop.

### Example Usage

```lua
local i = 1
repeat
    print(i)
    i = i + 1
until i > 5
-- Output 1 to 5
```"#,
    ),
    (
        "then",
        r#"The `then` keyword is used in an `if` statement to indicate the code block to execute when the condition is true.

### Example Usage

```lua
local x = 10
if x > 5 then
    print("x is greater than 5")
end
```"#,
    ),
    (
        "elseif",
        r#"The `elseif` keyword is used in an `if` statement to indicate another condition to check.

### Example Usage

```lua
local x = 10
if x > 5 then
    print("x is greater than 5")
elseif x == 5 then
    print("x is equal to 5")
end
```"#,
    ),
    (
        "in",
        r#"The `in` keyword is used in a generic `for` loop to indicate the collection or iterator to iterate over.

### Example Usage

```lua
local fruits = {"apple", "banana", "cherry"}
for index, fruit in ipairs(fruits) do
    print(index, fruit)
end
```"#,
    ),
    (
        "goto",
        r#"The `goto` keyword is used to jump to a label in the code.

### Example Usage

```lua
::label::
print("Hello")
goto label
```"#,
    ),
];

const TAG_DOCS: &[(&str, &str)] = &[
    (
        "class",
        r#"The `class` tag is used to document a class or a struct.
Example:
```lua
---@class MyClass
local MyClass = {}
```"#,
    ),
    (
        "enum",
        r#"The `enum` tag is used to document an enumeration.
Example:
```lua
---@enum MyEnum
local MyEnum = {
  Value1 = 1,
  Value2 = 2
}
```"#,
    ),
    (
        "interface",
        r#"The `interface` is deprecated, use `class` instead.
Example:
```lua
---@interface MyInterface
local MyInterface = {}
```"#,
    ),
    (
        "alias",
        r#"The `alias` tag is used to document a type alias.
Example:
```lua
---@alias MyTypeAlias string|number
```"#,
    ),
    (
        "field",
        r#"The `field` tag is used to document a field of a class or a struct.
Example:
```lua
---@class MyClass
---@field publicField string
MyClass = {}
```"#,
    ),
    (
        "type",
        r#"The `type` tag is used to document a type.
Example:
```lua
---@type string
local myString = "Hello"
```"#,
    ),
    (
        "param",
        r#"The `param` tag is used to document a function parameter.
Example:
```lua
---@param paramName string
function myFunction(paramName)
end
```"#,
    ),
    (
        "return",
        r#"The `return` tag is used to document the return value of a function.
Example:
```lua
---@return string
function myFunction()
  return "Hello"
end
```"#,
    ),
    (
        "generic",
        r#"The `generic` tag is used to document generic types.
Example:
```lua
---@generic T
---@param param T
---@return T
function identity(param)
  return param
end
```"#,
    ),
    (
        "see",
        r#"The `see` tag is used to reference another documentation entry.
Example:
```lua
---@see otherFunction
function myFunction()
end
```"#,
    ),
    (
        "deprecated",
        r#"The `deprecated` tag is used to mark a function or a field as deprecated.
Example:
```lua
---@deprecated
function oldFunction()
end
```"#,
    ),
    (
        "cast",
        r#"The `cast` tag is used to document a type cast.
Example:
```lua
---@cast varName string
local varName = someValue
```"#,
    ),
    (
        "overload",
        r#"The `overload` tag is used to document an overloaded function.
Example:
```lua
---@overload fun(param: string):void
function myFunction(param)
end
```"#,
    ),
    (
        "async",
        r#"The `async` tag is used to document an asynchronous function.
Example:
```lua
---@async
function asyncFunction()
end
```"#,
    ),
    (
        "public",
        r#"The `public` tag is used to mark a field or a function as public.
Example:
```lua
---@public
MyClass.publicField = ""
```"#,
    ),
    (
        "protected",
        r#"The `protected` tag is used to mark a field or a function as protected.
Example:
```lua
---@protected
MyClass.protectedField = ""
```"#,
    ),
    (
        "private",
        r#"The `private` tag is used to mark a field or a function as private.
Example:
```lua
---@private
local privateField = ""
```"#,
    ),
    (
        "package",
        r#"The `package` tag is used to document a package.
Example:
```lua
---@package
local myPackage = {}
```"#,
    ),
    (
        "meta",
        r#"The `meta` tag is used to document meta information.
Example:
```lua
---@meta
local metaInfo = {}
```"#,
    ),
    (
        "diagnostic",
        r#"The `diagnostic` tag is used to document diagnostic information.
Example:
```lua
---@diagnostic disable-next-line: unused-global
local unusedVar = 1
```"#,
    ),
    (
        "version",
        r#"The `version` tag is used to document the version of a module or a function.
Example:
```lua
---@version 1.0
function myFunction()
end
```"#,
    ),
    (
        "as",
        r#"The `as` tag is used to document type assertions.
Example:
```lua
---@as string
local varName = someValue
```"#,
    ),
    (
        "nodiscard",
        r#"The `nodiscard` tag is used to indicate that the return value should not be discarded.
Example:
```lua
---@nodiscard
function importantFunction()
  return "Important"
end
```"#,
    ),
    (
        "operator",
        r#"The `operator` tag is used to document operator overloads.
Example:
```lua
---@class
---@operator add(MyClass):MyClass
```"#,
    ),
    (
        "module",
        r#"The `module` tag is used to document a module.
Example:
```lua
---@module MyModule
local MyModule = {}
```"#,
    ),
    (
        "namespace",
        r#"The `namespace` tag is used to document a namespace.
Example:
```lua
---@namespace MyNamespace
```"#,
    ),
    (
        "using",
        r#"The `using` tag is used to document using declarations.
Example:
```lua
---@using MyNamespace
```"#,
    ),
    (
        "source",
        r#"The `source` tag is used to document the source of a function or a module.
Example:
```lua
---@source https://example.com/source
function myFunction()
end
```"#,
    ),
    (
        "readonly",
        r#"The `readonly` tag is used to mark a field as read-only.
but it is not supported in current
Example:
```lua
---@readonly
MyClass.readonlyField = "constant"
```"#,
    ),
    (
        "export",
        r#"The `export` tag is used to indicate that a variable is exported, supporting quick import.
It accepts `namespace` or `global` as parameters. If no parameter is provided, it defaults to `global`.
Example:
```lua
---@export namespace -- When set to `namespace`, only allows import within the same namespace
local export = {}

export.func = function()
  -- When typing `func` in other files, import suggestions will be shown
end

return export
```"#,
    ),
    (
        "language",
        r#"The `language` tag is used to specify language injection for code blocks.
Example:
```lua
---@language sql
local t = [[
    SELECT * FROM users WHERE id = 1;
    SELECT name, email FROM users WHERE active = 1;
    UPDATE users SET last_login = CURRENT_TIMESTAMP WHERE id = 1;
    DELETE FROM users WHERE id = 2;
    INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com');
]]
```"#,
    ),
    (
        "attribute",
        r#"`attribute` tag defines an attribute. Attribute is used to attach extra information to a definition.
Example:
```lua
---@attribute deprecated(message: string?)

---@class A
---@[deprecated("delete")] # `b` field is marked as deprecated
---@field b string
---@[deprecated] # If `attribute` allows no parameters, the parentheses can be omitted
---@field c string
```"#,
    ),
    (
        "hook",
        r#"The `hook` tag registers a function as a GMod hook.
Example:
```lua
---@hook PlayerSpawn
```"#,
    ),
    (
        "realm",
        r#"The `realm` tag indicates which realm(s) this field/function is available in (client, server, or shared).
Example:
```lua
---@realm client
```"#,
    ),
    (
        "fileparam",
        r#"The `fileparam` tag provides file-scoped default types for unannotated function parameters holding a specific name.
Example:
```lua
---@fileparam ply Player
```"#,
    ),
];

pub fn meta_keyword(key: &str) -> String {
    KEYWORD_DOCS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}

pub fn meta_doc_tag(key: &str) -> String {
    TAG_DOCS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}
