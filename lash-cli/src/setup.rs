use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use lash_core::oauth;
use lash_core::provider::{LashConfig, OPENAI_GENERIC_DEFAULT_BASE_URL, Provider, ProviderKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

use crate::theme;

// ── State machine ───────────────────────────────────────────────────

enum SetupStep {
    SelectProvider {
        selected: usize,
    },
    InputCredential {
        input: String,
        cursor: usize,
        error: Option<String>,
        verifier: Option<String>,
        auth_url: Option<String>,
        browser_error: Option<String>,
        mode: CredentialMode,
    },
    CodexDeviceAuth {
        user_code: String,
        device_auth_id: String,
        interval: u64,
        error: Option<String>,
    },
    InputBaseUrl {
        api_key: String,
        input: String,
        cursor: usize,
    },
    InputTavily {
        input: String,
        cursor: usize,
    },
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialMode {
    ClaudeOAuth,
    GoogleOAuth,
    OpenAiGenericKey,
}

struct SetupApp {
    step: SetupStep,
    tick: u64,
}

impl SetupApp {
    fn new() -> Self {
        Self {
            step: SetupStep::SelectProvider { selected: 0 },
            tick: 0,
        }
    }
}

// ── Public entry point ──────────────────────────────────────────────

pub async fn run_setup_with_existing(existing: Option<&LashConfig>) -> anyhow::Result<LashConfig> {
    // Enter alternate screen + raw mode
    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        crossterm::style::Print("\x1b]11;rgb:0e/0d/0b\x1b\\")
    )?;

    let result = run_setup_inner(&mut terminal, existing).await;

    // Restore terminal
    crossterm::execute!(std::io::stdout(), crossterm::style::Print("\x1b]111\x1b\\"))?;
    ratatui::restore();

    result
}

async fn run_setup_inner(
    terminal: &mut ratatui::DefaultTerminal,
    existing: Option<&LashConfig>,
) -> anyhow::Result<LashConfig> {
    let mut app = SetupApp::new();
    let mut provider: Option<Provider> = None;
    let mut tavily_key: Option<String> =
        existing.and_then(|c| c.tavily_api_key().map(str::to_string));
    let existing_agent_models = existing.and_then(|c| c.agent_models.clone());

    loop {
        terminal.draw(|frame| draw_setup(frame, &app))?;

        if matches!(app.step, SetupStep::Done) {
            // Show "Authenticated!" briefly
            terminal.draw(|frame| draw_setup(frame, &app))?;
            std::thread::sleep(std::time::Duration::from_millis(500));
            break;
        }

        // For CodexDeviceAuth: poll with timeout then poll the device auth endpoint
        if let SetupStep::CodexDeviceAuth {
            device_auth_id,
            user_code,
            interval,
            error,
        } = &mut app.step
        {
            let poll_secs = *interval;
            let poll_timeout = std::time::Duration::from_secs(poll_secs);

            if event::poll(poll_timeout)? {
                let ev = event::read()?;
                if let Event::Key(key) = ev {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        anyhow::bail!("Setup cancelled");
                    }
                    if key.code == KeyCode::Esc {
                        app.step = SetupStep::SelectProvider { selected: 1 };
                        continue;
                    }
                }
                // Tick for spinner
                app.tick += 1;
                continue;
            }

            // Timeout expired — poll the device auth endpoint
            app.tick += 1;
            let did = device_auth_id.clone();
            let uc = user_code.clone();
            match oauth::codex_poll_device_auth(&did, &uc).await {
                Ok(Some((auth_code, code_verifier))) => {
                    // Exchange for tokens
                    match oauth::codex_exchange_code(&auth_code, &code_verifier).await {
                        Ok(tokens) => {
                            provider = Some(Provider::Codex {
                                access_token: tokens.access_token,
                                refresh_token: tokens.refresh_token,
                                expires_at: tokens.expires_at,
                                account_id: tokens.account_id,
                            });
                            app.step = if tavily_key.is_some() {
                                SetupStep::Done
                            } else {
                                SetupStep::InputTavily {
                                    input: String::new(),
                                    cursor: 0,
                                }
                            };
                        }
                        Err(e) => {
                            *error = Some(format!("{}", e));
                        }
                    }
                }
                Ok(None) => {
                    // Still pending, continue polling
                }
                Err(e) => {
                    *error = Some(format!("{}", e));
                }
            }
            continue;
        }

        // Block until a key event (non-Codex steps)
        let ev = event::read()?;
        let Event::Key(key) = ev else { continue };

        // Ctrl+C always quits
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            anyhow::bail!("Setup cancelled");
        }

        match &mut app.step {
            SetupStep::SelectProvider { selected } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(ProviderKind::ALL.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    match ProviderKind::ALL[*selected] {
                        ProviderKind::Claude => {
                            // Claude — start OAuth
                            let (verifier, challenge) = oauth::generate_pkce();
                            let url = oauth::authorize_url(&challenge, &verifier);
                            persist_oauth_url(&url);
                            let browser_error = open_browser(&url).err().map(|e| e.to_string());
                            app.step = SetupStep::InputCredential {
                                input: String::new(),
                                cursor: 0,
                                error: None,
                                verifier: Some(verifier),
                                auth_url: Some(url),
                                browser_error,
                                mode: CredentialMode::ClaudeOAuth,
                            };
                        }
                        ProviderKind::Codex => {
                            // Codex — device code auth
                            match oauth::codex_request_device_code().await {
                                Ok(dc) => {
                                    let _ = open_browser(oauth::CODEX_DEVICE_VERIFY_URL);
                                    app.step = SetupStep::CodexDeviceAuth {
                                        user_code: dc.user_code,
                                        device_auth_id: dc.device_auth_id,
                                        interval: dc.interval,
                                        error: None,
                                    };
                                }
                                Err(e) => {
                                    app.step = SetupStep::CodexDeviceAuth {
                                        user_code: String::new(),
                                        device_auth_id: String::new(),
                                        interval: 5,
                                        error: Some(format!("{}", e)),
                                    };
                                }
                            }
                        }
                        ProviderKind::GoogleOAuth => {
                            // Google OAuth
                            let (verifier, challenge) = oauth::generate_pkce();
                            match oauth::google_authorize_url(&challenge) {
                                Ok(url) => {
                                    persist_oauth_url(&url);
                                    let browser_error =
                                        open_browser(&url).err().map(|e| e.to_string());
                                    app.step = SetupStep::InputCredential {
                                        input: String::new(),
                                        cursor: 0,
                                        error: None,
                                        verifier: Some(verifier),
                                        auth_url: Some(url),
                                        browser_error,
                                        mode: CredentialMode::GoogleOAuth,
                                    };
                                }
                                Err(e) => {
                                    app.step = SetupStep::InputCredential {
                                        input: String::new(),
                                        cursor: 0,
                                        error: Some(format!("{}", e)),
                                        verifier: Some(verifier),
                                        auth_url: None,
                                        browser_error: None,
                                        mode: CredentialMode::GoogleOAuth,
                                    };
                                }
                            }
                        }
                        ProviderKind::OpenAiGeneric => {
                            // OpenAI-generic
                            app.step = SetupStep::InputCredential {
                                input: String::new(),
                                cursor: 0,
                                error: None,
                                verifier: None,
                                auth_url: None,
                                browser_error: None,
                                mode: CredentialMode::OpenAiGenericKey,
                            };
                        }
                    }
                }
                _ => {}
            },

            SetupStep::InputCredential {
                input,
                cursor,
                error,
                verifier,
                auth_url,
                browser_error,
                mode,
            } => match key.code {
                KeyCode::Esc => {
                    app.step = SetupStep::SelectProvider { selected: 0 };
                }
                KeyCode::Char(c) => {
                    input.insert(*cursor, c);
                    *cursor += c.len_utf8();
                    *error = None;
                }
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        let prev = input[..*cursor]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        input.drain(prev..*cursor);
                        *cursor = prev;
                    }
                }
                KeyCode::Left => {
                    if *cursor > 0 {
                        *cursor = input[..*cursor]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                    }
                }
                KeyCode::Right => {
                    if *cursor < input.len() {
                        *cursor += input[*cursor..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(0);
                    }
                }
                KeyCode::Enter => {
                    let val = input.trim().to_string();
                    if val.is_empty() {
                        *error = Some("Cannot be empty".into());
                    } else {
                        match mode {
                            CredentialMode::OpenAiGenericKey => {
                                let default_url = OPENAI_GENERIC_DEFAULT_BASE_URL.to_string();
                                let cursor_pos = default_url.len();
                                app.step = SetupStep::InputBaseUrl {
                                    api_key: val,
                                    input: default_url,
                                    cursor: cursor_pos,
                                };
                            }
                            CredentialMode::ClaudeOAuth => {
                                let Some(v) = verifier.take() else {
                                    *error =
                                        Some("Missing auth state; press Esc and retry.".into());
                                    continue;
                                };
                                match oauth::exchange_code(&val, &v).await {
                                    Ok(tokens) => {
                                        provider = Some(Provider::Claude {
                                            access_token: tokens.access_token,
                                            refresh_token: tokens.refresh_token,
                                            expires_at: tokens.expires_at,
                                        });
                                        app.step = if tavily_key.is_some() {
                                            SetupStep::Done
                                        } else {
                                            SetupStep::InputTavily {
                                                input: String::new(),
                                                cursor: 0,
                                            }
                                        };
                                    }
                                    Err(e) => {
                                        *error = Some(format!("{}", e));
                                        let (new_v, challenge) = oauth::generate_pkce();
                                        let url = oauth::authorize_url(&challenge, &new_v);
                                        persist_oauth_url(&url);
                                        *browser_error =
                                            open_browser(&url).err().map(|err| err.to_string());
                                        *auth_url = Some(url);
                                        *verifier = Some(new_v);
                                    }
                                }
                            }
                            CredentialMode::GoogleOAuth => {
                                let Some(v) = verifier.take() else {
                                    *error =
                                        Some("Missing auth state; press Esc and retry.".into());
                                    continue;
                                };
                                match oauth::google_exchange_code(&val, &v).await {
                                    Ok(tokens) => {
                                        provider = Some(Provider::GoogleOAuth {
                                            access_token: tokens.access_token,
                                            refresh_token: tokens.refresh_token,
                                            expires_at: tokens.expires_at,
                                            project_id: std::env::var("GOOGLE_CLOUD_PROJECT")
                                                .ok()
                                                .or_else(|| {
                                                    std::env::var("GOOGLE_CLOUD_PROJECT_ID").ok()
                                                }),
                                        });
                                        app.step = if tavily_key.is_some() {
                                            SetupStep::Done
                                        } else {
                                            SetupStep::InputTavily {
                                                input: String::new(),
                                                cursor: 0,
                                            }
                                        };
                                    }
                                    Err(e) => {
                                        *error = Some(format!("{}", e));
                                        let (new_v, challenge) = oauth::generate_pkce();
                                        match oauth::google_authorize_url(&challenge) {
                                            Ok(url) => {
                                                persist_oauth_url(&url);
                                                *browser_error = open_browser(&url)
                                                    .err()
                                                    .map(|err| err.to_string());
                                                *auth_url = Some(url);
                                                *verifier = Some(new_v);
                                            }
                                            Err(auth_err) => {
                                                *error = Some(format!("{}", auth_err));
                                                *auth_url = None;
                                                *browser_error = None;
                                                *verifier = Some(new_v);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            },

            SetupStep::CodexDeviceAuth { .. } => {
                // Handled by polling loop above; this branch is unreachable
                unreachable!();
            }

            SetupStep::InputBaseUrl {
                api_key,
                input,
                cursor,
            } => match key.code {
                KeyCode::Esc => {
                    app.step = SetupStep::SelectProvider { selected: 0 };
                }
                KeyCode::Char(c) => {
                    input.insert(*cursor, c);
                    *cursor += c.len_utf8();
                }
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        let prev = input[..*cursor]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        input.drain(prev..*cursor);
                        *cursor = prev;
                    }
                }
                KeyCode::Left => {
                    if *cursor > 0 {
                        *cursor = input[..*cursor]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                    }
                }
                KeyCode::Right => {
                    if *cursor < input.len() {
                        *cursor += input[*cursor..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(0);
                    }
                }
                KeyCode::Enter => {
                    let base_url = input.trim().to_string();
                    let base_url = if base_url.is_empty() {
                        OPENAI_GENERIC_DEFAULT_BASE_URL.to_string()
                    } else {
                        base_url
                    };
                    provider = Some(Provider::OpenAiGeneric {
                        api_key: api_key.clone(),
                        base_url,
                    });
                    app.step = if tavily_key.is_some() {
                        SetupStep::Done
                    } else {
                        SetupStep::InputTavily {
                            input: String::new(),
                            cursor: 0,
                        }
                    };
                }
                _ => {}
            },

            SetupStep::InputTavily { input, cursor } => match key.code {
                KeyCode::Esc => {
                    // Skip tavily
                    tavily_key = None;
                    app.step = SetupStep::Done;
                }
                KeyCode::Char(c) => {
                    input.insert(*cursor, c);
                    *cursor += c.len_utf8();
                }
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        let prev = input[..*cursor]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        input.drain(prev..*cursor);
                        *cursor = prev;
                    }
                }
                KeyCode::Left => {
                    if *cursor > 0 {
                        *cursor = input[..*cursor]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                    }
                }
                KeyCode::Right => {
                    if *cursor < input.len() {
                        *cursor += input[*cursor..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(0);
                    }
                }
                KeyCode::Enter => {
                    let val = input.trim().to_string();
                    tavily_key = if val.is_empty() { None } else { Some(val) };
                    app.step = SetupStep::Done;
                }
                _ => {}
            },

            SetupStep::Done => {}
        }

        if matches!(app.step, SetupStep::Done) {
            // Show "Authenticated!" briefly
            terminal.draw(|frame| draw_setup(frame, &app))?;
            std::thread::sleep(std::time::Duration::from_millis(500));
            break;
        }
    }

    let mut config = LashConfig::new(provider.expect("provider must be set before Done"));
    config.set_tavily_api_key(tavily_key);
    config.agent_models = existing_agent_models;
    Ok(config)
}

// ── Rendering ───────────────────────────────────────────────────────

fn draw_setup(frame: &mut Frame, app: &SetupApp) {
    // Full background
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::FORM)),
        frame.area(),
    );

    let logo_height = 8; // 5 logo + 1 trailing slash + 1 scribe + 1 tagline

    let step_height: u16 =
        match &app.step {
            SetupStep::SelectProvider { .. } => 10, // blank + label + blank + 4 options + blank + help + pad
            SetupStep::InputCredential { error, mode, .. } => {
                match mode {
                    CredentialMode::OpenAiGenericKey => {
                        if error.is_some() {
                            10
                        } else {
                            8
                        }
                    }
                    CredentialMode::ClaudeOAuth | CredentialMode::GoogleOAuth => {
                        if error.is_some() { 12 } else { 11 }
                    }
                }
            }
            SetupStep::CodexDeviceAuth { error, .. } => {
                if error.is_some() {
                    10
                } else {
                    9
                }
            }
            SetupStep::InputBaseUrl { .. } => 8,
            SetupStep::InputTavily { .. } => 7,
            SetupStep::Done => 4,
        };

    let chunks = Layout::vertical([
        Constraint::Min(0),              // top spacer
        Constraint::Length(logo_height), // logo
        Constraint::Length(step_height), // step content
        Constraint::Min(0),              // bottom spacer
    ])
    .split(frame.area());

    draw_logo(frame, chunks[1]);

    match &app.step {
        SetupStep::SelectProvider { selected } => draw_provider_select(frame, chunks[2], *selected),
        SetupStep::InputCredential {
            input,
            cursor,
            error,
            auth_url,
            browser_error,
            mode,
            ..
        } => draw_credential_input(
            frame,
            chunks[2],
            input,
            *cursor,
            error.as_deref(),
            *mode,
            auth_url.as_deref(),
            browser_error.as_deref(),
        ),
        SetupStep::CodexDeviceAuth {
            user_code, error, ..
        } => draw_codex_device_auth(frame, chunks[2], user_code, error.as_deref(), app.tick),
        SetupStep::InputBaseUrl { input, cursor, .. } => {
            draw_base_url_input(frame, chunks[2], input, *cursor)
        }
        SetupStep::InputTavily { input, cursor } => {
            draw_tavily_input(frame, chunks[2], input, *cursor)
        }
        SetupStep::Done => draw_done(frame, chunks[2]),
    }
}

