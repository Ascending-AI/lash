use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use base64::Engine as _;
use monty::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, MontyRepl, NameLookupResult,
    NoLimitTracker, PrintWriter, PrintWriterCallback, ReplContinuationMode, ReplFunctionCall,
    ReplProgress, ReplResolveFutures, detect_repl_continuation_mode,
};
use serde::Deserialize;
use serde_json::{Value, json};

const SNAPSHOT_VERSION: u32 = 2;
const DONE_SENTINEL: &str = "__lash_done__";
const RESET_SENTINEL: &str = "__lash_reset__";
const MAX_OUTPUT_LEN: usize = 20_000;
const MAX_STDOUT_LEN: usize = 20_000;

#[derive(Debug)]
pub enum PythonRequest {
    Init {
        tools_json: String,
        agent_id: String,
        headless: bool,
        capabilities_json: String,
    },
    Exec {
        id: String,
        code: String,
    },
    Snapshot {
        id: String,
    },
    Restore {
        id: String,
        data: String,
    },
    Reset {
        id: String,
    },
    Reconfigure {
        tools_json: String,
        capabilities_json: String,
        generation: u64,
    },
    CheckComplete {
        code: String,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum PythonResponse {
    Ready,
    ToolCall {
        id: String,
        name: String,
        args: String,
        result_tx: std_mpsc::Sender<String>,
    },
    Message {
        text: String,
        kind: String,
    },
    ExecResult {
        id: String,
        output: String,
        error: Option<String>,
    },
    SnapshotResult {
        id: String,
        data: String,
    },
    ResetResult {
        id: String,
    },
    ReconfigureResult {
        generation: u64,
        error: Option<String>,
    },
    CheckCompleteResult {
        is_complete: bool,
    },
    AskUser {
        question: String,
        options: Vec<String>,
        result_tx: std_mpsc::Sender<String>,
    },
}

pub struct PythonRuntime {
    request_tx: std_mpsc::Sender<PythonRequest>,
    response_rx: std_mpsc::Receiver<PythonResponse>,
    thread: Option<JoinHandle<()>>,
}

impl PythonRuntime {
    pub fn start() -> Result<Self, std::io::Error> {
        let (request_tx, request_rx) = std_mpsc::channel::<PythonRequest>();
        let (response_tx, response_rx) = std_mpsc::channel::<PythonResponse>();

        let thread = std::thread::Builder::new()
            .name("monty-runtime".into())
            .spawn(move || runtime_thread_main(request_rx, response_tx))?;

        Ok(Self {
            request_tx,
            response_rx,
            thread: Some(thread),
        })
    }

    pub fn send(&self, request: PythonRequest) -> Result<(), std::io::Error> {
        self.request_tx.send(request).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "executor thread gone")
        })
    }

    pub fn recv(&self) -> Result<PythonResponse, std::io::Error> {
        self.response_rx.recv().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "executor thread gone")
        })
    }

    pub fn try_recv(&self) -> Result<Option<PythonResponse>, std::io::Error> {
        match self.response_rx.try_recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(std_mpsc::TryRecvError::Empty) => Ok(None),
            Err(std_mpsc::TryRecvError::Disconnected) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "executor thread gone",
            )),
        }
    }
}

