use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BoardState {
    pub(crate) cells: Vec<Option<String>>,
    pub(crate) turn: String,
}

pub(crate) fn default_board() -> BoardState {
    BoardState {
        cells: vec![None; 9],
        turn: "X".to_string(),
    }
}

/// The board context, written in the dialect this turn resolved.
///
/// ADR 0063: host prompt copy follows the session's own dialect. The finish
/// form and the noun for a unit of code are the two words in here that a
/// TypeScript reader has never seen; `board.play(...)` is the same call path in
/// both, because one tool binding serves both dialects.
pub(crate) fn board_prompt(board: &BoardState, dialect: lash::rlm::RlmDialect) -> String {
    let status = board_status(board);
    let (finish_form, cell_noun) = match dialect {
        lash::rlm::RlmDialect::Typescript => (
            "finish(\"<one short user-facing sentence>\")",
            "typescript cell",
        ),
        _ => (
            "finish \"<one short user-facing sentence>\"",
            "lashlang block",
        ),
    };
    format!(
        "You are O. The human is X.\nCurrent turn: {}.\nIndex map:\n0 top-left | 1 top-middle | 2 top-right\n3 middle-left | 4 center | 5 middle-right\n6 bottom-left | 7 bottom-middle | 8 bottom-right\nCurrent marks by index:\n{}\nVisual board:\n{}\nLegal moves: {:?}\nStatus: {}.\nIf it is O's turn and the game is not over, call `board.play(...)` exactly once before answering. Only choose one of the legal move indexes. Use `board.read(...)` only when needed. Finish with `{finish_form}`; do not repeat that sentence as prose outside the {cell_noun}. If your move ended the game, clearly say that you won or that the game ended in a draw; otherwise say it is the human's turn. Do not explain your strategy, do not describe threats, do not print an ASCII board, do not narrate every cell, and do not return JSON to the user.",
        board.turn,
        indexed_marks(board),
        board_rows(board),
        legal_moves(board),
        status
    )
}