fn draw_logo(frame: &mut Frame, area: Rect) {
    let chalk = Style::default().fg(theme::CHALK);
    let sodium = Style::default().fg(theme::SODIUM);

    let content_width = 30;
    let cx = (area.width as usize).saturating_sub(content_width) / 2;
    let pad = " ".repeat(cx);

    let logo: &[(&str, &str)] = &[
        ("██       ████   ██████  ", "  ██"),
        ("██      ██  ██  ██     ", "█  ██"),
        ("██      ██████  ██████", "██████"),
        ("██      ██  ██      █", " ██  ██"),
        ("██████  ██  ██  ████", "  ██  ██"),
    ];

    let mut lines: Vec<Line> = Vec::new();
    for &(before, after) in logo {
        lines.push(Line::from(vec![
            Span::styled(format!("{}{}", pad, before), chalk),
            Span::styled("██", sodium),
            Span::styled(after, chalk),
        ]));
    }
    // Trailing slash
    lines.push(Line::from(Span::styled(
        format!("{}                   ██", pad),
        sodium,
    )));
    // Scribe line
    lines.push(Line::from(vec![
        Span::styled(format!("{}──────────", pad), sodium),
        Span::styled("──────────", Style::default().fg(theme::ASH_MID)),
        Span::styled("──────────", Style::default().fg(theme::ASH)),
    ]));
    // Tagline (27 chars — center independently)
    let tagline = "A G E N T  \u{b7}  R U N T I M E";
    let tagline_pad = " ".repeat((area.width as usize).saturating_sub(tagline.len()) / 2);
    lines.push(Line::from(Span::styled(
        format!("{}{}", tagline_pad, tagline),
        Style::default().fg(theme::ASH_TEXT),
    )));

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::FORM));
    frame.render_widget(paragraph, area);
}

