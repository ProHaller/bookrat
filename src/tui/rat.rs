//! The home-screen mascot: a rat reading a book. Its eyes scan the lines,
//! it blinks now and then, turns a page at the end of each pass, and its
//! tail never stops swishing.

/// Width of every frame in columns.
pub const WIDTH: u16 = 32;
/// Height of every frame in rows.
#[cfg(test)]
const HEIGHT: u16 = 12;

/// Animation ticks per full reading cycle.
const CYCLE: u64 = 24;

/// Eye rows, all 13 columns wide to sit between the cheek `|`s.
const EYES_LEFT: &str = "  o     o    ";
const EYES_CENTER: &str = "   o     o   ";
const EYES_RIGHT: &str = "    o     o  ";
const EYES_BLINK: &str = "   -     -   ";

/// Right-page rows (7 columns each): text, then a page lifting off.
const PAGE_TEXT: [&str; 3] = [" ~~~~~ ", " ~~~~  ", " ~~~~~ "];
const PAGE_LIFT: [&str; 3] = ["  _.-' ", " /     ", "/      "];
const PAGE_OVER: [&str; 3] = ["'-.    ", "   \\   ", "    \\  "];

/// Tail rows to the right of the book, two-frame swish.
const TAIL: [[&str; 5]; 2] = [
    [
        "      _  ",
        "     ( ) ",
        "      \\ \\",
        "       ) )",
        "~~~~--' /",
    ],
    [
        "     _   ",
        "    / )  ",
        "   / /   ",
        "  ( (    ",
        "~~~-'    ",
    ],
];

/// The frame to show at `tick` (advance it roughly every 150 ms).
pub fn frame(tick: u64) -> Vec<String> {
    let phase = tick % CYCLE;
    let eyes = match phase {
        0..=5 => EYES_LEFT,
        6..=11 => EYES_CENTER,
        12 => EYES_BLINK,
        13..=19 => EYES_RIGHT,
        _ => EYES_CENTER,
    };
    let page = match phase {
        20 | 21 => PAGE_LIFT,
        22 | 23 => PAGE_OVER,
        _ => PAGE_TEXT,
    };
    let tail = TAIL[usize::from(tick / 2 % 2 == 1)];

    let mut lines = vec![
        "     __           __".to_string(),
        r#"    /  \.-"""""-./  \"#.to_string(),
        r"    \    -     -    /".to_string(),
        format!("     |{eyes}|"),
        r"     \ =  ._Y_.  = /".to_string(),
        "      '-.._____..-'".to_string(),
        format!("    ____/   |   \\____{}", tail[0]),
        format!("    | ~~~~~ |{}|{}", page[0], tail[1]),
        format!("    | ~~~~  |{}|{}", page[1], tail[2]),
        format!("    | ~~~~~ |{}|{}", page[2], tail[3]),
        format!("    '-------'-------'{}", tail[4]),
        String::new(),
    ];
    for line in &mut lines {
        let width = line.chars().count();
        line.extend(std::iter::repeat_n(
            ' ',
            usize::from(WIDTH).saturating_sub(width),
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_fits_the_declared_size() {
        for tick in 0..CYCLE * 2 {
            let f = frame(tick);
            assert_eq!(f.len(), usize::from(HEIGHT));
            assert!(
                f.iter().all(|l| l.chars().count() == usize::from(WIDTH)),
                "{f:#?}"
            );
        }
    }

    #[test]
    fn eyes_move_across_a_cycle() {
        let distinct: std::collections::HashSet<String> =
            (0..CYCLE).map(|t| frame(t)[3].clone()).collect();
        assert!(distinct.len() >= 3);
    }

    #[test]
    #[ignore = "prints the animation for eyeballing: cargo test frames -- --ignored --nocapture"]
    fn print_frames() {
        for tick in [0, 8, 12, 15, 20, 22] {
            println!("tick {tick}:\n{}", frame(tick).join("\n"));
        }
    }
}
