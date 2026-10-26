//! Text into 608 codes: what each character of a caption is sent as.
//!
//! 608 has three sets. The basic set is one byte a character and is what
//! every decoder draws. The special set (`®`, `½`, `♪`, `è` and a few more)
//! is two bytes, a control code in form, and every decoder draws it too. The
//! extended set (accented capitals, curly quotes, box corners) is two bytes
//! that overwrite the character before them, so each goes out after a basic
//! stand-in which a decoder without the set keeps. Anything else is spelled
//! with basic characters where it has an obvious spelling and dropped where
//! it has none.

use cea608_types::tables::{Channel, Code};

/// One character as it goes out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    /// A basic character: one byte, two to a pair.
    Basic(Code),
    /// A special character: a pair of its own, sent twice like a control.
    Special(Code),
    /// An extended character after its stand-in, which it overwrites.
    Extended { stand_in: Code, code: Code },
}

/// The glyphs `c` is sent as, each one column wide. Empty for a character
/// 608 cannot show at all.
pub fn glyphs(c: char, channel: Channel) -> Vec<Glyph> {
    let c = canonical(c);
    if let Some(glyph) = direct(c, channel) {
        return vec![glyph];
    }
    spelled(c)
        .chars()
        .filter_map(|c| match Code::from_char(canonical(c), channel) {
            Some(code) if code.byte_len() == 1 => Some(Glyph::Basic(code)),
            _ => None,
        })
        .collect()
}

/// The basic set's 0x27 is the apostrophe, which the code table spells `’`
/// and gives the ASCII `'` an extended code of its own. A transcript's `'`
/// is an apostrophe, so it goes as the one byte.
fn canonical(c: char) -> char {
    match c {
        '\'' => '’',
        c => c,
    }
}

/// The character in one of 608's own sets, if it is in one.
fn direct(c: char, channel: Channel) -> Option<Glyph> {
    let code = Code::from_char(c, channel)?;
    if code.byte_len() == 1 {
        return Some(Glyph::Basic(code));
    }
    if !code.needs_backspace() {
        return Some(Glyph::Special(code));
    }
    let stand_in = spelled(c)
        .chars()
        .next()
        .and_then(|c| Code::from_char(canonical(c), channel))
        .filter(|code| code.byte_len() == 1)
        .unwrap_or(Code::Space);
    Some(Glyph::Extended { stand_in, code })
}

/// A spelling in plain ASCII: the letter under an accent, straight quotes
/// for curly ones, a hyphen for a dash. Some ASCII is not in the basic set
/// either, because 608 put accented letters in those places (`*` is `á`,
/// `\` is `é`, `_` is `ó`, `{` is `ç`), so those are spelled too.
fn spelled(c: char) -> &'static str {
    match c {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => "A",
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => "a",
        'Ç' => "C",
        'ç' => "c",
        'È' | 'É' | 'Ê' | 'Ë' => "E",
        'è' | 'é' | 'ê' | 'ë' => "e",
        'Ì' | 'Í' | 'Î' | 'Ï' => "I",
        'ì' | 'í' | 'î' | 'ï' => "i",
        'Ñ' => "N",
        'ñ' => "n",
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => "O",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => "o",
        'Ù' | 'Ú' | 'Û' | 'Ü' => "U",
        'ù' | 'ú' | 'û' | 'ü' => "u",
        'Ý' | 'Ÿ' => "Y",
        'ý' | 'ÿ' => "y",
        'ß' => "ss",
        'Æ' => "AE",
        'æ' => "ae",
        'Œ' => "OE",
        'œ' => "oe",
        '‘' | '‚' | '′' | '`' | '´' => "'",
        '“' | '”' | '„' | '″' | '«' | '»' => "\"",
        '–' | '—' | '―' | '−' | '_' | '~' => "-",
        '…' => "...",
        '•' | '·' | '*' => ".",
        '\\' | '|' => "/",
        '{' | '[' => "(",
        '}' | ']' => ")",
        '×' => "x",
        '\u{a0}' | '\t' => " ",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(c: char) -> Glyph {
        Glyph::Basic(Code::from_char(canonical(c), Channel::ONE).expect("basic"))
    }

    #[test]
    fn capitals_digits_and_the_speaker_marks_are_basic() {
        for c in "ABCXYZ0129 >><<.,?!'\"-:;()&%$#@/".chars() {
            assert_eq!(glyphs(c, Channel::ONE), vec![basic(c)], "{c:?}");
        }
    }

    #[test]
    fn the_special_set_goes_as_one_glyph() {
        let g = glyphs('♪', Channel::ONE);
        assert_eq!(g.len(), 1);
        assert!(matches!(g[0], Glyph::Special(_)), "{g:?}");
    }

    #[test]
    fn an_accented_capital_rides_behind_its_letter() {
        let g = glyphs('É', Channel::ONE);
        assert_eq!(
            g,
            vec![Glyph::Extended {
                stand_in: Code::from_char('E', Channel::ONE).unwrap(),
                code: Code::from_char('É', Channel::ONE).unwrap(),
            }]
        );
    }

    #[test]
    fn ascii_that_608_moved_goes_extended_behind_a_stand_in() {
        // `*` is `á` in 608's basic set, so the asterisk is in the extended
        // set, and a decoder without that set shows the stand-in.
        for (c, stand_in) in [('*', '.'), ('_', '-'), ('\\', '/'), ('—', '-'), ('{', '(')] {
            let code = Code::from_char(c, Channel::ONE).unwrap();
            assert_eq!(
                glyphs(c, Channel::ONE),
                vec![Glyph::Extended {
                    stand_in: Code::from_char(stand_in, Channel::ONE).unwrap(),
                    code
                }],
                "{c:?}"
            );
        }
    }

    #[test]
    fn what_608_has_no_code_for_is_spelled() {
        for (c, want) in [('…', "..."), ('`', "'"), ('Œ', "OE")] {
            let got: Vec<Glyph> = glyphs(c, Channel::ONE);
            let want: Vec<Glyph> = want.chars().map(basic).collect();
            assert_eq!(got, want, "{c:?}");
        }
    }

    #[test]
    fn a_character_with_no_spelling_is_dropped() {
        assert!(glyphs('😀', Channel::ONE).is_empty());
        assert!(glyphs('中', Channel::ONE).is_empty());
    }
}