fn draw_provider_select(frame: &mut Frame, area: Rect, selected: usize) {
    let cx = center_pad(area.width as usize, 46);
    let pad = " ".repeat(cx);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("{}Select provider:", pad),
        Style::default().fg(theme::ASH_TEXT),
    )));
    lines.push(Line::from(""));

    for (i, kind) in ProviderKind::ALL.iter().enumerate() {
        let name = kind.setup_name();
        let desc = kind.setup_description();
        if i == selected {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}\u{25b8} ", pad),
                    Style::default().fg(theme::SODIUM),
                ),
                Span::styled(
                    format!("{:<16}", name),
                    Style::default()
                        .fg(theme::CHALK)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc, Style::default().fg(theme::CHALK_DIM)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{}  ", pad), Style::default()),
                Span::styled(
                    format!("{:<16}", name),
                    Style::default().fg(theme::ASH_TEXT),
                ),
                Span::styled(desc, Style::default().fg(theme::ASH_MID)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(""));
    // Help bar
    lines.push(Line::from(vec![
        Span::styled(format!("{}\u{2191}\u{2193}", pad), theme::help_key()),
        Span::styled(" navigate  ", theme::help_desc()),
        Span::styled("enter", theme::help_key()),
        Span::styled(" select", theme::help_desc()),
    ]));

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::FORM));
    frame.render_widget(paragraph, area);
}

