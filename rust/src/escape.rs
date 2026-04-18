//! Escape-combo parsing and matching.
//!
//! Port of `parse_esc_key_str()` and `register_esc_combo_event()` in
//! [c/src/kloak.c]. See §6 of the behavior matrix.
//!
//! Grammar:
//!
//! ```text
//! combo   = slot ("," slot)*
//! slot    = key ("|" key)*
//! key     = "KEY_<NAME>"   (matches keys::lookup)
//! ```
//!
//! All slots must be simultaneously pressed to trigger an exit. Within a
//! slot, any one of the aliased keys counts as that slot being held.

use crate::keys;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    EmptySlot,
    EmptyAlias,
    UnknownKey(String),
    EmptyInput,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::EmptySlot => f.write_str("empty key name in escape key list"),
            ParseError::EmptyAlias => f.write_str("empty key name in escape key list"),
            ParseError::UnknownKey(n) => write!(f, "unrecognized key name '{n}'"),
            ParseError::EmptyInput => f.write_str("escape key list is empty"),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone)]
pub struct EscCombo {
    /// One inner `Vec<u32>` per comma-separated slot; each inner vec lists the
    /// pipe-separated aliases for that slot. Always non-empty on success.
    slots: Vec<Vec<u32>>,
    /// Active state per slot, updated by `observe`. Same length as `slots`.
    active: Vec<bool>,
}

impl EscCombo {
    pub fn parse(spec: &str) -> Result<Self, ParseError> {
        if spec.is_empty() {
            return Err(ParseError::EmptyInput);
        }
        let mut slots: Vec<Vec<u32>> = Vec::new();
        for slot_tok in spec.split(',') {
            if slot_tok.is_empty() {
                return Err(ParseError::EmptySlot);
            }
            let mut aliases: Vec<u32> = Vec::new();
            for key_tok in slot_tok.split('|') {
                if key_tok.is_empty() {
                    return Err(ParseError::EmptyAlias);
                }
                match keys::lookup(key_tok) {
                    Some(code) => aliases.push(code),
                    None => return Err(ParseError::UnknownKey(key_tok.to_string())),
                }
            }
            slots.push(aliases);
        }
        let active = vec![false; slots.len()];
        Ok(Self { slots, active })
    }

    /// Number of mandatory slots in the combo.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Keycodes declared for slot `i`. Panics if `i` is out of range.
    pub fn slot(&self, i: usize) -> &[u32] {
        &self.slots[i]
    }

    /// Update active state from a single keyboard event. Returns `true` if
    /// every slot is currently active (i.e. caller should exit).
    pub fn observe(&mut self, key: u32, pressed: bool) -> bool {
        for (slot, active) in self.slots.iter().zip(self.active.iter_mut()) {
            if slot.contains(&key) {
                *active = pressed;
                break;
            }
        }
        self.active.iter().all(|a| *a)
    }
}

pub const DEFAULT_COMBO: &str = "KEY_RIGHTSHIFT,KEY_ESC";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parses() {
        let c = EscCombo::parse(DEFAULT_COMBO).unwrap();
        assert_eq!(c.slot_count(), 2);
        assert_eq!(c.slot(0), &[54]); // KEY_RIGHTSHIFT
        assert_eq!(c.slot(1), &[1]); // KEY_ESC
    }

    #[test]
    fn alias_parses() {
        let c = EscCombo::parse("KEY_LEFTCTRL|KEY_RIGHTCTRL,KEY_ESC").unwrap();
        assert_eq!(c.slot_count(), 2);
        assert_eq!(c.slot(0), &[29, 97]);
        assert_eq!(c.slot(1), &[1]);
    }

    #[test]
    fn empty_slot_rejected() {
        assert_eq!(
            EscCombo::parse("KEY_A,,KEY_B").unwrap_err(),
            ParseError::EmptySlot
        );
    }

    #[test]
    fn empty_alias_rejected() {
        assert_eq!(
            EscCombo::parse("KEY_A|,KEY_B").unwrap_err(),
            ParseError::EmptyAlias
        );
    }

    #[test]
    fn trailing_comma_rejected() {
        assert_eq!(
            EscCombo::parse("KEY_A,").unwrap_err(),
            ParseError::EmptySlot
        );
    }

    #[test]
    fn unknown_key_rejected() {
        match EscCombo::parse("KEY_A,KEY_DOES_NOT_EXIST").unwrap_err() {
            ParseError::UnknownKey(n) => assert_eq!(n, "KEY_DOES_NOT_EXIST"),
            e => panic!("wrong err: {e:?}"),
        }
    }

    #[test]
    fn empty_input_rejected() {
        assert_eq!(EscCombo::parse("").unwrap_err(), ParseError::EmptyInput);
    }

    #[test]
    fn observe_triggers_only_when_all_slots_active() {
        let mut c = EscCombo::parse(DEFAULT_COMBO).unwrap();
        assert!(!c.observe(54, true), "only rightshift held");
        assert!(c.observe(1, true), "both held -> trigger");
        assert!(!c.observe(54, false), "release rightshift -> no trigger");
    }

    #[test]
    fn observe_ignores_unrelated_keys() {
        let mut c = EscCombo::parse(DEFAULT_COMBO).unwrap();
        assert!(!c.observe(30, true), "KEY_A is not in the combo");
    }

    #[test]
    fn observe_respects_aliases() {
        let mut c = EscCombo::parse("KEY_LEFTCTRL|KEY_RIGHTCTRL,KEY_ESC").unwrap();
        assert!(!c.observe(29, true), "ctrl down, esc up");
        assert!(c.observe(1, true), "ctrl + esc -> trigger");
        c.observe(1, false);
        assert!(!c.observe(97, true), "rightctrl (alias) up, esc released");
        assert!(c.observe(1, true), "rightctrl + esc -> trigger");
    }
}
