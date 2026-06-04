//! Local browser console for the full Cockpit Tools UI.
//! This is intentionally separate from `web_report`, which remains the tokened report endpoint.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use tauri::{Emitter, Listener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout, Duration};
use url::Url;

use super::config::PORT_RANGE;

const DEFAULT_WEB_CONSOLE_PORT: u16 = 18081;
const MAX_HTTP_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(8);
const EVENT_POLL_TIMEOUT: Duration = Duration::from_secs(25);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(150);
const MAX_EVENT_QUEUE_LEN: usize = 1024;
const INDEX_HTML: &str = "index.html";

static ACTUAL_WEB_CONSOLE_PORT: OnceLock<RwLock<Option<u16>>> = OnceLock::new();
static WEB_EVENT_STATE: OnceLock<RwLock<WebEventState>> = OnceLock::new();

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: Option<String>,
    body: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct InvokeRequest {
    cmd: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Serialize)]
struct InvokeResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WebEventMessage {
    sequence: u64,
    event: String,
    payload: Value,
}

#[derive(Debug, Default)]
struct WebEventState {
    next_listener_id: u32,
    next_sequence: u64,
    browser_listeners: HashMap<u32, WebBrowserEventListener>,
    tauri_listeners: HashMap<String, tauri::EventId>,
    queue: VecDeque<WebEventMessage>,
}

#[derive(Debug, Clone)]
struct WebBrowserEventListener {
    event: String,
    client_id: String,
    registered_after_sequence: u64,
}

fn web_console_port_state() -> &'static RwLock<Option<u16>> {
    ACTUAL_WEB_CONSOLE_PORT.get_or_init(|| RwLock::new(None))
}

fn set_actual_port(port: Option<u16>) {
    if let Ok(mut guard) = web_console_port_state().write() {
        *guard = port;
    }
}

pub fn get_actual_port() -> Option<u16> {
    web_console_port_state()
        .read()
        .ok()
        .and_then(|guard| *guard)
}

pub async fn start_server() {
    let Some(dist_root) = find_frontend_dist() else {
        set_actual_port(None);
        super::logger::log_warn("[WebConsole] frontend dist directory not found, skip startup");
        return;
    };

    let mut port = DEFAULT_WEB_CONSOLE_PORT;
    let mut listener = None;
    for attempt in 0..PORT_RANGE {
        let addr = format!("127.0.0.1:{}", port);
        match TcpListener::bind(&addr).await {
            Ok(bound) => {
                listener = Some(bound);
                if attempt > 0 {
                    super::logger::log_info(&format!(
                        "[WebConsole] preferred port {} is busy, switched to {}",
                        DEFAULT_WEB_CONSOLE_PORT, port
                    ));
                }
                break;
            }
            Err(err) => {
                super::logger::log_warn(&format!(
                    "[WebConsole] failed to bind 127.0.0.1:{}: {}",
                    port, err
                ));
                port = port.saturating_add(1);
            }
        }
    }

    let Some(listener) = listener else {
        set_actual_port(None);
        super::logger::log_error("[WebConsole] no available local port");
        return;
    };

    set_actual_port(Some(port));
    super::logger::log_info(&format!(
        "[WebConsole] serving full UI at http://127.0.0.1:{}/",
        port
    ));

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let dist_root = dist_root.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, dist_root).await {
                        super::logger::log_warn(&format!("[WebConsole] request failed: {}", err));
                    }
                });
            }
            Err(err) => {
                super::logger::log_warn(&format!("[WebConsole] accept failed: {}", err));
            }
        }
    }
}

async fn handle_connection(mut stream: TcpStream, dist_root: PathBuf) -> Result<(), String> {
    let Some(request) = read_http_request(&mut stream).await? else {
        return Ok(());
    };

    if request.method == "OPTIONS" {
        return write_response(
            &mut stream,
            204,
            "No Content",
            "text/plain; charset=utf-8",
            b"",
        )
        .await;
    }

    if request.method == "POST" && request.path == "/__cockpit_web__/invoke" {
        return handle_invoke_request(&mut stream, &request).await;
    }

    if request.method == "GET" && request.path == "/__cockpit_web__/events" {
        return handle_event_poll_request(&mut stream, &request).await;
    }

    if request.method == "GET" && request.path == "/__cockpit_web__/health" {
        let body = json!({
            "ok": true,
            "port": get_actual_port(),
            "version": env!("CARGO_PKG_VERSION"),
        });
        let body = serde_json::to_vec(&body).map_err(|err| err.to_string())?;
        return write_response(
            &mut stream,
            200,
            "OK",
            "application/json; charset=utf-8",
            &body,
        )
        .await;
    }

    if request.method != "GET" && request.method != "HEAD" {
        return write_response(
            &mut stream,
            405,
            "Method Not Allowed",
            "text/plain; charset=utf-8",
            b"method not allowed",
        )
        .await;
    }

    let file_path = resolve_static_path(&dist_root, &request.path)?;
    let (file_path, content_type) = if file_path.exists() && file_path.is_file() {
        let content_type = content_type_for_path(&file_path);
        (file_path, content_type)
    } else {
        (dist_root.join(INDEX_HTML), "text/html; charset=utf-8")
    };

    let body = tokio::fs::read(&file_path)
        .await
        .map_err(|err| format!("read {} failed: {}", file_path.display(), err))?;
    if request.method == "HEAD" {
        return write_response(&mut stream, 200, "OK", content_type, b"").await;
    }
    write_response(&mut stream, 200, "OK", content_type, &body).await
}

async fn handle_invoke_request(
    stream: &mut TcpStream,
    request: &HttpRequest,
) -> Result<(), String> {
    let invoke: InvokeRequest =
        serde_json::from_slice(&request.body).map_err(|err| format!("invalid JSON: {}", err))?;
    let response = match dispatch_invoke(&invoke.cmd, &invoke.args).await {
        Ok(value) => InvokeResponse {
            ok: true,
            value: Some(value),
            error: None,
        },
        Err(error) => InvokeResponse {
            ok: false,
            value: None,
            error: Some(Value::String(error)),
        },
    };
    let status = if response.ok { 200 } else { 400 };
    let body = serde_json::to_vec(&response).map_err(|err| err.to_string())?;
    write_response(
        stream,
        status,
        if status == 200 { "OK" } else { "Bad Request" },
        "application/json; charset=utf-8",
        &body,
    )
    .await
}

async fn handle_event_poll_request(
    stream: &mut TcpStream,
    request: &HttpRequest,
) -> Result<(), String> {
    let after = query_param(request.query.as_deref(), "after")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let client_id = query_param(request.query.as_deref(), "clientId");
    let events = wait_for_web_events(client_id.as_deref(), after).await;
    let body = json!({
        "events": events,
        "latestSequence": latest_web_event_sequence(),
    });
    let body = serde_json::to_vec(&body).map_err(|err| err.to_string())?;
    write_response(stream, 200, "OK", "application/json; charset=utf-8", &body).await
}