#[allow(clippy::too_many_arguments)]
fn draw_credential_input(
    frame: &mut Frame,
    area: Rect,
    input: &str,
    cursor: usize,
    error: Option<&str>,
    mode: CredentialMode,
    auth_url: Option<&str>,
    browser_error: Option<&str>,
) {
    let box_width = 42usize;
    let cx = center_pad(area.width as usize, box_width);
    let pad = " ".repeat(cx);
    let inner_w = box_width.saturating_sub(4); // inside border + prompt char

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    let oauth_url_hint = oauth_url_display();

    match mode {
        CredentialMode::ClaudeOAuth => {
            lines.push(Line::from(Span::styled(
                format!("{}Claude sign-in", pad),
                Style::default().fg(theme::ASH_TEXT),
            )));
            if let Some(url) = auth_url {
                if browser_error.is_some() {
                    lines.push(Line::from(Span::styled(
                        format!("{}Browser didn't open. Use manual URL.", pad),
                        Style::default().fg(theme::ERROR),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("{}Tried opening browser automatically.", pad),
                        Style::default().fg(theme::ASH_TEXT),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    format!("{}Paste code or callback URL.", pad),
                    Style::default().fg(theme::ASH_TEXT),
                )));
                let _ = url;
                lines.push(Line::from(Span::styled(
                    format!("{}Manual URL: {}", pad, oauth_url_hint),
                    Style::default().fg(theme::ASH_TEXT),
                )));
                lines.push(Line::from(""));
            }
        }
        CredentialMode::GoogleOAuth => {
            lines.push(Line::from(Span::styled(
                format!("{}Google sign-in", pad),
                Style::default().fg(theme::ASH_TEXT),
            )));
            if let Some(url) = auth_url {
                if browser_error.is_some() {
                    lines.push(Line::from(Span::styled(
                        format!("{}Browser didn't open. Use manual URL.", pad),
                        Style::default().fg(theme::ERROR),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("{}Tried opening browser automatically.", pad),
                        Style::default().fg(theme::ASH_TEXT),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    format!("{}Paste code or callback URL.", pad),
                    Style::default().fg(theme::ASH_TEXT),
                )));
                let _ = url;
                lines.push(Line::from(Span::styled(
                    format!("{}Manual URL: {}", pad, oauth_url_hint),
                    Style::default().fg(theme::ASH_TEXT),
                )));
                lines.push(Line::from(""));
            }
        }
        CredentialMode::OpenAiGenericKey => {
            lines.push(Line::from(Span::styled(
                format!(
                    "{}Enter your OpenAI-generic API key. Default base URL: {}",
                    pad, OPENAI_GENERIC_DEFAULT_BASE_URL
                ),
                Style::default().fg(theme::ASH_TEXT),
            )));
            lines.push(Line::from(""));
        }
    }

    // Label
    let label = match mode {
        CredentialMode::OpenAiGenericKey => "API key",
        CredentialMode::ClaudeOAuth | CredentialMode::GoogleOAuth => "Authorization code",
    };

    // Input box
    let input_row = area.y + lines.len() as u16 + 1;

    let top_border = format!(
        "{}\u{256d} {} {}\u{256e}",
        pad,
        label,
        "\u{2500}".repeat(box_width.saturating_sub(label.len() + 4)),
    );
    lines.push(Line::from(Span::styled(
        top_border,
        Style::default().fg(theme::ASH),
    )));

    // Input line: │ / text____│
    let display_text = visible_slice(input, inner_w, cursor);
    let text_pad = inner_w.saturating_sub(display_text.chars().count());
    lines.push(Line::from(vec![
        Span::styled(format!("{}\u{2502} ", pad), Style::default().fg(theme::ASH)),
        Span::styled(
            format!("{} ", theme::PROMPT_CHAR),
            Style::default().fg(theme::SODIUM),
        ),
        Span::styled(
            display_text.to_string(),
            Style::default().fg(theme::CHALK_MID),
        ),
        Span::styled(
            format!("{}\u{2502}", " ".repeat(text_pad)),
            Style::default().fg(theme::ASH),
        ),
    ]));

    let bottom_border = format!(
        "{}\u{2570}{}\u{256f}",
        pad,
        "\u{2500}".repeat(box_width.saturating_sub(2)),
    );
    lines.push(Line::from(Span::styled(
        bottom_border,
        Style::default().fg(theme::ASH),
    )));

    // Error message if any
    if let Some(err) = error {
        lines.push(Line::from(Span::styled(
            format!("{}  {}", pad, err),
            Style::default().fg(theme::ERROR),
        )));
    }

    lines.push(Line::from(""));
    // Help bar
    lines.push(Line::from(vec![
        Span::styled(format!("{}enter", pad), theme::help_key()),
        Span::styled(" submit  ", theme::help_desc()),
        Span::styled("esc", theme::help_key()),
        Span::styled(" back", theme::help_desc()),
    ]));

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::FORM));
    frame.render_widget(paragraph, area);

    // Position cursor inside the input box
    let cursor_chars = input[..cursor].chars().count();
    let vis_start = visible_start(input, inner_w, cursor);
    let cursor_offset = cursor_chars.saturating_sub(vis_start);
    let cursor_x = (cx + 4 + cursor_offset) as u16; // pad + "│ / " = 4
    let cursor_y = input_row;
    if cursor_x < area.x + area.width && cursor_y < area.y + area.height {
        frame.set_cursor_position((area.x + cursor_x, cursor_y));
    }
}

