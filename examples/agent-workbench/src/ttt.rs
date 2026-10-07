//! Mocked tic-tac-toe world for the workbench's memory scenario (FIG-4441).
//!
//! The agent plays X against a programmatic opponent through two tools,
//! projected at module path `ttt`:
//!
//! - `view_board({})`: the current game's number, board, whose turn it is and
//!   its status;
//! - `make_move({ cell })`: place X on a cell, 0–8 row by row; the opponent
//!   answers inside the same call, and the call returns the board and the
//!   status.
//!
//! A host route starts each game, so a driver opens game `g` with its user
//! turn. The opponent's strength per game comes from a seeded schedule
//! (`perfect` is minimax, `random` is uniform over the legal cells), and every
//! random draw comes from a stream keyed by the seed and the game number, so a
//! seed replays the same games for the same agent moves.
//!
//! The world keeps the authoritative game log. A memory answer is scored
//! against that log, never against the schedule, because the agent may
//! blunder.

use lash::sync::MutexExt;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use lash::tools::{
    ExecutionPolicy, ToolAttemptOutcome, ToolBinding, ToolContract, ToolDefinition,
    ToolDefinitionBindingExt, ToolManifest, ToolOutcome, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Turns the world on: `on`, or `off` (the default when unset or empty).
pub(crate) const AGENT_WORKBENCH_TTT_ENV: &str = "AGENT_WORKBENCH_TTT";
/// The seed every opponent draw derives from (default 1).
pub(crate) const AGENT_WORKBENCH_TTT_SEED_ENV: &str = "AGENT_WORKBENCH_TTT_SEED";
/// The opponent schedule: `mixed` (default), `perfect`, `random`, or a
/// comma-separated list of `perfect`/`random`, one per game, repeated when
/// the games outnumber it.
pub(crate) const AGENT_WORKBENCH_TTT_SCHEDULE_ENV: &str = "AGENT_WORKBENCH_TTT_SCHEDULE";
/// Who moves first in every game: `agent` (default) or `opponent`.
pub(crate) const AGENT_WORKBENCH_TTT_FIRST_ENV: &str = "AGENT_WORKBENCH_TTT_FIRST";

const VIEW_BOARD: &str = "view_board";
const MAKE_MOVE: &str = "make_move";
const OPERATIONS: [&str; 2] = [VIEW_BOARD, MAKE_MOVE];

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Mark {
    X,
    O,
}

impl Mark {
    fn other(self) -> Self {
        match self {
            Self::X => Self::O,
            Self::O => Self::X,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::O => "O",
        }
    }
}

/// The agent always plays X.
const AGENT: Mark = Mark::X;

type Board = [Option<Mark>; 9];

fn winner(board: &Board) -> Option<Mark> {
    LINES.iter().find_map(|[a, b, c]| {
        let mark = board[*a]?;
        (board[*b] == Some(mark) && board[*c] == Some(mark)).then_some(mark)
    })
}

fn empty_cells(board: &Board) -> Vec<usize> {
    (0..9).filter(|cell| board[*cell].is_none()).collect()
}

/// A game's state, from the agent's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Ongoing,
    AgentWon,
    OpponentWon,
    Draw,
}

impl Status {
    fn of(board: &Board) -> Self {
        match winner(board) {
            Some(mark) if mark == AGENT => Self::AgentWon,
            Some(_) => Self::OpponentWon,
            None if board.iter().all(Option::is_some) => Self::Draw,
            None => Self::Ongoing,
        }
    }

    /// The status as the agent's tools state it.
    fn label(self) -> &'static str {
        match self {
            Self::Ongoing => "ongoing",
            Self::AgentWon => "you won",
            Self::OpponentWon => "opponent won",
            Self::Draw => "draw",
        }
    }

    fn result(self) -> Option<GameResult> {
        match self {
            Self::Ongoing => None,
            Self::AgentWon => Some(GameResult::Assistant),
            Self::OpponentWon => Some(GameResult::User),
            Self::Draw => Some(GameResult::Draw),
        }
    }
}

/// The same typed result is used by the world's log and the memory scorer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum GameResult {
    Assistant,
    User,
    Draw,
}

impl GameResult {
    fn from_answer(item: &Value) -> Option<Self> {
        match item.as_str()?.trim().to_uppercase().as_str() {
            "ASSISTANT" => Some(Self::Assistant),
            "USER" => Some(Self::User),
            "DRAW" => Some(Self::Draw),
            _ => None,
        }
    }
}

