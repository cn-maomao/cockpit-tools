use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::modules::{atomic_write, config, logger};

const EVENT_NAME: &str = "grok-tools:event";
const DEFAULT_API_PORT: u16 = 8000;
const REGISTER_CONFIG_TEMPLATE: &str =
    include_str!("../../../sidecars/grok-register/config.example.json");
const API_CONFIG_TEMPLATE: &str = include_str!("../../../sidecars/grok2api/config.example.yaml");

static API_CHILD: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
static REGISTER_CHILD: OnceLock<Mutex<Option<Child>>> = OnceLock::new();

fn api_child() -> &'static Mutex<Option<Child>> {
    API_CHILD.get_or_init(|| Mutex::new(None))
}

fn register_child() -> &'static Mutex<Option<Child>> {
    REGISTER_CHILD.get_or_init(|| Mutex::new(None))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokToolsSettings {
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default)]
    pub api_auto_start: bool,
    #[serde(default)]
    pub registration: Value,
}

fn default_api_port() -> u16 {
    DEFAULT_API_PORT
}

impl Default for GrokToolsSettings {
    fn default() -> Self {
        let mut registration = serde_json::from_str(REGISTER_CONFIG_TEMPLATE)
            .unwrap_or_else(|_| Value::Object(Default::default()));
        if let Some(object) = registration.as_object_mut() {
            object.insert("email_provider".to_string(), json!("duckmail"));
            for key in [
                "cloudflare_api_base",
                "cloudmail_api_base",
                "cloudmail_domains",
                "defaultDomains",
            ] {
                object.insert(key.to_string(), json!(""));
            }
        }
        Self {
            api_port: DEFAULT_API_PORT,
            api_auto_start: false,
            registration,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrokToolsSecrets {
    jwt_secret: String,
    credential_encryption_key: String,
    admin_username: String,
    admin_password: String,
    #[serde(default)]
    client_api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokToolsStatus {
    pub api_running: bool,
    pub api_ready: bool,
    pub registration_running: bool,
    pub api_base_url: String,
    pub api_key: Option<String>,
    pub settings: GrokToolsSettings,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrokToolsEvent {
    kind: String,
    level: String,
    message: String,
    data: Option<Value>,
}

fn emit_event(
    app: &AppHandle,
    kind: &str,
    level: &str,
    message: impl Into<String>,
    data: Option<Value>,
) {
    let _ = app.emit(
        EVENT_NAME,
        GrokToolsEvent {
            kind: kind.to_string(),
            level: level.to_string(),
            message: message.into(),
            data,
        },
    );
}

fn tools_dir() -> Result<PathBuf, String> {
    let path = config::get_data_dir()?.join("grok_tools");
    fs::create_dir_all(&path).map_err(|error| format!("创建 Grok 工具目录失败: {error}"))?;
    Ok(path)
}

fn settings_path() -> Result<PathBuf, String> {
    Ok(tools_dir()?.join("settings.json"))
}

fn secrets_path() -> Result<PathBuf, String> {
    Ok(tools_dir()?.join("secrets.json"))
}

fn load_settings() -> Result<GrokToolsSettings, String> {
    let path = settings_path()?;
    if !path.is_file() {
        return Ok(GrokToolsSettings::default());
    }
    let content =
        fs::read_to_string(&path).map_err(|error| format!("读取 Grok 工具设置失败: {error}"))?;
    serde_json::from_str(&content).map_err(|error| format!("解析 Grok 工具设置失败: {error}"))
}

fn write_private(path: &Path, content: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建配置目录失败: {error}"))?;
    }
    atomic_write::write_bytes_atomic(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("设置配置权限失败: {error}"))?;
    }
    Ok(())
}

fn save_settings(settings: &GrokToolsSettings) -> Result<(), String> {
    if settings.api_port == 0 {
        return Err("Grok2API 端口必须大于 0".to_string());
    }
    let content = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("序列化 Grok 工具设置失败: {error}"))?;
    write_private(&settings_path()?, &content)
}

fn random_hex(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    OsRng.fill_bytes(&mut value);
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn load_or_create_secrets() -> Result<GrokToolsSecrets, String> {
    let path = secrets_path()?;
    if path.is_file() {
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("读取 Grok2API 密钥失败: {error}"))?;
        return serde_json::from_str(&content)
            .map_err(|error| format!("解析 Grok2API 密钥失败: {error}"));
    }
    let mut encryption_key = [0_u8; 32];
    OsRng.fill_bytes(&mut encryption_key);
    let secrets = GrokToolsSecrets {
        jwt_secret: random_hex(32),
        credential_encryption_key: base64::engine::general_purpose::STANDARD.encode(encryption_key),
        admin_username: "cockpit".to_string(),
        admin_password: format!("g2a-{}", random_hex(24)),
        client_api_key: None,
    };
    let content = serde_json::to_vec_pretty(&secrets)
        .map_err(|error| format!("序列化 Grok2API 密钥失败: {error}"))?;
    write_private(&path, &content)?;
    Ok(secrets)
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn prepare_api_config(
    settings: &GrokToolsSettings,
    secrets: &GrokToolsSecrets,
) -> Result<PathBuf, String> {
    let root = tools_dir()?;
    let path = root.join("grok2api.yaml");
    let mut content = API_CONFIG_TEMPLATE.to_string();
    content = content.replace("replace-with-at-least-32-characters", &secrets.jwt_secret);
    content = content.replace(
        "replace-with-base64-key",
        &secrets.credential_encryption_key,
    );
    content = content.replace(
        "username: \"admin\"",
        &format!("username: {}", yaml_string(&secrets.admin_username)),
    );
    content = content.replace(
        "password: \"replace-with-a-strong-password\"",
        &format!("password: {}", yaml_string(&secrets.admin_password)),
    );
    content = content.replace(
        "path: \"./data/backend.db\"",
        "path: \"./grok2api-data/backend.db\"",
    );
    content = content.replace("path: \"./data/media\"", "path: \"./grok2api-data/media\"");
    content = content.replace("staticPath: \"./frontend/dist\"", "staticPath: \"\"");
    write_private(&path, content.as_bytes())?;
    fs::create_dir_all(root.join("grok2api-data"))
        .map_err(|error| format!("创建 Grok2API 数据目录失败: {error}"))?;
    let _ = settings;
    Ok(path)
}

fn sidecar_names(base: &str) -> Vec<String> {
    let target = env!("COCKPIT_RUST_TARGET");
    if cfg!(target_os = "windows") {
        vec![format!("{base}.exe"), format!("{base}-{target}.exe")]
    } else {
        let mut names = vec![base.to_string(), format!("{base}-{target}")];
        if target == "universal-apple-darwin" {
            let arch_target = if cfg!(target_arch = "aarch64") {
                "aarch64-apple-darwin"
            } else {
                "x86_64-apple-darwin"
            };
            names.insert(1, format!("{base}-{arch_target}"));
        }
        names
    }
}

fn sidecar_binary(base: &str, dev_dir: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| format!("读取程序路径失败: {error}"))?;
    let parent = exe
        .parent()
        .ok_or_else(|| "程序路径缺少父目录".to_string())?;
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut dirs = vec![manifest.join(dev_dir), parent.to_path_buf()];
    if let Some(contents) = parent.parent() {
        dirs.push(contents.join("Resources"));
    }
    let candidates: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|dir| {
            sidecar_names(base)
                .into_iter()
                .map(move |name| dir.join(name))
        })
        .collect();
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            format!(
                "{} sidecar 不存在，请重新构建应用。已检查: {}",
                base,
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn process_running(slot: &Mutex<Option<Child>>) -> bool {
    let Ok(mut guard) = slot.lock() else {
        return false;
    };
    let Some(child) = guard.as_mut() else {
        return false;
    };
    match child.try_wait() {
        Ok(None) => true,
        _ => {
            *guard = None;
            false
        }
    }
}

fn pipe_logs(app: AppHandle, source: &'static str, reader: impl std::io::Read + Send + 'static) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                logger::log_info(&format!("[GrokTools][{source}] {line}"));
                emit_event(&app, source, "info", line, None);
            }
        }
    });
}