fn indexed_marks(board: &BoardState) -> String {
    (0..9)
        .map(|index| {
            let mark = board
                .cells
                .get(index)
                .and_then(|cell| cell.as_deref())
                .unwrap_or("empty");
            format!("{index}: {mark}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn board_rows(board: &BoardState) -> String {
    (0..3)
        .map(|row| {
            (0..3)
                .map(|col| {
                    let index = row * 3 + col;
                    board
                        .cells
                        .get(index)
                        .and_then(|cell| cell.as_deref())
                        .unwrap_or(".")
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn legal_moves(board: &BoardState) -> Vec<usize> {
    if winner(&board.cells).is_some() {
        return Vec::new();
    }
    board
        .cells
        .iter()
        .enumerate()
        .filter_map(|(index, cell)| cell.is_none().then_some(index))
        .collect()
}

fn board_status(board: &BoardState) -> String {
    if let Some(winner) = winner(&board.cells) {
        return format!("{winner} won");
    }
    if legal_moves(board).is_empty() {
        return "draw".to_string();
    }
    format!("{} to move", board.turn)
}

pub(crate) fn board_snapshot(board: &BoardState) -> serde_json::Value {
    json!({
        "cells": board.cells,
        "index_map": [
            "0 top-left", "1 top-middle", "2 top-right",
            "3 middle-left", "4 center", "5 middle-right",
            "6 bottom-left", "7 bottom-middle", "8 bottom-right"
        ],
        "marks_by_index": indexed_marks(board),
        "turn": board.turn,
        "legal_moves": legal_moves(board),
        "status": board_status(board),
        "winner": winner(&board.cells),
    })
}

pub(crate) fn apply_agent_move(board: &BoardState, cell: usize) -> serde_json::Value {
    if board.turn != "O" {
        return json!({
            "accepted": false,
            "reason": "It is not O's turn.",
            "board": board_snapshot(board),
        });
    }
    if cell >= 9
        || board
            .cells
            .get(cell)
            .and_then(|value| value.as_ref())
            .is_some()
    {
        return json!({
            "accepted": false,
            "reason": "Cell is not legal.",
            "board": board_snapshot(board),
        });
    }
    let mut next = board.clone();
    next.cells[cell] = Some("O".to_string());
    next.turn = "X".to_string();
    json!({
        "accepted": true,
        "move": { "mark": "O", "cell": cell },
        "board": board_snapshot(&next),
    })
}

fn winner(cells: &[Option<String>]) -> Option<&'static str> {
    const LINES: [[usize; 3]; 8] = [
        [0, 1, 2],
        [3, 4, 5],
        [6, 7, 8],
        [0, 3, 6],
        [1, 4, 7],
        [2, 5, 8],
        [0, 4, 8],
        [2, 4, 6],
    ];
    for [a, b, c] in LINES {
        let Some(mark) = cells.get(a).and_then(|cell| cell.as_deref()) else {
            continue;
        };
        if cells.get(b).and_then(|cell| cell.as_deref()) == Some(mark)
            && cells.get(c).and_then(|cell| cell.as_deref()) == Some(mark)
        {
            return match mark {
                "X" => Some("X"),
                "O" => Some("O"),
                _ => None,
            };
        }
    }
    None
}

#[cfg(test)]
mod dialect_tests {
    use super::*;

    /// ADR 0063, host side: the board context names a finish form and a unit of
    /// code, and both are dialect words. The pair is asserted in both
    /// directions so neither can silently become the other's.
    #[test]
    fn the_board_prompt_speaks_the_sessions_dialect() {
        let lashlang = board_prompt(&default_board(), lash::rlm::RlmDialect::Lashlang);
        assert!(lashlang.contains("finish \"<one short user-facing sentence>\""));
        assert!(lashlang.contains("outside the lashlang block"));
        assert!(!lashlang.contains("finish("));
        assert!(!lashlang.contains("typescript"));

        let typescript = board_prompt(&default_board(), lash::rlm::RlmDialect::Typescript);
        assert!(typescript.contains("finish(\"<one short user-facing sentence>\")"));
        assert!(typescript.contains("outside the typescript cell"));
        assert!(!typescript.contains("lashlang"));
    }

    /// The turn's resolved options decide, so a session that recorded the other
    /// dialect is described in the one it is running.
    #[test]
    fn the_dialect_comes_from_the_turns_resolved_options() {
        let recorded = lash::runtime::ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
            dialect: Some(lash::rlm::RlmDialect::Typescript),
            ..Default::default()
        })
        .expect("typed options");
        assert_eq!(
            crate::state::rlm_dialect_from_turn_options(&recorded),
            lash::rlm::RlmDialect::Typescript
        );
        assert_eq!(
            crate::state::rlm_dialect_from_turn_options(
                &lash::runtime::ProtocolTurnOptions::default()
            ),
            lash::rlm::RlmDialect::Lashlang
        );
    }

    /// FIG-1555: a refused pin is a typed value, not a message to match on.
    ///
    /// The message-string test this replaces existed because the host had to
    /// tell a pin conflict from every other protocol failure by reading prose.
    /// The typed refusal carries both dialects, so the host reads the values it
    /// needs and the message is only ever rendered for the operator.
    #[test]
    fn a_refused_pin_is_typed_and_carries_both_dialects() {
        let conflict = lash::rlm::RlmSessionConfigConflict::Dialect {
            recorded: lash::rlm::RlmDialect::Typescript,
            requested: lash::rlm::RlmDialect::Lashlang,
        };
        let lash::rlm::RlmSessionConfigConflict::Dialect {
            recorded,
            requested,
        } = &conflict
        else {
            panic!("a dialect refusal must be the dialect variant");
        };
        assert_eq!(*recorded, lash::rlm::RlmDialect::Typescript);
        assert_eq!(*requested, lash::rlm::RlmDialect::Lashlang);
        assert_eq!(conflict.field(), "dialect");
    }
}