fn draw_codex_device_auth(
    frame: &mut Frame,
    area: Rect,
    user_code: &str,
    error: Option<&str>,
    tick: u64,
) {
    let cx = center_pad(area.width as usize, 40);
    let pad = " ".repeat(cx);

    let spinner_chars = ["\u{2572}", "\u{2502}", "\u{2571}", "\u{2500}"];
    let spinner = spinner_chars[(tick as usize / 2) % spinner_chars.len()];

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("{}Authenticating with Codex...", pad),
        Style::default().fg(theme::ASH_TEXT),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{}Visit: ", pad),
            Style::default().fg(theme::ASH_TEXT),
        ),
        Span::styled(
            "auth.openai.com/codex/device",
            Style::default().fg(theme::CHALK_MID),
        ),
    ]));

    if !user_code.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}Enter code: ", pad),
                Style::default().fg(theme::ASH_TEXT),
            ),
            Span::styled(
                user_code,
                Style::default()
                    .fg(theme::SODIUM)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{}{}", pad, spinner),
            Style::default().fg(theme::SODIUM),
        ),
        Span::styled(
            "\u{2500} Waiting for authorization...",
            Style::default().fg(theme::ASH_MID),
        ),
    ]));

    if let Some(err) = error {
        lines.push(Line::from(Span::styled(
            format!("{}  {}", pad, err),
            Style::default().fg(theme::ERROR),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("{}esc", pad), theme::help_key()),
        Span::styled(" back", theme::help_desc()),
    ]));

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::FORM));
    frame.render_widget(paragraph, area);
}

