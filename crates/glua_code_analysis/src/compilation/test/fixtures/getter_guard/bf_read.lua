---@meta

--- This is the object passed to [usermessage.Hook](https://wiki.facepunch.com/gmod/usermessage.Hook) when a message is received. It contains each value stored in the message in sequential order. You should read values from it in the order you wrote them.
---@source https://wiki.facepunch.com/gmod/bf_read
---@deprecated You should be using net instead
---@class (partial) bf_read
local bf_read = {}

---Reads and returns an angle object from the bitstream.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadAngle
---@return Angle # The read angle
---@[fresh_result]
function bf_read:ReadAngle() end


---Reads 1 bit and returns a bool representing the bit.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadBool
---@return boolean # bit
---@[fresh_result]
function bf_read:ReadBool() end


---Reads a signed char and returns a number from -127 to 127 representing the ascii value of that char.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadChar
---@return number # asciiVal
---@[fresh_result]
function bf_read:ReadChar() end


---Reads a short representing an entity index and returns the matching entity handle.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadEntity
---@return Entity # ent
---@[fresh_result]
function bf_read:ReadEntity() end


---Reads a 4 byte float from the bitstream and returns it.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadFloat
---@return number # float
---@[fresh_result]
function bf_read:ReadFloat() end


---Reads a 4 byte long from the bitstream and returns it.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadLong
---@return number # int
---@[fresh_result]
function bf_read:ReadLong() end


---Reads a 2 byte short from the bitstream and returns it.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadShort
---@return number # short
---@[fresh_result]
function bf_read:ReadShort() end


---Reads a null terminated string from the bitstream.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadString
---@return string # str
---@[fresh_result]
function bf_read:ReadString() end


---Reads a special encoded vector from the bitstream and returns it, this function is not suitable to send normals.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadVector
---@return Vector # vec
---@[fresh_result]
function bf_read:ReadVector() end


---Reads a special encoded vector normal from the bitstream and returns it, this function is not suitable to send vectors that represent a position.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:ReadVectorNormal
---@return Vector # normal
---@[fresh_result]
function bf_read:ReadVectorNormal() end


---Rewinds the bitstream so it can be read again.
---@realm client
---@source https://wiki.facepunch.com/gmod/bf_read:Reset
function bf_read:Reset() end
