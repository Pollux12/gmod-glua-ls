use rowan::TextSize;

#[derive(Debug, Clone)]
pub struct LineIndex {
    line_offsets: Vec<TextSize>,
    line_only_ascii_vec: Vec<bool>,
}

impl LineIndex {
    pub fn parse(text: &str) -> LineIndex {
        let mut line_offsets = Vec::new();
        let mut line_only_ascii_vec = Vec::new();
        let mut offset = 0;

        line_offsets.push(TextSize::from(offset as u32));

        let mut is_line_only_ascii = true;
        for (i, c) in text.char_indices() {
            if c == '\n' {
                offset = i + 1; // 记录每行的字节偏移量
                line_offsets.push(TextSize::from(offset as u32));
                line_only_ascii_vec.push(is_line_only_ascii);
                is_line_only_ascii = true;
            } else if !c.is_ascii() {
                is_line_only_ascii = false;
            }
        }

        line_only_ascii_vec.push(is_line_only_ascii);

        assert_eq!(line_offsets.len(), line_only_ascii_vec.len());
        line_offsets.shrink_to_fit();
        line_only_ascii_vec.shrink_to_fit();
        LineIndex {
            line_offsets,
            line_only_ascii_vec,
        }
    }

    pub fn get_line_offset(&self, line: usize) -> Option<TextSize> {
        let line_index = line;
        if line_index < self.line_offsets.len() {
            let line_offset = self.line_offsets[line_index];
            Some(line_offset)
        } else {
            None
        }
    }

    // get line base 0
    pub fn get_line(&self, offset: TextSize) -> Option<usize> {
        let offset_value = usize::from(offset);
        match self
            .line_offsets
            .binary_search(&TextSize::from(offset_value as u32))
        {
            Ok(line) => Some(line),
            Err(line) => {
                if line > 0 {
                    Some(line - 1)
                } else {
                    None
                }
            }
        }
    }

    pub fn get_line_with_start_offset(&self, offset: TextSize) -> Option<(usize, TextSize)> {
        let line = self.get_line(offset)?;
        let start_offset = self.line_offsets[line];
        Some((line, start_offset))
    }

    fn is_line_only_ascii(&self, line: usize) -> bool {
        self.line_only_ascii_vec.get(line).copied().unwrap_or(false)
    }

    pub fn line_count(&self) -> usize {
        self.line_offsets.len()
    }

    /// Byte range of `line` without its line terminator.
    fn line_content_range(&self, line: usize, source_text: &str) -> Option<(usize, usize)> {
        let start = usize::from(*self.line_offsets.get(line)?);
        let end = match self.line_offsets.get(line + 1) {
            Some(next) => {
                let content = &source_text[start..usize::from(*next) - 1];
                start + content.strip_suffix('\r').unwrap_or(content).len()
            }
            None => source_text.len(),
        };
        Some((start, end))
    }

    /// UTF-16 column (the LSP default position encoding) of `offset`, base 0.
    pub fn get_col(&self, offset: TextSize, source_text: &str) -> Option<usize> {
        self.get_line_col(offset, source_text).map(|(_, col)| col)
    }

    /// Line and UTF-16 column of `offset`, both base 0.
    pub fn get_line_col(&self, offset: TextSize, source_text: &str) -> Option<(usize, usize)> {
        let (line, start_offset) = self.get_line_with_start_offset(offset)?;
        let col = if self.is_line_only_ascii(line) {
            usize::from(offset - start_offset)
        } else {
            source_text[usize::from(start_offset)..usize::from(offset)]
                .encode_utf16()
                .count()
        };
        Some((line, col))
    }

    /// Offset of a line and UTF-16 column. A column past the line end clamps to the line end
    /// (before its terminator); a line past the last line clamps to the end of the text.
    pub fn get_offset(&self, line: usize, col: usize, source_text: &str) -> TextSize {
        self.offset_at_column(line, col, source_text, char::len_utf16)
    }

    /// Offset of a line and Unicode scalar column, clamped like [`Self::get_offset`].
    pub fn get_offset_by_char_col(&self, line: usize, col: usize, source_text: &str) -> TextSize {
        self.offset_at_column(line, col, source_text, |_| 1)
    }

    /// Offset of a UTF-16 column relative to the start of `line`, clamped to the line end.
    pub fn get_col_offset_at_line(
        &self,
        line: usize,
        col: usize,
        source_text: &str,
    ) -> Option<TextSize> {
        let start_offset = self.get_line_offset(line)?;
        Some(self.get_offset(line, col, source_text) - start_offset)
    }

    fn offset_at_column(
        &self,
        line: usize,
        col: usize,
        source_text: &str,
        width: fn(char) -> usize,
    ) -> TextSize {
        let Some((start, end)) = self.line_content_range(line, source_text) else {
            return TextSize::of(source_text);
        };
        let offset = if self.is_line_only_ascii(line) {
            start + col.min(end - start)
        } else {
            let mut units = 0;
            source_text[start..end]
                .char_indices()
                .find_map(|(i, c)| {
                    units += width(c);
                    (units > col).then_some(start + i)
                })
                .unwrap_or(end)
        };
        TextSize::from(offset as u32)
    }
}
