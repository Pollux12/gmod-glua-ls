#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u64)]
pub enum LuaNonStdSymbol {
    DoubleSlash = 1,   // "//"
    SlashStar,         // "/**/"
    Backtick,          // "`"
    PlusAssign,        // "+="
    MinusAssign,       // "-="
    StarAssign,        // "*="
    SlashAssign,       // "/="
    PercentAssign,     // "%="
    CaretAssign,       // "^="
    DoubleSlashAssign, // "//="
    PipeAssign,        // "|="
    AmpAssign,         // "&="
    ShiftLeftAssign,   // "<<="
    ShiftRightAssign,  // ">>="
    DoublePipe,        // "||"
    DoubleAmp,         // "&&"
    Exclamation,       // "!"
    NotEqual,          // "!="
    Continue,          // "continue"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LuaNonStdSymbolSet(u64);

impl Default for LuaNonStdSymbolSet {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaNonStdSymbolSet {
    pub fn new() -> Self {
        LuaNonStdSymbolSet(0)
    }

    pub fn add(&mut self, symbol: LuaNonStdSymbol) {
        self.0 |= 1 << (symbol as u64);
    }

    pub fn extends(&mut self, other: Vec<LuaNonStdSymbol>) {
        for symbol in other {
            self.add(symbol);
        }
    }

    pub fn support(&self, symbol: LuaNonStdSymbol) -> bool {
        self.0 & (1 << (symbol as u64)) != 0
    }
}
