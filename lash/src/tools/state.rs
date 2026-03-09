use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use regex::Regex;
use serde_json::json;

use crate::store::{HistoryTurnRecord, MemRecord, Store};
use crate::{ToolDefinition, ToolParam, ToolProvider, ToolResult};

use super::run_blocking;

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Hybrid,
    Literal,
    Regex,
}

impl SearchMode {
    fn parse(value: Option<&str>) -> Self {
        match value
            .unwrap_or("hybrid")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "literal" => Self::Literal,
            "regex" => Self::Regex,
            _ => Self::Hybrid,
        }
    }
}

#[derive(Clone)]
struct SearchDoc {
    fields: HashMap<&'static str, String>,
}

type SkillCatalogEntry = (String, String, usize);
type SkillCatalogCache = Arc<RwLock<Option<Vec<SkillCatalogEntry>>>>;

#[derive(Clone)]
pub struct StateStore {
    store: Arc<Store>,
    skill_dirs: Vec<PathBuf>,
    skill_catalog_cache: SkillCatalogCache,
}

impl StateStore {
    pub fn new(store: Arc<Store>, skill_dirs: Vec<PathBuf>) -> Self {
        Self {
            store,
            skill_dirs,
            skill_catalog_cache: Arc::new(RwLock::new(None)),
        }
    }

    fn limit_from_args(args: &serde_json::Value) -> usize {
        args.get("limit")
            .and_then(|v| v.as_i64())
            .and_then(|n| usize::try_from(n).ok())
            .map(|n| n.clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT)
    }

    fn agent_id(args: &serde_json::Value) -> String {
        args.get("__agent_id__")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("root")
            .to_string()
    }

    fn compile_regex(pattern: Option<&str>) -> Option<Regex> {
        let raw = pattern.unwrap_or_default();
        if raw.is_empty() {
            return None;
        }
        match Regex::new(&format!("(?i){raw}")) {
            Ok(re) => Some(re),
            Err(_) => {
                let escaped = regex::escape(raw);
                Regex::new(&format!("(?i){escaped}")).ok()
            }
        }
    }

