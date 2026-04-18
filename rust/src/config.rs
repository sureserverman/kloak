//! Runtime configuration and command-line parsing.
//!
//! Port of `parse_cli_args()` and the option defaults in [c/src/kloak.c].
//! See §1 of the behavior matrix for the flag surface.
//!
//! Parity points:
//! - `-d / --delay` accepts a non-negative decimal integer ≤ `i32::MAX`; on
//!   range or format error, fail.
//! - `-s / --start-delay` same validation as `--delay`.
//! - `-n / --natural-scrolling` accepts any string; exactly `"true"` enables,
//!   everything else is false. This reproduces C's `strcmp(optarg,"true")==0`.
//! - `-k / --esc-key-combo` parsed eagerly into an [`EscCombo`] at startup.
//! - `-h / --help` prints usage to stderr and signals `Help` exit.
//! - Unknown / malformed args produce `Error`; caller prints usage and exits 1.

use crate::escape::{self, EscCombo};
use std::fmt;

pub const DEFAULT_MAX_DELAY_MS: i32 = 100;
pub const DEFAULT_STARTUP_DELAY_MS: i32 = 500;

#[derive(Debug, Clone)]
pub struct Config {
    pub max_delay_ms: i32,
    pub startup_delay_ms: i32,
    pub natural_scrolling: bool,
    pub esc_combo: EscCombo,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_delay_ms: DEFAULT_MAX_DELAY_MS,
            startup_delay_ms: DEFAULT_STARTUP_DELAY_MS,
            natural_scrolling: false,
            esc_combo: EscCombo::parse(escape::DEFAULT_COMBO)
                .expect("DEFAULT_COMBO is a known-good constant"),
        }
    }
}

#[derive(Debug)]
pub enum ParseOutcome {
    Ok(Config),
    /// `--help`: caller prints usage and exits 0.
    Help,
    /// Fatal input error. Caller prints usage and the wrapped message, then exits 1.
    Error(String),
}

#[derive(Debug)]
pub enum CliError {
    UnknownFlag(String),
    MissingValue(String),
    InvalidInt {
        flag: String,
        value: String,
    },
    InvalidEscCombo {
        value: String,
        source: escape::ParseError,
    },
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::UnknownFlag(s) => write!(f, "unknown flag: {s}"),
            CliError::MissingValue(s) => write!(f, "option '{s}' requires an argument"),
            CliError::InvalidInt { flag, value } => {
                write!(f, "invalid value '{value}' passed to '{flag}'!")
            }
            CliError::InvalidEscCombo { value, source } => {
                write!(f, "invalid esc-key-combo '{value}': {source}")
            }
        }
    }
}

/// Parse CLI arguments. `args` should NOT include the program name.
///
/// Supported forms (matching GNU getopt_long):
/// - `-d 100`, `-d100`, `--delay 100`, `--delay=100`
/// - short flag chains are NOT supported (neither is this in the C code; each
///   kloak flag takes an argument).
pub fn parse_args<I, S>(args: I) -> ParseOutcome
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    match try_parse_args(args) {
        Ok(Some(cfg)) => ParseOutcome::Ok(cfg),
        Ok(None) => ParseOutcome::Help,
        Err(e) => ParseOutcome::Error(format!("FATAL ERROR: {e}")),
    }
}

