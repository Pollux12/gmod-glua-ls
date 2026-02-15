# @field - Field Definition

Define fields for classes, supporting access control, optionality, and index signatures.

## Syntax

```lua
-- Named field
---@field [<access_control>] <field_name>[?] <type> [description]

-- Index signature field
---@field [<access_control>] [<key_type>] <value_type> [description]
```

## Access Control Modifiers

- `public` - Public field (default)
- `private` - Private field (only accessible within the class)
- `protected` - Protected field (accessible by class and subclasses)
- `package` - Package field (accessible within the same package)

## Examples

```lua
-- Basic field definition
---@class User
---@field id number User ID
---@field name string Username
---@field email string Email address
---@field createdAt string Creation time

-- Optional fields (using ? marker)
---@class UserProfile
---@field avatar? string Avatar URL (optional)
---@field bio? string Personal bio (optional)
---@field phone? string Phone number (optional)

-- Access control examples
---@class BankAccount
---@field public accountNumber string Account number
---@field public balance number Account balance
---@field private pin string PIN code
---@field protected accountType string Account type
---@field package internalId number Internal ID

-- Index signature fields
---@class Configuration
---@field host string Host address
---@field port number Port number
---@field [string] any Other configuration items (arbitrary string keys)

---@class ScoreBoard
---@field [string] number Mapping from student names to scores

---@class GenericContainer<T>
---@field [number] T Array index access

-- Complex field types
---@class APIResponse
---@field success boolean Whether request succeeded
---@field data table | nil Response data
---@field error string | nil Error message
---@field meta {page: number, limit: number, total: number} Metadata

-- Function type fields
---@class EventEmitter
---@field listeners table<string, fun(...)> Event listener mapping
---@field emit fun(self: EventEmitter, event: string, ...): boolean Emit event
---@field on fun(self: EventEmitter, event: string, listener: fun(...)): nil Register listener

-- Nested class fields
---@class Address
---@field street string Street
---@field city string City
---@field zipCode string ZIP code

---@class Company
---@field name string Company name
---@field headquarters Address Headquarters address
---@field branches Address[] Branch addresses list

-- Usage examples
---@type User
local user = {
    id = 1001,
    name = "John",
    email = "john@example.com",
    createdAt = "2024-01-01"
}

---@type Configuration
local config = {
    host = "localhost",
    port = 8080,
    database = "myapp",  -- Supported through index signature
    cache = true         -- Supported through index signature
}

---@type ScoreBoard
local scores = {
    ["John"] = 95,
    ["Jane"] = 87,
    ["Bob"] = 92
}
```

## Features

1. **Optional field support**
2. **Access control**
3. **Index signatures**
4. **Complex type support**
5. **Nested structures**
