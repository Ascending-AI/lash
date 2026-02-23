use std::ffi::CString;
use std::sync::mpsc as std_mpsc;
use std::sync::{Mutex, Once};
use std::thread::JoinHandle;

use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::python_home;

const REPL_PY: &str = include_str!("../python/repl.py");

static PYTHON_INIT: Once = Once::new();

/// Initialize the Python interpreter exactly once (process-wide).
/// With free-threaded Python 3.14t, there is no GIL — multiple threads
/// can call `Python::attach()` concurrently after this.
fn ensure_python_initialized(home_dir: &std::path::Path) {
    PYTHON_INIT.call_once(|| {
        unsafe {
            std::env::set_var("PYTHONHOME", home_dir);
            std::env::set_var("PYTHONNOUSERSITE", "1");
        }
        Python::initialize();
        unsafe {
            std::env::remove_var("PYTHONHOME");
            std::env::remove_var("PYTHONNOUSERSITE");
        }
    });
}

// ── Request/Response types ──

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
    CheckCompleteResult {
        is_complete: bool,
    },
    AskUser {
        question: String,
        options: Vec<String>,
        result_tx: std_mpsc::Sender<String>,
    },
}

// ── RustBridge: injected into Python as _rust_bridge ──

#[pyclass(frozen)]
struct RustBridge {
    response_tx: Mutex<std_mpsc::Sender<PythonResponse>>,
}

#[pymethods]
impl RustBridge {
    /// Called by Python to send messages/results back to Rust.
    fn send_message(&self, json_str: &str) -> PyResult<()> {
        let msg: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid JSON: {e}")))?;

        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");

        let response = match msg_type {
            "message" => {
                let kind = msg["kind"].as_str().ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("message missing 'kind' field")
                })?;
                PythonResponse::Message {
                    text: msg["text"].as_str().unwrap_or("").to_string(),
                    kind: kind.to_string(),
                }
            }
            "exec_result" => PythonResponse::ExecResult {
                id: msg["id"].as_str().unwrap_or("").to_string(),
                output: msg["output"].as_str().unwrap_or("").to_string(),
                error: msg["error"].as_str().map(String::from),
            },
            "snapshot_result" => PythonResponse::SnapshotResult {
                id: msg["id"].as_str().unwrap_or("").to_string(),
                data: msg["data"].as_str().unwrap_or("").to_string(),
            },
            "reset_result" => PythonResponse::ResetResult {
                id: msg["id"].as_str().unwrap_or("").to_string(),
            },
            "ready" => PythonResponse::Ready,
            other => {
                tracing::warn!("unknown message type from Python: {other}");
                return Ok(());
            }
        };

        let _ = self.response_tx.lock().unwrap().send(response);
        Ok(())
    }

    /// Called by Python to invoke a tool. Blocks until the tool result is ready.
    /// We detach from Python so other threads can call Python::attach().
    fn invoke_tool(
        &self,
        py: Python<'_>,
        call_id: String,
        name: String,
        args_json: String,
    ) -> PyResult<String> {
        let (result_tx, result_rx) = std_mpsc::channel();

        let _ = self
            .response_tx
            .lock()
            .unwrap()
            .send(PythonResponse::ToolCall {
                id: call_id,
                name,
                args: args_json,
                result_tx,
            });

        py.detach(move || {
            result_rx.recv().map_err(|e| {
                pyo3::exceptions::PyRuntimeError::new_err(format!(
                    "tool result channel closed: {e}"
                ))
            })
        })
    }

    /// Called by Python to ask the user a question. Blocks until the user responds.
    fn ask_user(&self, py: Python<'_>, payload_json: String) -> PyResult<String> {
        let payload: serde_json::Value = serde_json::from_str(&payload_json)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid JSON: {e}")))?;

        let question = payload
            .get("question")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .to_string();
        let options: Vec<String> = payload
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let (result_tx, result_rx) = std_mpsc::channel();

        let _ = self
            .response_tx
            .lock()
            .unwrap()
            .send(PythonResponse::AskUser {
                question,
                options,
                result_tx,
            });

        py.detach(move || {
            result_rx.recv().map_err(|e| {
                pyo3::exceptions::PyRuntimeError::new_err(format!("ask_user channel closed: {e}"))
            })
        })
    }
}

// ── PythonRuntime ──

pub struct PythonRuntime {
    request_tx: std_mpsc::Sender<PythonRequest>,
    response_rx: std_mpsc::Receiver<PythonResponse>,
    thread: Option<JoinHandle<()>>,
}