fn start_api_internal(app: &AppHandle, settings: &GrokToolsSettings) -> Result<(), String> {
    if process_running(api_child()) {
        return Ok(());
    }
    let secrets = load_or_create_secrets()?;
    let config_path = prepare_api_config(settings, &secrets)?;
    let binary = sidecar_binary("cockpit-grok2api", "../sidecars/grok2api/bin")?;
    let mut command = Command::new(binary);
    command
        .arg("--config")
        .arg(config_path)
        .arg("--listen")
        .arg(format!("127.0.0.1:{}", settings.api_port))
        .current_dir(tools_dir()?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 Grok2API 失败: {error}"))?;
    if let Some(stdout) = child.stdout.take() {
        pipe_logs(app.clone(), "api", stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        pipe_logs(app.clone(), "api", stderr);
    }
    *api_child()
        .lock()
        .map_err(|_| "Grok2API 进程锁已损坏".to_string())? = Some(child);
    emit_event(app, "api", "info", "Grok2API 正在启动", None);
    Ok(())
}

fn api_base_url(settings: &GrokToolsSettings) -> String {
    format!("http://127.0.0.1:{}", settings.api_port)
}

fn admin_access_token(
    client: &reqwest::blocking::Client,
    base: &str,
    secrets: &GrokToolsSecrets,
) -> Result<String, String> {
    let login: Value = client
        .post(format!("{base}/api/admin/v1/auth/login"))
        .json(&json!({"username": secrets.admin_username, "password": secrets.admin_password}))
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|error| format!("登录 Grok2API 管理端失败: {error}"))?
        .json()
        .map_err(|error| format!("解析 Grok2API 登录响应失败: {error}"))?;
    login
        .pointer("/data/tokens/accessToken")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "Grok2API 登录响应缺少 access token".to_string())
}