/// Negamax value of `board` for `to_move`: positive wins, zero draws,
/// negative loses; a sooner win scores higher and a sooner loss lower.
fn value(board: &mut Board, to_move: Mark, memo: &mut HashMap<(Board, Mark), i8>) -> i8 {
    if let Some(&known) = memo.get(&(*board, to_move)) {
        return known;
    }
    let empty = empty_cells(board);
    let score = if winner(board).is_some() {
        // The previous mover just completed a line.
        -(1 + empty.len() as i8)
    } else if empty.is_empty() {
        0
    } else {
        let mut best = i8::MIN;
        for cell in empty {
            board[cell] = Some(to_move);
            best = best.max(-value(board, to_move.other(), memo));
            board[cell] = None;
        }
        best
    };
    memo.insert((*board, to_move), score);
    score
}

/// The cells that keep `to_move`'s best minimax value, lowest first.
fn best_cells(board: &Board, to_move: Mark) -> Vec<usize> {
    let mut memo = HashMap::new();
    let mut scratch = *board;
    let scored: Vec<(usize, i8)> = empty_cells(board)
        .into_iter()
        .map(|cell| {
            scratch[cell] = Some(to_move);
            let score = -value(&mut scratch, to_move.other(), &mut memo);
            scratch[cell] = None;
            (cell, score)
        })
        .collect();
    let best = scored.iter().map(|(_, score)| *score).max();
    scored
        .into_iter()
        .filter(|(_, score)| Some(*score) == best)
        .map(|(cell, _)| cell)
        .collect()
}

/// The game-theoretic outcome class of a minimax value: win, draw or loss.
fn outcome_class(score: i8) -> i8 {
    score.signum()
}

/// SplitMix64: a small, seedable, portable generator, so a seed replays the
/// same draws on every host.
#[derive(Clone, Debug)]
struct SplitMix(u64);

impl SplitMix {
    /// The stream for one purpose of one game under one seed.
    fn stream(seed: u64, game: u32, purpose: u64) -> Self {
        let mut mix = Self(
            seed ^ u64::from(game).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ purpose.wrapping_mul(0xD1B5_4A32_D192_ED03),
        );
        mix.next();
        mix
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        if items.is_empty() {
            return None;
        }
        let index = (self.next() % items.len() as u64) as usize;
        Some(items[index])
    }
}

const STRENGTH_STREAM: u64 = 1;
const MOVE_STREAM: u64 = 2;

/// How the opponent picks its cells in one game.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Strength {
    /// Minimax; a tie between equally good cells is broken by the game's
    /// seeded stream.
    Perfect,
    /// Uniform over the legal cells, from the game's seeded stream.
    Random,
}

impl Strength {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "perfect" => Ok(Self::Perfect),
            "random" => Ok(Self::Random),
            other => Err(format!(
                "opponent strength must be `perfect` or `random`, got `{other}`"
            )),
        }
    }
}

/// The opponent's strength for every game.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Schedule {
    /// Each game draws `perfect` or `random` from the seed.
    Mixed,
    /// Every game at this strength.
    Every(Strength),
    /// One strength per game, in order, repeated when the games outnumber it.
    List(Vec<Strength>),
}

impl Schedule {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "" | "mixed" => Ok(Self::Mixed),
            single if !single.contains(',') => Strength::parse(single).map(Self::Every),
            list => list
                .split(',')
                .map(Strength::parse)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::List),
        }
    }

    /// The opponent's strength in game `game` (1-based) under `seed`.
    pub(crate) fn strength(&self, seed: u64, game: u32) -> Strength {
        match self {
            Self::Mixed => {
                if SplitMix::stream(seed, game, STRENGTH_STREAM).next() >> 63 == 0 {
                    Strength::Perfect
                } else {
                    Strength::Random
                }
            }
            Self::Every(strength) => *strength,
            Self::List(list) => list[(game as usize - 1) % list.len()],
        }
    }
}

/// Who moves first in every game.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum First {
    Agent,
    Opponent,
}

/// The world's settings, read from the `AGENT_WORKBENCH_TTT_*` variables.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TttConfig {
    pub(crate) seed: u64,
    pub(crate) schedule: Schedule,
    pub(crate) first: First,
}

impl Default for TttConfig {
    fn default() -> Self {
        Self {
            seed: 1,
            schedule: Schedule::Mixed,
            first: First::Agent,
        }
    }
}