impl Drop for PythonRuntime {
    fn drop(&mut self) {
        let _ = self.request_tx.send(PythonRequest::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CapabilityPayload {
    #[serde(default)]
    enabled_capabilities: Vec<String>,
    #[serde(default)]
    enabled_tools: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct RuntimeConfig {
    tool_defs: Vec<crate::ProjectedToolDefinition>,
    helper_bindings: BTreeSet<String>,
    enabled_capabilities: BTreeSet<String>,
    enabled_tools: BTreeSet<String>,
    agent_id: String,
    headless: bool,
}

impl RuntimeConfig {
    fn apply(
        &mut self,
        tools_json: &str,
        agent_id: String,
        headless: bool,
        capabilities_json: &str,
    ) -> Result<(), String> {
        self.tool_defs =
            serde_json::from_str(tools_json).map_err(|e| format!("invalid tools json: {e}"))?;
        let payload: CapabilityPayload =
            serde_json::from_str(capabilities_json).unwrap_or_default();
        self.enabled_capabilities = payload
            .enabled_capabilities
            .into_iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect();
        self.enabled_tools = payload
            .enabled_tools
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        self.helper_bindings = helper_bindings_for(&self.enabled_capabilities);
        self.agent_id = agent_id;
        self.headless = headless;
        Ok(())
    }

    fn visible_tools(&self) -> impl Iterator<Item = &crate::ProjectedToolDefinition> {
        self.tool_defs.iter().filter(|tool| !tool.hidden)
    }

    fn tool_def(&self, name: &str) -> Option<&crate::ProjectedToolDefinition> {
        self.tool_defs.iter().find(|tool| tool.name == name)
    }

    fn helper_visible(&self, name: &str) -> bool {
        match name {
            "done" | "list_tools" | "search_tools" | "reset_repl" => true,
            "ask" => !self.headless,
            "search_history" => self.helper_bindings.contains("search_history"),
            "search_mem" | "mem_set" | "mem_get" | "mem_delete" | "mem_all" => {
                self.helper_bindings.contains("search_mem")
            }
            "search_skills" => self.helper_bindings.contains("search_skills"),
            _ => false,
        }
    }

    fn lookup_name(&self, name: &str) -> NameLookupResult {
        if self.visible_tools().any(|tool| tool.name == name) || self.helper_visible(name) {
            let docstring = self
                .tool_def(name)
                .map(|tool| tool.description.clone())
                .filter(|value| !value.is_empty());
            NameLookupResult::Value(MontyObject::Function {
                name: name.to_string(),
                docstring,
            })
        } else {
            NameLookupResult::Undefined
        }
    }
}

struct RuntimeState {
    config: RuntimeConfig,
    repl: MontyRepl<NoLimitTracker>,
}

impl RuntimeState {
    fn new() -> Result<Self, std::io::Error> {
        Ok(Self {
            config: RuntimeConfig::default(),
            repl: empty_repl()?,
        })
    }
}

fn runtime_thread_main(
    request_rx: std_mpsc::Receiver<PythonRequest>,
    response_tx: std_mpsc::Sender<PythonResponse>,
) {
    let mut state = match RuntimeState::new() {
        Ok(state) => state,
        Err(err) => {
            tracing::error!("failed to start monty runtime: {err}");
            return;
        }
    };

    while let Ok(request) = request_rx.recv() {
        match request {
            PythonRequest::Init {
                tools_json,
                agent_id,
                headless,
                capabilities_json,
            } => {
                if let Err(err) =
                    state
                        .config
                        .apply(&tools_json, agent_id, headless, &capabilities_json)
                {
                    tracing::error!("executor init failed: {err}");
                }
                let _ = response_tx.send(PythonResponse::Ready);
            }
            PythonRequest::Exec { id, code } => {
                let result = execute_code(&mut state, &code, &response_tx);
                let _ = response_tx.send(PythonResponse::ExecResult {
                    id,
                    output: result.output,
                    error: result.error,
                });
            }
            PythonRequest::Snapshot { id } => {
                let _ = response_tx.send(PythonResponse::SnapshotResult {
                    id,
                    data: snapshot_runtime(&state.repl).unwrap_or_default(),
                });
            }
            PythonRequest::Restore { id, data } => {
                let error = match restore_runtime(&data) {
                    Ok(repl) => {
                        state.repl = repl;
                        None
                    }
                    Err(err) => Some(err),
                };
                let _ = response_tx.send(PythonResponse::ExecResult {
                    id,
                    output: String::new(),
                    error,
                });
            }
            PythonRequest::Reset { id } => {
                match empty_repl() {
                    Ok(repl) => state.repl = repl,
                    Err(err) => tracing::error!("failed to reset repl: {err}"),
                }
                let _ = response_tx.send(PythonResponse::ResetResult { id });
            }
            PythonRequest::Reconfigure {
                tools_json,
                capabilities_json,
                generation,
            } => {
                let error = state
                    .config
                    .apply(
                        &tools_json,
                        state.config.agent_id.clone(),
                        state.config.headless,
                        &capabilities_json,
                    )
                    .err();
                let _ = response_tx.send(PythonResponse::ReconfigureResult { generation, error });
            }
            PythonRequest::CheckComplete { code } => {
                let is_complete = matches!(
                    detect_repl_continuation_mode(&code),
                    ReplContinuationMode::Complete
                );
                let _ = response_tx.send(PythonResponse::CheckCompleteResult { is_complete });
            }
            PythonRequest::Shutdown => break,
        }
    }
}

struct ExecOutcome {
    output: String,
    error: Option<String>,
}

struct OutputCollector {
    buf: String,
}

impl OutputCollector {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    fn append_display(&mut self, value: &MontyObject) {
        if matches!(value, MontyObject::None) {
            return;
        }
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
        self.buf.push_str(&value.to_string());
        if !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
    }

    fn into_output(self) -> String {
        truncate_output(&self.buf)
    }
}

impl PrintWriterCallback for OutputCollector {
    fn stdout_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.buf.push_str(&output);
        Ok(())
    }

    fn stdout_push(&mut self, end: char) -> Result<(), MontyException> {
        self.buf.push(end);
        Ok(())
    }
}

struct ExecContext {
    final_response: Option<String>,
    pending_reset: bool,
    pending_calls: HashMap<u32, std_mpsc::Receiver<String>>,
}

impl ExecContext {
    fn new() -> Self {
        Self {
            final_response: None,
            pending_reset: false,
            pending_calls: HashMap::new(),
        }
    }
}

fn execute_code(
    state: &mut RuntimeState,
    code: &str,
    response_tx: &std_mpsc::Sender<PythonResponse>,
) -> ExecOutcome {
    let mut collector = OutputCollector::new();
    let mut print = PrintWriter::Callback(&mut collector);
    let mut context = ExecContext::new();

    let current_repl = std::mem::replace(
        &mut state.repl,
        empty_repl()
            .map_err(|err| err.to_string())
            .unwrap_or_else(|_| {
                MontyRepl::new(
                    String::new(),
                    "<repl>",
                    vec![],
                    vec![],
                    NoLimitTracker,
                    &mut PrintWriter::Disabled,
                )
                .expect("failed to allocate fallback REPL")
                .0
            }),
    );
    let start = current_repl.start(code, &mut print);
    let outcome = match start {
        Ok(progress) => resume_progress(state, progress, response_tx, &mut print, &mut context),
        Err(err) => {
            state.repl = err.repl;
            Err(err.error.to_string())
        }
    };

    if context.pending_reset {
        match empty_repl() {
            Ok(repl) => state.repl = repl,
            Err(err) => {
                return ExecOutcome {
                    output: collector.into_output(),
                    error: Some(format!("failed to reset REPL: {err}")),
                };
            }
        }
    }

    if let Some(final_text) = context.final_response.take() {
        let _ = response_tx.send(PythonResponse::Message {
            text: final_text,
            kind: "final".into(),
        });
    }

    let error = match outcome {
        Ok(Some(value)) => {
            collector.append_display(&value);
            None
        }
        Ok(None) => None,
        Err(err) => Some(err),
    };

    ExecOutcome {
        output: collector.into_output(),
        error,
    }
}

fn resume_progress(
    state: &mut RuntimeState,
    mut progress: ReplProgress<NoLimitTracker>,
    response_tx: &std_mpsc::Sender<PythonResponse>,
    print: &mut PrintWriter<'_>,
    context: &mut ExecContext,
) -> Result<Option<MontyObject>, String> {
    loop {
        match progress {
            ReplProgress::Complete { repl, value } => {
                state.repl = repl;
                return Ok(Some(value));
            }
            ReplProgress::NameLookup(lookup) => {
                let lookup_name = lookup.name.clone();
                progress = lookup
                    .resume(state.config.lookup_name(&lookup_name), print)
                    .map_err(|err| err.error.to_string())?;
            }
            ReplProgress::FunctionCall(call) => {
                progress = handle_function_call(state, call, response_tx, print, context)?;
            }
            ReplProgress::ResolveFutures(pending) => {
                progress = resolve_futures(pending, response_tx, print, context)?;
            }
            ReplProgress::OsCall(call) => {
                return Err(format!(
                    "unsupported Monty OS call in lash runtime: {}",
                    call.function
                ));
            }
        }
    }
}

fn handle_function_call(
    state: &RuntimeState,
    call: ReplFunctionCall<NoLimitTracker>,
    response_tx: &std_mpsc::Sender<PythonResponse>,
    print: &mut PrintWriter<'_>,
    context: &mut ExecContext,
) -> Result<ReplProgress<NoLimitTracker>, String> {
    let function_name = call.function_name.clone();

    match function_name.as_str() {
        "done" => {
            let value = call
                .args
                .first()
                .cloned()
                .unwrap_or(MontyObject::String(String::new()));
            context.final_response = format_output_value(&value);
            return call
                .resume(
                    MontyException::new(ExcType::RuntimeError, Some(DONE_SENTINEL.to_string())),
                    print,
                )
                .map_err(map_done_error);
        }
        "reset_repl" => {
            context.pending_reset = true;
            return call
                .resume(
                    MontyException::new(ExcType::RuntimeError, Some(RESET_SENTINEL.to_string())),
                    print,
                )
                .map_err(map_done_error);
        }
        "ask" => {
            let question = call.args.first().and_then(as_string).unwrap_or_default();
            let options = call.args.get(1).map(parse_string_list).unwrap_or_default();
            let answer = ask_user(response_tx, question, options)?;
            return call
                .resume(MontyObject::String(answer), print)
                .map_err(|err| err.error.to_string());
        }
        "list_tools" => {
            let items = list_tools(state, &call.args, &call.kwargs);
            return call
                .resume(items, print)
                .map_err(|err| err.error.to_string());
        }
        "search_tools" => {
            let payload = helper_payload(state, "search_tools", &call.args, &call.kwargs)?;
            let catalog: Vec<Value> = state.config.visible_tools().map(tool_info_json).collect();
            let mut args = payload.as_object().cloned().unwrap_or_default();
            args.insert("catalog".to_string(), Value::Array(catalog));
            let result = invoke_tool(
                response_tx,
                &state.config.agent_id,
                "search_tools",
                Value::Object(args),
            )?;
            return call
                .resume(result, print)
                .map_err(|err| err.error.to_string());
        }
        "search_history" | "search_mem" | "search_skills" | "mem_set" | "mem_get"
        | "mem_delete" => {
            let payload = helper_payload(state, &function_name, &call.args, &call.kwargs)?;
            let result = invoke_tool(response_tx, &state.config.agent_id, &function_name, payload)?;
            return call
                .resume(result, print)
                .map_err(|err| err.error.to_string());
        }
        "mem_all" => {
            let result = invoke_tool(response_tx, &state.config.agent_id, "mem_export", json!({}))?;
            return call
                .resume(result, print)
                .map_err(|err| err.error.to_string());
        }
        _ => {}
    }

    let payload = tool_payload(state, &function_name, &call.args, &call.kwargs)?;
    if is_async_tool(&function_name) {
        let rx = send_tool_call(response_tx, &state.config.agent_id, &function_name, payload)?;
        context.pending_calls.insert(call.call_id, rx);
        call.resume_pending(print)
            .map_err(|err| err.error.to_string())
    } else {
        let result = wait_tool_result(send_tool_call(
            response_tx,
            &state.config.agent_id,
            &function_name,
            payload,
        )?)?;
        call.resume(result, print)
            .map_err(|err| err.error.to_string())
    }
}

fn resolve_futures(
    pending: ReplResolveFutures<NoLimitTracker>,
    _response_tx: &std_mpsc::Sender<PythonResponse>,
    print: &mut PrintWriter<'_>,
    context: &mut ExecContext,
) -> Result<ReplProgress<NoLimitTracker>, String> {
    loop {
        let mut completed = Vec::new();
        for call_id in pending.pending_call_ids() {
            if let Some(rx) = context.pending_calls.get(call_id) {
                match rx.try_recv() {
                    Ok(result_json) => {
                        completed.push((*call_id, parse_tool_reply(&result_json)?));
                    }
                    Err(std_mpsc::TryRecvError::Empty) => {}
                    Err(std_mpsc::TryRecvError::Disconnected) => {
                        completed.push((
                            *call_id,
                            MontyException::new(
                                ExcType::RuntimeError,
                                Some("tool result channel closed".to_string()),
                            )
                            .into(),
                        ));
                    }
                }
            }
        }

        if !completed.is_empty() {
            for (call_id, _) in &completed {
                context.pending_calls.remove(call_id);
            }
            return pending
                .resume(completed, print)
                .map_err(|err| err.error.to_string());
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn ask_user(
    response_tx: &std_mpsc::Sender<PythonResponse>,
    question: String,
    options: Vec<String>,
) -> Result<String, String> {
    let (result_tx, result_rx) = std_mpsc::channel();
    response_tx
        .send(PythonResponse::AskUser {
            question,
            options,
            result_tx,
        })
        .map_err(|_| "failed to send prompt to host".to_string())?;
    result_rx
        .recv()
        .map_err(|_| "ask_user channel closed".to_string())
}

fn send_tool_call(
    response_tx: &std_mpsc::Sender<PythonResponse>,
    agent_id: &str,
    name: &str,
    mut payload: Value,
) -> Result<std_mpsc::Receiver<String>, String> {
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("__agent_id__".to_string())
            .or_insert_with(|| Value::String(agent_id.to_string()));
    }
    let (result_tx, result_rx) = std_mpsc::channel();
    response_tx
        .send(PythonResponse::ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            args: payload.to_string(),
            result_tx,
        })
        .map_err(|_| format!("failed to dispatch tool '{name}'"))?;
    Ok(result_rx)
}

fn invoke_tool(
    response_tx: &std_mpsc::Sender<PythonResponse>,
    agent_id: &str,
    name: &str,
    payload: Value,
) -> Result<ExtFunctionResult, String> {
    wait_tool_result(send_tool_call(response_tx, agent_id, name, payload)?)
}

fn wait_tool_result(result_rx: std_mpsc::Receiver<String>) -> Result<ExtFunctionResult, String> {
    let result_json = result_rx
        .recv()
        .map_err(|_| "tool result channel closed".to_string())?;
    parse_tool_reply(&result_json)
}

fn parse_tool_reply(result_json: &str) -> Result<ExtFunctionResult, String> {
    #[derive(Deserialize)]
    struct ToolReply {
        success: bool,
        result: String,
    }

    let reply: ToolReply =
        serde_json::from_str(result_json).map_err(|e| format!("invalid tool reply: {e}"))?;
    let parsed = if reply.result.is_empty() {
        Value::Null
    } else {
        serde_json::from_str::<Value>(&reply.result).unwrap_or(Value::String(reply.result))
    };
    if reply.success {
        let value = json_to_monty(parsed);
        Ok(value.into())
    } else {
        Ok(MontyException::new(
            ExcType::RuntimeError,
            Some(
                parsed
                    .as_str()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| parsed.to_string()),
            ),
        )
        .into())
    }
}

fn tool_payload(
    state: &RuntimeState,
    name: &str,
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
) -> Result<Value, String> {
    let tool = state
        .config
        .tool_def(name)
        .ok_or_else(|| format!("unknown tool: {name}"))?;
    call_payload(name, &tool.params, args, kwargs)
}

fn helper_payload(
    state: &RuntimeState,
    name: &str,
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
) -> Result<Value, String> {
    let params = state
        .config
        .tool_def(name)
        .map(|tool| tool.params.clone())
        .unwrap_or_default();
    call_payload(name, &params, args, kwargs)
}

fn call_payload(
    name: &str,
    params: &[crate::ToolParam],
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
) -> Result<Value, String> {
    let mut payload = serde_json::Map::new();

    for (key, value) in kwargs {
        let key = as_string(key).ok_or_else(|| format!("{name} keyword names must be strings"))?;
        payload.insert(key, monty_to_json(value));
    }

    for (index, arg) in args.iter().enumerate() {
        let Some(param) = params.get(index) else {
            return Err(format!("{name} received too many positional arguments"));
        };
        if param.name == "id"
            && let Some(id) = extract_handle_id(arg)
        {
            payload.insert("id".to_string(), Value::String(id));
            continue;
        }
        if let MontyObject::Dict(_) = arg
            && index == 0
            && payload.is_empty()
            && let Value::Object(obj) = monty_to_json(arg)
        {
            payload.extend(obj);
            continue;
        }
        payload.insert(param.name.clone(), monty_to_json(arg));
    }

    Ok(Value::Object(payload))
}

fn list_tools(
    state: &RuntimeState,
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
) -> MontyObject {
    let payload = call_payload(
        "list_tools",
        &[
            crate::ToolParam::optional("query", "str"),
            crate::ToolParam::optional("injected_only", "bool"),
            crate::ToolParam::optional("verbose", "bool"),
        ],
        args,
        kwargs,
    )
    .unwrap_or_else(|_| json!({}));
    let query = payload
        .get("query")
        .and_then(Value::as_str)
        .map(|value| value.to_ascii_lowercase());
    let injected_only = payload.get("injected_only").and_then(Value::as_bool);
    let verbose = payload
        .get("verbose")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let items: Vec<Value> = state
        .config
        .visible_tools()
        .filter(|tool| match injected_only {
            Some(flag) => tool.inject_into_prompt == flag,
            None => true,
        })
        .filter(|tool| {
            query.as_ref().is_none_or(|needle| {
                tool.name.to_ascii_lowercase().contains(needle)
                    || tool.description.to_ascii_lowercase().contains(needle)
            })
        })
        .map(|tool| {
            if verbose {
                tool_info_json(tool)
            } else {
                tool_info_compact(tool)
            }
        })
        .collect();
    json_to_monty(Value::Array(items))
}

fn tool_info_compact(tool: &crate::ProjectedToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "oneliner": tool.description.lines().next().unwrap_or("").trim(),
        "signature": format!(
            "{}({}) -> {}",
            tool.name,
            tool.params
                .iter()
                .map(|param| {
                    if param.required {
                        format!("{}: {}", param.name, param.r#type)
                    } else {
                        format!("{}?", param.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
            if tool.returns.is_empty() { "any" } else { &tool.returns }
        ),
        "hidden": tool.hidden,
    })
}

fn tool_info_json(tool: &crate::ProjectedToolDefinition) -> Value {
    let examples = if tool.examples.is_empty() {
        vec![default_example(tool)]
    } else {
        tool.examples.clone()
    };
    json!({
        "name": tool.name,
        "description": tool.description,
        "oneliner": tool.description.lines().next().unwrap_or("").trim(),
        "params": tool.params,
        "returns": tool.returns,
        "examples": examples,
        "signature": format!(
            "{}({}) -> {}",
            tool.name,
            tool.params
                .iter()
                .map(|param| {
                    if param.required {
                        format!("{}: {}", param.name, param.r#type)
                    } else {
                        format!("{}: {} = None", param.name, param.r#type)
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
            if tool.returns.is_empty() { "any" } else { &tool.returns }
        ),
        "score": 0.0,
        "hidden": tool.hidden,
        "inject_into_prompt": tool.inject_into_prompt,
    })
}

fn default_example(tool: &crate::ProjectedToolDefinition) -> String {
    let params = tool
        .params
        .iter()
        .map(|param| {
            let value = match param.r#type.as_str() {
                "str" | "string" => format!("\"{}\"", param.name),
                "int" | "integer" => "1".to_string(),
                "float" | "number" => "1.0".to_string(),
                "bool" | "boolean" => "True".to_string(),
                "list" => "[]".to_string(),
                "dict" => "{}".to_string(),
                _ => "...".to_string(),
            };
            format!("{}={value}", param.name)
        })
        .collect::<Vec<_>>();
    format!("{}({})", tool.name, params.join(", "))
}

fn helper_bindings_for(enabled_capabilities: &BTreeSet<String>) -> BTreeSet<String> {
    let mut bindings = BTreeSet::from([
        "list_tools".to_string(),
        "search_tools".to_string(),
        "reset_repl".to_string(),
    ]);
    if enabled_capabilities.contains("history") {
        bindings.insert("search_history".to_string());
    }
    if enabled_capabilities.contains("memory") {
        bindings.insert("search_mem".to_string());
        bindings.insert("mem_set".to_string());
        bindings.insert("mem_get".to_string());
        bindings.insert("mem_delete".to_string());
        bindings.insert("mem_all".to_string());
    }
    if enabled_capabilities.contains("skills") {
        bindings.insert("search_skills".to_string());
    }
    bindings
}

fn empty_repl() -> Result<MontyRepl<NoLimitTracker>, std::io::Error> {
    let (repl, _) = MontyRepl::new(
        String::new(),
        "<repl>",
        vec![],
        vec![],
        NoLimitTracker,
        &mut PrintWriter::Disabled,
    )
    .map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(repl)
}

fn snapshot_runtime(repl: &MontyRepl<NoLimitTracker>) -> Result<String, String> {
    let bytes = repl.dump().map_err(|e| e.to_string())?;
    Ok(json!({
        "version": SNAPSHOT_VERSION,
        "engine": "monty",
        "repl": base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .to_string())
}

fn restore_runtime(data: &str) -> Result<MontyRepl<NoLimitTracker>, String> {
    let parsed: Value = serde_json::from_str(data).map_err(|e| format!("invalid snapshot: {e}"))?;
    let version = parsed.get("version").and_then(Value::as_u64);
    if version != Some(SNAPSHOT_VERSION as u64)
        || parsed.get("engine").and_then(Value::as_str) != Some("monty")
    {
        return Err(
            "snapshot is from an unsupported legacy REPL executor and cannot be restored"
                .to_string(),
        );
    }
    let encoded = parsed
        .get("repl")
        .and_then(Value::as_str)
        .ok_or_else(|| "snapshot is missing REPL data".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("invalid snapshot payload: {e}"))?;
    MontyRepl::load(&bytes).map_err(|e| format!("failed to restore REPL: {e}"))
}

fn is_async_tool(name: &str) -> bool {
    matches!(
        name,
        "shell_wait"
            | "shell_read"
            | "shell_write"
            | "shell_kill"
            | "agent_result"
            | "agent_output"
            | "agent_kill"
    )
}

fn extract_handle_id(value: &MontyObject) -> Option<String> {
    let Value::Object(map) = serde_json::to_value(value).ok()? else {
        return None;
    };
    map.get("id").and_then(Value::as_str).map(ToOwned::to_owned)
}

fn parse_string_list(value: &MontyObject) -> Vec<String> {
    match value {
        MontyObject::List(items) | MontyObject::Tuple(items) => {
            items.iter().filter_map(as_string).collect()
        }
        _ => Vec::new(),
    }
}

fn monty_to_json(value: &MontyObject) -> Value {
    match value {
        MontyObject::None | MontyObject::Ellipsis => Value::Null,
        MontyObject::Bool(b) => Value::Bool(*b),
        MontyObject::Int(n) => json!(n),
        MontyObject::BigInt(n) => json!(n.to_string()),
        MontyObject::Float(f) => json!(f),
        MontyObject::String(s) | MontyObject::Path(s) => Value::String(s.clone()),
        MontyObject::Bytes(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
        MontyObject::List(items)
        | MontyObject::Tuple(items)
        | MontyObject::Set(items)
        | MontyObject::FrozenSet(items) => Value::Array(items.iter().map(monty_to_json).collect()),
        MontyObject::NamedTuple {
            field_names,
            values,
            ..
        } => {
            let mut map = serde_json::Map::new();
            for (k, v) in field_names.iter().zip(values.iter()) {
                map.insert(k.clone(), monty_to_json(v));
            }
            Value::Object(map)
        }
        MontyObject::Dict(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                let key = match k {
                    MontyObject::String(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                map.insert(key, monty_to_json(v));
            }
            Value::Object(map)
        }
        MontyObject::Dataclass { attrs, .. } => {
            let mut map = serde_json::Map::new();
            for (k, v) in attrs {
                let key = match k {
                    MontyObject::String(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                map.insert(key, monty_to_json(v));
            }
            Value::Object(map)
        }
        // For types that don't have a natural JSON representation, use repr
        other => Value::String(format!("{other:?}")),
    }
}

/// Convert a serde_json::Value into a MontyObject using natural JSON mapping.
///
/// This is the inverse of `monty_to_json` and avoids the derived serde deserialization
/// which expects tagged enum format (e.g. `{"String": "..."}` instead of `"..."`).
fn json_to_monty(value: Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::None,
        Value::Bool(b) => MontyObject::Bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else {
                MontyObject::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => MontyObject::String(s),
        Value::Array(arr) => MontyObject::List(arr.into_iter().map(json_to_monty).collect()),
        Value::Object(map) => {
            let pairs: Vec<(MontyObject, MontyObject)> = map
                .into_iter()
                .map(|(k, v)| (MontyObject::String(k), json_to_monty(v)))
                .collect();
            MontyObject::Dict(pairs.into())
        }
    }
}

fn as_string(value: &MontyObject) -> Option<String> {
    match value {
        MontyObject::String(text) => Some(text.clone()),
        _ => None,
    }
}

fn format_output_value(value: &MontyObject) -> Option<String> {
    match value {
        MontyObject::None => None,
        MontyObject::String(text) => {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(truncate_response(trimmed))
            }
        }
        _ => Some(truncate_response(value.to_string())),
    }
}

fn map_done_error(err: Box<monty::ReplStartError<NoLimitTracker>>) -> String {
    match err.error.message() {
        Some(DONE_SENTINEL) | Some(RESET_SENTINEL) => String::new(),
        _ => err.error.to_string(),
    }
}

fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_STDOUT_LEN {
        return text.to_string();
    }
    let half = MAX_STDOUT_LEN / 2;
    let omitted = text.len() - MAX_STDOUT_LEN;
    format!(
        "{}\n\n... ({omitted} chars omitted) ...\n\n{}",
        &text[..half],
        &text[text.len() - half..]
    )
}

fn truncate_response(mut text: String) -> String {
    if text.len() > MAX_OUTPUT_LEN {
        text.truncate(MAX_OUTPUT_LEN);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolProvider;

    // ── monty_to_json ──

    #[test]
    fn monty_to_json_primitives() {
        assert_eq!(monty_to_json(&MontyObject::None), Value::Null);
        assert_eq!(monty_to_json(&MontyObject::Bool(true)), Value::Bool(true));
        assert_eq!(monty_to_json(&MontyObject::Int(42)), json!(42));
        assert_eq!(monty_to_json(&MontyObject::Float(3.14)), json!(3.14));
        assert_eq!(
            monty_to_json(&MontyObject::String("hello".into())),
            json!("hello")
        );
    }

    #[test]
    fn monty_to_json_collections() {
        let list = MontyObject::List(vec![
            MontyObject::Int(1),
            MontyObject::String("two".into()),
            MontyObject::Bool(false),
        ]);
        assert_eq!(monty_to_json(&list), json!([1, "two", false]));

        let dict = MontyObject::Dict(
            vec![
                (MontyObject::String("a".into()), MontyObject::Int(1)),
                (
                    MontyObject::String("b".into()),
                    MontyObject::String("hello".into()),
                ),
            ]
            .into(),
        );
        assert_eq!(monty_to_json(&dict), json!({"a": 1, "b": "hello"}));
    }

    #[test]
    fn monty_to_json_nested() {
        let nested = MontyObject::Dict(
            vec![(
                MontyObject::String("items".into()),
                MontyObject::List(vec![MontyObject::Dict(
                    vec![(MontyObject::String("id".into()), MontyObject::Int(1))].into(),
                )]),
            )]
            .into(),
        );
        assert_eq!(monty_to_json(&nested), json!({"items": [{"id": 1}]}));
    }

    // ── json_to_monty ──

    #[test]
    fn json_to_monty_primitives() {
        assert!(matches!(json_to_monty(Value::Null), MontyObject::None));
        assert!(matches!(
            json_to_monty(json!(true)),
            MontyObject::Bool(true)
        ));
        assert!(matches!(json_to_monty(json!(42)), MontyObject::Int(42)));
        assert!(
            matches!(json_to_monty(json!(3.14)), MontyObject::Float(f) if (f - 3.14).abs() < 1e-10)
        );
        assert!(matches!(json_to_monty(json!("hi")), MontyObject::String(s) if s == "hi"));
    }

    #[test]
    fn json_to_monty_collections() {
        let val = json_to_monty(json!([1, "two", true]));
        let MontyObject::List(items) = val else {
            panic!("expected List");
        };
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], MontyObject::Int(1)));
        assert!(matches!(&items[1], MontyObject::String(s) if s == "two"));
        assert!(matches!(&items[2], MontyObject::Bool(true)));

        let val = json_to_monty(json!({"key": "value", "num": 5}));
        let MontyObject::Dict(pairs) = val else {
            panic!("expected Dict");
        };
        let pairs_vec: Vec<_> = pairs.into_iter().collect();
        assert_eq!(pairs_vec.len(), 2);
    }

    // ── round-trip ──

    #[test]
    fn monty_json_round_trip() {
        let cases: Vec<MontyObject> = vec![
            MontyObject::None,
            MontyObject::Bool(false),
            MontyObject::Int(-7),
            MontyObject::Float(2.718),
            MontyObject::String("round trip".into()),
            MontyObject::List(vec![MontyObject::Int(1), MontyObject::Int(2)]),
            MontyObject::Dict(
                vec![
                    (
                        MontyObject::String("x".into()),
                        MontyObject::String("y".into()),
                    ),
                    (MontyObject::String("n".into()), MontyObject::Int(10)),
                ]
                .into(),
            ),
        ];
        for original in &cases {
            let json_val = monty_to_json(original);
            let recovered = json_to_monty(json_val.clone());
            let re_json = monty_to_json(&recovered);
            assert_eq!(json_val, re_json, "round-trip failed for {original:?}");
        }
    }

    // ── call_payload ──

    #[test]
    fn call_payload_positional_string() {
        let params = vec![crate::ToolParam::typed("command", "str")];
        let args = vec![MontyObject::String("date".into())];
        let result = call_payload("shell", &params, &args, &[]).unwrap();
        assert_eq!(result, json!({"command": "date"}));
    }

    #[test]
    fn call_payload_kwargs() {
        let params = vec![crate::ToolParam::typed("command", "str")];
        let kwargs = vec![(
            MontyObject::String("command".into()),
            MontyObject::String("ls -la".into()),
        )];
        let result = call_payload("shell", &params, &[], &kwargs).unwrap();
        assert_eq!(result, json!({"command": "ls -la"}));
    }

    #[test]
    fn call_payload_mixed_types() {
        let params = vec![
            crate::ToolParam::typed("query", "str"),
            crate::ToolParam::optional("limit", "int"),
            crate::ToolParam::optional("recursive", "bool"),
        ];
        let args = vec![
            MontyObject::String("*.rs".into()),
            MontyObject::Int(10),
            MontyObject::Bool(true),
        ];
        let result = call_payload("glob", &params, &args, &[]).unwrap();
        assert_eq!(
            result,
            json!({"query": "*.rs", "limit": 10, "recursive": true})
        );
    }

    // ── parse_tool_reply ──

    #[test]
    fn parse_tool_reply_success_string() {
        let reply = json!({"success": true, "result": "\"hello world\""}).to_string();
        let result = parse_tool_reply(&reply).unwrap();
        // ExtFunctionResult is an Into<MontyObject> wrapper; just verify no error
        let obj = unwrap_result(result);
        assert!(matches!(obj, MontyObject::String(s) if s == "hello world"));
    }

    #[test]
    fn parse_tool_reply_success_dict() {
        let inner = json!({"id": "proc_1", "pid": 1234}).to_string();
        let reply = json!({"success": true, "result": inner}).to_string();
        let result = parse_tool_reply(&reply).unwrap();
        let obj = unwrap_result(result);
        let MontyObject::Dict(pairs) = obj else {
            panic!("expected Dict, got {obj:?}");
        };
        let pairs_vec: Vec<_> = pairs.into_iter().collect();
        assert!(
            pairs_vec
                .iter()
                .any(|(k, _)| matches!(k, MontyObject::String(s) if s == "id"))
        );
        assert!(
            pairs_vec
                .iter()
                .any(|(k, _)| matches!(k, MontyObject::String(s) if s == "pid"))
        );
    }

    #[test]
    fn parse_tool_reply_success_array() {
        let inner = json!([{"name": "shell"}, {"name": "read_file"}]).to_string();
        let reply = json!({"success": true, "result": inner}).to_string();
        let result = parse_tool_reply(&reply).unwrap();
        let obj = unwrap_result(result);
        let MontyObject::List(items) = obj else {
            panic!("expected List, got {obj:?}");
        };
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn parse_tool_reply_success_number() {
        let reply = json!({"success": true, "result": "42"}).to_string();
        let result = parse_tool_reply(&reply).unwrap();
        let obj = unwrap_result(result);
        assert!(matches!(obj, MontyObject::Int(42)));
    }

    #[test]
    fn parse_tool_reply_error() {
        let reply = json!({"success": false, "result": "\"Missing required parameter: command\""})
            .to_string();
        let result = parse_tool_reply(&reply).unwrap();
        let obj = unwrap_result(result);
        // Errors become exceptions
        assert!(matches!(obj, MontyObject::Exception { .. }));
    }

    // ── All core tool definitions survive the call_payload round-trip ──

    /// For every tool in ToolSet::core(), verify that its params can be
    /// fed through call_payload and produce a valid JSON payload (no panics,
    /// no errors from the serialization layer).
    #[test]
    fn all_core_tool_params_round_trip_through_call_payload() {
        let toolset = crate::tools::ToolSet::core();
        let defs = toolset.definitions();
        assert!(!defs.is_empty(), "core toolset should have tools");

        for def in &defs {
            let projected: crate::ProjectedToolDefinition = def.project(crate::ExecutionMode::Repl);
            // Build synthetic MontyObject args matching each param
            let args: Vec<MontyObject> = projected
                .params
                .iter()
                .map(|p| sample_monty_for_type(&p.r#type))
                .collect();

            let result = call_payload(&projected.name, &projected.params, &args, &[]);
            assert!(
                result.is_ok(),
                "call_payload failed for tool '{}': {:?}",
                projected.name,
                result.err()
            );

            let payload = result.unwrap();
            // Every param should be present in the resulting JSON object
            let obj = payload.as_object().expect("payload should be object");
            for param in &projected.params {
                assert!(
                    obj.contains_key(&param.name),
                    "tool '{}' payload missing param '{}'",
                    projected.name,
                    param.name
                );
                // Verify the value is a natural JSON type (not a tagged enum wrapper)
                let val = &obj[&param.name];
                assert!(
                    !val.is_object() || !val.as_object().unwrap().len() == 1 || !is_monty_tag(val),
                    "tool '{}' param '{}' has tagged-enum value: {}",
                    projected.name,
                    param.name,
                    val
                );
            }
        }
    }

    /// For every core tool, verify that a realistic tool result can
    /// survive parse_tool_reply (JSON → MontyObject) without error.
    #[test]
    fn all_core_tool_results_survive_parse_tool_reply() {
        // Realistic result shapes that tools return
        let result_shapes: Vec<(&str, Value)> = vec![
            // shell returns a handle dict
            ("shell", json!({"id": "proc_abc", "pid": 12345})),
            // shell_wait returns output string
            ("shell_wait", json!("Mon Mar 9 14:30:00 2026")),
            // shell_read returns output string
            ("shell_read", json!("partial output...")),
            // shell_write returns None
            ("shell_write", Value::Null),
            // shell_kill returns None
            ("shell_kill", Value::Null),
            // read_file returns file content
            ("read_file", json!("fn main() {}\n")),
            // write_file returns confirmation
            ("write_file", json!("wrote 42 bytes")),
            // edit_file returns confirmation
            ("edit_file", json!("applied 1 edit")),
            // find_replace returns confirmation
            ("find_replace", json!("replaced 3 occurrences")),
            // glob returns file list
            ("glob", json!(["src/main.rs", "src/lib.rs"])),
            // grep returns matches
            (
                "grep",
                json!([{"file": "src/lib.rs", "line": 10, "text": "fn foo()"}]),
            ),
            // ls returns directory listing
            ("ls", json!(["src/", "Cargo.toml", "README.md"])),
            // plan_mode returns confirmation
            ("plan_mode", json!("entered plan mode")),
        ];

        for (tool_name, result_value) in &result_shapes {
            let inner = serde_json::to_string(result_value).unwrap();
            let reply = json!({"success": true, "result": inner}).to_string();
            let parsed = parse_tool_reply(&reply);
            assert!(
                parsed.is_ok(),
                "parse_tool_reply failed for tool '{}': {:?}",
                tool_name,
                parsed.err()
            );
            // Verify it converts to MontyObject without panic
            let obj = unwrap_result(parsed.unwrap());
            // Verify the JSON round-trips back
            let re_json = monty_to_json(&obj);
            assert_eq!(
                result_value, &re_json,
                "tool '{}' result didn't round-trip: expected {}, got {}",
                tool_name, result_value, re_json
            );
        }
    }

    // ── Helpers ──

    fn sample_monty_for_type(ty: &str) -> MontyObject {
        match ty {
            "str" => MontyObject::String("test_value".into()),
            "int" => MontyObject::Int(42),
            "float" => MontyObject::Float(1.0),
            "bool" => MontyObject::Bool(true),
            "list" => MontyObject::List(vec![MontyObject::String("item".into())]),
            "dict" => MontyObject::Dict(
                vec![(
                    MontyObject::String("key".into()),
                    MontyObject::String("val".into()),
                )]
                .into(),
            ),
            _ => MontyObject::String("any_value".into()),
        }
    }

    fn unwrap_result(r: ExtFunctionResult) -> MontyObject {
        match r {
            ExtFunctionResult::Return(obj) => obj,
            ExtFunctionResult::Error(exc) => MontyObject::Exception {
                exc_type: exc.exc_type(),
                arg: exc.message().map(String::from),
            },
            ExtFunctionResult::Future(_) => panic!("unexpected Future"),
            ExtFunctionResult::NotFound(name) => panic!("unexpected NotFound: {name}"),
        }
    }

    /// Check if a JSON value looks like a serde tagged-enum wrapper
    /// (e.g. {"String": "..."} or {"Int": 42}).
    fn is_monty_tag(val: &Value) -> bool {
        if let Some(obj) = val.as_object() {
            if obj.len() == 1 {
                let key = obj.keys().next().unwrap();
                // MontyObject variant names are PascalCase
                return key.chars().next().is_some_and(|c| c.is_ascii_uppercase());
            }
        }
        false
    }
}