    fn tokenize(text: &str) -> Vec<String> {
        text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|t| !t.is_empty())
            .map(|t| t.to_ascii_lowercase())
            .collect()
    }

    fn bm25_scores(
        query_tokens: &[String],
        docs: &[SearchDoc],
        field_weights: &[(&'static str, f64)],
    ) -> Vec<f64> {
        let n_docs = docs.len();
        if n_docs == 0 {
            return Vec::new();
        }

        let mut doc_tfs: Vec<HashMap<String, f64>> = Vec::with_capacity(n_docs);
        let mut doc_lens: Vec<f64> = Vec::with_capacity(n_docs);
        let mut doc_freq: HashMap<String, usize> = HashMap::new();

        for doc in docs {
            let mut tf: HashMap<String, f64> = HashMap::new();
            let mut dlen = 0.0_f64;
            for (field, weight) in field_weights {
                if *weight <= 0.0 {
                    continue;
                }
                let text = doc
                    .fields
                    .get(field)
                    .map(String::as_str)
                    .unwrap_or_default();
                let tokens = Self::tokenize(text);
                if tokens.is_empty() {
                    continue;
                }
                let mut counts: HashMap<String, usize> = HashMap::new();
                for tok in &tokens {
                    *counts.entry(tok.clone()).or_insert(0) += 1;
                }
                for (tok, count) in counts {
                    *tf.entry(tok).or_insert(0.0) += (count as f64) * *weight;
                }
                dlen += (tokens.len() as f64) * *weight;
            }
            for tok in tf.keys() {
                *doc_freq.entry(tok.clone()).or_insert(0) += 1;
            }
            doc_tfs.push(tf);
            doc_lens.push(dlen);
        }

        let avgdl = {
            let sum: f64 = doc_lens.iter().sum();
            let avg = sum / (n_docs as f64);
            if avg <= 0.0 { 1.0 } else { avg }
        };

        let mut qtf: HashMap<String, usize> = HashMap::new();
        for tok in query_tokens {
            *qtf.entry(tok.clone()).or_insert(0) += 1;
        }

        let k1 = 1.5_f64;
        let b = 0.75_f64;
        let mut scores = vec![0.0_f64; n_docs];
        for (i, tf) in doc_tfs.iter().enumerate() {
            let dl = doc_lens[i];
            let norm = 1.0 - b + b * (dl / avgdl);
            for (tok, qcount) in &qtf {
                let freq = *tf.get(tok).unwrap_or(&0.0);
                if freq <= 0.0 {
                    continue;
                }
                let df = *doc_freq.get(tok).unwrap_or(&0) as f64;
                let idf = ((n_docs as f64 - df + 0.5) / (df + 0.5) + 1.0).ln();
                let denom = freq + k1 * norm;
                if denom <= 0.0 {
                    continue;
                }
                let term = idf * ((freq * (k1 + 1.0)) / denom);
                scores[i] += term * (1.0 + (*qcount as f64).ln());
            }
        }

        scores
    }

    fn field_hits(
        fields: &HashMap<&'static str, String>,
        query: &str,
        mode: SearchMode,
        regex_filter: Option<&Regex>,
    ) -> Vec<String> {
        let query_lower = query.to_ascii_lowercase();
        let query_tokens: HashSet<String> = Self::tokenize(query).into_iter().collect();
        let mut hits = Vec::new();
        for (field, value) in fields {
            if value.is_empty() {
                continue;
            }
            let value_lower = value.to_ascii_lowercase();
            let mut hit = match mode {
                SearchMode::Regex => regex_filter.is_some_and(|re| re.is_match(value)),
                SearchMode::Literal => !query.is_empty() && value_lower.contains(&query_lower),
                SearchMode::Hybrid => {
                    if !query_tokens.is_empty() {
                        let tokens: HashSet<String> = Self::tokenize(value).into_iter().collect();
                        query_tokens.iter().any(|t| tokens.contains(t))
                    } else if query.is_empty() {
                        true
                    } else {
                        value_lower.contains(&query_lower)
                    }
                }
            };
            if hit && regex_filter.is_some() && mode != SearchMode::Regex {
                hit = regex_filter.is_some_and(|re| re.is_match(value));
            }
            if hit {
                hits.push((*field).to_string());
            }
        }
        hits
    }

    fn rank_docs(
        docs: &[SearchDoc],
        query: &str,
        mode: SearchMode,
        regex: Option<&str>,
        field_weights: &[(&'static str, f64)],
    ) -> Vec<(usize, f64, Vec<String>)> {
        let query_tokens = Self::tokenize(query);
        let query_lower = query.to_ascii_lowercase();

        let regex_filter = match mode {
            SearchMode::Regex => Self::compile_regex(regex.or(Some(query))),
            _ => Self::compile_regex(regex),
        };

        let mut scores = vec![0.0_f64; docs.len()];
        if mode == SearchMode::Hybrid && !query_tokens.is_empty() {
            scores = Self::bm25_scores(&query_tokens, docs, field_weights);
        }

        let mut indices: Vec<usize> = (0..docs.len()).collect();
        if mode == SearchMode::Hybrid {
            indices.sort_by(|a, b| {
                scores[*b]
                    .partial_cmp(&scores[*a])
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            });
        }

        let mut out = Vec::new();
        for idx in indices {
            let mut include = true;
            let haystack = docs[idx]
                .fields
                .values()
                .filter(|v| !v.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let haystack_lower = haystack.to_ascii_lowercase();

            match mode {
                SearchMode::Regex => {
                    include = regex_filter
                        .as_ref()
                        .is_some_and(|re| re.is_match(&haystack));
                }
                SearchMode::Literal => {
                    include = !query.is_empty() && haystack_lower.contains(&query_lower);
                    if include && regex_filter.is_some() {
                        include = regex_filter
                            .as_ref()
                            .is_some_and(|re| re.is_match(&haystack));
                    }
                }
                SearchMode::Hybrid => {
                    if !query.is_empty() {
                        if !query_tokens.is_empty() {
                            include = scores[idx] > 0.0 || haystack_lower.contains(&query_lower);
                        } else {
                            include = haystack_lower.contains(&query_lower);
                        }
                    }
                    if include && regex_filter.is_some() {
                        include = regex_filter
                            .as_ref()
                            .is_some_and(|re| re.is_match(&haystack));
                    }
                }
            }

            if include {
                let hits = Self::field_hits(&docs[idx].fields, query, mode, regex_filter.as_ref());
                out.push((idx, scores[idx], hits));
            }
        }
        out
    }

    fn parse_skill_frontmatter(text: &str) -> Option<(String, String)> {
        let text = text.trim_start();
        if !text.starts_with("---") {
            return None;
        }
        let after_open = &text[3..];
        let close_idx = after_open.find("\n---")?;
        let frontmatter = &after_open[..close_idx];
        let mut name = String::new();
        let mut description = String::new();
        for line in frontmatter.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("name:") {
                name = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("description:") {
                description = v.trim().to_string();
            }
        }
        Some((name, description))
    }

    fn build_skill_catalog(&self) -> Vec<(String, String, usize)> {
        let mut by_name: HashMap<String, (String, usize)> = HashMap::new();
        for dir in &self.skill_dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                let text = match std::fs::read_to_string(&skill_md) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let Some((mut name, description)) = Self::parse_skill_frontmatter(&text) else {
                    continue;
                };
                if name.is_empty() {
                    name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string();
                }
                if name.is_empty() {
                    continue;
                }
                if !name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    continue;
                }
                let file_count = ignore::WalkBuilder::new(&path)
                    .hidden(false)
                    .build()
                    .filter_map(Result::ok)
                    .filter(|e| e.path().is_file())
                    .count()
                    .saturating_sub(1);
                by_name.insert(name, (description, file_count));
            }
        }
        let mut out: Vec<(String, String, usize)> = by_name
            .into_iter()
            .map(|(name, (description, file_count))| (name, description, file_count))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn discover_skills(&self) -> Vec<(String, String, usize)> {
        if let Ok(cache) = self.skill_catalog_cache.read()
            && let Some(skills) = cache.as_ref()
        {
            return skills.clone();
        }

        let skills = self.build_skill_catalog();
        if let Ok(mut cache) = self.skill_catalog_cache.write() {
            *cache = Some(skills.clone());
        }
        skills
    }

    fn list_tools(&self, args: &serde_json::Value) -> ToolResult {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(|v| v.to_ascii_lowercase());
        let injected_only = args.get("injected_only").and_then(|v| v.as_bool());
        let verbose = args
            .get("verbose")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let Some(catalog) = args.get("catalog").and_then(|v| v.as_array()) else {
            return ToolResult::err(json!("Missing required parameter: catalog"));
        };

        let items: Vec<serde_json::Value> = catalog
            .iter()
            .filter(|t| {
                if let Some(flag) = injected_only {
                    let inject = t
                        .get("inject_into_prompt")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if inject != flag {
                        return false;
                    }
                }
                if let Some(needle) = &query {
                    let name = t
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let desc = t
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    return name.contains(needle) || desc.contains(needle);
                }
                true
            })
            .map(|t| {
                if verbose {
                    t.clone()
                } else {
                    let name = t.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                    let desc = t
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let oneliner = desc.lines().next().unwrap_or("").trim();
                    json!({
                        "name": name,
                        "oneliner": oneliner,
                    })
                }
            })
            .collect();

        ToolResult::ok(json!(items))
    }

    fn search_tools(&self, args: &serde_json::Value) -> ToolResult {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mode = SearchMode::parse(args.get("mode").and_then(|v| v.as_str()));
        let regex = args.get("regex").and_then(|v| v.as_str());
        let limit = Self::limit_from_args(args);
        let injected_only = args.get("injected_only").and_then(|v| v.as_bool());

        let Some(catalog) = args.get("catalog").and_then(|v| v.as_array()) else {
            return ToolResult::err(json!("Missing required parameter: catalog"));
        };

        let mut filtered = Vec::new();
        for t in catalog {
            if let Some(injected) = injected_only {
                let inject = t
                    .get("inject_into_prompt")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if injected != inject {
                    continue;
                }
            }
            filtered.push(t.clone());
        }

        let docs: Vec<SearchDoc> = filtered
            .iter()
            .map(|t| {
                let examples = t
                    .get("examples")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let mut fields = HashMap::new();
                fields.insert(
                    "name",
                    t.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                );
                fields.insert(
                    "description",
                    t.get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                );
                fields.insert("examples", examples);
                SearchDoc { fields }
            })
            .collect();
        let ranked = Self::rank_docs(
            &docs,
            &query,
            mode,
            regex,
            &[("name", 4.0), ("description", 2.0), ("examples", 1.0)],
        );
        let items: Vec<serde_json::Value> = ranked
            .into_iter()
            .take(limit)
            .map(|(idx, score, _)| {
                let mut tool = filtered[idx].clone();
                if let Some(obj) = tool.as_object_mut() {
                    obj.insert("score".to_string(), json!(score));
                }
                tool
            })
            .collect();
        ToolResult::ok(json!(items))
    }

    fn search_skills(&self, args: &serde_json::Value) -> ToolResult {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mode = SearchMode::parse(args.get("mode").and_then(|v| v.as_str()));
        let regex = args.get("regex").and_then(|v| v.as_str());
        let limit = Self::limit_from_args(args);
        let skills = self.discover_skills();
        let docs: Vec<SearchDoc> = skills
            .iter()
            .map(|(name, description, _)| {
                let mut fields = HashMap::new();
                fields.insert("name", name.clone());
                fields.insert("description", description.clone());
                SearchDoc { fields }
            })
            .collect();
        let ranked = Self::rank_docs(
            &docs,
            &query,
            mode,
            regex,
            &[("name", 4.0), ("description", 2.0)],
        );
        let items: Vec<serde_json::Value> = ranked
            .into_iter()
            .take(limit)
            .map(|(idx, score, _)| {
                let (name, description, file_count) = &skills[idx];
                json!({
                    "__type__": "skill_summary",
                    "name": name,
                    "description": description,
                    "file_count": file_count,
                    "score": score,
                })
            })
            .collect();
        ToolResult::ok(json!(items))
    }

    fn truncate_preview(text: &str, limit: usize) -> String {
        let compact = text.trim().replace('\n', " ");
        if compact.len() <= limit {
            compact
        } else {
            format!("{}...", &compact[..limit.saturating_sub(3)])
        }
    }

    fn search_history(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mode = SearchMode::parse(args.get("mode").and_then(|v| v.as_str()));
        let regex = args.get("regex").and_then(|v| v.as_str());
        let limit = Self::limit_from_args(args);
        let since_turn = args.get("since_turn").and_then(|v| v.as_i64());

        let selected_fields: HashSet<String> = args
            .get("fields")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_else(|| {
                [
                    "user_message".to_string(),
                    "code".to_string(),
                    "prose".to_string(),
                    "output".to_string(),
                    "tool_calls".to_string(),
                ]
                .into_iter()
                .collect()
            });

        let turns: Vec<HistoryTurnRecord> = self
            .store
            .history_export(&agent_id)
            .into_iter()
            .filter(|t| since_turn.is_none_or(|min_turn| t.index >= min_turn))
            .collect();

        let docs: Vec<SearchDoc> = turns
            .iter()
            .map(|t| {
                let mut fields = HashMap::new();
                if selected_fields.contains("user_message") {
                    fields.insert("user_message", t.user_message.clone());
                }
                if selected_fields.contains("code") {
                    fields.insert("code", t.code.clone());
                }
                if selected_fields.contains("prose") {
                    fields.insert("prose", t.prose.clone());
                }
                if selected_fields.contains("output") {
                    fields.insert("output", t.output.clone());
                }
                if selected_fields.contains("tool_calls") {
                    let tc = serde_json::to_string(&t.tool_calls).unwrap_or_default();
                    fields.insert("tool_calls", tc);
                }
                SearchDoc { fields }
            })
            .collect();

        let ranked = Self::rank_docs(
            &docs,
            &query,
            mode,
            regex,
            &[
                ("user_message", 3.0),
                ("prose", 2.0),
                ("code", 2.0),
                ("output", 1.0),
                ("tool_calls", 1.0),
            ],
        );

        let items: Vec<serde_json::Value> = ranked
            .into_iter()
            .take(limit)
            .map(|(idx, score, field_hits)| {
                let t = &turns[idx];
                let preview_source = if !t.user_message.trim().is_empty() {
                    &t.user_message
                } else if !t.prose.trim().is_empty() {
                    &t.prose
                } else {
                    &t.output
                };
                json!({
                    "turn": t.index,
                    "score": score,
                    "field_hits": field_hits,
                    "preview": Self::truncate_preview(preview_source, 220),
                    "tool_calls": t.tool_calls,
                    "files_read": t.files_read,
                    "files_written": t.files_written,
                })
            })
            .collect();
        ToolResult::ok(json!(items))
    }

    fn search_mem(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mode = SearchMode::parse(args.get("mode").and_then(|v| v.as_str()));
        let regex = args.get("regex").and_then(|v| v.as_str());
        let limit = Self::limit_from_args(args);
        let key_filter: Option<HashSet<String>> =
            args.get("keys").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect::<HashSet<_>>()
            });

        let entries: Vec<MemRecord> = self
            .store
            .mem_export(&agent_id)
            .into_iter()
            .filter(|e| key_filter.as_ref().is_none_or(|keys| keys.contains(&e.key)))
            .collect();

        let docs: Vec<SearchDoc> = entries
            .iter()
            .map(|e| {
                let mut fields = HashMap::new();
                fields.insert("key", e.key.clone());
                fields.insert("description", e.description.clone());
                fields.insert("value", e.value.clone());
                SearchDoc { fields }
            })
            .collect();
        let ranked = Self::rank_docs(
            &docs,
            &query,
            mode,
            regex,
            &[("key", 4.0), ("description", 2.0), ("value", 1.0)],
        );
        let items: Vec<serde_json::Value> = ranked
            .into_iter()
            .take(limit)
            .map(|(idx, score, field_hits)| {
                let e = &entries[idx];
                json!({
                    "key": e.key,
                    "description": e.description,
                    "value": e.value,
                    "turn": e.turn,
                    "score": score,
                    "field_hits": field_hits,
                })
            })
            .collect();
        ToolResult::ok(json!(items))
    }

    fn history_add_turn(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let Some(turn) = args.get("turn") else {
            return ToolResult::err(json!("Missing required parameter: turn"));
        };
        self.store.history_add_turn(&agent_id, turn);
        ToolResult::ok(json!(null))
    }

    fn history_export(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let turns = self
            .store
            .history_export(&agent_id)
            .into_iter()
            .map(|t| {
                json!({
                    "index": t.index,
                    "user_message": t.user_message,
                    "prose": t.prose,
                    "code": t.code,
                    "output": t.output,
                    "error": t.error,
                    "tool_calls": t.tool_calls,
                    "files_read": t.files_read,
                    "files_written": t.files_written,
                })
            })
            .collect::<Vec<_>>();
        ToolResult::ok(json!(turns))
    }

    fn history_load(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let turns = args
            .get("turns")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        self.store.history_load(&agent_id, &turns);
        ToolResult::ok(json!(null))
    }

    fn mem_set_turn(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let turn = args.get("turn").and_then(|v| v.as_i64()).unwrap_or(0);
        self.store.mem_set_turn(&agent_id, turn);
        ToolResult::ok(json!(null))
    }

    fn mem_set(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let key = args.get("key").and_then(|v| v.as_str()).unwrap_or_default();
        if key.is_empty() {
            return ToolResult::err(json!("Missing required parameter: key"));
        }
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let value = args
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or(description);
        self.store.mem_set(&agent_id, key, description, value);
        ToolResult::ok(json!(null))
    }

    fn mem_get(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let key = args.get("key").and_then(|v| v.as_str()).unwrap_or_default();
        if key.is_empty() {
            return ToolResult::err(json!("Missing required parameter: key"));
        }
        match self.store.mem_get(&agent_id, key) {
            Some(entry) => ToolResult::ok(json!({
                "key": entry.key,
                "description": entry.description,
                "value": entry.value,
                "turn": entry.turn,
            })),
            None => ToolResult::ok(json!(null)),
        }
    }

    fn mem_delete(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let key = args.get("key").and_then(|v| v.as_str()).unwrap_or_default();
        if key.is_empty() {
            return ToolResult::err(json!("Missing required parameter: key"));
        }
        let _ = self.store.mem_delete(&agent_id, key);
        ToolResult::ok(json!(null))
    }

    fn mem_export(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let entries = self
            .store
            .mem_export(&agent_id)
            .into_iter()
            .map(|e| {
                json!({
                    "key": e.key,
                    "description": e.description,
                    "value": e.value,
                    "turn": e.turn,
                })
            })
            .collect::<Vec<_>>();
        ToolResult::ok(json!(entries))
    }

    fn mem_load(&self, args: &serde_json::Value) -> ToolResult {
        let agent_id = Self::agent_id(args);
        let entries = args
            .get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        self.store.mem_load(&agent_id, &entries);
        ToolResult::ok(json!(null))
    }
}