impl TttConfig {
    /// The configured world, or `None` when `AGENT_WORKBENCH_TTT` is off.
    pub(crate) fn from_env(
        read_env: impl Fn(&str) -> Result<String, std::env::VarError>,
    ) -> anyhow::Result<Option<Self>> {
        let read = |name: &str| match read_env(name) {
            Ok(value) => Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty())),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(anyhow::anyhow!(
                "agent-workbench: {name} is not valid Unicode"
            )),
        };
        match read(AGENT_WORKBENCH_TTT_ENV)?.as_deref() {
            None | Some("off") => return Ok(None),
            Some("on") => {}
            Some(other) => anyhow::bail!(
                "agent-workbench: {AGENT_WORKBENCH_TTT_ENV} must be `on` or `off`, got `{other}`"
            ),
        }
        let mut config = Self::default();
        if let Some(seed) = read(AGENT_WORKBENCH_TTT_SEED_ENV)? {
            config.seed = seed.parse().map_err(|_| {
                anyhow::anyhow!(
                    "agent-workbench: {AGENT_WORKBENCH_TTT_SEED_ENV} must be an unsigned integer, got `{seed}`"
                )
            })?;
        }
        if let Some(schedule) = read(AGENT_WORKBENCH_TTT_SCHEDULE_ENV)? {
            config.schedule = Schedule::parse(&schedule).map_err(|error| {
                anyhow::anyhow!("agent-workbench: {AGENT_WORKBENCH_TTT_SCHEDULE_ENV}: {error}")
            })?;
        }
        config.first = match read(AGENT_WORKBENCH_TTT_FIRST_ENV)?.as_deref() {
            None | Some("agent") => First::Agent,
            Some("opponent") => First::Opponent,
            Some(other) => anyhow::bail!(
                "agent-workbench: {AGENT_WORKBENCH_TTT_FIRST_ENV} must be `agent` or `opponent`, got `{other}`"
            ),
        };
        Ok(Some(config))
    }
}

/// Who made a move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Player {
    Agent,
    Opponent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MoveRecord {
    pub(crate) by: Player,
    pub(crate) cell: usize,
}

/// One game as the log records it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct GameRecord {
    /// 1-based.
    pub(crate) game: u32,
    pub(crate) opponent: Strength,
    pub(crate) first: First,
    pub(crate) moves: Vec<MoveRecord>,
    /// `ongoing` for a game a later start abandoned.
    pub(crate) status: Status,
    /// The answer key's word for this game.
    pub(crate) result: Option<GameResult>,
    /// Calls to `make_move` that were refused, and changed nothing.
    pub(crate) illegal_moves: u32,
    /// The agent's moves (1-based ordinals) that lowered its minimax outcome:
    /// a won position played to a draw or loss, or a drawn one to a loss.
    pub(crate) misplays: Vec<usize>,
    pub(crate) picture: String,
}

#[derive(Clone, Debug)]
struct Game {
    number: u32,
    opponent: Strength,
    first: First,
    board: Board,
    moves: Vec<MoveRecord>,
    illegal_moves: u32,
    misplays: Vec<usize>,
    draws: SplitMix,
}

impl Game {
    fn new(config: &TttConfig, number: u32) -> Self {
        let mut game = Self {
            number,
            opponent: config.schedule.strength(config.seed, number),
            first: config.first,
            board: [None; 9],
            moves: Vec::new(),
            illegal_moves: 0,
            misplays: Vec::new(),
            draws: SplitMix::stream(config.seed, number, MOVE_STREAM),
        };
        if game.first == First::Opponent {
            game.opponent_move();
        }
        game
    }

    fn status(&self) -> Status {
        Status::of(&self.board)
    }

    fn opponent_move(&mut self) -> Option<usize> {
        let mark = AGENT.other();
        let cell = match self.opponent {
            Strength::Perfect => self.draws.pick(&best_cells(&self.board, mark)),
            Strength::Random => self.draws.pick(&empty_cells(&self.board)),
        }?;
        self.board[cell] = Some(mark);
        self.moves.push(MoveRecord {
            by: Player::Opponent,
            cell,
        });
        Some(cell)
    }