fn try_parse_args<I, S>(args: I) -> Result<Option<Config>, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut cfg = Config::default();
    let mut esc_combo_set = false;

    let vec: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut iter = vec.into_iter().peekable();
    while let Some(tok) = iter.next() {
        // Long form: --name[=value]
        if let Some(long) = tok.strip_prefix("--") {
            if long == "help" {
                return Ok(None);
            }
            let (name, eq_value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let value = match name {
                "delay" | "start-delay" | "natural-scrolling" | "esc-key-combo" => match eq_value {
                    Some(v) => v,
                    None => iter
                        .next()
                        .ok_or_else(|| CliError::MissingValue(format!("--{name}")))?,
                },
                _ => return Err(CliError::UnknownFlag(format!("--{long}"))),
            };
            apply(name, &value, &mut cfg, &mut esc_combo_set)?;
            continue;
        }
        // Short form: -X, -X value, or -Xvalue
        if let Some(rest) = tok.strip_prefix('-') {
            if rest.is_empty() {
                return Err(CliError::UnknownFlag(tok));
            }
            let mut chars = rest.chars();
            let c = chars.next().expect("non-empty");
            let inline = chars.as_str();
            match c {
                'h' => return Ok(None),
                'd' | 's' | 'n' | 'k' => {
                    let long_name = match c {
                        'd' => "delay",
                        's' => "start-delay",
                        'n' => "natural-scrolling",
                        'k' => "esc-key-combo",
                        _ => unreachable!(),
                    };
                    let value = if !inline.is_empty() {
                        inline.to_string()
                    } else {
                        iter.next()
                            .ok_or_else(|| CliError::MissingValue(format!("-{c}")))?
                    };
                    apply(long_name, &value, &mut cfg, &mut esc_combo_set)?;
                }
                _ => return Err(CliError::UnknownFlag(format!("-{c}"))),
            }
            continue;
        }
        return Err(CliError::UnknownFlag(tok));
    }
    Ok(Some(cfg))
}

fn apply(
    name: &str,
    value: &str,
    cfg: &mut Config,
    esc_combo_set: &mut bool,
) -> Result<(), CliError> {
    match name {
        "delay" => {
            cfg.max_delay_ms = parse_uint31(name, value)?;
        }
        "start-delay" => {
            cfg.startup_delay_ms = parse_uint31(name, value)?;
        }
        "natural-scrolling" => {
            // Match C: exactly "true" enables; anything else silently becomes false.
            cfg.natural_scrolling = value == "true";
        }
        "esc-key-combo" => {
            cfg.esc_combo = EscCombo::parse(value).map_err(|source| CliError::InvalidEscCombo {
                value: value.to_string(),
                source,
            })?;
            *esc_combo_set = true;
        }
        _ => unreachable!("apply called with unrecognized name '{name}'"),
    }
    Ok(())
}

/// Parse a non-negative decimal integer ≤ `i32::MAX`. Matches C
/// `parse_uint31_arg`: empty string, negatives, non-decimal characters, and
/// values above i32::MAX all fail.
fn parse_uint31(flag: &str, value: &str) -> Result<i32, CliError> {
    // Reject empty, leading '+' / '-', leading whitespace — C strtoul with an
    // unsigned receiver and tail-char check has the same effect.
    if value.is_empty()
        || value.starts_with('-')
        || value.starts_with('+')
        || value.chars().any(|c| !c.is_ascii_digit())
    {
        return Err(CliError::InvalidInt {
            flag: flag.to_string(),
            value: value.to_string(),
        });
    }
    let as_u64: u64 = value.parse().map_err(|_| CliError::InvalidInt {
        flag: flag.to_string(),
        value: value.to_string(),
    })?;
    if as_u64 > i32::MAX as u64 {
        return Err(CliError::InvalidInt {
            flag: flag.to_string(),
            value: value.to_string(),
        });
    }
    Ok(as_u64 as i32)
}

pub const USAGE: &str = "\
Usage: kloak [options]
Anonymizes keyboard and mouse input timing by randomly delaying events.
Works on any Linux graphical stack: GNOME, KDE, wlroots, Xorg, tty.