fn ensure_client_api_key(settings: &GrokToolsSettings) -> Result<String, String> {
    let mut secrets = load_or_create_secrets()?;
    if let Some(key) = secrets
        .client_api_key
        .as_deref()
        .filter(|key| !key.is_empty())
    {
        return Ok(key.to_string());
    }
    let base = api_base_url(settings);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("创建 Grok2API 客户端失败: {error}"))?;
    let token = admin_access_token(&client, &base, &secrets)?;
    let response: Value = client
        .post(format!("{base}/api/admin/v1/client-keys"))
        .bearer_auth(token)
        .json(&json!({
            "name": "Cockpit Tools",
            "enabled": true,
            "rpmLimit": 0,
            "maxConcurrent": 0,
            "billingLimitUsdTicks": 0,
            "allowedModelIds": []
        }))
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|error| format!("创建 Grok2API 客户端密钥失败: {error}"))?
        .json()
        .map_err(|error| format!("解析 Grok2API 客户端密钥失败: {error}"))?;
    let key = response
        .pointer("/data/secret")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "Grok2API 创建密钥响应缺少 secret".to_string())?
        .to_string();
    secrets.client_api_key = Some(key.clone());
    let content = serde_json::to_vec_pretty(&secrets)
        .map_err(|error| format!("序列化 Grok2API 密钥失败: {error}"))?;
    write_private(&secrets_path()?, &content)?;
    Ok(key)
}