    /// Play the agent's `cell` and the opponent's reply. A refused move
    /// changes nothing but the refusal count.
    fn play(&mut self, cell: Option<i64>) -> Result<Option<usize>, String> {
        let status = self.status();
        if status != Status::Ongoing {
            self.illegal_moves += 1;
            return Err(format!(
                "game {} is over ({}); wait for the next game",
                self.number,
                status.label()
            ));
        }
        let Some(cell) = cell
            .and_then(|cell| usize::try_from(cell).ok())
            .filter(|cell| *cell < 9)
        else {
            self.illegal_moves += 1;
            return Err("`cell` must be an integer from 0 to 8".to_string());
        };
        if self.board[cell].is_some() {
            self.illegal_moves += 1;
            return Err(format!(
                "cell {cell} is taken; the empty cells are {:?}",
                empty_cells(&self.board)
            ));
        }
        let mut memo = HashMap::new();
        let before = outcome_class(value(&mut self.board.clone(), AGENT, &mut memo));
        self.board[cell] = Some(AGENT);
        let after = outcome_class(-value(&mut self.board.clone(), AGENT.other(), &mut memo));
        self.moves.push(MoveRecord {
            by: Player::Agent,
            cell,
        });
        if after < before {
            let ordinal = self
                .moves
                .iter()
                .filter(|record| record.by == Player::Agent)
                .count();
            self.misplays.push(ordinal);
        }
        if self.status() != Status::Ongoing {
            return Ok(None);
        }
        Ok(self.opponent_move())
    }