#[async_trait::async_trait]
impl ToolProvider for StateStore {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = vec![
            ToolDefinition {
                name: "list_tools".into(),
                description: vec![crate::ToolText::new(
                    "List available tools with compact summaries. Use verbose=true for full details. Use search_tools for ranked/filtered results.",
                    [crate::ExecutionMode::NativeTools],
                )],
                params: vec![
                    ToolParam::optional("query", "str"),
                    ToolParam::optional("injected_only", "bool"),
                    ToolParam::optional("verbose", "bool"),
                ],
                returns: "list".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "search_tools".into(),
                description: vec![crate::ToolText::new(
                    "Search tools using hybrid/literal/regex matching. Results include relevance scores.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![
                    ToolParam::typed("query", "str"),
                    ToolParam::optional("mode", "str"),
                    ToolParam::optional("regex", "str"),
                    ToolParam::optional("limit", "int"),
                    ToolParam::optional("injected_only", "bool"),
                ],
                returns: "list".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
        ];
        defs.push(ToolDefinition {
            name: "search_skills".into(),
            description: vec![crate::ToolText::new(
                "Search installed skills using hybrid/literal/regex matching.",
                [
                    crate::ExecutionMode::Repl,
                    crate::ExecutionMode::NativeTools,
                ],
            )],
            params: vec![
                ToolParam::typed("query", "str"),
                ToolParam::optional("mode", "str"),
                ToolParam::optional("regex", "str"),
                ToolParam::optional("limit", "int"),
            ],
            returns: "list[SkillSummary]".into(),
            examples: vec![],
            hidden: true,
            inject_into_prompt: false,
        });
        defs.extend(vec![
            ToolDefinition {
                name: "search_history".into(),
                description: vec![crate::ToolText::new(
                    "Search turn history using hybrid/literal/regex matching.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![
                    ToolParam::typed("query", "str"),
                    ToolParam::optional("mode", "str"),
                    ToolParam::optional("regex", "str"),
                    ToolParam::optional("limit", "int"),
                    ToolParam::optional("fields", "list"),
                    ToolParam::optional("since_turn", "int"),
                ],
                returns: "list".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "search_mem".into(),
                description: vec![crate::ToolText::new(
                    "Search persistent memory using hybrid/literal/regex matching.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![
                    ToolParam::typed("query", "str"),
                    ToolParam::optional("mode", "str"),
                    ToolParam::optional("regex", "str"),
                    ToolParam::optional("limit", "int"),
                    ToolParam::optional("keys", "list"),
                ],
                returns: "list".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "history_add_turn".into(),
                description: vec![crate::ToolText::new(
                    "Internal: append a turn record to history.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("turn", "dict")],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "history_export".into(),
                description: vec![crate::ToolText::new(
                    "Internal: export history turns.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![],
                returns: "list".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "history_load".into(),
                description: vec![crate::ToolText::new(
                    "Internal: replace history turns.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("turns", "list")],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "mem_set_turn".into(),
                description: vec![crate::ToolText::new(
                    "Internal: set current memory turn counter.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("turn", "int")],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "mem_set".into(),
                description: vec![crate::ToolText::new(
                    "Internal: set memory entry.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![
                    ToolParam::typed("key", "str"),
                    ToolParam::typed("description", "str"),
                    ToolParam::optional("value", "str"),
                ],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "mem_get".into(),
                description: vec![crate::ToolText::new(
                    "Internal: get memory entry.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("key", "str")],
                returns: "dict".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "mem_delete".into(),
                description: vec![crate::ToolText::new(
                    "Internal: delete memory entry.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("key", "str")],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "mem_export".into(),
                description: vec![crate::ToolText::new(
                    "Internal: export memory entries.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![],
                returns: "list".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "mem_load".into(),
                description: vec![crate::ToolText::new(
                    "Internal: replace memory entries.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("entries", "list")],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
        ]);
        defs
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        let this = self.clone();
        let name = name.to_string();
        let args = args.clone();
        run_blocking(move || match name.as_str() {
            "list_tools" => this.list_tools(&args),
            "search_tools" => this.search_tools(&args),
            "search_skills" => this.search_skills(&args),
            "search_history" => this.search_history(&args),
            "search_mem" => this.search_mem(&args),
            "history_add_turn" => this.history_add_turn(&args),
            "history_export" => this.history_export(&args),
            "history_load" => this.history_load(&args),
            "mem_set_turn" => this.mem_set_turn(&args),
            "mem_set" => this.mem_set(&args),
            "mem_get" => this.mem_get(&args),
            "mem_delete" => this.mem_delete(&args),
            "mem_export" => this.mem_export(&args),
            "mem_load" => this.mem_load(&args),
            _ => ToolResult::err_fmt(format_args!("Unknown tool: {name}")),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> Arc<Store> {
        Arc::new(Store::memory().expect("in-memory store"))
    }

    fn provider() -> StateStore {
        StateStore::new(make_store(), Vec::new())
    }

    #[test]
    fn mem_round_trip_and_search() {
        let p = provider();
        let args = json!({"__agent_id__":"root","turn":7});
        let _ = p.mem_set_turn(&args);
        let _ = p.mem_set(&json!({
            "__agent_id__":"root",
            "key":"decision",
            "description":"chosen provider",
            "value":"openai-generic"
        }));

        let get = p.mem_get(&json!({"__agent_id__":"root","key":"decision"}));
        assert!(get.success);
        assert_eq!(
            get.result.get("value").and_then(|v| v.as_str()),
            Some("openai-generic")
        );

        let search = p.search_mem(&json!({
            "__agent_id__":"root",
            "query":"provider",
            "mode":"hybrid",
            "limit":10
        }));
        assert!(search.success);
        let items = search.result.as_array().cloned().unwrap_or_default();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].get("key").and_then(|v| v.as_str()),
            Some("decision")
        );
    }

    #[test]
    fn history_round_trip_and_search() {
        let p = provider();
        let turn = json!({
            "index": 4,
            "user_message": "add search",
            "prose": "implemented rust search",
            "code": "x = 1",
            "output": "done",
            "error": null,
            "tool_calls": [{"tool":"read_file","args":{"path":"a.txt"},"result":"","success":true,"duration_ms":1}]
        });
        let add = p.history_add_turn(&json!({"__agent_id__":"root","turn":turn}));
        assert!(add.success);

        let export = p.history_export(&json!({"__agent_id__":"root"}));
        assert!(export.success);
        let turns = export.result.as_array().cloned().unwrap_or_default();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].get("index").and_then(|v| v.as_i64()), Some(4));

        let search = p.search_history(&json!({
            "__agent_id__":"root",
            "query":"implemented",
            "mode":"hybrid",
            "limit":10
        }));
        assert!(search.success);
        let items = search.result.as_array().cloned().unwrap_or_default();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].get("turn").and_then(|v| v.as_i64()), Some(4));
    }

    #[test]
    fn tool_search_uses_catalog() {
        let p = provider();
        let result = p.search_tools(&json!({
            "query":"write",
            "mode":"hybrid",
            "limit":10,
            "catalog":[
                {"name":"read_file","description":"Read file","examples":[],"inject_into_prompt":true},
                {"name":"write_file","description":"Write file","examples":[],"inject_into_prompt":true}
            ]
        }));
        assert!(result.success);
        let items = result.result.as_array().cloned().unwrap_or_default();
        assert!(!items.is_empty());
        assert_eq!(
            items[0].get("name").and_then(|v| v.as_str()),
            Some("write_file")
        );
    }

    #[test]
    fn skills_search_is_defined() {
        let p = StateStore::new(make_store(), Vec::new());
        let names: Vec<String> = p.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.iter().any(|n| n == "search_skills"));
    }
}