impl PythonRuntime {
    /// Start the embedded Python runtime on a dedicated OS thread.
    pub fn start() -> Result<Self, std::io::Error> {
        tracing::info!("PythonRuntime::start() called");

        // Extract stdlib to cache before starting the thread
        let lib_dir = python_home::ensure_python_home()?;
        let home_dir = python_home::python_home(&lib_dir);

        // Initialize Python once (process-wide). With free-threaded 3.14t,
        // subsequent threads can call Python::attach() concurrently.
        ensure_python_initialized(&home_dir);
        tracing::info!("Python initialized, spawning thread");

        let (request_tx, request_rx) = std_mpsc::channel::<PythonRequest>();
        let (response_tx, response_rx) = std_mpsc::channel::<PythonResponse>();

        let thread = std::thread::Builder::new()
            .name("python-runtime".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    python_thread_main(request_rx, response_tx);
                }));
                if let Err(e) = result {
                    let msg = if let Some(s) = e.downcast_ref::<String>() {
                        s.clone()
                    } else if let Some(s) = e.downcast_ref::<&str>() {
                        s.to_string()
                    } else {
                        "unknown panic".to_string()
                    };
                    tracing::error!("python-runtime thread panicked: {msg}");
                }
            })?;
        tracing::info!("python-runtime thread spawned");

        Ok(Self {
            request_tx,
            response_rx,
            thread: Some(thread),
        })
    }

    pub fn send(&self, request: PythonRequest) -> Result<(), std::io::Error> {
        self.request_tx
            .send(request)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "python thread gone"))
    }

    pub fn recv(&self) -> Result<PythonResponse, std::io::Error> {
        self.response_rx
            .recv()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "python thread gone"))
    }

    /// Non-blocking try_recv.
    pub fn try_recv(&self) -> Result<Option<PythonResponse>, std::io::Error> {
        match self.response_rx.try_recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(std_mpsc::TryRecvError::Empty) => Ok(None),
            Err(std_mpsc::TryRecvError::Disconnected) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "python thread gone",
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

// ── Python thread ──

fn python_thread_main(
    request_rx: std_mpsc::Receiver<PythonRequest>,
    response_tx: std_mpsc::Sender<PythonResponse>,
) {
    // Python is already initialized by ensure_python_initialized().
    // With free-threaded 3.14t, attach() gives us thread state without a GIL.
    Python::attach(|py| {
        // Each thread needs a UNIQUE module name to avoid clobbering each other
        // in sys.modules (shared in free-threaded Python).
        static MODULE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let module_id = MODULE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let module_name = format!("_lash_repl_{module_id}");
        let module_name_cstr = CString::new(module_name.as_str()).unwrap();
        let file_name = format!("repl_{module_id}.py");
        let file_name_cstr = CString::new(file_name.as_str()).unwrap();

        let code_cstr = CString::new(REPL_PY).expect("repl.py contains null byte");
        let repl = match PyModule::from_code(py, &code_cstr, &file_name_cstr, &module_name_cstr) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("Failed to load repl.py: {e}");
                return;
            }
        };

        // Inject the RustBridge
        let bridge = Py::new(
            py,
            RustBridge {
                response_tx: Mutex::new(response_tx.clone()),
            },
        )
        .expect("Failed to create RustBridge");

        if let Err(e) = repl.setattr("_rust_bridge", bridge) {
            tracing::error!("Failed to inject _rust_bridge: {e}");
            return;
        }
        // Main request loop
        while let Ok(request) = request_rx.recv() {
            match request {
                PythonRequest::Init {
                    tools_json,
                    agent_id,
                    headless,
                    capabilities_json,
                } => {
                    if let Err(e) = repl.call_method1(
                        "_register_tools",
                        (&tools_json, &agent_id, headless, &capabilities_json),
                    ) {
                        tracing::error!("_register_tools failed: {e}");
                    }
                    let _ = response_tx.send(PythonResponse::Ready);
                }
                PythonRequest::Exec { id, code } => {
                    // Run _handle_exec via asyncio.run()
                    let asyncio = py.import("asyncio").expect("asyncio import failed");
                    let coro = match repl.call_method1("_handle_exec", (&id, &code)) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!("_handle_exec failed: {e}");
                            let _ = response_tx.send(PythonResponse::ExecResult {
                                id,
                                output: String::new(),
                                error: Some(format!("Python error: {e}")),
                            });
                            continue;
                        }
                    };
                    if let Err(e) = asyncio.call_method1("run", (coro,)) {
                        tracing::error!("asyncio.run failed: {e}");
                    }
                }
                PythonRequest::Snapshot { id } => {
                    if let Err(e) = repl.call_method1("_handle_snapshot", (&id,)) {
                        tracing::error!("_handle_snapshot failed: {e}");
                    }
                }
                PythonRequest::Restore { id, data } => {
                    if let Err(e) = repl.call_method1("_handle_restore", (&id, &data)) {
                        tracing::error!("_handle_restore failed: {e}");
                    }
                }
                PythonRequest::Reset { id } => {
                    if let Err(e) = repl.call_method1("_handle_reset", (&id,)) {
                        tracing::error!("_handle_reset failed: {e}");
                    }
                }
                PythonRequest::CheckComplete { code } => {
                    let is_complete = repl
                        .call_method1("_check_complete", (&code,))
                        .and_then(|r| r.extract::<bool>())
                        .unwrap_or(false);
                    let _ = response_tx.send(PythonResponse::CheckCompleteResult { is_complete });
                }
                PythonRequest::Shutdown => break,
            }
        }
    });
}