fn draw_base_url_input(frame: &mut Frame, area: Rect, input: &str, cursor: usize) {
    let box_width = 52usize;
    let cx = center_pad(area.width as usize, box_width);
    let pad = " ".repeat(cx);
    let inner_w = box_width.saturating_sub(4);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("{}Base URL (enter to keep default):", pad),
        Style::default().fg(theme::ASH_TEXT),
    )));
    lines.push(Line::from(""));

    let label = "Base URL";
    let top_border = format!(
        "{}\u{256d} {} {}\u{256e}",
        pad,
        label,
        "\u{2500}".repeat(box_width.saturating_sub(label.len() + 4)),
    );
    lines.push(Line::from(Span::styled(
        top_border,
        Style::default().fg(theme::ASH),
    )));

    let display_text = visible_slice(input, inner_w, cursor);
    let text_pad = inner_w.saturating_sub(display_text.chars().count());
    lines.push(Line::from(vec![
        Span::styled(format!("{}\u{2502} ", pad), Style::default().fg(theme::ASH)),
        Span::styled(
            format!("{} ", theme::PROMPT_CHAR),
            Style::default().fg(theme::SODIUM),
        ),
        Span::styled(
            display_text.to_string(),
            Style::default().fg(theme::CHALK_MID),
        ),
        Span::styled(
            format!("{}\u{2502}", " ".repeat(text_pad)),
            Style::default().fg(theme::ASH),
        ),
    ]));

    let bottom_border = format!(
        "{}\u{2570}{}\u{256f}",
        pad,
        "\u{2500}".repeat(box_width.saturating_sub(2)),
    );
    lines.push(Line::from(Span::styled(
        bottom_border,
        Style::default().fg(theme::ASH),
    )));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("{}enter", pad), theme::help_key()),
        Span::styled(" submit  ", theme::help_desc()),
        Span::styled("esc", theme::help_key()),
        Span::styled(" back", theme::help_desc()),
    ]));

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::FORM));
    frame.render_widget(paragraph, area);

    // Position cursor
    let cursor_chars = input[..cursor].chars().count();
    let vis_start = visible_start(input, inner_w, cursor);
    let cursor_offset = cursor_chars.saturating_sub(vis_start);
    let cursor_x = (cx + 4 + cursor_offset) as u16;
    let cursor_y = area.y + 4; // input line is 5th line (index 4)
    if cursor_x < area.x + area.width && cursor_y < area.y + area.height {
        frame.set_cursor_position((area.x + cursor_x, cursor_y));
    }
}