Options:
  -h, --help                      Print help.
  -d, --delay=milliseconds        Max delay of released events. Default 100.
  -s, --start-delay=milliseconds  Time to wait before startup. Default 500.
  -n, --natural-scrolling=(true|false)
                                  Natural scrolling. Default false.
  -k, --esc-key-combo=KEY_1[,KEY_2|KEY_3...]
                                  Exit-combo. Default KEY_RIGHTSHIFT,KEY_ESC.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ParseOutcome {
        parse_args(args.iter().copied().map(String::from))
    }

    fn ok(args: &[&str]) -> Config {
        match parse(args) {
            ParseOutcome::Ok(c) => c,
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    fn err(args: &[&str]) -> String {
        match parse(args) {
            ParseOutcome::Error(m) => m,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let c = ok(&[]);
        assert_eq!(c.max_delay_ms, 100);
        assert_eq!(c.startup_delay_ms, 500);
        assert!(!c.natural_scrolling);
        assert_eq!(c.esc_combo.slot_count(), 2);
    }

    #[test]
    fn help_flag_short() {
        assert!(matches!(parse(&["-h"]), ParseOutcome::Help));
    }

    #[test]
    fn help_flag_long() {
        assert!(matches!(parse(&["--help"]), ParseOutcome::Help));
    }

    #[test]
    fn delay_short_separated() {
        assert_eq!(ok(&["-d", "250"]).max_delay_ms, 250);
    }

    #[test]
    fn delay_short_concatenated() {
        assert_eq!(ok(&["-d250"]).max_delay_ms, 250);
    }

    #[test]
    fn delay_long_equals() {
        assert_eq!(ok(&["--delay=250"]).max_delay_ms, 250);
    }

    #[test]
    fn delay_long_separated() {
        assert_eq!(ok(&["--delay", "250"]).max_delay_ms, 250);
    }

    #[test]
    fn start_delay_parses() {
        assert_eq!(ok(&["-s", "1000"]).startup_delay_ms, 1000);
    }

    #[test]
    fn delay_zero_is_valid() {
        assert_eq!(ok(&["-d", "0"]).max_delay_ms, 0);
    }

    #[test]
    fn delay_at_i32_max() {
        assert_eq!(ok(&["-d", "2147483647"]).max_delay_ms, i32::MAX);
    }

    #[test]
    fn delay_over_i32_max_rejected() {
        let m = err(&["-d", "2147483648"]);
        assert!(m.contains("invalid value"), "msg was: {m}");
    }

    #[test]
    fn delay_negative_rejected() {
        let m = err(&["-d", "-1"]);
        assert!(m.contains("invalid value"), "msg was: {m}");
    }

    #[test]
    fn delay_non_numeric_rejected() {
        let m = err(&["-d", "abc"]);
        assert!(m.contains("invalid value"), "msg was: {m}");
    }

    #[test]
    fn delay_hex_rejected() {
        // Matches C: strtoul(value, _, 10) — 0x prefix is not decimal.
        let m = err(&["-d", "0x10"]);
        assert!(m.contains("invalid value"), "msg was: {m}");
    }

    #[test]
    fn natural_scrolling_true() {
        assert!(ok(&["-n", "true"]).natural_scrolling);
    }

    #[test]
    fn natural_scrolling_false() {
        assert!(!ok(&["-n", "false"]).natural_scrolling);
    }

    #[test]
    fn natural_scrolling_nonsense_is_false() {
        // C behavior: any non-"true" is false.
        assert!(!ok(&["-n", "maybe"]).natural_scrolling);
    }

    #[test]
    fn esc_combo_parses() {
        let c = ok(&["-k", "KEY_LEFTCTRL|KEY_RIGHTCTRL,KEY_ESC"]);
        assert_eq!(c.esc_combo.slot_count(), 2);
        assert_eq!(c.esc_combo.slot(0), &[29, 97]);
    }

    #[test]
    fn esc_combo_unknown_rejected() {
        let m = err(&["-k", "KEY_NOT_REAL"]);
        assert!(m.contains("invalid esc-key-combo"), "msg was: {m}");
    }

    #[test]
    fn unknown_flag_rejected() {
        assert!(matches!(parse(&["--frobnicate"]), ParseOutcome::Error(_)));
        assert!(matches!(parse(&["-Z"]), ParseOutcome::Error(_)));
    }

    #[test]
    fn missing_value_rejected() {
        assert!(matches!(parse(&["-d"]), ParseOutcome::Error(_)));
        assert!(matches!(parse(&["--delay"]), ParseOutcome::Error(_)));
    }

    #[test]
    fn multiple_flags() {
        let c = ok(&[
            "-d",
            "20",
            "-s",
            "50",
            "-n",
            "true",
            "-k",
            "KEY_LEFTSHIFT,KEY_ESC",
        ]);
        assert_eq!(c.max_delay_ms, 20);
        assert_eq!(c.startup_delay_ms, 50);
        assert!(c.natural_scrolling);
        assert_eq!(c.esc_combo.slot(0), &[42]);
    }

    #[test]
    fn later_wins_on_repeated_flag() {
        // getopt_long style: last occurrence wins.
        assert_eq!(ok(&["-d", "10", "-d", "20"]).max_delay_ms, 20);
    }
}