fn api_ready(settings: &GrokToolsSettings) -> bool {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .and_then(|client| {
            client
                .get(format!("{}/healthz", api_base_url(settings)))
                .send()
        })
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

fn wait_for_api(settings: &GrokToolsSettings) -> Result<(), String> {
    for _ in 0..60 {
        if api_ready(settings) {
            return Ok(());
        }
        if !process_running(api_child()) {
            return Err("Grok2API 启动后意外退出，请查看日志".to_string());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err("等待 Grok2API 就绪超时".to_string())
}

fn merge_json(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                merge_json(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, patch) => *base = patch.clone(),
    }
}

fn prepare_registration_config(settings: &GrokToolsSettings) -> Result<PathBuf, String> {
    let mut config: Value = serde_json::from_str(REGISTER_CONFIG_TEMPLATE)
        .map_err(|error| format!("解析注册器默认配置失败: {error}"))?;
    merge_json(&mut config, &settings.registration);
    if let Some(object) = config.as_object_mut() {
        object.insert("grok2api_auto_add_local".to_string(), Value::Bool(false));
        object.insert("grok2api_auto_add_remote".to_string(), Value::Bool(false));
    }
    let path = tools_dir()?.join("register-config.json");
    let content = serde_json::to_vec_pretty(&config)
        .map_err(|error| format!("序列化注册器配置失败: {error}"))?;
    write_private(&path, &content)?;
    Ok(path)
}

fn registration_cancel_path() -> Result<PathBuf, String> {
    Ok(tools_dir()?.join("register.cancel"))
}

fn import_web_account(settings: &GrokToolsSettings, email: &str, sso: &str) -> Result<(), String> {
    let secrets = load_or_create_secrets()?;
    let base = api_base_url(settings);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|error| format!("创建 Grok2API 客户端失败: {error}"))?;
    let token = admin_access_token(&client, &base, &secrets)?;
    let part = reqwest::blocking::multipart::Part::text(sso.to_string())
        .file_name(format!("{}.txt", email.replace(['/', '\\'], "_")))
        .mime_str("text/plain")
        .map_err(|error| format!("创建账号导入请求失败: {error}"))?;
    let response = client
        .post(format!("{base}/api/admin/v1/accounts/web/import"))
        .bearer_auth(&token)
        .multipart(reqwest::blocking::multipart::Form::new().part("file", part))
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|error| format!("导入账号到 Grok2API 失败: {error}"))?;
    let body = response
        .text()
        .map_err(|error| format!("读取 Grok2API 导入结果失败: {error}"))?;
    if body.contains("event: error") {
        return Err("Grok2API 拒绝了账号导入，请查看服务日志".to_string());
    }
    if !body.contains("event: complete") {
        return Err("Grok2API 导入连接提前结束".to_string());
    }
    Ok(())
}

fn handle_registration_stdout(
    app: AppHandle,
    settings: GrokToolsSettings,
    reader: impl std::io::Read + Send + 'static,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                logger::log_warn(&format!("[GrokTools][register] 非 JSON 输出: {line}"));
                continue;
            };
            match event.get("type").and_then(Value::as_str) {
                Some("account") => {
                    let email = event
                        .get("email")
                        .and_then(Value::as_str)
                        .unwrap_or("未知账号");
                    let sso = event.get("sso").and_then(Value::as_str).unwrap_or("");
                    if sso.is_empty() {
                        emit_event(
                            &app,
                            "register",
                            "error",
                            format!("账号 {email} 缺少 SSO，未导入"),
                            None,
                        );
                    } else {
                        let mut import_result = Err("账号导入未执行".to_string());
                        for attempt in 1_u64..=3 {
                            import_result = import_web_account(&settings, email, sso);
                            if import_result.is_ok() {
                                break;
                            }
                            if attempt < 3 {
                                std::thread::sleep(Duration::from_secs(attempt));
                            }
                        }
                        match import_result {
                            Ok(()) => emit_event(
                                &app,
                                "account-imported",
                                "success",
                                format!("账号 {email} 已自动导入 Grok2API"),
                                Some(json!({"email": email})),
                            ),
                            Err(error) => emit_event(
                                &app,
                                "account-imported",
                                "error",
                                error,
                                Some(json!({"email": email})),
                            ),
                        }
                    }
                }
                Some("log") => {
                    let message = event
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    emit_event(&app, "register", "info", message, None);
                }
                Some(kind @ ("progress" | "complete" | "state")) => {
                    emit_event(&app, kind, "info", "", Some(event.clone()));
                }
                Some("error") => {
                    let message = event
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("注册任务失败");
                    emit_event(&app, "register", "error", message, None);
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        emit_event(&app, "registration-exited", "info", "注册任务已结束", None);
    });
}

pub fn status() -> Result<GrokToolsStatus, String> {
    let settings = load_settings()?;
    let api_running = process_running(api_child());
    Ok(GrokToolsStatus {
        api_running,
        api_ready: api_running && api_ready(&settings),
        registration_running: process_running(register_child()),
        api_base_url: api_base_url(&settings),
        api_key: load_or_create_secrets()?.client_api_key,
        settings,
    })
}

pub fn update_settings(settings: GrokToolsSettings) -> Result<GrokToolsStatus, String> {
    save_settings(&settings)?;
    status()
}

