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
    collections::{BTreeMap, BTreeSet},
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
const MATERIALS: [&str; 8] = [
    "copper", "oak", "silver", "brass", "glass", "iron", "clay", "silk",
];
const PETS: [&str; 16] = [
    "Pepper", "Biscuit", "Clover", "Mango", "Pip", "Sunny", "Scout", "Mochi", "Olive", "Pebble",
    "Ruby", "Socks", "Toffee", "Waffle", "Willow", "Ziggy",
];
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Attribute {
    Age,
    DoorCode,
    Color,
    Material,
    PetName,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Entity {
    name: String,
    attribute: Attribute,
    value: Value,
}
impl Entity {
    fn statement(&self) -> String {
        match self.attribute {
            Attribute::Age => format!("{} just turned {}.", self.name, self.value),
            Attribute::DoorCode => format!("The gate of {} opens with {}.", self.name, self.value),
            Attribute::Color => format!(
                "{} is {}.",
                self.name,
                self.value.as_str().unwrap_or_default()
            ),
            Attribute::Material => format!(
                "{} is made of {}.",
                self.name,
                self.value.as_str().unwrap_or_default()
            ),
            Attribute::PetName => format!(
                "{} is called {}.",
                self.name,
                self.value.as_str().unwrap_or_default()
            ),
        }
    }
    fn question(&self) -> String {
        match self.attribute {
            Attribute::Age => format!("How old is {}?", self.name),
            Attribute::DoorCode => format!("What number opens the gate of {}?", self.name),
            Attribute::Color => format!("What colour is {}?", self.name),
            Attribute::Material => format!("What is {} made of?", self.name),
            Attribute::PetName => format!("What is {} called?", self.name),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Facts {
    entities: Vec<Entity>,
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
fn unique_name(rng: &mut Random, ends: &[&str], used: &mut BTreeSet<String>) -> String {
    let base = token(rng, ends);
    let mut name = base.clone();
    let mut suffix = 2;
    while !used.insert(name.clone()) {
        name = format!("{base} {suffix}");
        suffix += 1;
    }
    name
}
fn node(config: &StoryConfig, path: &[usize], used: &mut BTreeSet<String>) -> Node {
    let mut rng = Random(config.seed ^ config.vocabulary_seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
    for &option in path {
        rng.0 = rng.next() ^ (option as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
    }
    let person = unique_name(&mut rng, &PEOPLE, used);
    let age = Entity {
        name: person.clone(),
        attribute: Attribute::Age,
        value: json!(18 + rng.index(68)),
    };
    let other = match rng.index(4) {
        0 => Entity {
            name: unique_name(&mut rng, &PLACES, used),
            attribute: Attribute::DoorCode,
            value: json!(10000 + rng.next() % 90000),
        },
        1 => Entity {
            name: unique_name(&mut rng, &ITEMS, used),
            attribute: Attribute::Color,
            value: json!(COLORS[rng.index(COLORS.len())].to_lowercase()),
        },
        2 => Entity {
            name: unique_name(&mut rng, &ITEMS, used),
            attribute: Attribute::Material,
            value: json!(MATERIALS[rng.index(MATERIALS.len())]),
        },
        _ => {
            let name = format!("{person}'s dog");
            used.insert(name.clone());
            Entity {
                name,
                attribute: Attribute::PetName,
                value: json!(unique_name(&mut rng, &PETS, used)),
            }
        }
    };
    let facts = Facts {
        entities: vec![age, other],
    };
    let passage = format!(
        "You meet {}. {} {}",
        person,
        facts.entities[0].statement(),
        facts.entities[1].statement()
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
}
#[derive(Default)]
struct WorldState {
    path: Vec<usize>,
    rounds: Vec<Round>,
    calls: BTreeMap<String, Result<Value, String>>,
    nodes: BTreeMap<Vec<usize>, Node>,
    names: BTreeSet<String>,
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
            state: Arc::new(Mutex::new(WorldState::default())),
        }
    }
    fn start(&self) -> Result<Value, String> {
        let mut state = self.state.lock_recover();
        if state.rounds.last().is_some_and(|r| r.choice.is_none()) {
            return Err("the current round is unfinished".into());
        }
        let path = state.path.clone();
        self.reserve_node(&mut state, &path);
        let round = Round {
            round: state.rounds.len() + 1,
            node: state.nodes[&path].clone(),
            choice: None,
        };
        state.rounds.push(round);
        Ok(self.read_in(&state))
    }
    // Reserve a branch's siblings together, before any destination is exposed.
    // Names belong to one node for the entire journey, including unseen branches.
    fn reserve_node(&self, state: &mut WorldState, path: &[usize]) {
        if !state.nodes.contains_key(path) {
            state
                .nodes
                .insert(path.to_vec(), node(&self.config, path, &mut state.names));
        }
    }
    fn read_in(&self, state: &WorldState) -> Value {
        let Some(current) = state.nodes.get(&state.path) else {
            return json!({"error": "no round has started"});
        };
        let choice = state.rounds.last().and_then(|r| r.choice.as_ref());
        json!({
            "round": state.rounds.len(), "round_finished": choice.is_some(),
            "passage_round": state.rounds.len() + usize::from(choice.is_some()),
            "choice": choice, "path": current.path,
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
        let parent_path = state.path.clone();
        for option in 0..self.config.branching {
            let mut path = parent_path.clone();
            path.push(option);
            self.reserve_node(state, &path);
        }
        state.path.push(index);
        Ok(self.read_in(state))
    }
    fn log(&self) -> Vec<Round> {
        self.state.lock_recover().rounds.clone()
    }
    fn questions(&self, request: &QuizRequest) -> Result<Vec<Question>, String> {
        let state = self.state.lock_recover();
        questions(&self.config, &state.rounds, &state.nodes, request)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Fact,
    Order,
    Negative,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Order => "order",
            Self::Negative => "negative",
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuizRequest {
    seed: u64,
    questions: usize,
    #[serde(default = "default_types")]
    types: Vec<Kind>,
}
fn default_types() -> Vec<Kind> {
    vec![Kind::Fact, Kind::Fact, Kind::Negative, Kind::Order]
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
    nodes: &BTreeMap<Vec<usize>, Node>,
    request: &QuizRequest,
) -> Result<Vec<Question>, String> {
    if log.is_empty() || log.iter().any(|r| r.choice.is_none()) {
        return Err("only a completed path can be quizzed".into());
    }
    if request.questions == 0 || request.types.is_empty() {
        return Err("questions and type mix must be nonempty".into());
    }
    let mut facts = Vec::new();
    let mut orders = Vec::new();
    let mut visited = Vec::new();
    let mut unvisited = Vec::new();
    let mut rng = Random(request.seed ^ 0x5155_495A);
    let question = |kind, prompt, expected, round: &Round, source_path| Question {
        id: String::new(),
        kind,
        prompt,
        expected,
        lookback: log.len() - round.round,
        fact_round: round.round,
        source_path,
    };
    for (i, round) in log.iter().enumerate() {
        let person = &round.node.facts.entities[0].name;
        for entity in &round.node.facts.entities {
            facts.push(question(
                Kind::Fact,
                entity.question(),
                entity.value.clone(),
                round,
                round.node.path.clone(),
            ));
        }
        visited.push(question(
            Kind::Negative,
            format!("Did you meet {person}?"),
            json!("YES"),
            round,
            round.node.path.clone(),
        ));
        for later in &log[i + 1..] {
            let later_person = &later.node.facts.entities[0].name;
            let yes = rng.index(2) == 0;
            let (first, second) = if yes {
                (person, later_person)
            } else {
                (later_person, person)
            };
            orders.push(question(
                Kind::Order,
                format!("Did you meet {first} before you met {second}?"),
                json!(if yes { "YES" } else { "NO" }),
                round,
                round.node.path.clone(),
            ));
        }
        let chosen = round
            .node
            .choices
            .iter()
            .position(|c| Some(&c.option) == round.choice.as_ref())
            .ok_or("missing logged choice")?;
        for option in 0..config.branching {
            if option == chosen {
                continue;
            }
            let mut path = round.node.path.clone();
            path.push(option);
            let sibling = nodes.get(&path).ok_or("missing reserved sibling")?;
            unvisited.push(question(
                Kind::Negative,
                format!("Did you meet {}?", sibling.facts.entities[0].name),
                json!("NO"),
                round,
                path,
            ));
        }
    }
    // An answer must have one literal source, including every reserved sibling
    // and the final destination already exposed by choose.
    facts.retain(|q| {
        let answer = q
            .expected
            .as_str()
            .map_or_else(|| q.expected.to_string(), str::to_owned);
        nodes
            .values()
            .map(|node| node.passage.matches(&answer).count())
            .sum::<usize>()
            == 1
    });
    // Every pool is finite and each fact/unordered pair is present once.
    let count = |kind| {
        (0..request.questions)
            .filter(|i| request.types[i % request.types.len()] == kind)
            .count()
    };
    let negatives = count(Kind::Negative);
    if count(Kind::Fact) > facts.len()
        || count(Kind::Order) > orders.len()
        || negatives.div_ceil(2) > unvisited.len()
        || negatives / 2 > visited.len()
    {
        return Err(
            "more questions than distinct facts or pairs for the requested type mix".into(),
        );
    }
    let mut result = Vec::new();
    let mut negative_index: usize = 0;
    for i in 0..request.questions {
        let kind = request.types[i % request.types.len()];
        let pool = match kind {
            Kind::Fact => &mut facts,
            Kind::Order => &mut orders,
            Kind::Negative => {
                let no = negative_index.is_multiple_of(2);
                negative_index += 1;
                if no { &mut unvisited } else { &mut visited }
            }
        };
        let mut q = pool.swap_remove(rng.index(pool.len()));
        q.id = format!("q{}", i + 1);
        result.push(q);
    }
    Ok(result)
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
        let correct = given == Some(&q.expected);
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
        if choose { "Choose exactly one listed option (A, B, ...). Returns the next round's passage with round_finished=true. That completes this user round: answer now and wait for the next USER turn. Execution steps are not new rounds. Invalid options change nothing." } else { "Read the current story: passage, facts, choices (option and label), round (user round), passage_round and round_finished. If round_finished=true, answer now; wait for the next USER turn. Current node only; no history." },
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
        "Choose-your-own-adventure: {read} reads your current passage and labelled choices. {choose} selects a listed option and returns the new passage. Choose exactly once per USER turn, then answer immediately. The result sets round_finished=true. Your execution steps are not new rounds: do not make another choice until the user sends the next round. Read returns top-level passage, facts, choices, round, passage_round, round_finished and choice. The choices array contains option and label. The returned passage belongs to the next round. An invalid choice changes nothing. No tool exposes past nodes or the journey log. If the passage is supplied in the user's message, you may choose directly."
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
    #[serde(default = "default_types")]
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
            types: default_types(),
        }
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
        for (i, round) in world.log().iter().enumerate() {
            assert_eq!(round.round, i + 1);
            assert_eq!(round.node.path, path);
            assert_eq!(round.choice.as_deref(), Some(["A", "B", "C"][i % 3]));
            path.push(i % 3);
        }
        assert_eq!(world.state.lock_recover().path, path);
    }
    #[test]
    fn seeded_journeys_reproduce_passages_and_questions() {
        let a = played();
        let b = played();
        assert_eq!(a.log(), b.log());
        assert_eq!(
            a.questions(&quiz()).expect("quiz"),
            b.questions(&quiz()).expect("quiz")
        );
        let other = StoryWorld::new(StoryConfig {
            seed: 2,
            ..config()
        });
        assert_ne!(
            a.log()[0].node.passage,
            other.start().expect("start")["passage"]
        );
    }
    #[test]
    fn entity_names_are_unique_across_the_path_and_unvisited_siblings() {
        let world = StoryWorld::new(config());
        for i in 0..500 {
            world.start().expect("start");
            world
                .choose(&format!("c{i}"), &json!({"option":"A"}))
                .expect("choose");
        }
        let state = world.state.lock_recover();
        let mut names = BTreeSet::new();
        for node in state.nodes.values() {
            for entity in &node.facts.entities {
                assert!(names.insert(entity.name.clone()), "{}", entity.name);
                if entity.attribute == Attribute::PetName {
                    assert!(names.insert(entity.value.as_str().expect("pet").to_owned()));
                }
            }
        }
    }
    #[test]
    fn questions_use_plain_names_without_temporal_or_coin_wording() {
        for q in played().questions(&quiz()).expect("quiz") {
            let lower = q.prompt.to_lowercase();
            let words: Vec<_> = lower.split(|c: char| !c.is_alphabetic()).collect();
            assert!(
                !words.iter().any(|word| matches!(
                    *word,
                    "round"
                        | "rounds"
                        | "turn"
                        | "turns"
                        | "coin"
                        | "coins"
                        | "when"
                        | "now"
                        | "currently"
                )),
                "{}",
                q.prompt
            );
        }
    }
    #[test]
    fn quizzes_use_distinct_facts_and_pairs_and_refuse_exhausted_pools() {
        let world = played();
        let log = world.log();
        for (kind, capacity) in [
            (Kind::Fact, fact_capacity(&world)),
            (Kind::Order, 66),
            (Kind::Negative, 25),
        ] {
            let request = QuizRequest {
                seed: 1,
                questions: capacity,
                types: vec![kind],
            };
            let qs = world.questions(&request).expect("full pool");
            let prompts: BTreeSet<_> = qs.iter().map(|q| &q.prompt).collect();
            assert_eq!(prompts.len(), capacity);
            for q in &qs {
                assert_eq!(q.lookback, 12 - q.fact_round);
                if kind == Kind::Negative && q.expected == json!("NO") {
                    assert!(!log.iter().any(|r| r.node.path == q.source_path));
                    assert!(
                        log.iter()
                            .any(|r| r.node.path == q.source_path[..q.source_path.len() - 1])
                    );
                } else {
                    assert!(log.iter().any(|r| r.node.path == q.source_path));
                }
                if kind == Kind::Order {
                    let mentioned: Vec<_> = log
                        .iter()
                        .filter(|r| q.prompt.contains(&r.node.facts.entities[0].name))
                        .collect();
                    assert_eq!(mentioned.len(), 2);
                    let yes = q.prompt.find(&mentioned[0].node.facts.entities[0].name)
                        < q.prompt.find(&mentioned[1].node.facts.entities[0].name);
                    assert_eq!(q.expected, json!(if yes { "YES" } else { "NO" }));
                    assert_eq!(q.fact_round, mentioned[0].round);
                }
            }
            assert!(
                world
                    .questions(&QuizRequest {
                        questions: capacity + 1,
                        ..request
                    })
                    .is_err()
            );
        }
        let unfinished = StoryWorld::new(config());
        unfinished.start().expect("start");
        assert!(unfinished.questions(&quiz()).is_err());
    }
    #[test]
    fn answers_require_exact_typed_values_and_malformed_answers_score_zero() {
        let world = played();
        let mut qs = Vec::new();
        for (kind, count) in [
            (Kind::Fact, fact_capacity(&world)),
            (Kind::Order, 66),
            (Kind::Negative, 25),
        ] {
            for mut q in world
                .questions(&QuizRequest {
                    seed: 1,
                    questions: count,
                    types: vec![kind],
                })
                .expect("quiz")
            {
                q.id = format!("q{}", qs.len() + 1);
                qs.push(q);
            }
        }
        let exact: serde_json::Map<String, Value> = qs
            .iter()
            .map(|q| (q.id.clone(), q.expected.clone()))
            .collect();
        let answer = Value::Object(exact).to_string();
        for envelope in [
            answer.clone(),
            format!("```json\n{answer}\n```"),
            format!("Answers: {answer}\nExplanation after the answers."),
        ] {
            assert_eq!(score(&envelope, &qs)["correct"], json!(qs.len()));
        }
        for malformed in [
            "garbage".to_owned(),
            "{not json}".to_owned(),
            "[]".to_owned(),
            "{}".to_owned(),
        ] {
            assert_eq!(score(&malformed, &qs)["correct"], json!(0));
        }
        assert!(qs.iter().any(|q| q.expected.is_number()));
        assert!(
            qs.iter()
                .any(|q| q.kind == Kind::Fact && q.expected.is_string())
        );
        for q in &qs {
            let mut wrong = vec![
                json!(true),
                json!(null),
                json!([]),
                json!({}),
                json!(1.5),
                json!("wrong"),
            ];
            if let Some(text) = q.expected.as_str() {
                wrong.extend(
                    [
                        json!(text.to_lowercase()),
                        json!(format!(" {text} ")),
                        json!(text.to_uppercase()),
                    ]
                    .into_iter()
                    .filter(|v| v != &q.expected),
                );
            } else {
                wrong.extend([
                    json!(q.expected.to_string()),
                    json!(q.expected.as_f64().expect("number")),
                ]);
            }
            if matches!(q.kind, Kind::Order | Kind::Negative) {
                wrong.extend([json!(1), json!("true"), json!("y"), json!("0")]);
            }
            for value in wrong {
                assert_eq!(
                    score(
                        &json!({q.id.clone():value}).to_string(),
                        std::slice::from_ref(q)
                    )["correct"],
                    json!(0)
                );
            }
        }
    }
    fn fact_capacity(world: &StoryWorld) -> usize {
        let state = world.state.lock_recover();
        state
            .rounds
            .iter()
            .flat_map(|r| &r.node.facts.entities)
            .filter(|entity| {
                let value = entity
                    .value
                    .as_str()
                    .map_or_else(|| entity.value.to_string(), str::to_owned);
                state
                    .nodes
                    .values()
                    .map(|n| n.passage.matches(&value).count())
                    .sum::<usize>()
                    == 1
            })
            .count()
    }
    #[test]
    fn fact_answers_have_exactly_one_literal_passage_source_and_no_sibling_source() {
        let world = played();
        let qs = world
            .questions(&QuizRequest {
                seed: 1,
                questions: fact_capacity(&world),
                types: vec![Kind::Fact],
            })
            .expect("facts");
        assert!(qs.len() >= 6);
        let log = world.log();
        let state = world.state.lock_recover();
        let mut attributes = BTreeSet::new();
        for q in qs {
            let source = log
                .iter()
                .find(|r| r.node.path == q.source_path)
                .expect("visited source");
            let entity = source
                .node
                .facts
                .entities
                .iter()
                .find(|e| e.question() == q.prompt)
                .expect("full name and direct attribute");
            attributes.insert(format!("{:?}", entity.attribute));
            assert_eq!(entity.value, q.expected);
            assert!(source.node.passage.contains(&entity.statement()));
            let answer = q
                .expected
                .as_str()
                .map_or_else(|| q.expected.to_string(), str::to_owned);
            assert_eq!(
                log.iter()
                    .map(|r| r.node.passage.matches(&answer).count())
                    .sum::<usize>(),
                1
            );
            for node in state.nodes.values().filter(|n| n.path != q.source_path) {
                assert!(
                    !node.passage.contains(&answer),
                    "{} in {}",
                    answer,
                    node.passage
                );
            }
        }
        assert!(attributes.len() >= 2);
    }
    #[test]
    fn default_mix_is_six_facts_three_negatives_three_orders_and_refuses_short_paths() {
        let request: QuizRequest =
            serde_json::from_value(json!({"seed":1,"questions":12})).expect("default mix");
        let qs = played().questions(&request).expect("quiz");
        for (kind, expected) in [(Kind::Fact, 6), (Kind::Negative, 3), (Kind::Order, 3)] {
            assert_eq!(qs.iter().filter(|q| q.kind == kind).count(), expected);
        }
        let world = StoryWorld::new(config());
        world.start().expect("start");
        world.choose("one", &json!({"option":"A"})).expect("choose");
        assert!(world.questions(&request).is_err());
        assert!(
            world
                .questions(&QuizRequest {
                    seed: 1,
                    questions: 0,
                    types: default_types()
                })
                .is_err()
        );
        assert!(
            world
                .questions(&QuizRequest {
                    seed: 1,
                    questions: 1,
                    types: vec![]
                })
                .is_err()
        );
    }
}
