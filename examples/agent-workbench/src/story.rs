//! Seeded, lazy story tree and authoritative memory quiz for FIG-4441.
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use lash::{
    sync::MutexExt,
    tools::{
        ExecutionPolicy, ToolAttemptOutcome, ToolBinding, ToolContract, ToolDefinition,
        ToolDefinitionBindingExt, ToolManifest, ToolOutcome, ToolProvider,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StoryConfig {
    seed: u64,
    branching: usize,
    vocabulary_seed: u64,
}
impl StoryConfig {
    pub(crate) fn from_env() -> anyhow::Result<Option<Self>> {
        match std::env::var("AGENT_WORKBENCH_STORY")
            .unwrap_or_default()
            .as_str()
        {
            "" | "off" => return Ok(None),
            "on" => {}
            value => anyhow::bail!("AGENT_WORKBENCH_STORY must be on or off, got {value}"),
        }
        let number = |key, default: &str| -> anyhow::Result<u64> {
            Ok(std::env::var(key)
                .unwrap_or_else(|_| default.to_owned())
                .parse()?)
        };
        let branching = usize::try_from(number("AGENT_WORKBENCH_STORY_BRANCHING", "3")?)?;
        anyhow::ensure!(
            (2..=26).contains(&branching),
            "story branching must be 2..26"
        );
        Ok(Some(Self {
            seed: number("AGENT_WORKBENCH_STORY_SEED", "1")?,
            branching,
            vocabulary_seed: number("AGENT_WORKBENCH_STORY_VOCABULARY_SEED", "1")?,
        }))
    }
}

struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn index(&mut self, length: usize) -> usize {
        (self.next() % length as u64) as usize
    }
}
const COLORS: [&str; 16] = [
    "Amber", "Azure", "Bronze", "Coral", "Crimson", "Emerald", "Golden", "Indigo", "Ivory", "Jade",
    "Lilac", "Onyx", "Pearl", "Silver", "Violet", "White",
];
const ROOTS: [&str; 16] = [
    "Ash", "Birch", "Cedar", "Dawn", "Elm", "Fern", "Flint", "Hazel", "Iris", "Juniper", "Laurel",
    "Maple", "Moss", "Oak", "Reed", "Willow",
];
const PLACES: [&str; 16] = [
    "Mill", "Tower", "Bridge", "Harbor", "Garden", "Library", "Gate", "Market", "Cave", "Palace",
    "Forge", "Inn", "Temple", "Quay", "Vault", "Well",
];
const PEOPLE: [&str; 16] = [
    "Ada", "Bram", "Cleo", "Dara", "Esme", "Finn", "Gita", "Hugo", "Ida", "Joss", "Kira", "Leon",
    "Mira", "Nico", "Orla", "Pavel",
];
const ITEMS: [&str; 16] = [
    "Bell", "Compass", "Crown", "Flute", "Gem", "Key", "Lantern", "Map", "Medal", "Mirror", "Ring",
    "Scroll", "Shell", "Spindle", "Token", "Whistle",
];
fn token(rng: &mut Random, ends: &[&str]) -> String {
    format!(
        "{} {} {}",
        COLORS[rng.index(COLORS.len())],
        ROOTS[rng.index(ROOTS.len())],
        ends[rng.index(ends.len())]
    )
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Facts {
    place: String,
    person: String,
    item: String,
    code: u64,
    coin_delta: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Choice {
    option: String,
    label: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Node {
    path: Vec<usize>,
    facts: Facts,
    passage: String,
    choices: Vec<Choice>,
}
fn node(config: &StoryConfig, path: &[usize]) -> Node {
    let mut rng = Random(config.seed ^ config.vocabulary_seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
    for &option in path {
        rng.0 = rng.next() ^ (option as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
    }
    let facts = Facts {
        place: token(&mut rng, &PLACES),
        person: token(&mut rng, &PEOPLE),
        item: token(&mut rng, &ITEMS),
        code: 10000 + rng.next() % 90000,
        coin_delta: rng.index(41) as i64 - 20,
    };
    let passage = format!(
        "At {}, you meet {} carrying a {}. The door code is {}. Your coins change by {:+} when you leave.",
        facts.place, facts.person, facts.item, facts.code, facts.coin_delta
    );
    let choices = (0..config.branching)
        .map(|i| Choice {
            option: ((b'A' + i as u8) as char).to_string(),
            label: format!(
                "Take the {} {} trail",
                COLORS[rng.index(16)],
                ROOTS[rng.index(16)]
            ),
        })
        .collect();
    Node {
        path: path.to_vec(),
        facts,
        passage,
        choices,
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Round {
    round: usize,
    node: Node,
    choice: Option<String>,
    coins: i64,
}
#[derive(Default)]
struct WorldState {
    path: Vec<usize>,
    rounds: Vec<Round>,
    coins: i64,
    calls: BTreeMap<String, Result<Value, String>>,
}
#[derive(Clone)]
pub(crate) struct StoryWorld {
    config: StoryConfig,
    state: Arc<Mutex<WorldState>>,
}
impl StoryWorld {
    pub(crate) fn new(config: StoryConfig) -> Self {
        Self {
            config,
            state: Arc::default(),
        }
    }
    fn start(&self) -> Result<Value, String> {
        let mut state = self.state.lock_recover();
        if state.rounds.last().is_some_and(|r| r.choice.is_none()) {
            return Err("the current round is unfinished".into());
        }
        let round = Round {
            round: state.rounds.len() + 1,
            node: node(&self.config, &state.path),
            choice: None,
            coins: state.coins,
        };
        state.rounds.push(round);
        Ok(self.read_in(&state))
    }
    fn read_in(&self, state: &WorldState) -> Value {
        let current = node(&self.config, &state.path);
        let choice = state.rounds.last().and_then(|r| r.choice.as_ref());
        json!({
            "round": state.rounds.len(), "round_finished": choice.is_some(),
            "passage_round": state.rounds.len() + usize::from(choice.is_some()),
            "choice": choice, "coins": state.coins, "path": current.path,
            "passage": current.passage, "facts": current.facts, "choices": current.choices,
        })
    }
    fn choose(&self, call_id: &str, args: &Value) -> Result<Value, String> {
        let mut state = self.state.lock_recover();
        if let Some(result) = state.calls.get(call_id) {
            return result.clone();
        }
        let result = self.choose_in(&mut state, args);
        state.calls.insert(call_id.to_owned(), result.clone());
        result
    }
    fn choose_in(&self, state: &mut WorldState, args: &Value) -> Result<Value, String> {
        let round = state.rounds.last_mut().ok_or("no round has started")?;
        if round.choice.is_some() {
            return Err("this round already has a choice; wait for the next round".into());
        }
        let option = args
            .get("option")
            .and_then(Value::as_str)
            .ok_or("option must be a choice label")?;
        let index = round
            .node
            .choices
            .iter()
            .position(|c| c.option == option)
            .ok_or("invalid option; choose one of the listed option labels")?;
        round.choice = Some(option.to_owned());
        state.coins += round.node.facts.coin_delta;
        round.coins = state.coins;
        state.path.push(index);
        Ok(self.read_in(state))
    }
    fn log(&self) -> Vec<Round> {
        self.state.lock_recover().rounds.clone()
    }
    fn questions(&self, request: &QuizRequest) -> Result<Vec<Question>, String> {
        questions(&self.config, &self.log(), request)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Recall,
    Order,
    State,
    Negative,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Recall => "recall",
            Self::Order => "order",
            Self::State => "state",
            Self::Negative => "negative",
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuizRequest {
    seed: u64,
    questions: usize,
    types: Vec<Kind>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Question {
    id: String,
    #[serde(rename = "type")]
    kind: Kind,
    prompt: String,
    lookback: usize,
    fact_round: usize,
    expected: Value,
    source_path: Vec<usize>,
}
fn questions(
    config: &StoryConfig,
    log: &[Round],
    request: &QuizRequest,
) -> Result<Vec<Question>, String> {
    if log.is_empty() || log.iter().any(|r| r.choice.is_none()) {
        return Err("only a completed path can be quizzed".into());
    }
    if request.questions == 0 || request.types.is_empty() {
        return Err("questions and type mix must be nonempty".into());
    }
    let mut pairs = Vec::new();
    for a in 0..log.len() {
        for b in a + 1..log.len() {
            if log[a].node.facts.place != log[b].node.facts.place {
                pairs.push((a, b));
            }
        }
    }
    let orders = (0..request.questions)
        .filter(|i| request.types[i % request.types.len()] == Kind::Order)
        .count();
    if orders > pairs.len() {
        return Err("more order questions than distinct path pairs".into());
    }
    let mut rng = Random(request.seed ^ 0x5155_495A);
    let mut result = Vec::new();
    for i in 0..request.questions {
        let kind = request.types[i % request.types.len()];
        let index = rng.index(log.len());
        let round = &log[index];
        let facts = &round.node.facts;
        let mut source_path = round.node.path.clone();
        let (prompt, expected, fact_round) = match kind {
            Kind::Recall => match rng.index(4) {
                0 => (
                    format!("Who did you meet in round {}?", round.round),
                    json!(facts.person),
                    round.round,
                ),
                1 => (
                    format!("What item did you see in round {}?", round.round),
                    json!(facts.item),
                    round.round,
                ),
                2 => (
                    format!(
                        "What was the door code in round {} at {}?",
                        round.round, facts.place
                    ),
                    json!(facts.code),
                    round.round,
                ),
                _ => (
                    format!("Which place did you visit in round {}?", round.round),
                    json!(facts.place),
                    round.round,
                ),
            },
            Kind::Order => {
                let (a, b) = pairs.swap_remove(rng.index(pairs.len()));
                source_path = log[a].node.path.clone();
                let (x, y) = if rng.index(2) == 0 { (a, b) } else { (b, a) };
                (
                    format!(
                        "Which did you visit first, {} or {}?",
                        log[x].node.facts.place, log[y].node.facts.place
                    ),
                    json!(log[a].node.facts.place),
                    log[a].round,
                )
            }
            Kind::State => (
                format!("How many coins did you have after round {}?", round.round),
                json!(round.coins),
                round.round,
            ),
            Kind::Negative => {
                let no = (i / request.types.len()).is_multiple_of(2);
                if no {
                    // A sibling of the chosen destination, never the destination itself.
                    let mut candidates = Vec::new();
                    for parent in log {
                        let chosen = parent
                            .node
                            .choices
                            .iter()
                            .position(|c| Some(&c.option) == parent.choice.as_ref())
                            .ok_or("missing logged choice")?;
                        for option in 0..config.branching {
                            if option == chosen {
                                continue;
                            }
                            let mut path = parent.node.path.clone();
                            path.push(option);
                            let sibling = node(config, &path);
                            if !log
                                .iter()
                                .any(|r| r.node.facts.place == sibling.facts.place)
                            {
                                candidates.push((parent.round, sibling));
                            }
                        }
                    }
                    if candidates.is_empty() {
                        return Err("no unvisited sibling place available".into());
                    }
                    let (seen, sibling) = candidates.swap_remove(rng.index(candidates.len()));
                    source_path = sibling.path;
                    (
                        format!("Did you visit {}? Answer YES or NO.", sibling.facts.place),
                        json!("NO"),
                        seen,
                    )
                } else {
                    (
                        format!("Did you visit {}? Answer YES or NO.", facts.place),
                        json!("YES"),
                        round.round,
                    )
                }
            }
        };
        result.push(Question {
            id: format!("q{}", i + 1),
            kind,
            prompt,
            lookback: log.len() - fact_round,
            fact_round,
            expected,
            source_path,
        });
    }
    Ok(result)
}
fn normalize(value: &Value, kind: Kind) -> Option<String> {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let text = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if kind == Kind::Negative {
        return match text.as_str() {
            "yes" | "y" | "true" | "1" => Some("yes".into()),
            "no" | "n" | "false" | "0" => Some("no".into()),
            _ => None,
        };
    }
    let numeric = text.replace([',', '_', ' '], "");
    if let Ok(n) = numeric.parse::<f64>()
        && n.is_finite()
    {
        return Some(n.to_string());
    }
    Some(text)
}
fn score(answer: &str, questions: &[Question]) -> Value {
    let object = answer.match_indices('{').find_map(|(start, _)| {
        serde_json::Deserializer::from_str(&answer[start..])
            .into_iter::<Value>()
            .next()
            .and_then(Result::ok)
            .filter(Value::is_object)
    });
    let mut by_type: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut by_lookback: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut correct_count = 0;
    let rows: Vec<Value> = questions.iter().map(|q| {
        let given = object.as_ref().and_then(|o| o.get(&q.id));
        let correct = given.and_then(|v| normalize(v,q.kind)).is_some_and(|v| Some(v)==normalize(&q.expected,q.kind));
        correct_count += usize::from(correct);
        let bucket = match q.lookback { 0..=3 => "0-3", 4..=7 => "4-7", _ => "8+" };
        for counter in [by_type.entry(q.kind.label().into()).or_default(), by_lookback.entry(bucket.into()).or_default()] { counter.0 += usize::from(correct); counter.1 += 1; }
        json!({"id":q.id, "type":q.kind, "prompt":q.prompt, "lookback":q.lookback, "expected":q.expected, "given":given, "correct":correct})
    }).collect();
    let breakdown = |map: BTreeMap<String, (usize, usize)>| -> Value {
        map.into_iter().map(|(key,(correct,total))| (key,json!({"correct":correct,"total":total,"accuracy":correct as f64 / total as f64}))).collect()
    };
    json!({"questions":rows, "correct":correct_count, "total":questions.len(), "accuracy":correct_count as f64 / questions.len() as f64, "by_type":breakdown(by_type), "by_lookback":breakdown(by_lookback)})
}

#[expect(
    clippy::expect_used,
    reason = "static tool schemas and nonzero attempt bound"
)]
fn definition(operation: &str) -> ToolDefinition {
    let choose = operation == "choose";
    ToolDefinition::raw(format!("tool:story__{operation}"), format!("story__{operation}"),
        if choose { "Choose exactly one listed option (A, B, ...). Returns the next round's passage with round_finished=true. That completes this user round: answer now and wait for the next USER turn. Execution steps are not new rounds. Invalid options change nothing." } else { "Read the current story: passage, facts, choices (option and label), coins, round (user round), passage_round and round_finished. If round_finished=true, answer now; wait for the next USER turn. Current node only; no history." },
        if choose { json!({"type":"object","properties":{"option":{"type":"string"}},"required":["option"],"additionalProperties":false}) } else { json!({"type":"object","properties":{},"additionalProperties":false}) }, json!({"type":"object"}))
        .expect("valid story schema").with_execution_policy(if choose { ExecutionPolicy::Once } else { ExecutionPolicy::repeatable(std::num::NonZeroU32::new(3).expect("nonzero"),25,250) })
        .with_tool_binding(ToolBinding::new(["story"],operation))
}
pub(crate) struct StoryProvider {
    world: StoryWorld,
}
impl StoryProvider {
    pub(crate) fn new(world: StoryWorld) -> Self {
        Self { world }
    }
}
#[async_trait]
impl ToolProvider for StoryProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        ["read", "choose"]
            .iter()
            .map(|op| definition(op).manifest())
            .collect()
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        ["read", "choose"]
            .iter()
            .find(|op| name == format!("story__{op}"))
            .map(|op| Arc::new(definition(op).contract()))
    }
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> ToolAttemptOutcome {
        let result = match call.name() {
            "story__read" => Ok(self.world.read_in(&self.world.state.lock_recover())),
            "story__choose" => self
                .world
                .choose(call.context.call_id().as_str(), call.args),
            name => Err(format!("unknown story tool {name}")),
        };
        match result {
            Ok(v) => ToolOutcome::ok(v).into(),
            Err(e) => ToolOutcome::err_fmt(e).into(),
        }
    }
}
pub(crate) fn prompt(protocol: crate::session_protocol::SessionProtocol) -> String {
    let (read, choose) = match protocol {
        crate::session_protocol::SessionProtocol::Rlm => (
            "await story.read({})",
            "await story.choose({ option: \"A\" })",
        ),
        crate::session_protocol::SessionProtocol::Standard => ("story__read", "story__choose"),
    };
    format!(
        "Choose-your-own-adventure: start with zero coins. {read} reads your current passage and labelled choices. {choose} selects a listed option and returns the new passage. Choose exactly once per USER turn, then answer immediately. The result sets round_finished=true. Your execution steps are not new rounds: do not make another choice until the user sends the next round. Read returns top-level passage, facts, choices, coins, round, passage_round, round_finished and choice. The choices array contains option and label. The coins of the passage you leave are added on choosing; the returned passage belongs to the next round. An invalid choice changes nothing. No tool exposes past nodes or the journey log. If the passage is supplied in the user's message, you may choose directly."
    )
}
pub(crate) fn router(world: StoryWorld) -> Router {
    Router::new()
        .route("/api/story", get(log_route))
        .route("/api/story/rounds", post(start_route))
        .route("/api/story/questions", post(questions_route))
        .route("/api/story/score", post(score_route))
        .with_state(world)
}
type ApiResult = Result<Json<Value>, (StatusCode, String)>;
fn api(result: Result<Value, String>) -> ApiResult {
    result.map(Json).map_err(|e| (StatusCode::BAD_REQUEST, e))
}
async fn log_route(State(world): State<StoryWorld>) -> Json<Value> {
    Json(json!({"config":world.config,"rounds":world.log()}))
}
async fn start_route(State(world): State<StoryWorld>) -> ApiResult {
    api(world.start())
}
async fn questions_route(
    State(world): State<StoryWorld>,
    Json(request): Json<QuizRequest>,
) -> ApiResult {
    api(world.questions(&request).map(|q| json!(q)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoreRequest {
    answer: String,
    seed: u64,
    questions: usize,
    types: Vec<Kind>,
}
async fn score_route(
    State(world): State<StoryWorld>,
    Json(request): Json<ScoreRequest>,
) -> ApiResult {
    api(world
        .questions(&QuizRequest {
            seed: request.seed,
            questions: request.questions,
            types: request.types,
        })
        .map(|q| score(&request.answer, &q)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> StoryConfig {
        StoryConfig {
            seed: 1,
            branching: 3,
            vocabulary_seed: 1,
        }
    }
    fn played() -> StoryWorld {
        let world = StoryWorld::new(config());
        for i in 0..12 {
            world.start().expect("start");
            world
                .choose(&format!("c{i}"), &json!({"option":(["A","B","C"][i%3])}))
                .expect("choose");
        }
        world
    }
    fn quiz() -> QuizRequest {
        QuizRequest {
            seed: 1,
            questions: 12,
            types: vec![Kind::Recall, Kind::Order, Kind::State, Kind::Negative],
        }
    }
    #[test]
    fn the_same_seed_gives_the_same_tree_and_passages() {
        for path in [vec![], vec![0], vec![2, 1, 0], vec![1; 10000]] {
            assert_eq!(node(&config(), &path), node(&config(), &path));
            assert_ne!(
                node(&config(), &path),
                node(
                    &StoryConfig {
                        seed: 2,
                        ..config()
                    },
                    &path
                )
            );
            assert_ne!(
                node(&config(), &path),
                node(
                    &StoryConfig {
                        vocabulary_seed: 2,
                        ..config()
                    },
                    &path
                )
            );
        }
        assert_ne!(node(&config(), &[0]), node(&config(), &[1]));
    }
    #[test]
    fn an_invalid_choice_changes_nothing() {
        let world = StoryWorld::new(config());
        assert!(world.choose("before", &json!({"option":"A"})).is_err());
        world.start().expect("start");
        let before = world.read_in(&world.state.lock_recover());
        for (i, args) in [
            json!({"option":"Z"}),
            json!({"option":0}),
            json!({}),
            json!({"option":"a"}),
        ]
        .iter()
        .enumerate()
        {
            assert!(world.choose(&format!("bad{i}"), args).is_err());
            assert_eq!(world.read_in(&world.state.lock_recover()), before);
            assert!(world.log()[0].choice.is_none());
        }
        let moved = world.choose("ok", &json!({"option":"B"})).expect("valid");
        assert_eq!(
            world.choose("ok", &json!({"option":"B"})).expect("replay"),
            moved
        );
        assert!(world.choose("extra", &json!({"option":"A"})).is_err());
        assert_eq!(world.read_in(&world.state.lock_recover()), moved);
    }
    #[test]
    fn the_path_log_matches_the_moves() {
        let live = StoryWorld::new(config());
        let opened = live.start().expect("open round");
        assert_eq!(opened["round_finished"], json!(false));
        assert_eq!(opened["passage_round"], json!(1));
        let moved = live
            .choose("first", &json!({"option":"A"}))
            .expect("choose");
        assert_eq!(moved["round_finished"], json!(true));
        assert_eq!(moved["passage_round"], json!(2));
        assert_eq!(moved["choices"].as_array().expect("choices").len(), 3);
        assert_eq!(moved["round"], json!(1));
        assert_eq!(
            live.start().expect("next round")["round_finished"],
            json!(false)
        );
        let world = played();
        let mut path = Vec::new();
        let mut coins = 0;
        for (i, round) in world.log().iter().enumerate() {
            assert_eq!(round.round, i + 1);
            assert_eq!(round.node, node(&config(), &path));
            assert_eq!(round.choice.as_deref(), Some(["A", "B", "C"][i % 3]));
            coins += round.node.facts.coin_delta;
            assert_eq!(round.coins, coins);
            path.push(i % 3);
        }
        assert_eq!(world.state.lock_recover().path, path);
        assert_eq!(world.state.lock_recover().coins, coins);
    }
    #[test]
    fn the_question_generator_is_reproducible_and_uses_only_the_path_or_unvisited_siblings() {
        let world = played();
        let log = world.log();
        let questions = world.questions(&quiz()).expect("quiz");
        assert_eq!(questions, world.questions(&quiz()).expect("repeat"));
        assert!(questions.iter().any(|q| q.expected == json!("YES")));
        assert!(questions.iter().any(|q| q.expected == json!("NO")));
        for q in &questions {
            if q.kind == Kind::Negative && q.expected == json!("NO") {
                assert!(!log.iter().any(|r| r.node.path == q.source_path));
                assert!(
                    log.iter()
                        .any(|r| r.node.path == q.source_path[..q.source_path.len() - 1])
                );
                let place = node(&config(), &q.source_path).facts.place;
                assert!(q.prompt.contains(&place));
                assert!(!log.iter().any(|r| r.node.facts.place == place));
            } else {
                assert!(log.iter().any(|r| r.node.path == q.source_path));
            }
            match q.kind {
                Kind::State => assert_eq!(q.expected, json!(log[q.fact_round - 1].coins)),
                Kind::Order => {
                    let first = &log[q.fact_round - 1];
                    assert_eq!(q.expected, json!(first.node.facts.place));
                    assert!(
                        log.iter().any(
                            |r| r.round > first.round && q.prompt.contains(&r.node.facts.place)
                        )
                    );
                }
                Kind::Recall => {
                    let facts = &log[q.fact_round - 1].node.facts;
                    assert!(
                        [
                            json!(facts.person),
                            json!(facts.item),
                            json!(facts.place),
                            json!(facts.code)
                        ]
                        .contains(&q.expected)
                    );
                }
                Kind::Negative => {}
            }
        }
        let too_many = QuizRequest {
            seed: 1,
            questions: 67,
            types: vec![Kind::Order],
        };
        assert!(world.questions(&too_many).is_err());
        let unfinished = StoryWorld::new(config());
        unfinished.start().expect("start");
        assert!(unfinished.questions(&quiz()).is_err());
    }
    #[test]
    fn the_scorer_normalizes_answers_and_handles_malformed_answers() {
        let qs = played().questions(&quiz()).expect("quiz");
        let answers: serde_json::Map<String, Value> = qs
            .iter()
            .map(|q| {
                let value = match &q.expected {
                    Value::String(s) if q.kind == Kind::Negative => {
                        json!(if s == "YES" { " true " } else { " n " })
                    }
                    Value::String(s) => {
                        json!(format!("  {}  ", s.to_uppercase().replace(' ', "  ")))
                    }
                    Value::Number(n) => json!(format!("{n}.00")),
                    _ => panic!("scalar key"),
                };
                (q.id.clone(), value)
            })
            .collect();
        let text = format!("```json\n{}\n```", Value::Object(answers.clone()));
        assert_eq!(score(&text, &qs)["correct"], json!(12));
        for malformed in ["garbage", "{not json}", "[]", "{\"q1\":{},\"q2\":null}"] {
            assert_eq!(score(malformed, &qs)["correct"], json!(0));
        }
        let one = json!({qs[0].id.clone(): answers[&qs[0].id].clone()});
        assert_eq!(score(&one.to_string(), &qs)["correct"], json!(1));
        assert_eq!(
            normalize(&json!(" 12,345.00 "), Kind::Recall),
            Some("12345".into())
        );
        assert_eq!(
            normalize(&json!("false"), Kind::Negative),
            Some("no".into())
        );
        assert_eq!(normalize(&json!("maybe"), Kind::Negative), None);
    }
    #[test]
    fn lookback_is_rounds_before_the_end_and_order_uses_the_first_place() {
        let world = played();
        let qs = world
            .questions(&QuizRequest {
                seed: 1,
                questions: 100,
                types: vec![Kind::State],
            })
            .expect("questions");
        for q in &qs {
            assert_eq!(q.lookback, 12 - q.fact_round);
        }
        assert!(qs.iter().any(|q| q.lookback == 0));
        assert!(qs.iter().any(|q| q.lookback == 11));
        let scored = score("", &qs);
        for bucket in ["0-3", "4-7", "8+"] {
            let expected = qs
                .iter()
                .filter(|q| match bucket {
                    "0-3" => (0..=3).contains(&q.lookback),
                    "4-7" => (4..=7).contains(&q.lookback),
                    _ => (8..).contains(&q.lookback),
                })
                .count();
            assert_eq!(scored["by_lookback"][bucket]["total"], json!(expected));
        }
        for q in world
            .questions(&quiz())
            .expect("quiz")
            .iter()
            .filter(|q| q.kind == Kind::Order)
        {
            assert_eq!(q.lookback, 12 - q.fact_round);
        }
    }
}