pub fn start_api(app: AppHandle) -> Result<GrokToolsStatus, String> {
    let settings = load_settings()?;
    start_api_internal(&app, &settings)?;
    wait_for_api(&settings)?;
    let _ = ensure_client_api_key(&settings)?;
    emit_event(&app, "api", "success", "Grok2API 已就绪", None);
    status()
}

pub fn stop_api(app: AppHandle) -> Result<GrokToolsStatus, String> {
    if process_running(register_child()) {
        return Err("注册任务运行期间不能停止 Grok2API".to_string());
    }
    let mut guard = api_child()
        .lock()
        .map_err(|_| "Grok2API 进程锁已损坏".to_string())?;
    if let Some(child) = guard.as_mut() {
        child
            .kill()
            .map_err(|error| format!("停止 Grok2API 失败: {error}"))?;
        let _ = child.wait();
    }
    *guard = None;
    emit_event(&app, "api", "info", "Grok2API 已停止", None);
    drop(guard);
    status()
}

pub fn start_registration(app: AppHandle) -> Result<GrokToolsStatus, String> {
    if process_running(register_child()) {
        return Err("已有 Grok 注册任务正在运行".to_string());
    }
    let settings = load_settings()?;
    start_api_internal(&app, &settings)?;
    wait_for_api(&settings)?;
    let _ = ensure_client_api_key(&settings)?;
    let config_path = prepare_registration_config(&settings)?;
    let cancel_path = registration_cancel_path()?;
    if cancel_path.exists() {
        fs::remove_file(&cancel_path)
            .map_err(|error| format!("清理旧注册停止标记失败: {error}"))?;
    }
    let output_dir = tools_dir()?.join("registered-accounts");
    fs::create_dir_all(&output_dir).map_err(|error| format!("创建账号输出目录失败: {error}"))?;
    let output_path = output_dir.join(format!(
        "accounts_{}.txt",
        chrono::Utc::now().format("%Y%m%d_%H%M%S")
    ));
    let binary = sidecar_binary("cockpit-grok-register", "../sidecars/grok-register/bin")?;
    let mut command = Command::new(binary);
    command
        .arg("--config")
        .arg(config_path)
        .arg("--output")
        .arg(output_path)
        .arg("--cancel-file")
        .arg(cancel_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 Grok 注册器失败: {error}"))?;
    if let Some(stdout) = child.stdout.take() {
        handle_registration_stdout(app.clone(), settings, stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        pipe_logs(app.clone(), "register", stderr);
    }
    *register_child()
        .lock()
        .map_err(|_| "注册器进程锁已损坏".to_string())? = Some(child);
    emit_event(&app, "register", "info", "自动注册任务已启动", None);
    status()
}

pub fn cancel_registration(app: AppHandle) -> Result<GrokToolsStatus, String> {
    write_private(&registration_cancel_path()?, b"stop")?;
    let mut guard = register_child()
        .lock()
        .map_err(|_| "注册器进程锁已损坏".to_string())?;
    if let Some(child) = guard.as_mut() {
        let mut exited = false;
        for _ in 0..40 {
            match child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(250)),
                Err(_) => break,
            }
        }
        if !exited {
            child
                .kill()
                .map_err(|error| format!("停止注册任务失败: {error}"))?;
            let _ = child.wait();
        }
    }
    *guard = None;
    emit_event(&app, "register", "info", "注册任务已停止", None);
    drop(guard);
    status()
}

pub fn restore_auto_start(app: AppHandle) {
    let Ok(settings) = load_settings() else {
        return;
    };
    if !settings.api_auto_start {
        return;
    }
    std::thread::spawn(move || {
        if let Err(error) = start_api(app.clone()) {
            logger::log_warn(&format!("[GrokTools] 自动启动 Grok2API 失败: {error}"));
            emit_event(&app, "api", "error", error, None);
        }
    });
}

pub fn shutdown() {
    if let Ok(cancel_path) = registration_cancel_path() {
        let _ = write_private(&cancel_path, b"stop");
    }
    if let Ok(mut guard) = register_child().lock() {
        if let Some(child) = guard.as_mut() {
            for _ in 0..12 {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        *guard = None;
    }
    if let Ok(mut guard) = api_child().lock() {
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *guard = None;
    }
}