async fn dispatch_invoke(cmd: &str, args: &Value) -> Result<Value, String> {
    match cmd {
        "plugin:app|version" => Ok(Value::String(env!("CARGO_PKG_VERSION").to_string())),
        "plugin:app|name" => Ok(Value::String("Cockpit Tools".to_string())),
        "plugin:app|identifier" => Ok(Value::String("com.jlcodes.cockpit-tools".to_string())),
        "plugin:app|tauri_version" => Ok(Value::String("2".to_string())),
        "plugin:event|listen" => serialize_value(register_web_event_listener(
            arg(args, "event")?,
            optional_string(args, "clientId").unwrap_or_else(|| "default".to_string()),
        )?),
        "plugin:event|unlisten" => {
            unregister_web_event_listener(
                arg(args, "eventId")?,
                optional_string(args, "clientId").as_deref(),
            )?;
            Ok(Value::Null)
        }
        "plugin:event|emit" | "plugin:event|emit_to" => {
            emit_web_event_from_browser(arg(args, "event")?, args.get("payload").cloned())?;
            Ok(Value::Null)
        }
        "plugin:window|get_all_windows" => Ok(json!([{ "label": "main" }])),
        "plugin:webview|get_all_webviews" => {
            Ok(json!([{ "label": "main", "windowLabel": "main" }]))
        }
        "plugin:window|start_dragging"
        | "plugin:window|set_theme"
        | "plugin:webview|set_webview_zoom"
        | "plugin:webview|set_zoom" => Ok(Value::Null),

        "list_accounts" => to_value(crate::commands::account::list_accounts().await),
        "get_current_account" => to_value(crate::commands::account::get_current_account().await),
        "set_current_account" => to_value(
            crate::commands::account::set_current_account(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "fetch_account_quota" => to_value(
            crate::commands::account::fetch_account_quota(arg(args, "accountId")?)
                .await
                .map_err(|err| err.to_string()),
        ),
        "refresh_all_quotas" => {
            to_value(crate::commands::account::refresh_all_quotas(app_handle()?).await)
        }
        "refresh_current_quota" => {
            to_value(crate::commands::account::refresh_current_quota(app_handle()?).await)
        }
        "switch_account" => to_value(
            crate::commands::account::switch_account(
                app_handle()?,
                arg(args, "accountId")?,
                opt_arg(args, "runtimeTarget")?,
            )
            .await,
        ),
        "update_account_tags" => to_value(
            crate::commands::account::update_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "update_account_notes" => to_value(
            crate::commands::account::update_account_notes(
                arg(args, "accountId")?,
                arg(args, "notes")?,
            )
            .await,
        ),
        "load_account_groups" => to_value(crate::commands::account::load_account_groups().await),
        "save_account_groups" => {
            to_value(crate::commands::account::save_account_groups(arg(args, "data")?).await)
        }

        "list_codex_accounts" => to_value(crate::commands::codex::list_codex_accounts()),
        "get_current_codex_account" => {
            to_value(crate::commands::codex::get_current_codex_account())
        }
        "refresh_current_codex_quota" => {
            to_value(crate::commands::codex::refresh_current_codex_quota(app_handle()?).await)
        }
        "refresh_codex_quota" => to_value(
            crate::commands::codex::refresh_codex_quota(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "refresh_codex_subscription_info" => to_value(
            crate::commands::codex::refresh_codex_subscription_info(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "refresh_codex_account_profile" => to_value(
            crate::commands::codex::refresh_codex_account_profile(arg(args, "accountId")?).await,
        ),
        "refresh_all_codex_quotas" => {
            to_value(crate::commands::codex::refresh_all_codex_quotas(app_handle()?).await)
        }
        "switch_codex_account" => to_value(
            crate::commands::codex::switch_codex_account(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "load_codex_account_groups" => {
            to_value(crate::commands::codex::load_codex_account_groups().await)
        }
        "save_codex_account_groups" => {
            to_value(crate::commands::codex::save_codex_account_groups(arg(args, "data")?).await)
        }
        "get_codex_quick_config" => to_value(crate::commands::codex::get_codex_quick_config()),
        "save_codex_quick_config" => to_value(crate::commands::codex::save_codex_quick_config(
            opt_arg(args, "modelContextWindow")?,
            opt_arg(args, "autoCompactTokenLimit")?,
        )),
        "get_codex_app_speed_config" => {
            to_value(crate::commands::codex::get_codex_app_speed_config())
        }
        "save_codex_app_speed" => to_value(crate::commands::codex::save_codex_app_speed(arg(
            args, "speed",
        )?)),
        "get_codex_api_service_app_speed_config" => {
            to_value(crate::commands::codex::get_codex_api_service_app_speed_config())
        }
        "save_codex_api_service_app_speed" => to_value(
            crate::commands::codex::save_codex_api_service_app_speed(arg(args, "speed")?),
        ),
        "codex_local_access_get_state" => {
            to_value(crate::commands::codex::codex_local_access_get_state().await)
        }

        "list_github_copilot_accounts" => {
            to_value(crate::commands::github_copilot::list_github_copilot_accounts())
        }
        "refresh_github_copilot_token" => to_value(
            crate::commands::github_copilot::refresh_github_copilot_token(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "refresh_all_github_copilot_tokens" => to_value(
            crate::commands::github_copilot::refresh_all_github_copilot_tokens(app_handle()?).await,
        ),
        "inject_github_copilot_to_vscode" => to_value(
            crate::commands::github_copilot::inject_github_copilot_to_vscode(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "update_github_copilot_account_tags" => to_value(
            crate::commands::github_copilot::update_github_copilot_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "list_windsurf_accounts" => to_value(crate::commands::windsurf::list_windsurf_accounts()),
        "refresh_windsurf_token" => to_value(
            crate::commands::windsurf::refresh_windsurf_token(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "refresh_all_windsurf_tokens" => {
            to_value(crate::commands::windsurf::refresh_all_windsurf_tokens(app_handle()?).await)
        }
        "inject_windsurf_to_vscode" => to_value(
            crate::commands::windsurf::inject_windsurf_to_vscode(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "update_windsurf_account_tags" => to_value(
            crate::commands::windsurf::update_windsurf_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "list_kiro_accounts" => to_value(crate::commands::kiro::list_kiro_accounts()),
        "refresh_kiro_token" => to_value(
            crate::commands::kiro::refresh_kiro_token(app_handle()?, arg(args, "accountId")?).await,
        ),
        "refresh_all_kiro_tokens" => {
            to_value(crate::commands::kiro::refresh_all_kiro_tokens(app_handle()?).await)
        }
        "inject_kiro_to_vscode" => to_value(
            crate::commands::kiro::inject_kiro_to_vscode(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "update_kiro_account_tags" => to_value(
            crate::commands::kiro::update_kiro_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "list_cursor_accounts" => to_value(crate::commands::cursor::list_cursor_accounts()),
        "refresh_cursor_token" => to_value(
            crate::commands::cursor::refresh_cursor_token(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "refresh_all_cursor_tokens" => {
            to_value(crate::commands::cursor::refresh_all_cursor_tokens(app_handle()?).await)
        }
        "inject_cursor_account" => to_value(
            crate::commands::cursor::inject_cursor_account(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "update_cursor_account_tags" => to_value(
            crate::commands::cursor::update_cursor_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "list_gemini_accounts" => to_value(crate::commands::gemini::list_gemini_accounts()),
        "refresh_gemini_token" => to_value(
            crate::commands::gemini::refresh_gemini_token(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "refresh_all_gemini_tokens" => {
            to_value(crate::commands::gemini::refresh_all_gemini_tokens(app_handle()?).await)
        }
        "inject_gemini_account" => to_value(crate::commands::gemini::inject_gemini_account(
            app_handle()?,
            arg(args, "accountId")?,
        )),
        "update_gemini_account_tags" => {
            to_value(crate::commands::gemini::update_gemini_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            ))
        }
        "list_codebuddy_accounts" => {
            to_value(crate::commands::codebuddy::list_codebuddy_accounts())
        }
        "refresh_codebuddy_token" => to_value(
            crate::commands::codebuddy::refresh_codebuddy_token(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "refresh_all_codebuddy_tokens" => {
            to_value(crate::commands::codebuddy::refresh_all_codebuddy_tokens(app_handle()?).await)
        }
        "inject_codebuddy_to_vscode" => to_value(
            crate::commands::codebuddy::inject_codebuddy_to_vscode(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "update_codebuddy_account_tags" => to_value(
            crate::commands::codebuddy::update_codebuddy_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "list_codebuddy_cn_accounts" => {
            to_value(crate::commands::codebuddy_cn::list_codebuddy_cn_accounts())
        }
        "refresh_codebuddy_cn_token" => to_value(
            crate::commands::codebuddy_cn::refresh_codebuddy_cn_token(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "refresh_all_codebuddy_cn_tokens" => to_value(
            crate::commands::codebuddy_cn::refresh_all_codebuddy_cn_tokens(app_handle()?).await,
        ),
        "inject_codebuddy_cn_to_vscode" => to_value(
            crate::commands::codebuddy_cn::inject_codebuddy_cn_to_vscode(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "update_codebuddy_cn_account_tags" => to_value(
            crate::commands::codebuddy_cn::update_codebuddy_cn_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "list_qoder_accounts" => to_value(crate::commands::qoder::list_qoder_accounts()),
        "refresh_qoder_token" => to_value(
            crate::commands::qoder::refresh_qoder_token(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "refresh_all_qoder_tokens" => {
            to_value(crate::commands::qoder::refresh_all_qoder_tokens(app_handle()?).await)
        }
        "inject_qoder_account" => to_value(
            crate::commands::qoder::inject_qoder_account(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "update_qoder_account_tags" => to_value(crate::commands::qoder::update_qoder_account_tags(
            arg(args, "accountId")?,
            arg(args, "tags")?,
        )),
        "list_trae_accounts" => to_value(crate::commands::trae::list_trae_accounts()),
        "refresh_trae_token" => to_value(
            crate::commands::trae::refresh_trae_token(app_handle()?, arg(args, "accountId")?).await,
        ),
        "refresh_all_trae_tokens" => {
            to_value(crate::commands::trae::refresh_all_trae_tokens(app_handle()?).await)
        }
        "inject_trae_account" => to_value(
            crate::commands::trae::inject_trae_account(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        "update_trae_account_tags" => to_value(
            crate::commands::trae::update_trae_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "list_workbuddy_accounts" => {
            to_value(crate::commands::workbuddy::list_workbuddy_accounts())
        }
        "refresh_workbuddy_token" => to_value(
            crate::commands::workbuddy::refresh_workbuddy_token(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "refresh_all_workbuddy_tokens" => {
            to_value(crate::commands::workbuddy::refresh_all_workbuddy_tokens(app_handle()?).await)
        }
        "inject_workbuddy_to_vscode" => to_value(
            crate::commands::workbuddy::inject_workbuddy_to_vscode(
                app_handle()?,
                arg(args, "accountId")?,
            )
            .await,
        ),
        "update_workbuddy_account_tags" => to_value(
            crate::commands::workbuddy::update_workbuddy_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "list_zed_accounts" => to_value(crate::commands::zed::list_zed_accounts()),
        "refresh_zed_token" => to_value(
            crate::commands::zed::refresh_zed_token(app_handle()?, arg(args, "accountId")?).await,
        ),
        "refresh_all_zed_tokens" => {
            to_value(crate::commands::zed::refresh_all_zed_tokens(app_handle()?).await)
        }
        "inject_zed_account" => to_value(
            crate::commands::zed::inject_zed_account(app_handle()?, arg(args, "accountId")?).await,
        ),
        "update_zed_account_tags" => to_value(crate::commands::zed::update_zed_account_tags(
            arg(args, "accountId")?,
            arg(args, "tags")?,
        )),

        "get_provider_current_account_id" => to_value(
            crate::commands::provider_current::get_provider_current_account_id(
                app_handle()?,
                arg(args, "platform")?,
            )
            .await,
        ),

        "get_network_config" => to_value(crate::commands::system::get_network_config()),
        "save_network_config" => to_value(crate::commands::system::save_network_config(
            arg(args, "wsEnabled")?,
            arg(args, "wsPort")?,
            opt_arg(args, "reportEnabled")?,
            opt_arg(args, "reportPort")?,
            opt_arg(args, "reportToken")?,
            opt_arg(args, "globalProxyEnabled")?,
            opt_arg(args, "globalProxyUrl")?,
            opt_arg(args, "globalProxyNoProxy")?,
        )),
        "get_general_config" => {
            to_value(crate::commands::system::get_general_config(app_handle()?))
        }
        "save_general_config" => dispatch_save_general_config(args),
        "get_available_terminals" => {
            to_value(crate::commands::system::get_available_terminals().await)
        }
        "set_app_path" => to_value(crate::commands::system::set_app_path(
            arg(args, "app")?,
            arg(args, "path")?,
        )),
        "set_codex_launch_on_switch" => to_value(
            crate::commands::system::set_codex_launch_on_switch(arg(args, "enabled")?),
        ),
        "set_codex_local_access_entry_visible" => to_value(
            crate::commands::system::set_codex_local_access_entry_visible(arg(args, "enabled")?),
        ),
        "save_tray_platform_layout" => {
            to_value(crate::commands::system::save_tray_platform_layout(
                app_handle()?,
                arg(args, "sortMode")?,
                arg(args, "orderedPlatformIds")?,
                arg(args, "trayPlatformIds")?,
                opt_arg(args, "orderedEntryIds")?,
                opt_arg(args, "platformGroups")?,
            ))
        }
        "set_wakeup_override" => to_value(crate::commands::system::set_wakeup_override(arg(
            args, "enabled",
        )?)),
        "external_import_take_pending" => {
            serialize_value(crate::commands::system::external_import_take_pending())
        }
        "external_import_fetch_import_url" => to_value(
            crate::commands::system::external_import_fetch_import_url(arg(args, "importUrl")?)
                .await,
        ),
        "detect_app_path" => to_value(crate::commands::system::detect_app_path(
            arg(args, "app")?,
            opt_arg(args, "force")?,
        )),
        "get_antigravity_installed_version_info" => to_value(
            crate::commands::system::get_antigravity_installed_version_info(
                opt_arg(args, "runtimeTarget")?,
                opt_arg(args, "scanMode")?,
            )
            .await,
        ),
        "get_auto_backup_settings" => to_value(crate::commands::system::get_auto_backup_settings()),
        "save_auto_backup_settings" => {
            to_value(crate::commands::system::save_auto_backup_settings(
                arg(args, "enabled")?,
                arg(args, "includeAccounts")?,
                arg(args, "includeConfig")?,
                arg(args, "retentionDays")?,
            ))
        }
        "update_auto_backup_last_run" => to_value(
            crate::commands::system::update_auto_backup_last_run(opt_arg(args, "lastBackupAt")?),
        ),
        "write_auto_backup_file" => to_value(crate::commands::system::write_auto_backup_file(
            arg(args, "fileName")?,
            arg(args, "content")?,
        )),
        "read_auto_backup_file" => to_value(crate::commands::system::read_auto_backup_file(arg(
            args, "fileName",
        )?)),
        "copy_auto_backup_file" => to_value(crate::commands::system::copy_auto_backup_file(
            arg(args, "fileName")?,
            arg(args, "targetPath")?,
        )),
        "list_auto_backup_files" => to_value(crate::commands::system::list_auto_backup_files()),
        "delete_auto_backup_file" => to_value(crate::commands::system::delete_auto_backup_file(
            arg(args, "fileName")?,
        )),
        "cleanup_auto_backup_files" => to_value(
            crate::commands::system::cleanup_auto_backup_files(arg(args, "retentionDays")?),
        ),
        "open_auto_backup_dir" => to_value(crate::commands::system::open_auto_backup_dir()),
        "open_data_folder" => to_value(crate::commands::system::open_data_folder().await),
        "open_folder" => to_value(crate::commands::system::open_folder(arg(args, "path")?).await),
        "show_floating_card_window" => to_value(
            crate::commands::system::show_floating_card_window(app_handle()?),
        ),
        "show_instance_floating_card_window" => {
            to_value(crate::commands::system::show_instance_floating_card_window(
                app_handle()?,
                arg(args, "context")?,
            ))
        }
        "get_floating_card_context" => to_value(
            crate::commands::system::get_floating_card_context(arg(args, "windowLabel")?),
        ),
        "hide_floating_card_window" => to_value(
            crate::commands::system::hide_floating_card_window(app_handle()?),
        ),
        "hide_current_floating_card_window" => Ok(Value::Null),
        "set_floating_card_always_on_top" => {
            to_value(crate::commands::system::set_floating_card_always_on_top(
                app_handle()?,
                arg(args, "alwaysOnTop")?,
            ))
        }
        "set_current_floating_card_window_always_on_top" => Ok(Value::Null),
        "set_floating_card_confirm_on_close" => {
            to_value(crate::commands::system::set_floating_card_confirm_on_close(
                arg(args, "confirmOnClose")?,
            ))
        }
        "save_floating_card_position" => to_value(
            crate::commands::system::save_floating_card_position(arg(args, "x")?, arg(args, "y")?),
        ),
        "show_main_window_and_navigate" => {
            to_value(crate::commands::system::show_main_window_and_navigate(
                app_handle()?,
                arg(args, "page")?,
            ))
        }
        "logs_get_snapshot" => to_value(crate::commands::logs::logs_get_snapshot(
            opt_arg(args, "fileName")?,
            Some(arg_or(args, "lineLimit", 500usize)?),
        )),
        "logs_open_log_directory" => to_value(crate::commands::logs::logs_open_log_directory()),

        "wakeup_ensure_runtime_ready" => {
            to_value(crate::commands::wakeup::wakeup_ensure_runtime_ready(
                opt_arg(args, "officialLsVersionMode")?,
            ))
        }
        "wakeup_set_official_ls_version_mode" => to_value(
            crate::commands::wakeup::wakeup_set_official_ls_version_mode(opt_arg(args, "mode")?),
        ),
        "trigger_wakeup" => to_value(
            crate::commands::wakeup::trigger_wakeup(
                arg(args, "accountId")?,
                arg(args, "model")?,
                opt_arg(args, "prompt")?,
                opt_arg(args, "maxOutputTokens")?,
                opt_arg(args, "cancelScopeId")?,
                opt_arg(args, "officialLsVersionMode")?,
            )
            .await,
        ),
        "fetch_available_models" => {
            to_value(crate::commands::wakeup::fetch_available_models().await)
        }
        "wakeup_validate_crontab" => to_value(crate::commands::wakeup::wakeup_validate_crontab(
            arg(args, "expr")?,
        )),
        "wakeup_sync_state" => to_value(
            crate::commands::wakeup::wakeup_sync_state(
                app_handle()?,
                arg(args, "enabled")?,
                arg(args, "tasks")?,
                opt_arg(args, "officialLsVersionMode")?,
                opt_arg(args, "runStartupTasks")?,
            )
            .await,
        ),
        "wakeup_run_enabled_tasks" => to_value(
            crate::commands::wakeup::wakeup_run_enabled_tasks(
                app_handle()?,
                opt_arg(args, "triggerSource")?,
                opt_arg(args, "officialLsVersionMode")?,
            )
            .await,
        ),
        "wakeup_load_history" => to_value(crate::commands::wakeup::wakeup_load_history()),
        "wakeup_add_history" => to_value(crate::commands::wakeup::wakeup_add_history(arg(
            args, "items",
        )?)),
        "wakeup_clear_history" => to_value(crate::commands::wakeup::wakeup_clear_history()),
        "wakeup_cancel_scope" => to_value(crate::commands::wakeup::wakeup_cancel_scope(arg(
            args,
            "cancelScopeId",
        )?)),
        "wakeup_release_scope" => to_value(crate::commands::wakeup::wakeup_release_scope(arg(
            args,
            "cancelScopeId",
        )?)),
        "wakeup_verification_load_state" => {
            to_value(crate::commands::wakeup::wakeup_verification_load_state())
        }
        "wakeup_verification_load_history" => {
            to_value(crate::commands::wakeup::wakeup_verification_load_history())
        }
        "wakeup_verification_delete_history" => to_value(
            crate::commands::wakeup::wakeup_verification_delete_history(arg(args, "batchIds")?),
        ),
        "wakeup_verification_run_batch" => to_value(
            crate::commands::wakeup::wakeup_verification_run_batch(
                app_handle()?,
                arg(args, "accountIds")?,
                arg(args, "model")?,
                opt_arg(args, "prompt")?,
                opt_arg(args, "maxOutputTokens")?,
                opt_arg(args, "officialLsVersionMode")?,
            )
            .await,
        ),
        "confirm_wakeup_task" => to_value(
            crate::commands::wakeup::confirm_wakeup_task(app_handle()?, arg(args, "taskId")?).await,
        ),
        "cancel_wakeup_task" => {
            to_value(crate::commands::wakeup::cancel_wakeup_task(arg(args, "taskId")?).await)
        }
        "check_wakeup_timeouts" => {
            to_value(crate::commands::wakeup::check_wakeup_timeouts(app_handle()?).await)
        }

        "codex_wakeup_get_cli_status" => {
            to_value(crate::commands::codex::codex_wakeup_get_cli_status())
        }
        "codex_wakeup_update_runtime_config" => {
            to_value(crate::commands::codex::codex_wakeup_update_runtime_config(
                opt_arg(args, "codexCliPath")?,
                opt_arg(args, "nodePath")?,
            ))
        }
        "codex_wakeup_get_overview" => {
            to_value(crate::commands::codex::codex_wakeup_get_overview())
        }
        "codex_wakeup_get_state" => to_value(crate::commands::codex::codex_wakeup_get_state()),
        "codex_wakeup_save_state" => to_value(crate::commands::codex::codex_wakeup_save_state(
            arg(args, "enabled")?,
            arg(args, "tasks")?,
            arg(args, "modelPresets")?,
            arg(args, "modelPresetMigrations")?,
        )),
        "codex_wakeup_load_history" => {
            to_value(crate::commands::codex::codex_wakeup_load_history())
        }
        "codex_wakeup_clear_history" => {
            to_value(crate::commands::codex::codex_wakeup_clear_history())
        }
        "codex_wakeup_cancel_scope" => to_value(crate::commands::codex::codex_wakeup_cancel_scope(
            arg(args, "cancelScopeId")?,
        )),
        "codex_wakeup_release_scope" => to_value(
            crate::commands::codex::codex_wakeup_release_scope(arg(args, "cancelScopeId")?),
        ),
        "codex_wakeup_test" => to_value(
            crate::commands::codex::codex_wakeup_test(
                app_handle()?,
                arg(args, "accountIds")?,
                opt_arg(args, "prompt")?,
                opt_arg(args, "model")?,
                opt_arg(args, "modelDisplayName")?,
                opt_arg(args, "modelReasoningEffort")?,
                opt_arg(args, "runId")?,
                opt_arg(args, "cancelScopeId")?,
            )
            .await,
        ),
        "codex_wakeup_run_task" => to_value(
            crate::commands::codex::codex_wakeup_run_task(
                app_handle()?,
                arg(args, "taskId")?,
                opt_arg(args, "runId")?,
            )
            .await,
        ),
        "codex_wakeup_run_enabled_tasks" => to_value(
            crate::commands::codex::codex_wakeup_run_enabled_tasks(
                app_handle()?,
                opt_arg(args, "triggerType")?,
            )
            .await,
        ),

        "get_update_settings" => to_value(crate::commands::update::get_update_settings()),
        "save_update_settings" => to_value(crate::commands::update::save_update_settings(arg(
            args, "settings",
        )?)),
        "should_check_updates" => to_value(crate::commands::update::should_check_updates()),
        "update_last_check_time" => to_value(crate::commands::update::update_last_check_time()),
        "check_version_jump" => to_value(crate::commands::update::check_version_jump()),
        "get_release_history" => to_value(crate::commands::update::get_release_history(
            opt_arg(args, "locale")?,
            opt_arg(args, "limit")?,
        )),
        "update_log" => to_value(crate::commands::update::update_log(
            arg(args, "level")?,
            arg(args, "message")?,
        )),
        "get_update_runtime_info" => to_value(crate::commands::update::get_update_runtime_info()),

        "announcement_get_state" => {
            to_value(crate::commands::announcement::announcement_get_state().await)
        }
        "announcement_mark_as_read" => to_value(
            crate::commands::announcement::announcement_mark_as_read(arg(args, "id")?).await,
        ),
        "announcement_mark_all_as_read" => {
            to_value(crate::commands::announcement::announcement_mark_all_as_read().await)
        }
        "announcement_force_refresh" => {
            to_value(crate::commands::announcement::announcement_force_refresh().await)
        }
        "announcement_get_top_right_ad" => {
            to_value(crate::commands::announcement::announcement_get_top_right_ad().await)
        }

        "get_group_settings" => to_value(crate::commands::group::get_group_settings()),
        "get_display_groups" => to_value(crate::commands::group::get_display_groups()),

        "codex_get_instance_defaults" => {
            to_value(crate::commands::codex_instance::codex_get_instance_defaults().await)
        }
        "codex_list_instances" => {
            to_value(crate::commands::codex_instance::codex_list_instances().await)
        }
        "github_copilot_get_instance_defaults" => to_value(
            crate::commands::github_copilot_instance::github_copilot_get_instance_defaults().await,
        ),
        "github_copilot_list_instances" => to_value(
            crate::commands::github_copilot_instance::github_copilot_list_instances().await,
        ),
        "windsurf_get_instance_defaults" => {
            to_value(crate::commands::windsurf_instance::windsurf_get_instance_defaults().await)
        }
        "windsurf_list_instances" => {
            to_value(crate::commands::windsurf_instance::windsurf_list_instances().await)
        }
        "kiro_get_instance_defaults" => {
            to_value(crate::commands::kiro_instance::kiro_get_instance_defaults().await)
        }
        "kiro_list_instances" => {
            to_value(crate::commands::kiro_instance::kiro_list_instances().await)
        }
        "cursor_get_instance_defaults" => {
            to_value(crate::commands::cursor_instance::cursor_get_instance_defaults().await)
        }
        "cursor_list_instances" => {
            to_value(crate::commands::cursor_instance::cursor_list_instances().await)
        }
        "gemini_get_instance_defaults" => {
            to_value(crate::commands::gemini_instance::gemini_get_instance_defaults().await)
        }
        "gemini_list_instances" => {
            to_value(crate::commands::gemini_instance::gemini_list_instances().await)
        }
        "codebuddy_get_instance_defaults" => {
            to_value(crate::commands::codebuddy_instance::codebuddy_get_instance_defaults().await)
        }
        "codebuddy_list_instances" => {
            to_value(crate::commands::codebuddy_instance::codebuddy_list_instances().await)
        }
        "codebuddy_cn_get_instance_defaults" => to_value(
            crate::commands::codebuddy_cn_instance::codebuddy_cn_get_instance_defaults().await,
        ),
        "codebuddy_cn_list_instances" => {
            to_value(crate::commands::codebuddy_cn_instance::codebuddy_cn_list_instances().await)
        }
        "qoder_get_instance_defaults" => {
            to_value(crate::commands::qoder_instance::qoder_get_instance_defaults().await)
        }
        "qoder_list_instances" => {
            to_value(crate::commands::qoder_instance::qoder_list_instances().await)
        }
        "trae_get_instance_defaults" => {
            to_value(crate::commands::trae_instance::trae_get_instance_defaults().await)
        }
        "trae_list_instances" => {
            to_value(crate::commands::trae_instance::trae_list_instances().await)
        }
        "workbuddy_get_instance_defaults" => {
            to_value(crate::commands::workbuddy_instance::workbuddy_get_instance_defaults().await)
        }
        "workbuddy_list_instances" => {
            to_value(crate::commands::workbuddy_instance::workbuddy_list_instances().await)
        }
        other => dispatch_registered_app_command(other, args).await,
    }
}

async fn dispatch_registered_app_command(cmd: &str, args: &Value) -> Result<Value, String> {
    match cmd {
        // account
        "add_account" => {
            to_value(crate::commands::account::add_account(arg(args, "refreshToken")?).await)
        }
        "delete_account" => {
            to_value(crate::commands::account::delete_account(arg(args, "accountId")?).await)
        }
        "delete_accounts" => {
            to_value(crate::commands::account::delete_accounts(arg(args, "accountIds")?).await)
        }
        "reorder_accounts" => {
            to_value(crate::commands::account::reorder_accounts(arg(args, "accountIds")?).await)
        }
        "load_antigravity_switch_history" => {
            to_value(crate::commands::account::load_antigravity_switch_history())
        }
        "clear_antigravity_switch_history" => {
            to_value(crate::commands::account::clear_antigravity_switch_history())
        }
        "bind_account_fingerprint" => to_value(
            crate::commands::account::bind_account_fingerprint(
                arg(args, "accountId")?,
                arg(args, "fingerprintId")?,
            )
            .await,
        ),
        "get_bound_accounts" => to_value(
            crate::commands::account::get_bound_accounts(arg(args, "fingerprintId")?).await,
        ),
        "sync_current_from_client" => {
            to_value(crate::commands::account::sync_current_from_client(app_handle()?).await)
        }
        "sync_from_extension" => {
            to_value(crate::commands::account::sync_from_extension(app_handle()?).await)
        }
        // device
        "get_device_profiles" => {
            to_value(crate::commands::device::get_device_profiles(arg(args, "accountId")?).await)
        }
        "bind_device_profile" => to_value(
            crate::commands::device::bind_device_profile(
                arg(args, "accountId")?,
                arg(args, "mode")?,
            )
            .await,
        ),
        "bind_device_profile_with_profile" => to_value(
            crate::commands::device::bind_device_profile_with_profile(
                arg(args, "accountId")?,
                arg(args, "profile")?,
            )
            .await,
        ),
        "list_device_versions" => {
            to_value(crate::commands::device::list_device_versions(arg(args, "accountId")?).await)
        }
        "restore_device_version" => to_value(
            crate::commands::device::restore_device_version(
                arg(args, "accountId")?,
                arg(args, "versionId")?,
            )
            .await,
        ),
        "delete_device_version" => to_value(
            crate::commands::device::delete_device_version(
                arg(args, "accountId")?,
                arg(args, "versionId")?,
            )
            .await,
        ),
        "restore_original_device" => {
            to_value(crate::commands::device::restore_original_device().await)
        }
        "open_device_folder" => to_value(crate::commands::device::open_device_folder().await),
        "preview_generate_profile" => {
            to_value(crate::commands::device::preview_generate_profile().await)
        }
        "preview_current_profile" => {
            to_value(crate::commands::device::preview_current_profile().await)
        }
        "list_fingerprints" => to_value(crate::commands::device::list_fingerprints().await),
        "get_fingerprint" => {
            to_value(crate::commands::device::get_fingerprint(arg(args, "fingerprintId")?).await)
        }
        "generate_new_fingerprint" => {
            to_value(crate::commands::device::generate_new_fingerprint(arg(args, "name")?).await)
        }
        "capture_current_fingerprint" => {
            to_value(crate::commands::device::capture_current_fingerprint(arg(args, "name")?).await)
        }
        "create_fingerprint_with_profile" => to_value(
            crate::commands::device::create_fingerprint_with_profile(
                arg(args, "name")?,
                arg(args, "profile")?,
            )
            .await,
        ),
        "apply_fingerprint" => {
            to_value(crate::commands::device::apply_fingerprint(arg(args, "fingerprintId")?).await)
        }
        "delete_fingerprint" => {
            to_value(crate::commands::device::delete_fingerprint(arg(args, "fingerprintId")?).await)
        }
        "delete_unbound_fingerprints" => {
            to_value(crate::commands::device::delete_unbound_fingerprints().await)
        }
        "rename_fingerprint" => to_value(
            crate::commands::device::rename_fingerprint(
                arg(args, "fingerprintId")?,
                arg(args, "name")?,
            )
            .await,
        ),
        "get_current_fingerprint_id" => {
            to_value(crate::commands::device::get_current_fingerprint_id().await)
        }
        // oauth
        "start_oauth_login" => {
            to_value(crate::commands::oauth::start_oauth_login(app_handle()?).await)
        }
        "prepare_oauth_url" => {
            to_value(crate::commands::oauth::prepare_oauth_url(app_handle()?).await)
        }
        "complete_oauth_login" => {
            to_value(crate::commands::oauth::complete_oauth_login(app_handle()?).await)
        }
        "submit_oauth_callback_url" => to_value(
            crate::commands::oauth::submit_oauth_callback_url(
                app_handle()?,
                arg(args, "callbackUrl")?,
            )
            .await,
        ),
        "cancel_oauth_login" => to_value(crate::commands::oauth::cancel_oauth_login().await),
        // import
        "import_from_old_tools" => to_value(crate::commands::import::import_from_old_tools().await),
        "import_fingerprints_from_old_tools" => {
            to_value(crate::commands::import::import_fingerprints_from_old_tools().await)
        }
        "import_fingerprints_from_json" => to_value(
            crate::commands::import::import_fingerprints_from_json(arg(args, "jsonContent")?).await,
        ),
        "import_from_local" => {
            to_value(crate::commands::import::import_from_local(app_handle()?).await)
        }
        "import_from_json" => {
            to_value(crate::commands::import::import_from_json(arg(args, "jsonContent")?).await)
        }
        "import_from_files" => {
            to_value(crate::commands::import::import_from_files(arg(args, "filePaths")?).await)
        }
        "export_accounts" => {
            to_value(crate::commands::import::export_accounts(arg(args, "accountIds")?).await)
        }
        // data_transfer
        "data_transfer_get_user_config" => {
            to_value(crate::commands::data_transfer::data_transfer_get_user_config())
        }
        "data_transfer_apply_user_config" => to_value(
            crate::commands::data_transfer::data_transfer_apply_user_config(
                app_handle()?,
                arg(args, "config")?,
            ),
        ),
        "data_transfer_get_instance_store" => to_value(
            crate::commands::data_transfer::data_transfer_get_instance_store(arg(
                args, "platform",
            )?),
        ),
        "data_transfer_replace_instance_store" => to_value(
            crate::commands::data_transfer::data_transfer_replace_instance_store(
                arg(args, "platform")?,
                arg(args, "store")?,
            ),
        ),
        // system
        "save_text_file" => to_value(
            crate::commands::system::save_text_file(arg(args, "path")?, arg(args, "content")?)
                .await,
        ),
        "get_downloads_dir" => to_value(crate::commands::system::get_downloads_dir()),
        "handle_window_close" => Ok(Value::Null),
        "delete_corrupted_file" => {
            to_value(crate::commands::system::delete_corrupted_file(arg(args, "path")?).await)
        }
        // update
        "save_pending_update_notes" => {
            to_value(crate::commands::update::save_pending_update_notes(
                arg(args, "version")?,
                arg(args, "releaseNotes")?,
                arg(args, "releaseNotesZh")?,
            ))
        }
        "install_linux_update" => to_value(
            crate::commands::update::install_linux_update(
                app_handle()?,
                opt_arg(args, "expectedVersion")?,
            )
            .await,
        ),
        // group
        "save_group_settings" => to_value(crate::commands::group::save_group_settings(
            app_handle()?,
            arg(args, "groupMappings")?,
            arg(args, "groupNames")?,
            arg(args, "groupOrder")?,
        )),
        "set_model_group" => to_value(crate::commands::group::set_model_group(
            app_handle()?,
            arg(args, "modelId")?,
            arg(args, "groupId")?,
        )),
        "remove_model_group" => to_value(crate::commands::group::remove_model_group(
            app_handle()?,
            arg(args, "modelId")?,
        )),
        "set_group_name" => to_value(crate::commands::group::set_group_name(
            app_handle()?,
            arg(args, "groupId")?,
            arg(args, "name")?,
        )),
        "delete_group" => to_value(crate::commands::group::delete_group(
            app_handle()?,
            arg(args, "groupId")?,
        )),
        "update_group_order" => to_value(crate::commands::group::update_group_order(
            app_handle()?,
            arg(args, "order")?,
        )),
        // codex
        "get_codex_config_toml_path" => {
            to_value(crate::commands::codex::get_codex_config_toml_path())
        }
        "open_codex_config_toml" => {
            to_value(crate::commands::codex::open_codex_config_toml(app_handle()?))
        }
        "update_codex_account_app_speed" => {
            to_value(crate::commands::codex::update_codex_account_app_speed(
                arg(args, "accountId")?,
                arg(args, "speed")?,
            ))
        }
        "delete_codex_account" => to_value(crate::commands::codex::delete_codex_account(arg(
            args,
            "accountId",
        )?)),
        "delete_codex_accounts" => to_value(crate::commands::codex::delete_codex_accounts(arg(
            args,
            "accountIds",
        )?)),
        "import_codex_from_local" => {
            to_value(crate::commands::codex::import_codex_from_local(app_handle()?).await)
        }
        "import_codex_from_json" => to_value(
            crate::commands::codex::import_codex_from_json(
                app_handle()?,
                arg(args, "jsonContent")?,
            )
            .await,
        ),
        "export_codex_accounts" => to_value(crate::commands::codex::export_codex_accounts(arg(
            args,
            "accountIds",
        )?)),
        "import_codex_from_files" => to_value(
            crate::commands::codex::import_codex_from_files(app_handle()?, arg(args, "filePaths")?)
                .await,
        ),
        "codex_oauth_login_start" => {
            to_value(crate::commands::codex::codex_oauth_login_start(app_handle()?).await)
        }
        "codex_oauth_login_completed" => to_value(
            crate::commands::codex::codex_oauth_login_completed(arg(args, "loginId")?).await,
        ),
        "codex_oauth_submit_callback_url" => {
            to_value(crate::commands::codex::codex_oauth_submit_callback_url(
                app_handle()?,
                arg(args, "loginId")?,
                arg(args, "callbackUrl")?,
            ))
        }
        "codex_oauth_login_cancel" => to_value(crate::commands::codex::codex_oauth_login_cancel(
            opt_arg(args, "loginId")?,
        )),
        "add_codex_account_with_token" => to_value(
            crate::commands::codex::add_codex_account_with_token(
                arg(args, "idToken")?,
                arg(args, "accessToken")?,
                opt_arg(args, "refreshToken")?,
            )
            .await,
        ),
        "add_codex_account_with_api_key" => {
            to_value(crate::commands::codex::add_codex_account_with_api_key(
                arg(args, "apiKey")?,
                opt_arg(args, "apiBaseUrl")?,
                opt_arg(args, "apiProviderMode")?,
                opt_arg(args, "apiProviderId")?,
                opt_arg(args, "apiProviderName")?,
            ))
        }
        "update_codex_account_name" => to_value(crate::commands::codex::update_codex_account_name(
            arg(args, "accountId")?,
            arg(args, "name")?,
        )),
        "update_codex_api_key_credentials" => {
            to_value(crate::commands::codex::update_codex_api_key_credentials(
                arg(args, "accountId")?,
                arg(args, "apiKey")?,
                opt_arg(args, "apiBaseUrl")?,
                opt_arg(args, "apiProviderMode")?,
                opt_arg(args, "apiProviderId")?,
                opt_arg(args, "apiProviderName")?,
            ))
        }
        "update_codex_api_key_bound_oauth_account" => to_value(
            crate::commands::codex::update_codex_api_key_bound_oauth_account(
                arg(args, "accountId")?,
                opt_arg(args, "boundOauthAccountId")?,
            )
            .await,
        ),
        "is_codex_oauth_port_in_use" => {
            to_value(crate::commands::codex::is_codex_oauth_port_in_use())
        }
        "close_codex_oauth_port" => to_value(crate::commands::codex::close_codex_oauth_port()),
        "update_codex_account_tags" => to_value(
            crate::commands::codex::update_codex_account_tags(
                arg(args, "accountId")?,
                arg(args, "tags")?,
            )
            .await,
        ),
        "update_codex_account_note" => to_value(
            crate::commands::codex::update_codex_account_note(
                arg(args, "accountId")?,
                arg(args, "note")?,
            )
            .await,
        ),
        "load_codex_model_providers" => {
            to_value(crate::commands::codex::load_codex_model_providers().await)
        }
        "save_codex_model_providers" => {
            to_value(crate::commands::codex::save_codex_model_providers(arg(args, "data")?).await)
        }
        "codex_local_access_save_accounts" => to_value(
            crate::commands::codex::codex_local_access_save_accounts(
                arg(args, "accountIds")?,
                opt_arg(args, "restrictFreeAccounts")?,
            )
            .await,
        ),
        "codex_local_access_remove_account" => to_value(
            crate::commands::codex::codex_local_access_remove_account(arg(args, "accountId")?)
                .await,
        ),
        "codex_local_access_rotate_api_key" => {
            to_value(crate::commands::codex::codex_local_access_rotate_api_key().await)
        }
        "codex_local_access_update_bound_oauth_account" => to_value(
            crate::commands::codex::codex_local_access_update_bound_oauth_account(opt_arg(
                args,
                "boundOauthAccountId",
            )?)
            .await,
        ),
        "codex_local_access_clear_stats" => {
            to_value(crate::commands::codex::codex_local_access_clear_stats().await)
        }
        "codex_local_access_query_request_logs" => to_value(
            crate::commands::codex::codex_local_access_query_request_logs(
                arg(args, "page")?,
                arg(args, "pageSize")?,
                opt_arg(args, "statsRange")?,
                opt_arg(args, "modelQuery")?,
                opt_arg(args, "accountQuery")?,
                opt_arg(args, "apiKeyQuery")?,
                opt_arg(args, "gatewayMode")?,
                opt_arg(args, "requestKind")?,
                opt_arg(args, "success")?,
                opt_arg(args, "errorCategory")?,
            )
            .await,
        ),
        "codex_local_access_prepare_restart" => {
            to_value(crate::commands::codex::codex_local_access_prepare_restart().await)
        }
        "codex_local_access_kill_port" => {
            to_value(crate::commands::codex::codex_local_access_kill_port().await)
        }
        "codex_local_access_update_port" => to_value(
            crate::commands::codex::codex_local_access_update_port(arg(args, "port")?).await,
        ),
        "codex_local_access_update_routing_strategy" => to_value(
            crate::commands::codex::codex_local_access_update_routing_strategy(arg(
                args, "strategy",
            )?)
            .await,
        ),
        "codex_local_access_update_custom_routing" => to_value(
            crate::commands::codex::codex_local_access_update_custom_routing(arg(args, "rules")?)
                .await,
        ),
        "codex_local_access_update_account_model_rules" => to_value(
            crate::commands::codex::codex_local_access_update_account_model_rules(arg(
                args, "rules",
            )?)
            .await,
        ),
        "codex_local_access_update_model_rules" => to_value(
            crate::commands::codex::codex_local_access_update_model_rules(
                arg(args, "modelAliases")?,
                arg(args, "excludedModels")?,
            )
            .await,
        ),
        "codex_local_access_update_model_pricings" => to_value(
            crate::commands::codex::codex_local_access_update_model_pricings(arg(
                args,
                "modelPricings",
            )?)
            .await,
        ),
        "codex_local_access_update_routing_options" => to_value(
            crate::commands::codex::codex_local_access_update_routing_options(
                arg(args, "sessionAffinity")?,
                arg(args, "sessionAffinityTtlMs")?,
                arg(args, "maxRetryCredentials")?,
                arg(args, "maxRetryIntervalMs")?,
                arg(args, "disableCooling")?,
            )
            .await,
        ),
        "codex_local_access_update_timeouts" => to_value(
            crate::commands::codex::codex_local_access_update_timeouts(
                arg(args, "timeouts")?,
                opt_arg(args, "activeTimeoutPresetId")?,
            )
            .await,
        ),
        "codex_local_access_update_timeout_presets" => to_value(
            crate::commands::codex::codex_local_access_update_timeout_presets(
                arg(args, "timeoutPresets")?,
                opt_arg(args, "activeTimeoutPresetId")?,
            )
            .await,
        ),
        "codex_local_access_update_upstream_proxy_config" => to_value(
            crate::commands::codex::codex_local_access_update_upstream_proxy_config(opt_arg(
                args,
                "upstreamProxyUrl",
            )?)
            .await,
        ),
        "codex_local_access_update_gateway_mode" => to_value(
            crate::commands::codex::codex_local_access_update_gateway_mode(arg(
                args,
                "gatewayMode",
            )?)
            .await,
        ),
        "codex_local_access_update_debug_logs" => to_value(
            crate::commands::codex::codex_local_access_update_debug_logs(arg(args, "debugLogs")?)
                .await,
        ),
        "codex_local_access_update_access_scope" => to_value(
            crate::commands::codex::codex_local_access_update_access_scope(arg(
                args,
                "accessScope",
            )?)
            .await,
        ),
        "codex_local_access_update_client_base_url_host" => to_value(
            crate::commands::codex::codex_local_access_update_client_base_url_host(arg(
                args,
                "clientBaseUrlHost",
            )?)
            .await,
        ),
        "codex_local_access_update_image_generation_mode" => to_value(
            crate::commands::codex::codex_local_access_update_image_generation_mode(arg(
                args,
                "imageGenerationMode",
            )?)
            .await,
        ),
        "codex_local_access_create_api_key" => to_value(
            crate::commands::codex::codex_local_access_create_api_key(opt_arg(args, "label")?)
                .await,
        ),
        "codex_local_access_update_api_key" => to_value(
            crate::commands::codex::codex_local_access_update_api_key(
                arg(args, "apiKeyId")?,
                opt_arg(args, "label")?,
                opt_arg(args, "enabled")?,
                opt_arg(args, "modelPrefix")?,
                opt_arg(args, "allowedModels")?,
                opt_arg(args, "excludedModels")?,
            )
            .await,
        ),
        "codex_local_access_rotate_named_api_key" => to_value(
            crate::commands::codex::codex_local_access_rotate_named_api_key(arg(args, "apiKeyId")?)
                .await,
        ),
        "codex_local_access_delete_api_key" => to_value(
            crate::commands::codex::codex_local_access_delete_api_key(arg(args, "apiKeyId")?).await,
        ),
        "codex_local_access_set_enabled" => to_value(
            crate::commands::codex::codex_local_access_set_enabled(arg(args, "enabled")?).await,
        ),
        "codex_local_access_activate" => {
            to_value(crate::commands::codex::codex_local_access_activate(app_handle()?).await)
        }
        "codex_local_access_test" => {
            to_value(crate::commands::codex::codex_local_access_test().await)
        }
        "codex_local_access_chat_test" => to_value(
            crate::commands::codex::codex_local_access_chat_test(
                arg(args, "modelId")?,
                arg(args, "messages")?,
            )
            .await,
        ),
        "codex_local_access_chat_test_stream" => to_value(
            crate::commands::codex::codex_local_access_chat_test_stream(
                app_handle()?,
                arg(args, "sessionId")?,
                arg(args, "modelId")?,
                arg(args, "messages")?,
            )
            .await,
        ),
        // github_copilot
        "delete_github_copilot_account" => to_value(
            crate::commands::github_copilot::delete_github_copilot_account(arg(args, "accountId")?),
        ),
        "delete_github_copilot_accounts" => to_value(
            crate::commands::github_copilot::delete_github_copilot_accounts(arg(
                args,
                "accountIds",
            )?),
        ),
        "import_github_copilot_from_json" => to_value(
            crate::commands::github_copilot::import_github_copilot_from_json(arg(
                args,
                "jsonContent",
            )?),
        ),
        "import_github_copilot_from_local" => to_value(
            crate::commands::github_copilot::import_github_copilot_from_local(app_handle()?).await,
        ),
        "export_github_copilot_accounts" => to_value(
            crate::commands::github_copilot::export_github_copilot_accounts(arg(
                args,
                "accountIds",
            )?),
        ),
        "github_copilot_oauth_login_start" => {
            to_value(crate::commands::github_copilot::github_copilot_oauth_login_start().await)
        }
        "github_copilot_oauth_login_complete" => to_value(
            crate::commands::github_copilot::github_copilot_oauth_login_complete(
                app_handle()?,
                arg(args, "loginId")?,
            )
            .await,
        ),
        "github_copilot_oauth_login_cancel" => to_value(
            crate::commands::github_copilot::github_copilot_oauth_login_cancel(opt_arg(
                args, "loginId",
            )?),
        ),
        "add_github_copilot_account_with_token" => to_value(
            crate::commands::github_copilot::add_github_copilot_account_with_token(
                app_handle()?,
                arg(args, "githubAccessToken")?,
            )
            .await,
        ),
        "get_github_copilot_accounts_index_path" => {
            to_value(crate::commands::github_copilot::get_github_copilot_accounts_index_path())
        }
        // github_copilot_instance
        "github_copilot_create_instance" => to_value(
            crate::commands::github_copilot_instance::github_copilot_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "github_copilot_update_instance" => to_value(
            crate::commands::github_copilot_instance::github_copilot_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "github_copilot_delete_instance" => to_value(
            crate::commands::github_copilot_instance::github_copilot_delete_instance(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "github_copilot_start_instance" => to_value(
            crate::commands::github_copilot_instance::github_copilot_start_instance(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "github_copilot_stop_instance" => to_value(
            crate::commands::github_copilot_instance::github_copilot_stop_instance(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "github_copilot_open_instance_window" => to_value(
            crate::commands::github_copilot_instance::github_copilot_open_instance_window(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "github_copilot_close_all_instances" => to_value(
            crate::commands::github_copilot_instance::github_copilot_close_all_instances().await,
        ),
        // windsurf
        "delete_windsurf_account" => to_value(crate::commands::windsurf::delete_windsurf_account(
            arg(args, "accountId")?,
        )),
        "delete_windsurf_accounts" => to_value(
            crate::commands::windsurf::delete_windsurf_accounts(arg(args, "accountIds")?),
        ),
        "import_windsurf_from_json" => to_value(
            crate::commands::windsurf::import_windsurf_from_json(arg(args, "jsonContent")?),
        ),
        "import_windsurf_from_local" => {
            to_value(crate::commands::windsurf::import_windsurf_from_local(app_handle()?).await)
        }
        "export_windsurf_accounts" => to_value(
            crate::commands::windsurf::export_windsurf_accounts(arg(args, "accountIds")?),
        ),
        "windsurf_oauth_login_start" => {
            to_value(crate::commands::windsurf::windsurf_oauth_login_start().await)
        }
        "windsurf_oauth_login_complete" => to_value(
            crate::commands::windsurf::windsurf_oauth_login_complete(
                app_handle()?,
                arg(args, "loginId")?,
            )
            .await,
        ),
        "windsurf_oauth_submit_callback_url" => to_value(
            crate::commands::windsurf::windsurf_oauth_submit_callback_url(
                arg(args, "loginId")?,
                arg(args, "callbackUrl")?,
            ),
        ),
        "windsurf_oauth_login_cancel" => to_value(
            crate::commands::windsurf::windsurf_oauth_login_cancel(opt_arg(args, "loginId")?),
        ),
        "add_windsurf_account_with_token" => to_value(
            crate::commands::windsurf::add_windsurf_account_with_token(
                app_handle()?,
                arg(args, "githubAccessToken")?,
            )
            .await,
        ),
        "add_windsurf_account_with_password" => to_value(
            crate::commands::windsurf::add_windsurf_account_with_password(
                app_handle()?,
                arg(args, "email")?,
                arg(args, "password")?,
            )
            .await,
        ),
        "add_windsurf_accounts_with_password" => to_value(
            crate::commands::windsurf::add_windsurf_accounts_with_password(
                app_handle()?,
                arg(args, "credentials")?,
            )
            .await,
        ),
        "get_windsurf_accounts_index_path" => {
            to_value(crate::commands::windsurf::get_windsurf_accounts_index_path())
        }
        // kiro
        "delete_kiro_account" => to_value(crate::commands::kiro::delete_kiro_account(arg(
            args,
            "accountId",
        )?)),
        "delete_kiro_accounts" => to_value(crate::commands::kiro::delete_kiro_accounts(arg(
            args,
            "accountIds",
        )?)),
        "import_kiro_from_json" => to_value(crate::commands::kiro::import_kiro_from_json(arg(
            args,
            "jsonContent",
        )?)),
        "import_kiro_from_local" => {
            to_value(crate::commands::kiro::import_kiro_from_local(app_handle()?).await)
        }
        "export_kiro_accounts" => to_value(crate::commands::kiro::export_kiro_accounts(arg(
            args,
            "accountIds",
        )?)),
        "kiro_oauth_login_start" => to_value(crate::commands::kiro::kiro_oauth_login_start().await),
        "kiro_oauth_login_complete" => to_value(
            crate::commands::kiro::kiro_oauth_login_complete(app_handle()?, arg(args, "loginId")?)
                .await,
        ),
        "kiro_oauth_submit_callback_url" => {
            to_value(crate::commands::kiro::kiro_oauth_submit_callback_url(
                arg(args, "loginId")?,
                arg(args, "callbackUrl")?,
            ))
        }
        "kiro_oauth_login_cancel" => to_value(crate::commands::kiro::kiro_oauth_login_cancel(
            opt_arg(args, "loginId")?,
        )),
        "add_kiro_account_with_token" => to_value(
            crate::commands::kiro::add_kiro_account_with_token(
                app_handle()?,
                arg(args, "accessToken")?,
            )
            .await,
        ),
        "get_kiro_accounts_index_path" => {
            to_value(crate::commands::kiro::get_kiro_accounts_index_path())
        }
        // codebuddy
        "delete_codebuddy_account" => to_value(
            crate::commands::codebuddy::delete_codebuddy_account(arg(args, "accountId")?),
        ),
        "delete_codebuddy_accounts" => to_value(
            crate::commands::codebuddy::delete_codebuddy_accounts(arg(args, "accountIds")?),
        ),
        "import_codebuddy_from_json" => to_value(
            crate::commands::codebuddy::import_codebuddy_from_json(arg(args, "jsonContent")?),
        ),
        "import_codebuddy_from_local" => {
            to_value(crate::commands::codebuddy::import_codebuddy_from_local(app_handle()?).await)
        }
        "export_codebuddy_accounts" => to_value(
            crate::commands::codebuddy::export_codebuddy_accounts(arg(args, "accountIds")?),
        ),
        "codebuddy_oauth_login_start" => {
            to_value(crate::commands::codebuddy::codebuddy_oauth_login_start().await)
        }
        "codebuddy_oauth_login_complete" => to_value(
            crate::commands::codebuddy::codebuddy_oauth_login_complete(
                app_handle()?,
                arg(args, "loginId")?,
            )
            .await,
        ),
        "codebuddy_oauth_login_cancel" => to_value(
            crate::commands::codebuddy::codebuddy_oauth_login_cancel(opt_arg(args, "loginId")?),
        ),
        "add_codebuddy_account_with_token" => to_value(
            crate::commands::codebuddy::add_codebuddy_account_with_token(
                app_handle()?,
                arg(args, "accessToken")?,
            )
            .await,
        ),
        "get_codebuddy_accounts_index_path" => {
            to_value(crate::commands::codebuddy::get_codebuddy_accounts_index_path())
        }
        // codebuddy_cn
        "delete_codebuddy_cn_account" => to_value(
            crate::commands::codebuddy_cn::delete_codebuddy_cn_account(arg(args, "accountId")?),
        ),
        "delete_codebuddy_cn_accounts" => to_value(
            crate::commands::codebuddy_cn::delete_codebuddy_cn_accounts(arg(args, "accountIds")?),
        ),
        "import_codebuddy_cn_from_json" => to_value(
            crate::commands::codebuddy_cn::import_codebuddy_cn_from_json(arg(args, "jsonContent")?),
        ),
        "import_codebuddy_cn_from_local" => to_value(
            crate::commands::codebuddy_cn::import_codebuddy_cn_from_local(app_handle()?).await,
        ),
        "export_codebuddy_cn_accounts" => to_value(
            crate::commands::codebuddy_cn::export_codebuddy_cn_accounts(arg(args, "accountIds")?),
        ),
        "codebuddy_cn_oauth_login_start" => {
            to_value(crate::commands::codebuddy_cn::codebuddy_cn_oauth_login_start().await)
        }
        "codebuddy_cn_oauth_login_complete" => to_value(
            crate::commands::codebuddy_cn::codebuddy_cn_oauth_login_complete(
                app_handle()?,
                arg(args, "loginId")?,
            )
            .await,
        ),
        "codebuddy_cn_oauth_login_cancel" => to_value(
            crate::commands::codebuddy_cn::codebuddy_cn_oauth_login_cancel(opt_arg(
                args, "loginId",
            )?),
        ),
        "add_codebuddy_cn_account_with_token" => to_value(
            crate::commands::codebuddy_cn::add_codebuddy_cn_account_with_token(
                app_handle()?,
                arg(args, "accessToken")?,
            )
            .await,
        ),
        "get_codebuddy_cn_accounts_index_path" => {
            to_value(crate::commands::codebuddy_cn::get_codebuddy_cn_accounts_index_path())
        }
        "sync_codebuddy_cn_to_workbuddy" => to_value(
            crate::commands::codebuddy_cn::sync_codebuddy_cn_to_workbuddy(app_handle()?).await,
        ),
        // workbuddy
        "delete_workbuddy_account" => to_value(
            crate::commands::workbuddy::delete_workbuddy_account(arg(args, "accountId")?),
        ),
        "delete_workbuddy_accounts" => to_value(
            crate::commands::workbuddy::delete_workbuddy_accounts(arg(args, "accountIds")?),
        ),
        "import_workbuddy_from_json" => to_value(
            crate::commands::workbuddy::import_workbuddy_from_json(arg(args, "jsonContent")?),
        ),
        "import_workbuddy_from_local" => {
            to_value(crate::commands::workbuddy::import_workbuddy_from_local(app_handle()?).await)
        }
        "export_workbuddy_accounts" => to_value(
            crate::commands::workbuddy::export_workbuddy_accounts(arg(args, "accountIds")?),
        ),
        "workbuddy_oauth_login_start" => {
            to_value(crate::commands::workbuddy::workbuddy_oauth_login_start().await)
        }
        "workbuddy_oauth_login_complete" => to_value(
            crate::commands::workbuddy::workbuddy_oauth_login_complete(
                app_handle()?,
                arg(args, "loginId")?,
            )
            .await,
        ),
        "workbuddy_oauth_login_cancel" => to_value(
            crate::commands::workbuddy::workbuddy_oauth_login_cancel(opt_arg(args, "loginId")?),
        ),
        "add_workbuddy_account_with_token" => to_value(
            crate::commands::workbuddy::add_workbuddy_account_with_token(
                app_handle()?,
                arg(args, "accessToken")?,
            )
            .await,
        ),
        "get_workbuddy_accounts_index_path" => {
            to_value(crate::commands::workbuddy::get_workbuddy_accounts_index_path())
        }
        "sync_workbuddy_to_codebuddy_cn" => to_value(
            crate::commands::workbuddy::sync_workbuddy_to_codebuddy_cn(app_handle()?).await,
        ),
        "get_checkin_status_workbuddy" => to_value(
            crate::commands::workbuddy::get_checkin_status_workbuddy(arg(args, "accountId")?).await,
        ),
        "checkin_workbuddy" => to_value(
            crate::commands::workbuddy::checkin_workbuddy(app_handle()?, arg(args, "accountId")?)
                .await,
        ),
        // workbuddy_instance
        "workbuddy_create_instance" => to_value(
            crate::commands::workbuddy_instance::workbuddy_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "workbuddy_update_instance" => to_value(
            crate::commands::workbuddy_instance::workbuddy_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "workbuddy_delete_instance" => to_value(
            crate::commands::workbuddy_instance::workbuddy_delete_instance(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "workbuddy_start_instance" => to_value(
            crate::commands::workbuddy_instance::workbuddy_start_instance(arg(args, "instanceId")?)
                .await,
        ),
        "workbuddy_stop_instance" => to_value(
            crate::commands::workbuddy_instance::workbuddy_stop_instance(arg(args, "instanceId")?)
                .await,
        ),
        "workbuddy_open_instance_window" => to_value(
            crate::commands::workbuddy_instance::workbuddy_open_instance_window(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "workbuddy_close_all_instances" => {
            to_value(crate::commands::workbuddy_instance::workbuddy_close_all_instances().await)
        }
        // codebuddy_instance
        "codebuddy_create_instance" => to_value(
            crate::commands::codebuddy_instance::codebuddy_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "codebuddy_update_instance" => to_value(
            crate::commands::codebuddy_instance::codebuddy_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "codebuddy_delete_instance" => to_value(
            crate::commands::codebuddy_instance::codebuddy_delete_instance(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "codebuddy_start_instance" => to_value(
            crate::commands::codebuddy_instance::codebuddy_start_instance(arg(args, "instanceId")?)
                .await,
        ),
        "codebuddy_stop_instance" => to_value(
            crate::commands::codebuddy_instance::codebuddy_stop_instance(arg(args, "instanceId")?)
                .await,
        ),
        "codebuddy_open_instance_window" => to_value(
            crate::commands::codebuddy_instance::codebuddy_open_instance_window(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "codebuddy_close_all_instances" => {
            to_value(crate::commands::codebuddy_instance::codebuddy_close_all_instances().await)
        }
        // codebuddy_cn_instance
        "codebuddy_cn_create_instance" => to_value(
            crate::commands::codebuddy_cn_instance::codebuddy_cn_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "codebuddy_cn_update_instance" => to_value(
            crate::commands::codebuddy_cn_instance::codebuddy_cn_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "codebuddy_cn_delete_instance" => to_value(
            crate::commands::codebuddy_cn_instance::codebuddy_cn_delete_instance(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "codebuddy_cn_start_instance" => to_value(
            crate::commands::codebuddy_cn_instance::codebuddy_cn_start_instance(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "codebuddy_cn_stop_instance" => to_value(
            crate::commands::codebuddy_cn_instance::codebuddy_cn_stop_instance(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "codebuddy_cn_open_instance_window" => to_value(
            crate::commands::codebuddy_cn_instance::codebuddy_cn_open_instance_window(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "codebuddy_cn_close_all_instances" => to_value(
            crate::commands::codebuddy_cn_instance::codebuddy_cn_close_all_instances().await,
        ),
        // qoder
        "delete_qoder_account" => to_value(crate::commands::qoder::delete_qoder_account(arg(
            args,
            "accountId",
        )?)),
        "delete_qoder_accounts" => to_value(crate::commands::qoder::delete_qoder_accounts(arg(
            args,
            "accountIds",
        )?)),
        "import_qoder_from_json" => to_value(crate::commands::qoder::import_qoder_from_json(arg(
            args,
            "jsonContent",
        )?)),
        "import_qoder_from_local" => to_value(crate::commands::qoder::import_qoder_from_local(
            app_handle()?,
        )),
        "qoder_oauth_login_start" => {
            to_value(crate::commands::qoder::qoder_oauth_login_start().await)
        }
        "qoder_oauth_login_peek" => {
            serialize_value(crate::commands::qoder::qoder_oauth_login_peek())
        }
        "qoder_oauth_login_complete" => to_value(
            crate::commands::qoder::qoder_oauth_login_complete(
                app_handle()?,
                arg(args, "loginId")?,
            )
            .await,
        ),
        "qoder_oauth_login_cancel" => to_value(crate::commands::qoder::qoder_oauth_login_cancel(
            opt_arg(args, "loginId")?,
        )),
        "export_qoder_accounts" => to_value(crate::commands::qoder::export_qoder_accounts(arg(
            args,
            "accountIds",
        )?)),
        "get_qoder_accounts_index_path" => {
            to_value(crate::commands::qoder::get_qoder_accounts_index_path())
        }
        // zed
        "delete_zed_account" => to_value(crate::commands::zed::delete_zed_account(
            app_handle()?,
            arg(args, "accountId")?,
        )),
        "delete_zed_accounts" => to_value(crate::commands::zed::delete_zed_accounts(
            app_handle()?,
            arg(args, "accountIds")?,
        )),
        "import_zed_from_json" => to_value(crate::commands::zed::import_zed_from_json(
            app_handle()?,
            arg(args, "jsonContent")?,
        )),
        "import_zed_from_local" => {
            to_value(crate::commands::zed::import_zed_from_local(app_handle()?).await)
        }
        "export_zed_accounts" => to_value(crate::commands::zed::export_zed_accounts(arg(
            args,
            "accountIds",
        )?)),
        "zed_oauth_login_start" => to_value(crate::commands::zed::zed_oauth_login_start().await),
        "zed_oauth_login_peek" => serialize_value(crate::commands::zed::zed_oauth_login_peek()),
        "zed_oauth_login_complete" => to_value(
            crate::commands::zed::zed_oauth_login_complete(app_handle()?, arg(args, "loginId")?)
                .await,
        ),
        "zed_oauth_login_cancel" => to_value(crate::commands::zed::zed_oauth_login_cancel(
            opt_arg(args, "loginId")?,
        )),
        "zed_oauth_submit_callback_url" => {
            to_value(crate::commands::zed::zed_oauth_submit_callback_url(
                arg(args, "loginId")?,
                arg(args, "callbackUrl")?,
            ))
        }
        "zed_logout_current_account" => {
            to_value(crate::commands::zed::zed_logout_current_account(app_handle()?).await)
        }
        "zed_get_runtime_status" => to_value(crate::commands::zed::zed_get_runtime_status()),
        "zed_start_default_session" => to_value(crate::commands::zed::zed_start_default_session()),
        "zed_stop_default_session" => to_value(crate::commands::zed::zed_stop_default_session()),
        "zed_restart_default_session" => {
            to_value(crate::commands::zed::zed_restart_default_session())
        }
        "zed_focus_default_session" => to_value(crate::commands::zed::zed_focus_default_session()),
        // qoder_instance
        "qoder_create_instance" => to_value(
            crate::commands::qoder_instance::qoder_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "qoder_update_instance" => to_value(
            crate::commands::qoder_instance::qoder_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "qoder_delete_instance" => to_value(
            crate::commands::qoder_instance::qoder_delete_instance(arg(args, "instanceId")?).await,
        ),
        "qoder_start_instance" => to_value(
            crate::commands::qoder_instance::qoder_start_instance(arg(args, "instanceId")?).await,
        ),
        "qoder_stop_instance" => to_value(
            crate::commands::qoder_instance::qoder_stop_instance(arg(args, "instanceId")?).await,
        ),
        "qoder_open_instance_window" => to_value(
            crate::commands::qoder_instance::qoder_open_instance_window(arg(args, "instanceId")?)
                .await,
        ),
        "qoder_close_all_instances" => {
            to_value(crate::commands::qoder_instance::qoder_close_all_instances().await)
        }
        // trae
        "delete_trae_account" => to_value(crate::commands::trae::delete_trae_account(arg(
            args,
            "accountId",
        )?)),
        "delete_trae_accounts" => to_value(crate::commands::trae::delete_trae_accounts(arg(
            args,
            "accountIds",
        )?)),
        "import_trae_from_json" => to_value(crate::commands::trae::import_trae_from_json(arg(
            args,
            "jsonContent",
        )?)),
        "import_trae_from_local" => {
            to_value(crate::commands::trae::import_trae_from_local(app_handle()?).await)
        }
        "trae_oauth_login_start" => to_value(crate::commands::trae::trae_oauth_login_start().await),
        "trae_oauth_login_complete" => to_value(
            crate::commands::trae::trae_oauth_login_complete(app_handle()?, arg(args, "loginId")?)
                .await,
        ),
        "trae_oauth_submit_callback_url" => {
            to_value(crate::commands::trae::trae_oauth_submit_callback_url(
                arg(args, "loginId")?,
                arg(args, "callbackUrl")?,
            ))
        }
        "trae_oauth_login_cancel" => to_value(crate::commands::trae::trae_oauth_login_cancel(
            opt_arg(args, "loginId")?,
        )),
        "export_trae_accounts" => to_value(crate::commands::trae::export_trae_accounts(arg(
            args,
            "accountIds",
        )?)),
        "add_trae_account_with_token" => {
            to_value(crate::commands::trae::add_trae_account_with_token(
                app_handle()?,
                arg(args, "accessToken")?,
            ))
        }
        "get_trae_accounts_index_path" => {
            to_value(crate::commands::trae::get_trae_accounts_index_path())
        }
        // trae_instance
        "trae_create_instance" => to_value(
            crate::commands::trae_instance::trae_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "trae_update_instance" => to_value(
            crate::commands::trae_instance::trae_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "trae_delete_instance" => to_value(
            crate::commands::trae_instance::trae_delete_instance(arg(args, "instanceId")?).await,
        ),
        "trae_start_instance" => to_value(
            crate::commands::trae_instance::trae_start_instance(arg(args, "instanceId")?).await,
        ),
        "trae_stop_instance" => to_value(
            crate::commands::trae_instance::trae_stop_instance(arg(args, "instanceId")?).await,
        ),
        "trae_open_instance_window" => to_value(
            crate::commands::trae_instance::trae_open_instance_window(arg(args, "instanceId")?)
                .await,
        ),
        "trae_close_all_instances" => {
            to_value(crate::commands::trae_instance::trae_close_all_instances().await)
        }
        // cursor
        "delete_cursor_account" => to_value(crate::commands::cursor::delete_cursor_account(arg(
            args,
            "accountId",
        )?)),
        "delete_cursor_accounts" => to_value(crate::commands::cursor::delete_cursor_accounts(arg(
            args,
            "accountIds",
        )?)),
        "import_cursor_from_json" => to_value(crate::commands::cursor::import_cursor_from_json(
            arg(args, "jsonContent")?,
        )),
        "import_cursor_from_local" => to_value(crate::commands::cursor::import_cursor_from_local(
            app_handle()?,
        )),
        "export_cursor_accounts" => to_value(crate::commands::cursor::export_cursor_accounts(arg(
            args,
            "accountIds",
        )?)),
        "add_cursor_account_with_token" => {
            to_value(crate::commands::cursor::add_cursor_account_with_token(
                app_handle()?,
                arg(args, "accessToken")?,
            ))
        }
        "get_cursor_accounts_index_path" => {
            to_value(crate::commands::cursor::get_cursor_accounts_index_path())
        }
        "cursor_oauth_login_start" => to_value(crate::commands::cursor::cursor_oauth_login_start()),
        "cursor_oauth_login_complete" => to_value(
            crate::commands::cursor::cursor_oauth_login_complete(
                app_handle()?,
                arg(args, "loginId")?,
            )
            .await,
        ),
        "cursor_oauth_login_cancel" => to_value(
            crate::commands::cursor::cursor_oauth_login_cancel(opt_arg(args, "loginId")?),
        ),
        // gemini
        "delete_gemini_account" => to_value(crate::commands::gemini::delete_gemini_account(arg(
            args,
            "accountId",
        )?)),
        "delete_gemini_accounts" => to_value(crate::commands::gemini::delete_gemini_accounts(arg(
            args,
            "accountIds",
        )?)),
        "import_gemini_from_json" => to_value(
            crate::commands::gemini::import_gemini_from_json(
                app_handle()?,
                arg(args, "jsonContent")?,
            )
            .await,
        ),
        "import_gemini_from_local" => {
            to_value(crate::commands::gemini::import_gemini_from_local(app_handle()?).await)
        }
        "export_gemini_accounts" => to_value(crate::commands::gemini::export_gemini_accounts(arg(
            args,
            "accountIds",
        )?)),
        "gemini_oauth_login_start" => {
            to_value(crate::commands::gemini::gemini_oauth_login_start().await)
        }
        "gemini_oauth_login_complete" => to_value(
            crate::commands::gemini::gemini_oauth_login_complete(
                app_handle()?,
                arg(args, "loginId")?,
            )
            .await,
        ),
        "gemini_oauth_submit_callback_url" => {
            to_value(crate::commands::gemini::gemini_oauth_submit_callback_url(
                arg(args, "loginId")?,
                arg(args, "callbackUrl")?,
            ))
        }
        "gemini_oauth_login_cancel" => to_value(
            crate::commands::gemini::gemini_oauth_login_cancel(opt_arg(args, "loginId")?),
        ),
        "add_gemini_account_with_token" => to_value(
            crate::commands::gemini::add_gemini_account_with_token(
                app_handle()?,
                arg(args, "accessToken")?,
            )
            .await,
        ),
        "get_gemini_accounts_index_path" => {
            to_value(crate::commands::gemini::get_gemini_accounts_index_path())
        }
        // gemini_instance
        "gemini_create_instance" => to_value(
            crate::commands::gemini_instance::gemini_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "workingDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "gemini_update_instance" => to_value(
            crate::commands::gemini_instance::gemini_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "workingDir")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "gemini_delete_instance" => to_value(
            crate::commands::gemini_instance::gemini_delete_instance(arg(args, "instanceId")?)
                .await,
        ),
        "gemini_start_instance" => to_value(
            crate::commands::gemini_instance::gemini_start_instance(arg(args, "instanceId")?).await,
        ),
        "gemini_stop_instance" => to_value(
            crate::commands::gemini_instance::gemini_stop_instance(arg(args, "instanceId")?).await,
        ),
        "gemini_open_instance_window" => to_value(
            crate::commands::gemini_instance::gemini_open_instance_window(arg(args, "instanceId")?)
                .await,
        ),
        "gemini_close_all_instances" => {
            to_value(crate::commands::gemini_instance::gemini_close_all_instances().await)
        }
        "gemini_get_instance_launch_command" => to_value(
            crate::commands::gemini_instance::gemini_get_instance_launch_command(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "gemini_execute_instance_launch_command" => to_value(
            crate::commands::gemini_instance::gemini_execute_instance_launch_command(
                arg(args, "instanceId")?,
                opt_arg(args, "terminal")?,
            )
            .await,
        ),
        // cursor_instance
        "cursor_create_instance" => to_value(
            crate::commands::cursor_instance::cursor_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "cursor_update_instance" => to_value(
            crate::commands::cursor_instance::cursor_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "cursor_delete_instance" => to_value(
            crate::commands::cursor_instance::cursor_delete_instance(arg(args, "instanceId")?)
                .await,
        ),
        "cursor_start_instance" => to_value(
            crate::commands::cursor_instance::cursor_start_instance(arg(args, "instanceId")?).await,
        ),
        "cursor_stop_instance" => to_value(
            crate::commands::cursor_instance::cursor_stop_instance(arg(args, "instanceId")?).await,
        ),
        "cursor_open_instance_window" => to_value(
            crate::commands::cursor_instance::cursor_open_instance_window(arg(args, "instanceId")?)
                .await,
        ),
        "cursor_close_all_instances" => {
            to_value(crate::commands::cursor_instance::cursor_close_all_instances().await)
        }
        // windsurf_instance
        "windsurf_create_instance" => to_value(
            crate::commands::windsurf_instance::windsurf_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "windsurf_update_instance" => to_value(
            crate::commands::windsurf_instance::windsurf_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "windsurf_delete_instance" => to_value(
            crate::commands::windsurf_instance::windsurf_delete_instance(arg(args, "instanceId")?)
                .await,
        ),
        "windsurf_start_instance" => to_value(
            crate::commands::windsurf_instance::windsurf_start_instance(arg(args, "instanceId")?)
                .await,
        ),
        "windsurf_stop_instance" => to_value(
            crate::commands::windsurf_instance::windsurf_stop_instance(arg(args, "instanceId")?)
                .await,
        ),
        "windsurf_open_instance_window" => to_value(
            crate::commands::windsurf_instance::windsurf_open_instance_window(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "windsurf_close_all_instances" => {
            to_value(crate::commands::windsurf_instance::windsurf_close_all_instances().await)
        }
        // kiro_instance
        "kiro_create_instance" => to_value(
            crate::commands::kiro_instance::kiro_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "kiro_update_instance" => to_value(
            crate::commands::kiro_instance::kiro_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "kiro_delete_instance" => to_value(
            crate::commands::kiro_instance::kiro_delete_instance(arg(args, "instanceId")?).await,
        ),
        "kiro_start_instance" => to_value(
            crate::commands::kiro_instance::kiro_start_instance(arg(args, "instanceId")?).await,
        ),
        "kiro_stop_instance" => to_value(
            crate::commands::kiro_instance::kiro_stop_instance(arg(args, "instanceId")?).await,
        ),
        "kiro_open_instance_window" => to_value(
            crate::commands::kiro_instance::kiro_open_instance_window(arg(args, "instanceId")?)
                .await,
        ),
        "kiro_close_all_instances" => {
            to_value(crate::commands::kiro_instance::kiro_close_all_instances().await)
        }
        // codex_instance
        "codex_get_instance_quick_config" => to_value(
            crate::commands::codex_instance::codex_get_instance_quick_config(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "codex_save_instance_quick_config" => to_value(
            crate::commands::codex_instance::codex_save_instance_quick_config(
                arg(args, "instanceId")?,
                opt_arg(args, "modelContextWindow")?,
                opt_arg(args, "autoCompactTokenLimit")?,
            )
            .await,
        ),
        "codex_open_instance_config_toml" => to_value(
            crate::commands::codex_instance::codex_open_instance_config_toml(
                app_handle()?,
                arg(args, "instanceId")?,
            )
            .await,
        ),
        "codex_sync_threads_across_instances" => {
            to_value(crate::commands::codex_instance::codex_sync_threads_across_instances().await)
        }
        "codex_sync_sessions_to_instance" => to_value(
            crate::commands::codex_instance::codex_sync_sessions_to_instance(
                arg(args, "sessionIds")?,
                arg(args, "targetInstanceId")?,
            )
            .await,
        ),
        "codex_repair_session_visibility_across_instances" => to_value(
            crate::commands::codex_instance::codex_repair_session_visibility_across_instances()
                .await,
        ),
        "codex_list_sessions_across_instances" => {
            to_value(crate::commands::codex_instance::codex_list_sessions_across_instances().await)
        }
        "codex_get_session_token_stats_across_instances" => to_value(
            crate::commands::codex_instance::codex_get_session_token_stats_across_instances(arg(
                args,
                "sessionIds",
            )?)
            .await,
        ),
        "codex_move_sessions_to_trash_across_instances" => to_value(
            crate::commands::codex_instance::codex_move_sessions_to_trash_across_instances(arg(
                args,
                "sessionIds",
            )?)
            .await,
        ),
        "codex_list_trashed_sessions_across_instances" => to_value(
            crate::commands::codex_instance::codex_list_trashed_sessions_across_instances().await,
        ),
        "codex_restore_sessions_from_trash_across_instances" => to_value(
            crate::commands::codex_instance::codex_restore_sessions_from_trash_across_instances(
                arg(args, "sessionIds")?,
            )
            .await,
        ),
        "codex_create_instance" => to_value(
            crate::commands::codex_instance::codex_create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "workingDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
                opt_arg(args, "launchMode")?,
                opt_arg(args, "appSpeed")?,
            )
            .await,
        ),
        "codex_update_instance" => to_value(
            crate::commands::codex_instance::codex_update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "workingDir")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
                opt_arg(args, "launchMode")?,
                opt_arg(args, "appSpeed")?,
                opt_arg(args, "autoSyncThreads")?,
            )
            .await,
        ),
        "codex_delete_instance" => to_value(
            crate::commands::codex_instance::codex_delete_instance(arg(args, "instanceId")?).await,
        ),
        "codex_start_instance" => to_value(
            crate::commands::codex_instance::codex_start_instance(arg(args, "instanceId")?).await,
        ),
        "codex_stop_instance" => to_value(
            crate::commands::codex_instance::codex_stop_instance(arg(args, "instanceId")?).await,
        ),
        "codex_open_instance_window" => to_value(
            crate::commands::codex_instance::codex_open_instance_window(arg(args, "instanceId")?)
                .await,
        ),
        "codex_close_all_instances" => {
            to_value(crate::commands::codex_instance::codex_close_all_instances().await)
        }
        "codex_get_instance_launch_command" => to_value(
            crate::commands::codex_instance::codex_get_instance_launch_command(arg(
                args,
                "instanceId",
            )?)
            .await,
        ),
        "codex_execute_instance_launch_command" => to_value(
            crate::commands::codex_instance::codex_execute_instance_launch_command(
                arg(args, "instanceId")?,
                opt_arg(args, "terminal")?,
            )
            .await,
        ),
        // instance
        "get_instance_defaults" => {
            to_value(crate::commands::instance::get_instance_defaults().await)
        }
        "list_instances" => to_value(crate::commands::instance::list_instances().await),
        "create_instance" => to_value(
            crate::commands::instance::create_instance(
                arg(args, "name")?,
                arg(args, "userDataDir")?,
                opt_arg(args, "extraArgs")?,
                opt_arg(args, "bindAccountId")?,
                opt_arg(args, "copySourceInstanceId")?,
                opt_arg(args, "initMode")?,
            )
            .await,
        ),
        "update_instance" => to_value(
            crate::commands::instance::update_instance(
                arg(args, "instanceId")?,
                opt_arg(args, "name")?,
                opt_arg(args, "extraArgs")?,
                opt_nullable_arg(args, "bindAccountId")?,
                opt_arg(args, "followLocalAccount")?,
            )
            .await,
        ),
        "delete_instance" => {
            to_value(crate::commands::instance::delete_instance(arg(args, "instanceId")?).await)
        }
        "start_instance" => {
            to_value(crate::commands::instance::start_instance(arg(args, "instanceId")?).await)
        }
        "stop_instance" => {
            to_value(crate::commands::instance::stop_instance(arg(args, "instanceId")?).await)
        }
        "open_instance_window" => to_value(
            crate::commands::instance::open_instance_window(arg(args, "instanceId")?).await,
        ),
        "close_all_instances" => to_value(crate::commands::instance::close_all_instances().await),
        other => Err(format!(
            "Command '{}' is not exposed through the local web console yet",
            other
        )),
    }
}

fn web_event_state() -> &'static RwLock<WebEventState> {
    WEB_EVENT_STATE.get_or_init(|| RwLock::new(WebEventState::default()))
}

fn register_web_event_listener(event_name: String, client_id: String) -> Result<u32, String> {
    let mut state = web_event_state()
        .write()
        .map_err(|_| "web event state is poisoned".to_string())?;

    state.next_listener_id = state.next_listener_id.saturating_add(1).max(1);
    let listener_id = state.next_listener_id;
    let registered_after_sequence = state.next_sequence;
    state.browser_listeners.insert(
        listener_id,
        WebBrowserEventListener {
            event: event_name.clone(),
            client_id,
            registered_after_sequence,
        },
    );

    if !state.tauri_listeners.contains_key(&event_name) {
        let app = app_handle()?;
        let captured_event = event_name.clone();
        let tauri_listener_id = app.listen_any(event_name.clone(), move |event| {
            push_web_event(&captured_event, event.payload());
        });
        state.tauri_listeners.insert(event_name, tauri_listener_id);
    }

    Ok(listener_id)
}

fn unregister_web_event_listener(listener_id: u32, client_id: Option<&str>) -> Result<(), String> {
    let tauri_listener_to_remove = {
        let mut state = web_event_state()
            .write()
            .map_err(|_| "web event state is poisoned".to_string())?;
        let Some(listener) = state.browser_listeners.get(&listener_id) else {
            return Ok(());
        };
        if client_id.is_some_and(|expected| expected != listener.client_id) {
            return Ok(());
        }
        let Some(listener) = state.browser_listeners.remove(&listener_id) else {
            return Ok(());
        };
        let event_name = listener.event;

        let still_used = state
            .browser_listeners
            .values()
            .any(|registered_event| registered_event.event == event_name);
        if still_used {
            None
        } else {
            state.tauri_listeners.remove(&event_name)
        }
    };

    if let Some(tauri_listener_id) = tauri_listener_to_remove {
        if let Ok(app) = app_handle() {
            app.unlisten(tauri_listener_id);
        }
    }

    Ok(())
}

fn emit_web_event_from_browser(event_name: String, payload: Option<Value>) -> Result<(), String> {
    let payload = payload.unwrap_or(Value::Null);
    app_handle()?
        .emit(event_name.as_str(), payload)
        .map_err(|err| err.to_string())
}

fn push_web_event(event_name: &str, raw_payload: &str) {
    let payload = serde_json::from_str(raw_payload)
        .unwrap_or_else(|_| Value::String(raw_payload.to_string()));
    push_web_event_value(event_name, payload);
}

fn push_web_event_value(event_name: &str, payload: Value) {
    let Ok(mut state) = web_event_state().write() else {
        return;
    };
    state.next_sequence = state.next_sequence.saturating_add(1);
    let sequence = state.next_sequence;
    state.queue.push_back(WebEventMessage {
        sequence,
        event: event_name.to_string(),
        payload,
    });
    while state.queue.len() > MAX_EVENT_QUEUE_LEN {
        state.queue.pop_front();
    }
}

async fn wait_for_web_events(client_id: Option<&str>, after: u64) -> Vec<WebEventMessage> {
    let started = std::time::Instant::now();
    loop {
        let events = collect_web_events(client_id, after);
        if !events.is_empty() || started.elapsed() >= EVENT_POLL_TIMEOUT {
            return events;
        }
        sleep(EVENT_POLL_INTERVAL).await;
    }
}

fn collect_web_events(client_id: Option<&str>, after: u64) -> Vec<WebEventMessage> {
    web_event_state()
        .read()
        .map(|state| {
            let active_listeners = state
                .browser_listeners
                .values()
                .filter(|listener| client_id.map_or(true, |id| listener.client_id == id))
                .cloned()
                .collect::<Vec<_>>();

            state
                .queue
                .iter()
                .filter(|event| {
                    event.sequence > after
                        && active_listeners.iter().any(|listener| {
                            listener.event == event.event
                                && event.sequence > listener.registered_after_sequence
                        })
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn latest_web_event_sequence() -> u64 {
    web_event_state()
        .read()
        .map(|state| state.next_sequence)
        .unwrap_or_default()
}

fn to_value<T: Serialize>(result: Result<T, String>) -> Result<Value, String> {
    serde_json::to_value(result?).map_err(|err| format!("serialize response failed: {}", err))
}

fn serialize_value<T: Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| format!("serialize response failed: {}", err))
}

fn dispatch_save_general_config(args: &Value) -> Result<Value, String> {
    to_value(crate::commands::system::save_general_config(
        app_handle()?,
        arg(args, "language")?,
        opt_arg(args, "defaultTerminal")?,
        arg(args, "theme")?,
        opt_arg(args, "uiScale")?,
        arg(args, "autoRefreshMinutes")?,
        arg(args, "codexAutoRefreshMinutes")?,
        opt_arg(args, "zedAutoRefreshMinutes")?,
        opt_arg(args, "ghcpAutoRefreshMinutes")?,
        opt_arg(args, "windsurfAutoRefreshMinutes")?,
        opt_arg(args, "kiroAutoRefreshMinutes")?,
        opt_arg(args, "cursorAutoRefreshMinutes")?,
        opt_arg(args, "geminiAutoRefreshMinutes")?,
        opt_arg(args, "geminiSyncWsl")?,
        opt_arg(args, "codebuddyAutoRefreshMinutes")?,
        opt_arg(args, "codebuddyCnAutoRefreshMinutes")?,
        opt_arg(args, "workbuddyAutoRefreshMinutes")?,
        opt_arg(args, "qoderAutoRefreshMinutes")?,
        opt_arg(args, "traeAutoRefreshMinutes")?,
        arg(args, "closeBehavior")?,
        opt_arg(args, "minimizeBehavior")?,
        opt_arg(args, "hideDockIcon")?,
        opt_arg(args, "trayIconStyle")?,
        opt_arg(args, "floatingCardShowOnStartup")?,
        opt_arg(args, "floatingCardAlwaysOnTop")?,
        opt_arg(args, "appAutoLaunchEnabled")?,
        opt_arg(args, "antigravityStartupWakeupEnabled")?,
        opt_arg(args, "antigravityStartupWakeupDelaySeconds")?,
        opt_arg(args, "codexStartupWakeupEnabled")?,
        opt_arg(args, "codexStartupWakeupDelaySeconds")?,
        opt_arg(args, "floatingCardConfirmOnClose")?,
        arg(args, "opencodeAppPath")?,
        arg(args, "antigravityAppPath")?,
        arg(args, "codexAppPath")?,
        opt_arg(args, "codexSpecifiedAppPath")?,
        opt_arg(args, "zedAppPath")?,
        arg(args, "vscodeAppPath")?,
        opt_arg(args, "windsurfAppPath")?,
        opt_arg(args, "kiroAppPath")?,
        opt_arg(args, "cursorAppPath")?,
        opt_arg(args, "codebuddyAppPath")?,
        opt_arg(args, "codebuddyCnAppPath")?,
        opt_arg(args, "qoderAppPath")?,
        opt_arg(args, "traeAppPath")?,
        opt_arg(args, "workbuddyAppPath")?,
        arg(args, "opencodeSyncOnSwitch")?,
        opt_arg(args, "opencodeAuthOverwriteOnSwitch")?,
        opt_arg(args, "ghcpOpencodeSyncOnSwitch")?,
        opt_arg(args, "ghcpOpencodeAuthOverwriteOnSwitch")?,
        opt_arg(args, "ghcpLaunchOnSwitch")?,
        opt_arg(args, "openclawAuthOverwriteOnSwitch")?,
        arg(args, "codexLaunchOnSwitch")?,
        opt_arg(args, "codexRestartSpecifiedAppOnSwitch")?,
        opt_arg(args, "codexLocalAccessEntryVisible")?,
        opt_arg(args, "antigravityDualSwitchNoRestartEnabled")?,
        opt_arg(args, "autoSwitchEnabled")?,
        opt_arg(args, "autoSwitchThreshold")?,
        opt_arg(args, "autoSwitchCreditsEnabled")?,
        opt_arg(args, "autoSwitchCreditsThreshold")?,
        opt_arg(args, "autoSwitchScopeMode")?,
        opt_arg(args, "autoSwitchSelectedGroupIds")?,
        opt_arg(args, "autoSwitchAccountScopeMode")?,
        opt_arg(args, "autoSwitchSelectedAccountIds")?,
        opt_arg(args, "codexAutoSwitchEnabled")?,
        opt_arg(args, "codexAutoSwitchPrimaryThreshold")?,
        opt_arg(args, "codexAutoSwitchSecondaryThreshold")?,
        opt_arg(args, "codexAutoSwitchAccountScopeMode")?,
        opt_arg(args, "codexAutoSwitchSelectedAccountIds")?,
        opt_arg(args, "quotaAlertEnabled")?,
        opt_arg(args, "quotaAlertThreshold")?,
        opt_arg(args, "codexQuotaAlertEnabled")?,
        opt_arg(args, "codexQuotaAlertThreshold")?,
        opt_arg(args, "zedQuotaAlertEnabled")?,
        opt_arg(args, "zedQuotaAlertThreshold")?,
        opt_arg(args, "codexQuotaAlertPrimaryThreshold")?,
        opt_arg(args, "codexQuotaAlertSecondaryThreshold")?,
        opt_arg(args, "ghcpQuotaAlertEnabled")?,
        opt_arg(args, "ghcpQuotaAlertThreshold")?,
        opt_arg(args, "windsurfQuotaAlertEnabled")?,
        opt_arg(args, "windsurfQuotaAlertThreshold")?,
        opt_arg(args, "kiroQuotaAlertEnabled")?,
        opt_arg(args, "kiroQuotaAlertThreshold")?,
        opt_arg(args, "cursorQuotaAlertEnabled")?,
        opt_arg(args, "cursorQuotaAlertThreshold")?,
        opt_arg(args, "geminiQuotaAlertEnabled")?,
        opt_arg(args, "geminiQuotaAlertThreshold")?,
        opt_arg(args, "codebuddyQuotaAlertEnabled")?,
        opt_arg(args, "codebuddyQuotaAlertThreshold")?,
        opt_arg(args, "codebuddyCnQuotaAlertEnabled")?,
        opt_arg(args, "codebuddyCnQuotaAlertThreshold")?,
        opt_arg(args, "qoderQuotaAlertEnabled")?,
        opt_arg(args, "qoderQuotaAlertThreshold")?,
        opt_arg(args, "traeQuotaAlertEnabled")?,
        opt_arg(args, "traeQuotaAlertThreshold")?,
        opt_arg(args, "workbuddyQuotaAlertEnabled")?,
        opt_arg(args, "workbuddyQuotaAlertThreshold")?,
    ))
}

fn app_handle() -> Result<tauri::AppHandle, String> {
    crate::get_app_handle()
        .cloned()
        .ok_or_else(|| "App runtime is not available".to_string())
}

fn arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    let value = args
        .get(key)
        .cloned()
        .ok_or_else(|| format!("missing argument '{}'", key))?;
    serde_json::from_value(value).map_err(|err| format!("invalid argument '{}': {}", key, err))
}

fn optional_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn arg_or<T: DeserializeOwned>(args: &Value, key: &str, default: T) -> Result<T, String> {
    match args.get(key) {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|err| format!("invalid argument '{}': {}", key, err)),
        None => Ok(default),
    }
}

fn opt_arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<Option<T>, String> {
    match args.get(key) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|err| format!("invalid argument '{}': {}", key, err)),
    }
}

fn opt_nullable_arg<T: DeserializeOwned>(
    args: &Value,
    key: &str,
) -> Result<Option<Option<T>>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => serde_json::from_value(value.clone())
            .map(|value| Some(Some(value)))
            .map_err(|err| format!("invalid argument '{}': {}", key, err)),
    }
}

async fn read_http_request(stream: &mut TcpStream) -> Result<Option<HttpRequest>, String> {
    let mut buffer = Vec::new();
    let mut temp = [0u8; 4096];
    let header_end = loop {
        let read = timeout(REQUEST_READ_TIMEOUT, stream.read(&mut temp))
            .await
            .map_err(|_| "request read timed out".to_string())?
            .map_err(|err| err.to_string())?;
        if read == 0 {
            if buffer.is_empty() {
                return Ok(None);
            }
            return Err("connection closed before headers completed".to_string());
        }
        buffer.extend_from_slice(&temp[..read]);
        if buffer.len() > MAX_HTTP_REQUEST_BYTES {
            return Err("request too large".to_string());
        }
        if let Some(pos) = find_header_end(&buffer) {
            break pos;
        }
    };

    let header_text =
        String::from_utf8(buffer[..header_end].to_vec()).map_err(|err| err.to_string())?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "missing request line".to_string())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or("").to_string();
    let raw_path = request_parts.next().unwrap_or("/");
    let (path, query) = normalize_request_path(raw_path)?;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value
                    .parse::<usize>()
                    .map_err(|_| "invalid content-length".to_string())?;
            }
        }
    }

    if content_length > MAX_HTTP_REQUEST_BYTES {
        return Err("request body too large".to_string());
    }

    let body_start = header_end + 4;
    let mut body = buffer.get(body_start..).unwrap_or_default().to_vec();
    while body.len() < content_length {
        let read = timeout(REQUEST_READ_TIMEOUT, stream.read(&mut temp))
            .await
            .map_err(|_| "request body read timed out".to_string())?
            .map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("connection closed before body completed".to_string());
        }
        body.extend_from_slice(&temp[..read]);
        if body.len() > MAX_HTTP_REQUEST_BYTES {
            return Err("request body too large".to_string());
        }
    }
    body.truncate(content_length);

    Ok(Some(HttpRequest {
        method,
        path,
        query,
        body,
    }))
}

fn normalize_request_path(raw_path: &str) -> Result<(String, Option<String>), String> {
    let url = Url::parse(&format!("http://127.0.0.1{}", raw_path))
        .map_err(|err| format!("invalid request path: {}", err))?;
    Ok((url.path().to_string(), url.query().map(str::to_string)))
}

fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let headers = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\naccess-control-allow-origin: http://127.0.0.1:{}\r\naccess-control-allow-methods: GET,POST,OPTIONS\r\naccess-control-allow-headers: content-type\r\nconnection: close\r\n\r\n",
        status,
        reason,
        content_type,
        body.len(),
        get_actual_port().unwrap_or(DEFAULT_WEB_CONSOLE_PORT)
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(|err| err.to_string())?;
    stream
        .write_all(body)
        .await
        .map_err(|err| err.to_string())?;
    stream.shutdown().await.map_err(|err| err.to_string())
}

fn find_frontend_dist() -> Option<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest_dir.join("../dist"),
        std::env::current_dir().ok()?.join("dist"),
        std::env::current_dir().ok()?.join("../dist"),
    ];

    candidates
        .into_iter()
        .map(|path| normalize_path(&path))
        .find(|path| path.join(INDEX_HTML).exists())
}

fn normalize_path(path: &Path) -> PathBuf {
    path.components().fold(PathBuf::new(), |mut acc, part| {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                acc.pop();
            }
            other => acc.push(other.as_os_str()),
        }
        acc
    })
}

fn resolve_static_path(root: &Path, request_path: &str) -> Result<PathBuf, String> {
    let path = if request_path == "/" || request_path.is_empty() {
        INDEX_HTML.to_string()
    } else {
        request_path.trim_start_matches('/').to_string()
    };
    let decoded =
        urlencoding::decode(&path).map_err(|err| format!("invalid URL encoding: {}", err))?;
    let mut result = PathBuf::from(root);
    for segment in decoded.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." || segment.contains('\\') {
            return Err("invalid static path".to_string());
        }
        result.push(segment);
    }
    Ok(result)
}

fn content_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}