    fn picture(&self) -> String {
        self.board
            .chunks(3)
            .enumerate()
            .map(|(row, cells)| {
                cells
                    .iter()
                    .enumerate()
                    .map(|(column, mark)| match mark {
                        Some(mark) => mark.symbol().to_string(),
                        None => (row * 3 + column).to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The board as the tools return it.
    fn view(&self) -> Value {
        let status = self.status();
        json!({
            "game": self.number,
            "board": self
                .board
                .iter()
                .map(|mark| mark.map_or("", Mark::symbol))
                .collect::<Vec<_>>(),
            "picture": self.picture(),
            "empty_cells": empty_cells(&self.board),
            "to_move": if status == Status::Ongoing { "you" } else { "nobody: the game is over" },
            "status": status.label(),
        })
    }

    fn record(&self) -> GameRecord {
        let status = self.status();
        GameRecord {
            game: self.number,
            opponent: self.opponent,
            first: self.first,
            moves: self.moves.clone(),
            status,
            result: status.result(),
            illegal_moves: self.illegal_moves,
            misplays: self.misplays.clone(),
            picture: self.picture(),
        }
    }
}

#[derive(Default)]
struct WorldState {
    games: Vec<Game>,
    /// `make_move` results by tool-call id, so a redriven call returns its
    /// first result instead of moving again.
    moves_by_call_id: BTreeMap<String, Result<Value, String>>,
}

/// The shared tic-tac-toe world. A cloneable handle around the store.
#[derive(Clone)]
pub(crate) struct TttWorld {
    config: TttConfig,
    state: Arc<Mutex<WorldState>>,
}

impl TttWorld {
    pub(crate) fn new(config: TttConfig) -> Self {
        Self {
            config,
            state: Arc::default(),
        }
    }

    /// Start the next game; a game still in progress stays in the log as
    /// unfinished.
    pub(crate) fn start_game(&self) -> Value {
        let mut state = self.state.lock_recover();
        let number = state.games.len() as u32 + 1;
        let game = Game::new(&self.config, number);
        let view = game.view();
        state.games.push(game);
        view
    }

    pub(crate) fn log(&self) -> Vec<GameRecord> {
        self.state
            .lock_recover()
            .games
            .iter()
            .map(Game::record)
            .collect()
    }

    pub(crate) fn view_board(&self) -> Value {
        match self.state.lock_recover().games.last() {
            Some(game) => game.view(),
            None => json!({
                "game": null,
                "status": "no game in progress",
                "to_move": "nobody: no game has started",
            }),
        }
    }

    #[cfg(test)]
    fn make_move(&self, args: &Value) -> Result<Value, String> {
        make_move_in(&mut self.state.lock_recover(), args)
    }

    /// `make_move` at most once per tool-call id.
    fn make_move_once(&self, call_id: &str, args: &Value) -> Result<Value, String> {
        let mut state = self.state.lock_recover();
        if let Some(result) = state.moves_by_call_id.get(call_id) {
            return result.clone();
        }
        let result = make_move_in(&mut state, args);
        state
            .moves_by_call_id
            .insert(call_id.to_string(), result.clone());
        result
    }

    pub(crate) fn config(&self) -> &TttConfig {
        &self.config
    }
}

fn make_move_in(state: &mut WorldState, args: &Value) -> Result<Value, String> {
    let game = state
        .games
        .last_mut()
        .ok_or_else(|| "no game is in progress".to_string())?;
    let cell = args.get("cell").and_then(|cell| {
        cell.as_i64()
            .or_else(|| cell.as_str().and_then(|text| text.trim().parse().ok()))
    });
    let opponent_move = game.play(cell)?;
    let mut view = game.view();
    view["your_move"] = json!(cell);
    view["opponent_move"] = json!(opponent_move);
    Ok(view)
}

/// How well a memory answer matches the log.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Score {
    pub(crate) ask: usize,
    /// The log's answer key for the first `ask` games.
    pub(crate) expected: Vec<GameResult>,
    /// The first JSON array in the answer, its items normalized; `None` when
    /// the answer holds none.
    pub(crate) answer: Option<Vec<Option<GameResult>>>,
    pub(crate) per_game: Vec<bool>,
    pub(crate) correct: usize,
    pub(crate) accuracy: f64,
    /// The array has exactly `ask` items and every one is right.
    pub(crate) exact: bool,
}

/// Score `answer` against the first `ask` games of `log`. `ask` must be at
/// least 1 and below the number of games played.
pub(crate) fn score(answer: &str, log: &[GameRecord], ask: usize) -> Result<Score, String> {
    if ask == 0 || ask >= log.len() {
        return Err(format!(
            "ask about K games with 1 <= K < N; K = {ask}, N = {} games played",
            log.len()
        ));
    }
    if log.iter().any(|record| record.result.is_none()) {
        return Err("a run with an ongoing game is invalid and cannot be scored".to_string());
    }
    let expected: Vec<GameResult> = log[..ask]
        .iter()
        .filter_map(|record| record.result)
        .collect();
    let answer = first_json_array(answer).map(|items| {
        items
            .iter()
            .map(GameResult::from_answer)
            .collect::<Vec<_>>()
    });
    let per_game: Vec<bool> = expected
        .iter()
        .enumerate()
        .map(|(index, expected)| {
            answer.as_ref().is_some_and(|items| {
                items.len() == ask && items.get(index) == Some(&Some(*expected))
            })
        })
        .collect();
    let correct = per_game.iter().filter(|right| **right).count();
    let exact = answer.as_ref().is_some_and(|items| items.len() == ask) && correct == ask;
    Ok(Score {
        ask,
        expected,
        answer,
        per_game,
        correct,
        accuracy: correct as f64 / ask as f64,
        exact,
    })
}

/// The first `[` in `text` that opens a well-formed JSON array.
fn first_json_array(text: &str) -> Option<Vec<Value>> {
    text.match_indices('[').find_map(|(start, _)| {
        match serde_json::Deserializer::from_str(&text[start..])
            .into_iter::<Value>()
            .next()
        {
            Some(Ok(Value::Array(items))) => Some(items),
            _ => None,
        }
    })
}

/// Tool name for one operation, e.g. `ttt__make_move`.
fn tool_name(operation: &str) -> String {
    format!("ttt__{operation}")
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool schemas and admission checks their invariant"
)]
fn definition_for(operation: &str) -> ToolDefinition {
    let (input_schema, description, policy) = match operation {
        MAKE_MOVE => (
            json!({
                "type": "object",
                "properties": {
                    "cell": {
                        "type": "integer",
                        "description": "0-8, row by row: 0 1 2 / 3 4 5 / 6 7 8"
                    }
                },
                "required": ["cell"],
                "additionalProperties": false
            }),
            "Tic-tac-toe: place your X on `cell` (0-8, row by row: 0 1 2 / 3 4 5 / 6 7 8). \
             The opponent (O) answers at once. Returns the board after both moves, \
             `your_move`, `opponent_move` and `status`: `ongoing`, `you won`, `opponent won` \
             or `draw`. An illegal move is an error and changes nothing.",
            ExecutionPolicy::Once,
        ),
        _ => (
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            "Tic-tac-toe: the current game's number, `board` (9 cells, row by row, \"X\", \"O\" \
             or \"\"), a `picture` with empty cells shown by number, `empty_cells`, `to_move` \
             and `status`. It shows the current game only.",
            ExecutionPolicy::repeatable(
                std::num::NonZeroU32::new(3).expect("nonzero attempt bound"),
                25,
                250,
            ),
        ),
    };
    let name = tool_name(operation);
    ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        input_schema,
        json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution_policy(policy)
    .with_tool_binding(ToolBinding::new(["ttt"], operation))
}

/// The two tic-tac-toe tools over one world.
pub(crate) struct TttProvider {
    world: TttWorld,
}

impl TttProvider {
    pub(crate) fn new(world: TttWorld) -> Self {
        Self { world }
    }
}

#[async_trait]
impl ToolProvider for TttProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        OPERATIONS
            .iter()
            .map(|operation| definition_for(operation).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let operation = OPERATIONS
            .into_iter()
            .find(|operation| tool_name(operation) == name)?;
        Some(Arc::new(definition_for(operation).contract()))
    }

    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> ToolAttemptOutcome {
        let result = if call.name() == tool_name(VIEW_BOARD) {
            Ok(self.world.view_board())
        } else if call.name() == tool_name(MAKE_MOVE) {
            self.world
                .make_move_once(call.context.call_id().as_str(), call.args)
        } else {
            Err(format!("unknown tic-tac-toe tool `{}`", call.name()))
        };
        match result {
            Ok(value) => ToolOutcome::ok(value).into(),
            Err(message) => ToolOutcome::err_fmt(message).into(),
        }
    }
}

/// The tools as the system prompt teaches them, in the session's protocol.
pub(crate) fn prompt(protocol: crate::session_protocol::SessionProtocol) -> String {
    let (view, make) = match protocol {
        crate::session_protocol::SessionProtocol::Rlm => (
            "`await ttt.view_board({})`",
            "`await ttt.make_move({ cell: 4 })`",
        ),
        crate::session_protocol::SessionProtocol::Standard => {
            ("the `ttt__view_board` tool", "the `ttt__make_move` tool")
        }
    };
    format!(
        "Tic-tac-toe: the user may ask you to play games of tic-tac-toe against them. You are X; \
         cells are numbered 0-8 row by row (0 1 2 / 3 4 5 / 6 7 8).\n\
         - {view} returns the current game: `game` (its number), `board` (9 cells, \"X\", \"O\" \
         or \"\"), `picture`, `empty_cells`, `to_move` and `status`. It shows the current game \
         only.\n\
         - {make} places your X on that cell; your opponent (O) answers inside the same call. \
         It returns the board after both moves, `your_move`, `opponent_move` and `status`: \
         `ongoing`, `you won`, `opponent won` or `draw`. An illegal move (a taken cell, a cell \
         outside 0-8, or a game that is over) is an error and changes nothing.\n\
         The user starts each game; play it to the end, then tell the user how it ended."
    )
}

/// `GET /api/ttt` (settings and the game log), `POST /api/ttt/games` (start
/// the next game) and `POST /api/ttt/score` (score a memory answer).
pub(crate) fn router(world: TttWorld) -> Router {
    Router::new()
        .route("/api/ttt", get(log_route))
        .route("/api/ttt/games", post(start_route))
        .route("/api/ttt/score", post(score_route))
        .with_state(world)
}

async fn log_route(State(world): State<TttWorld>) -> Json<Value> {
    Json(json!({ "config": world.config(), "games": world.log() }))
}

async fn start_route(State(world): State<TttWorld>) -> Json<Value> {
    Json(world.start_game())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoreRequest {
    answer: String,
    ask: usize,
}

async fn score_route(
    State(world): State<TttWorld>,
    Json(request): Json<ScoreRequest>,
) -> Result<Json<Score>, (StatusCode, String)> {
    score(&request.answer, &world.log(), request.ask)
        .map(Json)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world(schedule: Schedule, seed: u64, first: First) -> TttWorld {
        TttWorld::new(TttConfig {
            seed,
            schedule,
            first,
        })
    }

    fn play(world: &TttWorld, cell: i64) -> Result<Value, String> {
        world.make_move(&json!({ "cell": cell }))
    }

    /// The outcome of every game the agent can play from `game`, by every
    /// sequence of legal agent moves.
    fn outcomes_from(game: &Game, outcomes: &mut Vec<Status>) {
        if game.status() != Status::Ongoing {
            outcomes.push(game.status());
            return;
        }
        for cell in empty_cells(&game.board) {
            let mut next = game.clone();
            next.play(Some(cell as i64)).expect("a legal move");
            outcomes_from(&next, outcomes);
        }
    }

    #[test]
    fn a_line_wins_a_full_board_without_one_draws_and_neither_is_ongoing() {
        use Mark::{O, X};
        let mut board: Board = [None; 9];
        assert_eq!(Status::of(&board), Status::Ongoing);
        for line in LINES {
            let mut won = board;
            for cell in line {
                won[cell] = Some(X);
            }
            assert_eq!(Status::of(&won), Status::AgentWon, "{line:?}");
            for cell in line {
                won[cell] = Some(O);
            }
            assert_eq!(Status::of(&won), Status::OpponentWon, "{line:?}");
        }
        board = [
            Some(X),
            Some(O),
            Some(X),
            Some(X),
            Some(O),
            Some(O),
            Some(O),
            Some(X),
            Some(X),
        ];
        assert_eq!(Status::of(&board), Status::Draw);
        assert_eq!(Status::Draw.result(), Some(GameResult::Draw));
        assert_eq!(Status::AgentWon.result(), Some(GameResult::Assistant));
        assert_eq!(Status::OpponentWon.result(), Some(GameResult::User));
    }

    #[test]
    fn an_illegal_move_is_an_error_and_changes_nothing() {
        let world = world(Schedule::Every(Strength::Random), 1, First::Agent);
        assert!(play(&world, 4).is_err(), "no game in progress");
        world.start_game();
        let before = world.view_board();
        for bad in [json!({ "cell": 9 }), json!({ "cell": -1 }), json!({})] {
            assert!(world.make_move(&bad).is_err(), "{bad}");
        }
        assert_eq!(world.view_board(), before);
        let after_first = play(&world, 4).expect("a legal move");
        let taken = after_first["opponent_move"]
            .as_i64()
            .expect("opponent moved");
        assert!(play(&world, 4).is_err(), "own cell");
        assert!(play(&world, taken).is_err(), "opponent's cell");
        let board = world.view_board();
        assert_eq!(board["board"], after_first["board"]);
        assert_eq!(world.log()[0].illegal_moves, 5);
        assert_eq!(world.log()[0].moves.len(), 2);

        // A finished game refuses further moves.
        let mut status = after_first["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        while status == "ongoing" {
            let cell = world.view_board()["empty_cells"][0]
                .as_i64()
                .expect("an empty cell");
            status = play(&world, cell).expect("legal")["status"]
                .as_str()
                .unwrap_or_default()
                .to_string();
        }
        let finished = world.view_board();
        let empty = finished["empty_cells"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(cell) = empty.first().and_then(Value::as_i64) {
            assert!(play(&world, cell).is_err(), "the game is over");
        }
        assert_eq!(world.view_board(), finished);
    }

    #[test]
    fn the_perfect_opponent_never_loses() {
        for first in [First::Agent, First::Opponent] {
            let config = TttConfig {
                seed: 7,
                schedule: Schedule::Every(Strength::Perfect),
                first,
            };
            let mut outcomes = Vec::new();
            outcomes_from(&Game::new(&config, 1), &mut outcomes);
            assert!(!outcomes.is_empty());
            assert!(
                !outcomes.contains(&Status::AgentWon),
                "{first:?}: the agent beat the perfect opponent"
            );
            assert!(outcomes.contains(&Status::Draw), "{first:?}");
            assert!(outcomes.contains(&Status::OpponentWon), "{first:?}");
        }
    }

    /// The opponent's moves in game 1 when the agent always takes the lowest
    /// empty cell.
    fn opponent_moves(schedule: Schedule, seed: u64) -> Vec<usize> {
        let world = world(schedule, seed, First::Agent);
        world.start_game();
        while world.view_board()["status"] == "ongoing" {
            let cell = world.view_board()["empty_cells"][0]
                .as_i64()
                .expect("an empty cell");
            play(&world, cell).expect("legal");
        }
        world.log()[0]
            .moves
            .iter()
            .filter(|record| record.by == Player::Opponent)
            .map(|record| record.cell)
            .collect()
    }

    #[test]
    fn the_random_opponent_is_reproducible_for_a_seed() {
        let random = || Schedule::Every(Strength::Random);
        assert_eq!(opponent_moves(random(), 1), opponent_moves(random(), 1));
        assert_eq!(opponent_moves(random(), 42), opponent_moves(random(), 42));
        let seeds: Vec<Vec<usize>> = (1..=8).map(|seed| opponent_moves(random(), seed)).collect();
        assert!(
            seeds.iter().any(|moves| *moves != seeds[0]),
            "different seeds must draw different games"
        );
    }

    #[test]
    fn the_mixed_schedule_is_reproducible_for_a_seed() {
        let draw = |seed| -> Vec<Strength> {
            (1..=20)
                .map(|game| Schedule::Mixed.strength(seed, game))
                .collect()
        };
        assert_eq!(draw(1), draw(1));
        assert_eq!(draw(9), draw(9));
        assert!(draw(1).contains(&Strength::Perfect) && draw(1).contains(&Strength::Random));
        assert_ne!(draw(1), draw(2));

        // The world follows the schedule, game by game.
        let world = world(Schedule::Mixed, 1, First::Agent);
        for _ in 0..20 {
            world.start_game();
        }
        let played: Vec<Strength> = world.log().iter().map(|record| record.opponent).collect();
        assert_eq!(played, draw(1));

        assert_eq!(
            Schedule::parse("perfect,random")
                .expect("a list")
                .strength(1, 3),
            Strength::Perfect
        );
        assert!(Schedule::parse("perfect,strong").is_err());
    }

    #[test]
    fn the_log_records_each_games_result() {
        let world = world(
            Schedule::List(vec![Strength::Perfect, Strength::Random]),
            3,
            First::Agent,
        );
        let mut statuses = Vec::new();
        for _ in 0..4 {
            world.start_game();
            let mut status = String::from("ongoing");
            while status == "ongoing" {
                let cell = world.view_board()["empty_cells"][0]
                    .as_i64()
                    .expect("an empty cell");
                status = play(&world, cell).expect("legal")["status"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
            }
            statuses.push(status);
        }
        world.start_game();
        let log = world.log();
        assert_eq!(log.len(), 5);
        for (record, status) in log.iter().zip(&statuses) {
            assert_eq!(record.status.label(), status, "game {}", record.game);
            let mut board: Board = [None; 9];
            for record in &record.moves {
                board[record.cell] = Some(match record.by {
                    Player::Agent => AGENT,
                    Player::Opponent => AGENT.other(),
                });
            }
            assert_eq!(Status::of(&board), record.status, "game {}", record.game);
        }
        assert_eq!(log[4].status, Status::Ongoing);
        assert_eq!(log[4].result, None);
        assert!(score(r#"["ASSISTANT"]"#, &log, 1).is_err());
        for record in &log[..4] {
            assert_eq!(record.result, record.status.result());
        }
        assert_eq!(
            log.iter().map(|record| record.game).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
    }

    fn record(game: u32, status: Status) -> GameRecord {
        GameRecord {
            game,
            opponent: Strength::Random,
            first: First::Agent,
            moves: Vec::new(),
            status,
            result: status.result(),
            illegal_moves: 0,
            misplays: Vec::new(),
            picture: String::new(),
        }
    }

    #[test]
    fn the_scorer_reads_the_first_json_array_and_scores_it_against_the_log() {
        let log = vec![
            record(1, Status::AgentWon),
            record(2, Status::Draw),
            record(3, Status::OpponentWon),
            record(4, Status::Draw),
        ];

        use GameResult::{Assistant, Draw, User};
        let exact = score(
            "Here you go: [maybe] ```json\n[\" assistant \", \"Draw\", \"user\"]\n```",
            &log,
            3,
        )
        .expect("scored");
        assert_eq!(exact.expected, [Assistant, Draw, User]);
        assert_eq!(
            exact.answer,
            Some(vec![Some(Assistant), Some(Draw), Some(User)])
        );
        assert!(exact.exact);
        assert_eq!(exact.correct, 3);
        assert_eq!(
            serde_json::to_value(&exact.expected).expect("typed key"),
            json!(["ASSISTANT", "DRAW", "USER"])
        );

        let partial = score(r#"["ASSISTANT", "USER", "USER"]"#, &log, 3).expect("scored");
        assert_eq!(partial.per_game, [true, false, true]);
        assert!(!partial.exact);
        assert!((partial.accuracy - 2.0 / 3.0).abs() < 1e-9);

        for invalid in [
            r#"["you", "DRAW", "me"]"#,
            r#"[null, "DRAW", 1]"#,
            r#"["unknown", "DRAW", "?"]"#,
        ] {
            let scored = score(invalid, &log, 3).expect("invalid items score wrong");
            assert_eq!(scored.per_game, [false, true, false]);
        }
        for wrong_length in [
            r#"["ASSISTANT", "DRAW"]"#,
            r#"["ASSISTANT", "DRAW", "USER", "DRAW"]"#,
        ] {
            let scored = score(wrong_length, &log, 3).expect("scored");
            assert_eq!(scored.correct, 0, "the array must have exactly K items");
            assert!(!scored.exact);
        }

        let malformed = score("I won the first, then we drew: [ASSISTANT, DRAW", &log, 3)
            .expect("a malformed answer still scores");
        assert_eq!(malformed.answer, None);
        assert_eq!(malformed.correct, 0);
        assert!(!malformed.exact);

        assert!(score("[]", &log, 0).is_err(), "K must be at least 1");
        assert!(score("[]", &log, 4).is_err(), "K must be below N");
    }
}