fn draw_tavily_input(frame: &mut Frame, area: Rect, input: &str, cursor: usize) {
    let box_width = 42usize;
    let cx = center_pad(area.width as usize, box_width);
    let pad = " ".repeat(cx);
    let inner_w = box_width.saturating_sub(4);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    // Label + input box
    let label = "Tavily API key (optional)";
    let top_border = format!(
        "{}\u{256d} {} {}\u{256e}",
        pad,
        label,
        "\u{2500}".repeat(box_width.saturating_sub(label.len() + 4)),
    );
    lines.push(Line::from(Span::styled(
        top_border,
        Style::default().fg(theme::ASH),
    )));

    let display_text = visible_slice(input, inner_w, cursor);
    let text_pad = inner_w.saturating_sub(display_text.chars().count());
    lines.push(Line::from(vec![
        Span::styled(format!("{}\u{2502} ", pad), Style::default().fg(theme::ASH)),
        Span::styled(
            format!("{} ", theme::PROMPT_CHAR),
            Style::default().fg(theme::SODIUM),
        ),
        Span::styled(
            display_text.to_string(),
            Style::default().fg(theme::CHALK_MID),
        ),
        Span::styled(
            format!("{}\u{2502}", " ".repeat(text_pad)),
            Style::default().fg(theme::ASH),
        ),
    ]));

    let bottom_border = format!(
        "{}\u{2570}{}\u{256f}",
        pad,
        "\u{2500}".repeat(box_width.saturating_sub(2)),
    );
    lines.push(Line::from(Span::styled(
        bottom_border,
        Style::default().fg(theme::ASH),
    )));

    lines.push(Line::from(""));
    // Help bar
    lines.push(Line::from(vec![
        Span::styled(format!("{}enter", pad), theme::help_key()),
        Span::styled(" submit  ", theme::help_desc()),
        Span::styled("esc", theme::help_key()),
        Span::styled(" skip", theme::help_desc()),
    ]));

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::FORM));
    frame.render_widget(paragraph, area);

    // Position cursor
    let cursor_chars = input[..cursor].chars().count();
    let vis_start = visible_start(input, inner_w, cursor);
    let cursor_offset = cursor_chars.saturating_sub(vis_start);
    let cursor_x = (cx + 4 + cursor_offset) as u16;
    let cursor_y = area.y + 2; // input line is 3rd line (index 2)
    if cursor_x < area.x + area.width && cursor_y < area.y + area.height {
        frame.set_cursor_position((area.x + cursor_x, cursor_y));
    }
}

