//! Drive a session without a person at the keyboard.
//!
//! Some verification needs an application taken through a sequence — a game's
//! own benchmark, say — and doing that by hand is both slow and unrepeatable.
//! Timing matters: a menu that has not finished animating swallows a keypress,
//! and a run that pressed the wrong thing looks like a decoding fault rather
//! than a mistimed script.
//!
//! So a script is a flat list of waits, keypresses and screenshots, written
//! inline:
//!
//! ```text
//! --input-script "30s down down enter 10s down down down 10s r 220s shot"
//! ```
//!
//! Read as: wait 30 s, press Down twice, press Enter, wait 10 s, press Down
//! three times, wait 10 s, press R, wait 220 s, save a frame.
//!
//! Keys are HID usages, the same as a real keypress takes, so this exercises
//! the input path rather than going around it.

use std::time::Duration;

/// One thing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Wait before the next step.
    Wait(Duration),
    /// Press and release a key, by HID usage (page 0x07).
    Key(u16),
    /// Write the next decoded frame out.
    Shot,
}

/// Parse a script. Tokens are whitespace-separated; anything ending in `s` or
/// `ms` is a wait, `shot` saves a frame, everything else is a key name.
///
/// Fails on an unknown token rather than skipping it: a script that silently
/// drops a keypress walks the wrong menu and reports nonsense.
pub fn parse(text: &str) -> Result<Vec<Step>, String> {
    text.split_whitespace()
        .map(|token| {
            if token == "shot" {
                return Ok(Step::Shot);
            }
            if let Some(rest) = token.strip_suffix("ms") {
                let ms: u64 = rest
                    .parse()
                    .map_err(|_| format!("not a number of milliseconds: {token:?}"))?;
                return Ok(Step::Wait(Duration::from_millis(ms)));
            }
            if let Some(rest) = token.strip_suffix('s')
                && let Ok(secs) = rest.parse::<f64>()
            {
                return Ok(Step::Wait(Duration::from_secs_f64(secs)));
            }
            hid_usage(token)
                .map(Step::Key)
                .ok_or_else(|| format!("not a key this script knows: {token:?}"))
        })
        .collect()
}

impl Step {
    /// A short name for the frame saved after this step, so a directory of
    /// shots reads as a sequence rather than a pile of numbers.
    #[must_use]
    pub fn slug(self) -> String {
        match self {
            Self::Wait(d) => format!("wait-{}ms", d.as_millis()),
            Self::Key(usage) => format!("key-{}", key_name(usage)),
            Self::Shot => "shot".to_owned(),
        }
    }
}

/// The name a usage came from, for readable filenames.
fn key_name(usage: u16) -> String {
    match usage {
        0x52 => "up".to_owned(),
        0x51 => "down".to_owned(),
        0x50 => "left".to_owned(),
        0x4F => "right".to_owned(),
        0x28 => "enter".to_owned(),
        0x29 => "escape".to_owned(),
        0x2C => "space".to_owned(),
        0x2B => "tab".to_owned(),
        0x04..=0x1D => char::from(b'a' + (usage - 0x04) as u8).to_string(),
        0x1E..=0x26 => char::from(b'1' + (usage - 0x1E) as u8).to_string(),
        0x27 => "0".to_owned(),
        other => format!("{other:#04x}"),
    }
}

/// How long the whole script takes, so a run can be given time to finish.
#[must_use]
pub fn duration(steps: &[Step]) -> Duration {
    steps
        .iter()
        .filter_map(|s| match s {
            Step::Wait(d) => Some(*d),
            _ => None,
        })
        .sum()
}

/// Key name to HID usage (page 0x07), covering what a menu needs plus the
/// letters and digits a game might bind.
fn hid_usage(name: &str) -> Option<u16> {
    Some(match name {
        "up" => 0x52,
        "down" => 0x51,
        "left" => 0x50,
        "right" => 0x4F,
        "enter" | "return" => 0x28,
        "escape" | "esc" => 0x29,
        "space" => 0x2C,
        "tab" => 0x2B,
        "backspace" => 0x2A,
        // Letters are contiguous from A = 0x04.
        single if single.len() == 1 && single.chars().all(|c| c.is_ascii_alphabetic()) => {
            let c = single.chars().next()?.to_ascii_lowercase();
            0x04 + (c as u16 - 'a' as u16)
        }
        // Digits are contiguous from 1 = 0x1E, with zero after nine.
        single if single.len() == 1 && single.chars().all(|c| c.is_ascii_digit()) => {
            let c = single.chars().next()?;
            if c == '0' {
                0x27
            } else {
                0x1E + (c as u16 - '1' as u16)
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The benchmark sequence, as it is actually written on the command line.
    #[test]
    fn the_benchmark_script_parses_to_what_it_reads_as() {
        let steps = parse("30s down down enter 10s down down down 10s r 220s shot")
            .expect("a valid script");
        assert_eq!(
            steps,
            vec![
                Step::Wait(Duration::from_secs(30)),
                Step::Key(0x51),
                Step::Key(0x51),
                Step::Key(0x28),
                Step::Wait(Duration::from_secs(10)),
                Step::Key(0x51),
                Step::Key(0x51),
                Step::Key(0x51),
                Step::Wait(Duration::from_secs(10)),
                Step::Key(0x15),
                Step::Wait(Duration::from_secs(220)),
                Step::Shot,
            ]
        );
        assert_eq!(duration(&steps), Duration::from_secs(270));
    }

    /// An unknown token must stop the run. Skipping it would walk a different
    /// path through the menus and report the results of something else.
    #[test]
    fn an_unknown_token_is_refused_rather_than_skipped() {
        assert!(parse("30s wiggle enter").is_err());
        assert!(parse("30x down").is_err());
        assert!(parse("12ss down").is_err());
    }

    /// Fractional and millisecond waits, because menu animations are not
    /// whole seconds long.
    #[test]
    fn waits_can_be_finer_than_a_second() {
        assert_eq!(
            parse("1.5s 250ms").expect("valid"),
            vec![
                Step::Wait(Duration::from_millis(1500)),
                Step::Wait(Duration::from_millis(250)),
            ]
        );
    }

    /// A shot is saved after every step, so a script that walked the wrong
    /// menu shows which step went wrong rather than only that one did. The
    /// names have to be readable for that to be worth anything.
    #[test]
    fn every_step_names_its_own_screenshot() {
        assert_eq!(Step::Key(0x51).slug(), "key-down");
        assert_eq!(Step::Key(0x28).slug(), "key-enter");
        assert_eq!(Step::Key(0x15).slug(), "key-r");
        assert_eq!(Step::Wait(Duration::from_secs(30)).slug(), "wait-30000ms");
        assert_eq!(Step::Shot.slug(), "shot");
    }

    /// Letters and digits map onto the contiguous HID ranges, checked at both
    /// ends so an off-by-one cannot hide in the middle.
    #[test]
    fn letters_and_digits_land_on_the_right_usages() {
        assert_eq!(hid_usage("a"), Some(0x04));
        assert_eq!(hid_usage("z"), Some(0x1D));
        assert_eq!(hid_usage("r"), Some(0x15));
        assert_eq!(hid_usage("R"), Some(0x15), "case does not matter");
        assert_eq!(hid_usage("1"), Some(0x1E));
        assert_eq!(hid_usage("9"), Some(0x26));
        assert_eq!(hid_usage("0"), Some(0x27), "zero sits after nine");
    }
}
