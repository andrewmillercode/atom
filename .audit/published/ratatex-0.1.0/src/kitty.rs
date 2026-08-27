use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};

const PLACEHOLDER: char = '\u{10eeee}';
const MAX_GRID_DIMENSION: u16 = 297;
const BASE64_CHUNK: usize = 4096;

/// Returns whether a Ratatui cell symbol is a Kitty graphics placeholder.
///
/// Applications can use this to switch a clicked [`crate::FormulaWidget`] to
/// its source fallback before starting a normal text selection.
pub fn is_formula_placeholder(symbol: &str) -> bool {
    symbol.starts_with(PLACEHOLDER)
}

pub(crate) fn max_grid_dimension() -> u16 {
    MAX_GRID_DIMENSION
}

pub(crate) fn upload_and_place(
    png: &[u8],
    image_id: u32,
    columns: u16,
    rows: u16,
    tmux: bool,
) -> Arc<[u8]> {
    let encoded = STANDARD.encode(png);
    let mut output = Vec::with_capacity(
        encoded
            .len()
            .saturating_add(encoded.len() / BASE64_CHUNK * 64)
            .saturating_add(128),
    );
    let mut first = true;
    let chunk_count = encoded.len().div_ceil(BASE64_CHUNK);
    for (index, chunk) in encoded.as_bytes().chunks(BASE64_CHUNK).enumerate() {
        let more = u8::from(index + 1 < chunk_count);
        let control = if first {
            first = false;
            format!("a=t,f=100,t=d,i={image_id},q=2,N=1,m={more}")
        } else {
            format!("m={more}")
        };
        write_graphics_command(&mut output, &control, Some(chunk), tmux);
    }
    let placement = format!(
        "a=p,U=1,i={image_id},c={},r={},q=2",
        columns.min(MAX_GRID_DIMENSION),
        rows.min(MAX_GRID_DIMENSION)
    );
    write_graphics_command(&mut output, &placement, None, tmux);
    output.into()
}

fn write_graphics_command(output: &mut Vec<u8>, control: &str, payload: Option<&[u8]>, tmux: bool) {
    if tmux {
        output.extend_from_slice(b"\x1bPtmux;\x1b\x1b_G");
    } else {
        output.extend_from_slice(b"\x1b_G");
    }
    output.extend_from_slice(control.as_bytes());
    if let Some(payload) = payload {
        output.push(b';');
        output.extend_from_slice(payload);
    }
    if tmux {
        output.extend_from_slice(b"\x1b\x1b\\\x1b\\");
    } else {
        output.extend_from_slice(b"\x1b\\");
    }
}

pub(crate) fn placeholder_cells(columns: u16, rows: u16, image_id: u32) -> Vec<Box<str>> {
    let columns = columns.min(MAX_GRID_DIMENSION);
    let rows = rows.min(MAX_GRID_DIMENSION);
    let most_significant_byte = (image_id >> 24) as u8;
    let mut cells = Vec::with_capacity(usize::from(columns).saturating_mul(usize::from(rows)));
    for row in 0..rows {
        for column in 0..columns {
            let mut symbol = String::with_capacity(14);
            symbol.push(PLACEHOLDER);
            symbol.push(diacritic(row));
            symbol.push(diacritic(column));
            if most_significant_byte != 0 {
                symbol.push(diacritic(u16::from(most_significant_byte)));
            }
            cells.push(symbol.into_boxed_str());
        }
    }
    cells
}

fn diacritic(index: u16) -> char {
    DIACRITICS
        .get(usize::from(index))
        .copied()
        .unwrap_or(DIACRITICS[0])
}

// Kitty's normative row/column table:
// https://sw.kovidgoyal.net/kitty/_downloads/1792bad15b12979994cd6ecc54c967a6/rowcolumn-diacritics.txt
static DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

#[cfg(test)]
mod tests {
    use super::{DIACRITICS, is_formula_placeholder, placeholder_cells, upload_and_place};

    #[test]
    fn ships_the_complete_normative_diacritic_table() {
        assert_eq!(DIACRITICS.len(), 297);
    }

    #[test]
    fn placeholders_encode_explicit_rows_and_columns() {
        let cells = placeholder_cells(2, 2, 42);
        assert_eq!(cells.len(), 4);
        assert!(cells[0].starts_with('\u{10eeee}'));
        assert_ne!(cells[0], cells[1]);
        assert_ne!(cells[0], cells[2]);
    }

    #[test]
    fn placeholders_preserve_the_full_image_id() {
        let low_id = placeholder_cells(1, 1, 42);
        let high_id = placeholder_cells(1, 1, 0x0200_002a);

        assert!(is_formula_placeholder(&low_id[0]));
        assert!(!is_formula_placeholder(r"\frac{a}{b}"));
        assert_eq!(low_id[0].chars().count(), 3);
        assert_eq!(high_id[0].chars().count(), 4);
        assert_ne!(low_id[0], high_id[0]);
        assert_eq!(high_id[0].chars().last(), Some(DIACRITICS[2]));
    }

    #[test]
    fn png_commands_transmit_then_create_a_virtual_placement() {
        let command = upload_and_place(b"png", 42, 3, 2, false);
        let text = String::from_utf8_lossy(&command);
        assert!(text.contains("a=t,f=100,t=d,i=42"));
        assert!(text.contains("a=p,U=1,i=42,c=3,r=2"));
        assert!(text.ends_with("\u{1b}\\"));
    }

    #[test]
    fn tmux_commands_escape_inner_apc_sequences() {
        let command = upload_and_place(b"png", 42, 3, 2, true);
        assert!(command.starts_with(b"\x1bPtmux;\x1b\x1b_G"));
        assert!(command.ends_with(b"\x1b\x1b\\\x1b\\"));
    }
}