fn draw_done(frame: &mut Frame, area: Rect) {
    let cx = center_pad(area.width as usize, 20);
    let pad = " ".repeat(cx);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{}Authenticated!", pad),
            Style::default()
                .fg(theme::LICHEN)
                .add_modifier(Modifier::BOLD),
        )),
    ];

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::FORM));
    frame.render_widget(paragraph, area);
}

// ── Helpers ─────────────────────────────────────────────────────────

fn center_pad(viewport_width: usize, content_width: usize) -> usize {
    viewport_width.saturating_sub(content_width) / 2
}

/// Return the visible portion of text that fits in `width` chars around the cursor.
fn visible_slice(input: &str, width: usize, cursor_byte: usize) -> &str {
    if width == 0 {
        return "";
    }
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let total = chars.len();
    if total <= width {
        return input;
    }
    let cursor_char = input[..cursor_byte].chars().count();
    let start = if cursor_char > width.saturating_sub(1) {
        cursor_char - width + 1
    } else {
        0
    };
    let end = (start + width).min(total);
    let byte_start = chars[start].0;
    let byte_end = if end < total {
        chars[end].0
    } else {
        input.len()
    };
    &input[byte_start..byte_end]
}

/// Return the char index of the first visible character.
fn visible_start(input: &str, width: usize, cursor_byte: usize) -> usize {
    let total = input.chars().count();
    if total <= width {
        return 0;
    }
    let cursor_char = input[..cursor_byte].chars().count();
    if cursor_char > width.saturating_sub(1) {
        cursor_char - width + 1
    } else {
        0
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    // Try common openers
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
    }
    Ok(())
}

fn oauth_url_path() -> std::path::PathBuf {
    let mut p = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    p.push(".lash");
    p.push("oauth-url.txt");
    p
}

fn oauth_url_display() -> String {
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        let full = oauth_url_path();
        if let Ok(rel) = full.strip_prefix(&home) {
            let rel_str = rel.display().to_string();
            return format!("~/{rel_str}");
        }
    }
    oauth_url_path().display().to_string()
}

fn persist_oauth_url(url: &str) {
    let path = oauth_url_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, url);
}
