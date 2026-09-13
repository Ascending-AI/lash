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

/// The board context, written in the TypeScript surface the session runs.
///
/// ADR 0063: host prompt copy follows the session's language. TypeScript is the
/// sole RLM language (ADR 0096), so the finish form and the noun for a unit of
/// code are fixed rather than resolved per session.
pub(crate) fn board_prompt(board: &BoardState) -> String {
    let status = board_status(board);
    let (finish_form, cell_noun) = (
        "finish(\"<one short user-facing sentence>\")",
        "typescript cell",
    );
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
mod prompt_language_tests {
    use super::*;

    /// ADR 0063, host side: the board context names a finish form and a unit of
    /// code, and both are language words. TypeScript is the sole RLM language
    /// (ADR 0096), so the pair is asserted positively and the retired surface's
    /// wording is asserted absent.
    #[test]
    fn the_board_prompt_speaks_typescript() {
        let prompt = board_prompt(&default_board());
        assert!(prompt.contains("finish(\"<one short user-facing sentence>\")"));
        assert!(prompt.contains("outside the typescript cell"));
        assert!(!prompt.contains("lashlang"));
    }

    /// ADR 0096: a session bag that still records the retired `dialect` field is
    /// refused as an incompatible format, not read as absence. Reading it as
    /// absence would run a session recorded under the retired language against
    /// TypeScript semantics.
    #[test]
    fn a_recorded_dialect_field_is_refused_as_incompatible() {
        let recorded = lash::runtime::ProtocolTurnOptions::from_payload(
            serde_json::json!({ "dialect": "typescript" }),
        );
        assert_eq!(
            lash_protocol_rlm::rlm_session_config(&recorded)
                .expect_err("a recorded dialect field must refuse"),
            lash::rlm::RlmSessionConfigDecodeError::RetiredDialectField,
        );

        // A bag that records nothing still reads as an empty config.
        lash_protocol_rlm::rlm_session_config(&lash::runtime::ProtocolTurnOptions::default())
            .expect("an empty bag is a session that recorded nothing");
    }

    /// The turn bag cannot carry a language at all: the per-turn options type
    /// has no such field, so a turn naming a language the executor ignores is
    /// unrepresentable rather than merely unused (FIG-1979).
    #[test]
    fn a_turn_bag_carries_no_dialect_key() {
        let encoded = serde_json::to_value(lash::rlm::RlmTurnOptions {
            termination: Some(lash::rlm::RlmTermination::Natural),
            final_answer_format: None,
        })
        .expect("encode turn options");
        assert!(
            encoded.get("dialect").is_none(),
            "the per-turn bag has no dialect: {encoded}"
        );
    }
}
