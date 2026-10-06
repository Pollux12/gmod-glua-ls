#[cfg(test)]
mod tests {
    use crate::text::LineIndex;
    use rowan::TextSize;

    #[test]
    fn test_line_col() {
        let code = r#"
--hihii
--你好啊
--aiaiai
--1231313 好了好了
        "#;
        let tree = LineIndex::parse(code);
        let offset_1 = tree.get_offset(1, 3, code);
        assert_eq!(offset_1, 4.into());
        let offset_2 = tree.get_offset(2, 4, code);
        assert_eq!(offset_2, 17.into());
        let offset_3 = tree.get_offset(3, 0, code);
        assert_eq!(offset_3, 21.into());

        let line_col_1 = tree.get_line_col(offset_1, code).unwrap();
        assert_eq!(line_col_1, (1, 3));
        let line_col_2 = tree.get_line_col(offset_2, code).unwrap();
        assert_eq!(line_col_2, (2, 4));
        let line_col_3 = tree.get_line_col(offset_3, code).unwrap();
        assert_eq!(line_col_3, (3, 0));
    }

    const EMOJI_LINE: &str = "local s = \"h\u{e9}llo \u{1F642}\" x\nnext";

    fn x_offset() -> TextSize {
        TextSize::from(EMOJI_LINE.find(" x").unwrap() as u32 + 1)
    }

    #[test]
    fn columns_count_utf16_code_units() {
        let index = LineIndex::parse(EMOJI_LINE);
        assert_eq!(index.get_line_col(x_offset(), EMOJI_LINE), Some((0, 21)));
        assert_eq!(index.get_offset(0, 21, EMOJI_LINE), x_offset());
    }

    #[test]
    fn every_char_boundary_round_trips_through_utf16_columns() {
        let index = LineIndex::parse(EMOJI_LINE);
        for (offset, _) in EMOJI_LINE.char_indices() {
            let offset = TextSize::from(offset as u32);
            let (line, col) = index.get_line_col(offset, EMOJI_LINE).unwrap();
            assert_eq!(index.get_offset(line, col, EMOJI_LINE), offset);
        }
    }

    #[test]
    fn a_column_inside_a_surrogate_pair_maps_to_the_start_of_its_char() {
        let index = LineIndex::parse(EMOJI_LINE);
        let emoji = TextSize::from(EMOJI_LINE.find('\u{1F642}').unwrap() as u32);
        assert_eq!(index.get_offset(0, 18, EMOJI_LINE), emoji);
    }

    #[test]
    fn char_columns_count_unicode_scalars() {
        let index = LineIndex::parse(EMOJI_LINE);
        assert_eq!(index.get_offset_by_char_col(0, 20, EMOJI_LINE), x_offset());
    }

    #[test]
    fn a_column_past_the_line_end_clamps_before_the_terminator() {
        for text in ["ab\ncd", "ab\r\ncd"] {
            let index = LineIndex::parse(text);
            assert_eq!(index.get_offset(0, 99, text), TextSize::from(2));
        }
        let text = "\u{e9}\u{1F642}\nx";
        let index = LineIndex::parse(text);
        assert_eq!(index.get_offset(0, 99, text), TextSize::from(6));
        assert_eq!(
            index.get_col_offset_at_line(1, 99, text),
            Some(TextSize::from(1))
        );
    }

    #[test]
    fn positions_past_the_last_line_clamp_to_the_end_of_the_text() {
        let text = "ab\ncd";
        let index = LineIndex::parse(text);
        assert_eq!(index.get_offset(1, 99, text), TextSize::of(text));
        assert_eq!(index.get_offset(7, 0, text), TextSize::of(text));
    }
}
