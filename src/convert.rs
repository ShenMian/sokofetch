//! Convert sokobanonline classic `design` boards to XSB text.
//!
//! Only classic puzzles are converted: a classic design is already XSB (one
//! char per cell), so the only work is normalizing the `_`/`-` floor spellings
//! to spaces. modern designs use a 4-chars-per-cell encoding XSB cannot
//! express; the caller filters them out via `puzzletype`. Token tables:
//! docs/API.md §4.

/// classic design -> XSB board, or the reason the board cannot enter the
/// collection. soukoban (the XSB parser this tool targets) performs the final
/// validation: standard alphabet, exactly 1 player, boxes == goals.
pub fn to_xsb(design: &str) -> Result<String, String> {
    let board = design
        .split('\n')
        .map(|row| {
            row.trim_end_matches('\r')
                .replace(['_', '-'], " ")
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let board = board.trim_matches('\n');
    // soukoban skips blank lines, but a blank row inside a level would read
    // as the level separator of the collection format.
    if board.lines().any(str::is_empty) {
        return Err("board contains an empty row".into());
    }
    if let Err(err) = board.parse::<soukoban::map::Map>() {
        return Err(format!("XSB parser rejected the board: {err}"));
    }
    Ok(board.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_are_normalized() {
        assert_eq!(to_xsb("#--#\n_$@.\n####").unwrap(), "#  #\n $@.\n####");
    }

    #[test]
    fn blank_edge_rows_are_trimmed() {
        assert_eq!(
            to_xsb("\n#####\n#@$.#\n#####\n").unwrap(),
            "#####\n#@$.#\n#####"
        );
    }

    #[test]
    fn blank_interior_rows_are_rejected() {
        assert!(to_xsb("#####\n     \n#@$.\n#####").is_err());
    }

    #[test]
    fn parser_rejects_coop_boards() {
        // two players
        assert!(to_xsb("#####\n#@$@#\n#####").is_err());
    }

    #[test]
    fn parser_rejects_decoy_boxes() {
        // two boxes, one goal
        assert!(to_xsb("#####\n#$$.#\n# @ #\n#####").is_err());
    }
}
