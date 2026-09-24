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

/// True when the board is live, it is O's turn, and O still has a legal move
/// — i.e. the host's own system prompt obliges the agent to call
/// `board.play(...)` before it finishes this turn (FIG-3181).
///
/// `legal_moves` is already empty on a won or full board, so a terminal board
/// never owes a move.
pub(crate) fn agent_owes_move(board: &BoardState) -> bool {
    board.turn == "O" && !legal_moves(board).is_empty()
}

/// Hand a board the agent owes a move on back to the human.
///
/// No move is invented on the agent's behalf: the O move is forfeited for this
/// round and the board becomes X's again, which is the single fact the UI's
/// disable rule reads (`board.turn !== 'X'`).
pub(crate) fn yield_to_human(board: &BoardState) -> BoardState {
    let mut next = board.clone();
    next.turn = "X".to_string();
    next
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
mod liveness_tests {
    use super::*;

    fn board(cells: [Option<&str>; 9], turn: &str) -> BoardState {
        BoardState {
            cells: cells
                .iter()
                .map(|cell| cell.map(str::to_string))
                .collect::<Vec<_>>(),
            turn: turn.to_string(),
        }
    }

    /// FIG-3181: the condition the host guards on is exactly "O's turn on a
    /// board that still has a legal move", not "O's turn".
    #[test]
    fn a_live_o_turn_owes_a_move_and_a_terminal_one_does_not() {
        assert!(agent_owes_move(&board([None; 9], "O")));
        assert!(!agent_owes_move(&board([None; 9], "X")));

        // A residual `turn: "O"` on a board X just won owes nothing: the
        // runbook's Phase 3 already says that turn is never played.
        let won = board(
            [
                Some("X"),
                Some("X"),
                Some("X"),
                Some("O"),
                Some("O"),
                None,
                None,
                None,
                None,
            ],
            "O",
        );
        assert!(!agent_owes_move(&won));

        let full = board([Some("X"); 9], "O");
        assert!(!agent_owes_move(&full));
    }

    /// Yielding forfeits the move rather than inventing one: the marks are
    /// untouched and only the turn moves.
    #[test]
    fn yielding_moves_the_turn_and_nothing_else() {
        let owed = board(
            [
                Some("X"),
                None,
                Some("O"),
                None,
                Some("X"),
                None,
                None,
                None,
                None,
            ],
            "O",
        );
        let yielded = yield_to_human(&owed);
        assert_eq!(yielded.turn, "X");
        assert_eq!(yielded.cells, owed.cells);
        assert!(!agent_owes_move(&yielded));
    }
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
}
