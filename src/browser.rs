// SPDX-License-Identifier: MIT

use crate::error::{Result, SessionError};
use reqwest::{blocking::Client, Url};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::{Debug, Formatter};
use std::fs;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{client, Error as WebSocketError, Message, WebSocket};

#[cfg(target_os = "macos")]
const DEFAULT_DIA_BROWSER_PATH: &str = "/Applications/Dia.app/Contents/MacOS/Dia";

const BROWSER_CHOICES: &str =
    "chrome, brave, edge, chromium, firefox, zen, dia, default, or an executable path";
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserProtocol {
    Cdp,
    Bidi,
}

impl BrowserProtocol {
    pub fn endpoint_name(self) -> &'static str {
        match self {
            Self::Cdp => "DevTools",
            Self::Bidi => "WebDriver BiDi",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserKind {
    Chrome,
    Brave,
    Edge,
    Chromium,
    Firefox,
    Zen,
    Dia,
    Custom(String),
}

impl BrowserKind {
    fn name(&self) -> &str {
        match self {
            Self::Chrome => "chrome",
            Self::Brave => "brave",
            Self::Edge => "edge",
            Self::Chromium => "chromium",
            Self::Firefox => "firefox",
            Self::Zen => "zen",
            Self::Dia => "dia",
            Self::Custom(name) => name,
        }
    }

    fn protocol(&self) -> BrowserProtocol {
        match self {
            Self::Firefox | Self::Zen => BrowserProtocol::Bidi,
            _ => BrowserProtocol::Cdp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browser {
    kind: BrowserKind,
    executable: PathBuf,
}

impl Browser {
    pub fn name(&self) -> &str {
        self.kind.name()
    }

    pub fn protocol(&self) -> BrowserProtocol {
        self.kind.protocol()
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    fn uses_automatic_profile(&self) -> bool {
        matches!(
            self.kind,
            BrowserKind::Chrome | BrowserKind::Firefox | BrowserKind::Zen
        )
    }

    fn is_dia(&self) -> bool {
        self.kind == BrowserKind::Dia
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedProfile {
    pub browser: String,
    pub path: PathBuf,
}

#[derive(Clone, Deserialize)]
pub struct BrowserCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "root_cookie_path")]
    pub path: String,
    #[serde(default)]
    pub expires: f64,
    #[serde(default)]
    pub secure: bool,
    #[serde(rename = "httpOnly", default)]
    pub http_only: bool,
}

impl Debug for BrowserCookie {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserCookie")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("expires", &self.expires)
            .field("secure", &self.secure)
            .field("http_only", &self.http_only)
            .finish()
    }
}

fn root_cookie_path() -> String {
    "/".to_string()
}

#[derive(Debug, Deserialize)]
pub struct CdpVersion {
    #[serde(rename = "Browser")]
    browser_name: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl")]
    browser_websocket_url: Option<String>,
    #[serde(rename = "User-Agent")]
    user_agent: Option<String>,
}

impl CdpVersion {
    pub fn browser_websocket_url(&self) -> Option<&str> {
        self.browser_websocket_url.as_deref()
    }

    pub fn browser_name(&self) -> Option<&str> {
        self.browser_name.as_deref()
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }
}

#[derive(Debug, Deserialize)]
pub struct CdpTarget {
    #[serde(default)]
    id: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: String,
}

impl CdpTarget {
    pub fn websocket_url(&self) -> &str {
        &self.websocket_url
    }
}

type BrowserSocket = WebSocket<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Copy)]
pub(crate) struct RequestControl<'a> {
    deadline: Instant,
    cancelled: Option<&'a AtomicBool>,
}

impl<'a> RequestControl<'a> {
    pub(crate) fn new(timeout: Duration, cancelled: &'a AtomicBool) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancelled: Some(cancelled),
        }
    }

    pub(crate) fn cleanup(timeout: Duration) -> RequestControl<'static> {
        RequestControl {
            deadline: Instant::now() + timeout,
            cancelled: None,
        }
    }

    pub(crate) fn check(self) -> Result<()> {
        if self
            .cancelled
            .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
        {
            return Err(SessionError::cancelled(
                "browser session capture was cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(SessionError::timeout(
                "browser session request exceeded its deadline",
            ));
        }
        Ok(())
    }

    fn poll_timeout(self) -> Result<Duration> {
        self.check()?;
        Ok(self
            .deadline
            .saturating_duration_since(Instant::now())
            .min(SOCKET_POLL_INTERVAL))
    }
}

fn connect_browser_socket(url: &str, control: RequestControl<'_>) -> Result<BrowserSocket> {
    control.check()?;
    let parsed = Url::parse(url).map_err(|error| {
        SessionError::network(format!("invalid browser websocket URL: {error}"))
    })?;
    if parsed.scheme() != "ws" || !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost")) {
        return Err(SessionError::network(
            "browser websocket URL must use ws:// on loopback",
        ));
    }
    let port = parsed
        .port()
        .ok_or_else(|| SessionError::network("browser websocket URL did not include a port"))?;
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let stream =
        TcpStream::connect_timeout(&address, control.poll_timeout()?).map_err(|error| {
            SessionError::network(format!("cannot connect to browser websocket: {error}"))
        })?;
    let timeout = control.poll_timeout()?;
    stream.set_read_timeout(Some(timeout)).map_err(|error| {
        SessionError::network(format!(
            "cannot set browser websocket read timeout: {error}"
        ))
    })?;
    stream.set_write_timeout(Some(timeout)).map_err(|error| {
        SessionError::network(format!(
            "cannot set browser websocket write timeout: {error}"
        ))
    })?;
    client(url, MaybeTlsStream::Plain(stream))
        .map(|(socket, _)| socket)
        .map_err(|error| {
            SessionError::network(format!("cannot establish browser websocket: {error}"))
        })
}

fn read_browser_message(
    socket: &mut BrowserSocket,
    control: RequestControl<'_>,
    context: &str,
) -> Result<Message> {
    loop {
        control.check()?;
        match socket.read() {
            Ok(message) => return Ok(message),
            Err(WebSocketError::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(error) => {
                return Err(SessionError::network(format!(
                    "cannot read {context} response: {error}"
                )));
            }
        }
    }
}

pub struct BidiConnection {
    socket: BrowserSocket,
    next_id: u64,
    user_agent: Option<String>,
    browser_name: Option<String>,
}

impl BidiConnection {
    fn connect(url: &str, control: RequestControl<'_>) -> Result<Self> {
        Ok(Self {
            socket: connect_browser_socket(url, control)?,
            next_id: 1,
            user_agent: None,
            browser_name: None,
        })
    }

    fn start_session(&mut self, control: RequestControl<'_>) -> Result<()> {
        self.call("session.status", json!({}), control)?;
        let result = self.call("session.new", json!({ "capabilities": {} }), control)?;
        self.browser_name = result
            .pointer("/capabilities/browserName")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.user_agent = result
            .pointer("/capabilities/userAgent")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(())
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    pub fn browser_name(&self) -> Option<&str> {
        self.browser_name.as_deref()
    }

    pub(crate) fn call(
        &mut self,
        method: &str,
        params: Value,
        control: RequestControl<'_>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "id": id,
            "method": method,
            "params": params
        });
        control.check()?;
        self.socket
            .send(Message::Text(payload.to_string()))
            .map_err(|error| {
                SessionError::network(format!("cannot send WebDriver BiDi request: {error}"))
            })?;
        loop {
            let message = read_browser_message(&mut self.socket, control, "WebDriver BiDi")?;
            let Ok(text) = message.into_text() else {
                continue;
            };
            let response: Value = serde_json::from_str(&text).map_err(|error| {
                SessionError::helper_failure(format!(
                    "invalid WebDriver BiDi websocket response: {error}"
                ))
            })?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            match response.get("type").and_then(Value::as_str) {
                Some("success") => {
                    return Ok(response.get("result").cloned().unwrap_or_else(|| json!({})));
                }
                Some("error") => {
                    let error = response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    let message = response
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("no message");
                    return Err(SessionError::helper_failure(format!(
                        "WebDriver BiDi `{method}` failed: {error}: {message}"
                    )));
                }
                _ => {
                    return Err(SessionError::helper_failure(format!(
                        "WebDriver BiDi `{method}` returned an invalid response"
                    )));
                }
            }
        }
    }
}

pub fn resolve_browser(requested: Option<&str>) -> Result<Browser> {
    if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        if requested.eq_ignore_ascii_case("default") {
            return resolve_default_browser();
        }
        if requested.contains('/') || requested.contains('\\') {
            let executable = PathBuf::from(requested);
            if executable.exists() {
                let kind = supported_browser(&executable).unwrap_or_else(|| {
                    BrowserKind::Custom(
                        executable
                            .file_stem()
                            .and_then(|name| name.to_str())
                            .unwrap_or("custom")
                            .to_string(),
                    )
                });
                return Ok(Browser { kind, executable });
            }
            return Err(SessionError::invalid_request(format!(
                "cannot find browser executable `{requested}`"
            )));
        }
        let requested_lower = requested.to_ascii_lowercase();
        if let Some(browser) = browser_candidates().into_iter().find(|candidate| {
            candidate.name() == requested_lower && browser_is_available(candidate)
        }) {
            return Ok(browser);
        }
        return Err(SessionError::invalid_request(format!(
            "cannot find browser `{requested}`; try {BROWSER_CHOICES}"
        )));
    }
    resolve_default_browser()
}

pub fn effective_profile_dir(
    explicit: Option<&Path>,
    profile_root: Option<&Path>,
    browser: &Browser,
) -> Option<PathBuf> {
    explicit.map(Path::to_path_buf).or_else(|| {
        profile_root.and_then(|root| {
            browser
                .uses_automatic_profile()
                .then(|| automatic_profile_dir(root, browser))
        })
    })
}

fn automatic_profile_dir(root: &Path, browser: &Browser) -> PathBuf {
    let name = match browser.kind {
        BrowserKind::Chrome => "login-profile",
        BrowserKind::Firefox => "login-profile-firefox",
        BrowserKind::Zen => "login-profile-zen",
        _ => "login-profile",
    };
    root.join(name)
}

pub fn prepare_profile(
    url: &str,
    browser: &Browser,
    profile_dir: Option<&Path>,
    profile_root: Option<&Path>,
    timeout: Duration,
) -> Result<PreparedProfile> {
    let cancelled = AtomicBool::new(false);
    prepare_profile_with_cancel(url, browser, profile_dir, profile_root, timeout, &cancelled)
}

pub fn prepare_profile_with_cancel(
    url: &str,
    browser: &Browser,
    profile_dir: Option<&Path>,
    profile_root: Option<&Path>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<PreparedProfile> {
    if browser.protocol() != BrowserProtocol::Bidi {
        return Err(SessionError::invalid_request(
            "profile preparation supports only Firefox and Zen",
        ));
    }
    let automatic_profile = profile_dir.is_none() && profile_root.is_some();
    let profile_dir =
        effective_profile_dir(profile_dir, profile_root, browser).ok_or_else(|| {
            SessionError::invalid_request(
                "profile preparation requires an isolated profile directory",
            )
        })?;
    let arguments = profile_browser_arguments(&profile_dir, url);
    let mut child =
        spawn_browser_process(browser, Some(&profile_dir), automatic_profile, arguments)?;
    if let Err(error) = wait_for_profile_browser_exit(&mut child, timeout, url, cancelled) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(PreparedProfile {
        browser: browser.name().to_string(),
        path: profile_dir,
    })
}

pub fn browser_is_running(browser: &Browser) -> bool {
    browser_is_running_impl(browser)
}

pub fn running_browser_conflict(
    browser: &Browser,
    port: u16,
    isolated_profile: bool,
) -> Option<SessionError> {
    if !browser_is_running(browser) {
        return None;
    }
    if isolated_profile && !browser.is_dia() {
        return None;
    }
    if browser.is_dia() {
        return Some(SessionError::browser_restart_required(format!(
            "Dia is already running without {} on 127.0.0.1:{port}; close Dia and run capture again. Dia only permits one instance, so browser-session will not start a second Dia process.",
            browser.protocol().endpoint_name()
        )));
    }
    Some(SessionError::browser_restart_required(format!(
        "browser `{}` is already running without {} on 127.0.0.1:{port}; close it and run capture again, or configure an isolated profile directory",
        browser.executable.display(),
        browser.protocol().endpoint_name()
    )))
}

pub fn restart_running_browser(browser: &Browser) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let bundle_id = macos_app_bundle_for_executable(&browser.executable)
            .and_then(|app_path| macos_bundle_identifier(&app_path))
            .ok_or_else(|| {
                SessionError::helper_failure(format!(
                    "cannot determine app identity for `{}`; close it and run capture again",
                    browser.executable.display()
                ))
            })?;
        let quit_requested = macos_quit_app_id(&bundle_id)?;
        if !quit_requested {
            macos_terminate_browser_processes(browser)?;
        }
        match wait_for_browser_exit(browser, Duration::from_secs(8)) {
            Ok(()) => Ok(()),
            Err(error) if quit_requested => {
                macos_terminate_browser_processes(browser)?;
                wait_for_browser_exit(browser, Duration::from_secs(4)).map_err(|_| error)
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = browser;
        Err(SessionError::browser_restart_required(
            "automatic browser restart is only implemented on macOS; close the browser and run capture again",
        ))
    }
}

#[cfg(target_os = "macos")]
fn macos_quit_app_id(bundle_id: &str) -> Result<bool> {
    let script = format!(
        "tell application id \"{}\" to quit",
        bundle_id.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let output = Command::new("osascript")
        .args(["-e", &script])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            SessionError::helper_failure(format!("cannot ask browser to quit: {error}"))
        })?;
    if output.status.success() {
        return Ok(true);
    }
    let detail = String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no diagnostic from osascript")
        .trim()
        .to_string();
    if detail.contains("User canceled") || detail.contains("(-128)") {
        return Ok(false);
    }
    Err(SessionError::helper_failure(format!(
        "cannot ask browser to quit: {detail}"
    )))
}

#[cfg(target_os = "macos")]
fn macos_terminate_browser_processes(browser: &Browser) -> Result<()> {
    let executable = browser.executable.to_string_lossy();
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            SessionError::helper_failure(format!("cannot inspect browser processes: {error}"))
        })?;
    if !output.status.success() {
        return Err(SessionError::helper_failure(
            "cannot inspect browser processes",
        ));
    }
    let pids = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| macos_process_line_for_executable(line, &executable))
        .filter(|pid| pid != &std::process::id().to_string())
        .collect::<Vec<_>>();
    if pids.is_empty() {
        return Err(SessionError::helper_failure(format!(
            "browser `{}` did not quit and no matching process could be found",
            browser.executable.display()
        )));
    }
    for pid in pids {
        let status = Command::new("kill")
            .args(["-TERM", &pid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                SessionError::helper_failure(format!(
                    "cannot terminate browser process {pid}: {error}"
                ))
            })?;
        if !status.success() {
            return Err(SessionError::helper_failure(format!(
                "cannot terminate browser process {pid}"
            )));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_process_line_for_executable(line: &str, executable: &str) -> Option<String> {
    let line = line.trim_start();
    let (pid, command) = line.split_once(char::is_whitespace)?;
    if pid.chars().all(|character| character.is_ascii_digit()) && command.contains(executable) {
        Some(pid.to_string())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn wait_for_browser_exit(browser: &Browser, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while browser_is_running(browser) {
        if started.elapsed() >= timeout {
            return Err(SessionError::browser_restart_required(format!(
                "browser `{}` did not quit; close it and run capture again",
                browser.executable.display()
            )));
        }
        sleep(Duration::from_millis(200));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn browser_is_running_impl(browser: &Browser) -> bool {
    macos_app_bundle_for_executable(&browser.executable)
        .and_then(|app_path| macos_bundle_identifier(&app_path))
        .is_some_and(|bundle_id| macos_app_id_is_running(&bundle_id))
}

#[cfg(target_os = "linux")]
fn browser_is_running_impl(browser: &Browser) -> bool {
    let Some(process_name) = browser
        .executable
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    Command::new("pgrep")
        .args(["-x", process_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "windows")]
fn browser_is_running_impl(browser: &Browser) -> bool {
    windows_browser_process_names(browser)
        .into_iter()
        .any(|process_name| windows_process_is_running(&process_name))
}

#[cfg(target_os = "windows")]
fn windows_browser_process_names(browser: &Browser) -> Vec<String> {
    let mut names = match browser.kind {
        BrowserKind::Chrome => vec!["chrome.exe".to_string()],
        BrowserKind::Brave => vec!["brave.exe".to_string()],
        BrowserKind::Edge => vec!["msedge.exe".to_string()],
        BrowserKind::Chromium => vec!["chromium.exe".to_string(), "chrome.exe".to_string()],
        BrowserKind::Firefox => vec!["firefox.exe".to_string()],
        BrowserKind::Zen => vec!["zen.exe".to_string()],
        BrowserKind::Dia => vec!["dia.exe".to_string()],
        BrowserKind::Custom(_) => Vec::new(),
    };
    if let Some(file_name) = browser
        .executable
        .file_name()
        .and_then(|name| name.to_str())
    {
        let process_name = if file_name.contains('.') {
            file_name.to_string()
        } else {
            format!("{file_name}.exe")
        };
        names.push(process_name);
    }
    dedup_strings(names)
}

#[cfg(target_os = "windows")]
fn windows_process_is_running(process_name: &str) -> bool {
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH", "/FI"])
        .arg(format!("IMAGENAME eq {process_name}"))
        .stdin(Stdio::null())
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let needle = format!("\"{}\"", process_name.to_ascii_lowercase());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with(&needle))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn browser_is_running_impl(_browser: &Browser) -> bool {
    false
}

pub fn cdp_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub fn cdp_version(client: &Client, base_url: &str) -> Result<CdpVersion> {
    client
        .get(format!("{base_url}/json/version"))
        .send()
        .map_err(|error| {
            SessionError::network(format!("cannot connect to DevTools endpoint: {error}"))
        })?
        .error_for_status()
        .map_err(|error| {
            SessionError::helper_failure(format!(
                "DevTools endpoint rejected version request: {error}"
            ))
        })?
        .json()
        .map_err(|error| {
            SessionError::helper_failure(format!("invalid DevTools version response: {error}"))
        })
}

pub(crate) fn wait_for_cdp_version(
    client: &Client,
    base_url: &str,
    timeout: Duration,
    control: RequestControl<'_>,
) -> Result<Option<CdpVersion>> {
    let startup_deadline = Instant::now() + timeout;
    loop {
        control.check()?;
        if let Ok(version) = cdp_version(client, base_url) {
            return Ok(Some(version));
        }
        if Instant::now() >= startup_deadline {
            return Ok(None);
        }
        sleep(Duration::from_millis(150));
    }
}

pub fn open_cdp_target(client: &Client, base_url: &str, url: &str) -> Result<CdpTarget> {
    let target: CdpTarget = client
        .put(format!("{base_url}/json/new?{}", percent_encode(url)))
        .send()
        .map_err(|error| SessionError::network(format!("cannot open capture page: {error}")))?
        .error_for_status()
        .map_err(|error| {
            SessionError::helper_failure(format!("DevTools rejected capture page: {error}"))
        })?
        .json()
        .map_err(|error| {
            SessionError::helper_failure(format!("invalid DevTools target response: {error}"))
        })?;
    if target.websocket_url.is_empty() {
        return Err(SessionError::helper_failure(
            "DevTools target did not include a websocket URL",
        ));
    }
    if !target.id.is_empty() {
        let _ = client
            .get(format!("{base_url}/json/activate/{}", target.id))
            .send()
            .and_then(|response| response.error_for_status());
    }
    Ok(target)
}

pub(crate) fn cdp_cookies(
    websocket_url: &str,
    control: RequestControl<'_>,
) -> Result<Vec<BrowserCookie>> {
    let response = cdp_call(websocket_url, 1, "Network.getAllCookies", control)?;
    let cookies = response
        .pointer("/result/cookies")
        .cloned()
        .ok_or_else(|| SessionError::helper_failure("DevTools cookie response was incomplete"))?;
    serde_json::from_value(cookies).map_err(|error| {
        SessionError::helper_failure(format!("invalid DevTools cookie response: {error}"))
    })
}

pub(crate) fn evaluate_cdp(
    websocket_url: &str,
    expression: &str,
    control: RequestControl<'_>,
) -> Result<Value> {
    let response = cdp_call_with_params(
        websocket_url,
        2,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true
        }),
        control,
    )?;
    if response.get("exceptionDetails").is_some() {
        return Err(SessionError::helper_failure(
            "browser-session page script raised an exception",
        ));
    }
    Ok(response
        .pointer("/result/result/value")
        .cloned()
        .unwrap_or(Value::Null))
}

pub(crate) fn close_cdp_browser(websocket_url: Option<&str>, control: RequestControl<'_>) {
    if let Some(websocket_url) = websocket_url {
        let _ = cdp_call(websocket_url, 99, "Browser.close", control);
    }
}

pub(crate) fn cdp_call(
    websocket_url: &str,
    id: u64,
    method: &str,
    control: RequestControl<'_>,
) -> Result<Value> {
    cdp_call_with_params(websocket_url, id, method, Value::Null, control)
}

pub(crate) fn cdp_call_with_params(
    websocket_url: &str,
    id: u64,
    method: &str,
    params: Value,
    control: RequestControl<'_>,
) -> Result<Value> {
    let mut socket = connect_browser_socket(websocket_url, control)?;
    let mut payload = json!({"id": id, "method": method});
    if !params.is_null() {
        payload
            .as_object_mut()
            .expect("CDP payload is an object")
            .insert("params".to_string(), params);
    }
    control.check()?;
    socket
        .send(Message::Text(payload.to_string()))
        .map_err(|error| SessionError::network(format!("cannot send DevTools request: {error}")))?;
    loop {
        let message = read_browser_message(&mut socket, control, "DevTools")?;
        let Ok(text) = message.into_text() else {
            continue;
        };
        let response: Value = serde_json::from_str(&text).map_err(|error| {
            SessionError::helper_failure(format!("invalid DevTools websocket response: {error}"))
        })?;
        if response.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = response.get("error") {
            return Err(SessionError::helper_failure(format!(
                "DevTools `{method}` failed: {error}"
            )));
        }
        return Ok(response);
    }
}

pub(crate) fn connect_bidi(
    client: &Client,
    port: u16,
    control: RequestControl<'_>,
) -> Result<BidiConnection> {
    let mut last_error: Option<SessionError> = None;
    for url in bidi_websocket_urls(client, port) {
        match BidiConnection::connect(&url, control).and_then(|mut connection| {
            connection.start_session(control)?;
            Ok(connection)
        }) {
            Ok(connection) => return Ok(connection),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        SessionError::network(format!(
            "cannot find WebDriver BiDi websocket URL on 127.0.0.1:{port}"
        ))
    }))
}

pub(crate) fn wait_for_bidi_connection(
    client: &Client,
    port: u16,
    timeout: Duration,
    control: RequestControl<'_>,
) -> Result<Option<BidiConnection>> {
    let startup_deadline = Instant::now() + timeout;
    loop {
        control.check()?;
        if let Ok(connection) = connect_bidi(client, port, control) {
            return Ok(Some(connection));
        }
        if Instant::now() >= startup_deadline {
            return Ok(None);
        }
        sleep(Duration::from_millis(150));
    }
}

pub(crate) fn open_bidi_context(
    connection: &mut BidiConnection,
    url: &str,
    control: RequestControl<'_>,
) -> Result<String> {
    let result = connection.call("browsingContext.create", json!({ "type": "tab" }), control)?;
    let context = result
        .get("context")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SessionError::helper_failure("WebDriver BiDi did not return a browsing context")
        })?
        .to_string();
    connection.call(
        "browsingContext.navigate",
        json!({
            "context": context,
            "url": url,
            "wait": "interactive"
        }),
        control,
    )?;
    let _ = connection.call(
        "browsingContext.activate",
        json!({ "context": context }),
        control,
    );
    Ok(context)
}

pub(crate) fn bidi_cookies(
    connection: &mut BidiConnection,
    context: &str,
    control: RequestControl<'_>,
) -> Result<Vec<BrowserCookie>> {
    let result = connection
        .call(
            "storage.getCookies",
            json!({
                "partition": {
                    "type": "context",
                    "context": context
                }
            }),
            control,
        )
        .or_else(|_| connection.call("storage.getCookies", json!({}), control))?;
    let cookies = result.get("cookies").cloned().ok_or_else(|| {
        SessionError::helper_failure("WebDriver BiDi cookie response did not include cookies")
    })?;
    let cookies: Vec<BidiCookie> = serde_json::from_value(cookies).map_err(|error| {
        SessionError::helper_failure(format!("invalid WebDriver BiDi cookie response: {error}"))
    })?;
    cookies
        .into_iter()
        .map(BidiCookie::into_browser_cookie)
        .collect()
}

pub(crate) fn evaluate_bidi(
    connection: &mut BidiConnection,
    context: &str,
    expression: &str,
    control: RequestControl<'_>,
) -> Result<Value> {
    // A provider expression can end in a semicolon or a line comment.  Evaluate
    // it as an assignment so its source stays valid, then serialize the resolved
    // value rather than the Promise itself.
    let expression = format!(
        "(async () => {{\nconst __browser_session_result__ = {expression}\n\
         ;return JSON.stringify(await __browser_session_result__);\n}})()"
    );
    let result = connection.call(
        "script.evaluate",
        json!({
            "expression": expression,
            "target": { "context": context },
            "awaitPromise": true,
            "userActivation": false
        }),
        control,
    )?;
    if result.get("type").and_then(Value::as_str) == Some("exception") {
        return Err(SessionError::helper_failure(
            "browser-session page script raised an exception",
        ));
    }
    if result.pointer("/result/type").and_then(Value::as_str) == Some("undefined") {
        return Ok(Value::Null);
    }
    let text = result
        .pointer("/result/value")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SessionError::helper_failure(
                "WebDriver BiDi page script response did not include a JSON value",
            )
        })?;
    serde_json::from_str(text).map_err(|error| {
        SessionError::helper_failure(format!("invalid WebDriver BiDi page script value: {error}"))
    })
}

pub(crate) fn close_bidi_browser(connection: &mut BidiConnection, control: RequestControl<'_>) {
    if connection
        .call("browser.close", json!({}), control)
        .is_err()
    {
        let _ = connection.call("session.end", json!({}), control);
    }
}

pub fn spawn_remote_browser(
    browser: &Browser,
    profile_dir: Option<&Path>,
    automatic_profile: bool,
    port: u16,
    proxy: Option<&str>,
) -> Result<Child> {
    let arguments = match browser.protocol() {
        BrowserProtocol::Cdp => cdp_browser_arguments(profile_dir, port, proxy),
        BrowserProtocol::Bidi => bidi_browser_arguments(profile_dir, port),
    };
    spawn_browser_process(browser, profile_dir, automatic_profile, arguments)
}

pub fn wait_for_spawned_browser_exit(child: &mut Option<Child>, timeout: Duration) -> bool {
    let exited = {
        let Some(child) = child.as_mut() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(100)),
                Ok(None) | Err(_) => break false,
            }
        }
    };
    if exited {
        *child = None;
    }
    exited
}

pub fn terminate_spawned_browser(child: &mut Option<Child>) {
    if let Some(child) = child.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
    *child = None;
}

fn spawn_browser_process(
    browser: &Browser,
    profile_dir: Option<&Path>,
    automatic_profile: bool,
    arguments: Vec<String>,
) -> Result<Child> {
    if let Some(profile_dir) = profile_dir {
        prepare_profile_directory(profile_dir, automatic_profile)?;
    }
    Command::new(&browser.executable)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            SessionError::helper_failure(format!(
                "cannot start browser `{}`: {error}",
                browser.executable.display()
            ))
        })
}

fn prepare_profile_directory(profile_dir: &Path, automatic_profile: bool) -> Result<()> {
    if !automatic_profile {
        return fs::create_dir_all(profile_dir).map_err(|error| {
            SessionError::helper_failure(format!(
                "cannot create browser profile `{}`: {error}",
                profile_dir.display()
            ))
        });
    }
    prepare_automatic_profile_directory(profile_dir)
}

#[cfg(unix)]
fn prepare_automatic_profile_directory(profile_dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(profile_dir).map_err(|error| {
        SessionError::helper_failure(format!(
            "cannot create automatic browser profile `{}`: {error}",
            profile_dir.display()
        ))
    })?;
    let metadata = fs::symlink_metadata(profile_dir).map_err(|error| {
        SessionError::helper_failure(format!(
            "cannot inspect automatic browser profile `{}`: {error}",
            profile_dir.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionError::insecure_profile(format!(
            "automatic browser profile `{}` is not a private directory",
            profile_dir.display()
        )));
    }
    // SAFETY: `geteuid` has no preconditions and does not retain pointers.
    let owner = unsafe { libc::geteuid() };
    if metadata.uid() != owner {
        return Err(SessionError::insecure_profile(format!(
            "automatic browser profile `{}` is owned by another user",
            profile_dir.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(profile_dir, fs::Permissions::from_mode(0o700)).map_err(|error| {
            SessionError::insecure_profile(format!(
                "cannot protect automatic browser profile `{}`: {error}",
                profile_dir.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn prepare_automatic_profile_directory(profile_dir: &Path) -> Result<()> {
    fs::create_dir_all(profile_dir).map_err(|error| {
        SessionError::helper_failure(format!(
            "cannot create automatic browser profile `{}`: {error}",
            profile_dir.display()
        ))
    })
}

fn cdp_browser_arguments(
    profile_dir: Option<&Path>,
    port: u16,
    proxy: Option<&str>,
) -> Vec<String> {
    let mut arguments = vec![
        format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1".to_string(),
    ];
    if let Some(proxy) = proxy {
        arguments.push(format!("--proxy-server={proxy}"));
    }
    if let Some(profile_dir) = profile_dir {
        arguments.push(format!("--user-data-dir={}", profile_dir.display()));
    }
    arguments.push("--no-first-run".to_string());
    arguments.push("--new-window".to_string());
    arguments
}

fn profile_browser_arguments(profile_dir: &Path, url: &str) -> Vec<String> {
    vec![
        "--profile".to_string(),
        profile_dir.display().to_string(),
        "--no-remote".to_string(),
        "--new-window".to_string(),
        url.to_string(),
    ]
}

fn wait_for_profile_browser_exit(
    child: &mut Child,
    timeout: Duration,
    url: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(SessionError::cancelled(
                "browser profile preparation was cancelled",
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(SessionError::helper_failure(format!(
                    "profile browser exited with status {status}"
                )));
            }
            Ok(None) => {}
            Err(error) => {
                return Err(SessionError::helper_failure(format!(
                    "cannot wait for profile browser: {error}"
                )));
            }
        }
        if started.elapsed() >= timeout {
            return Err(SessionError::auth_required(format!(
                "finish signing in to {url}, close the isolated browser, and run profile preparation again"
            )));
        }
        sleep(Duration::from_millis(200));
    }
}

fn bidi_browser_arguments(profile_dir: Option<&Path>, port: u16) -> Vec<String> {
    let mut arguments = vec![format!("--remote-debugging-port={port}")];
    if let Some(profile_dir) = profile_dir {
        arguments.push("--profile".to_string());
        arguments.push(profile_dir.display().to_string());
        arguments.push("--no-remote".to_string());
    }
    arguments.push("--new-window".to_string());
    arguments.push("about:blank".to_string());
    arguments
}

fn resolve_default_browser() -> Result<Browser> {
    let browser = default_browser()?;
    if browser_is_available(&browser) {
        return Ok(browser);
    }
    Err(SessionError::invalid_request(format!(
        "default browser `{}` is not available; set --browser to {BROWSER_CHOICES}",
        browser.executable.display()
    )))
}

#[cfg(target_os = "macos")]
fn default_browser() -> Result<Browser> {
    let app_path = command_stdout(
        "osascript",
        &[
            "-e",
            "use framework \"AppKit\"",
            "-e",
            "set theURL to current application's NSURL's URLWithString:\"https://example.com\"",
            "-e",
            "set appURL to current application's NSWorkspace's sharedWorkspace()'s URLForApplicationToOpenURL:theURL",
            "-e",
            "if appURL is missing value then error \"no default browser\"",
            "-e",
            "return appURL's path() as string",
        ],
    )
    .ok_or_else(|| {
        SessionError::invalid_request("cannot determine the macOS default browser; set --browser")
    })?;
    let app_path = PathBuf::from(app_path);
    let executable = macos_app_executable(&app_path).unwrap_or(app_path);
    supported_default_browser(executable)
}

#[cfg(target_os = "macos")]
fn macos_app_executable(app_path: &Path) -> Option<PathBuf> {
    if app_path
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("app")
    {
        return None;
    }
    let executable = command_stdout(
        "plutil",
        &[
            "-extract",
            "CFBundleExecutable",
            "raw",
            "-o",
            "-",
            app_path.join("Contents/Info.plist").to_str()?,
        ],
    )?;
    Some(app_path.join("Contents/MacOS").join(executable))
}

#[cfg(target_os = "macos")]
fn macos_app_bundle_for_executable(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .find(|ancestor| {
            ancestor
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("app")
        })
        .map(Path::to_path_buf)
}

#[cfg(target_os = "macos")]
fn macos_bundle_identifier(app_path: &Path) -> Option<String> {
    command_stdout(
        "plutil",
        &[
            "-extract",
            "CFBundleIdentifier",
            "raw",
            "-o",
            "-",
            app_path.join("Contents/Info.plist").to_str()?,
        ],
    )
}

#[cfg(target_os = "macos")]
fn macos_app_id_is_running(bundle_id: &str) -> bool {
    let script = format!(
        "application id \"{}\" is running",
        bundle_id.replace('\\', "\\\\").replace('"', "\\\"")
    );
    command_stdout("osascript", &["-e", &script])
        .as_deref()
        .is_some_and(|output| output == "true")
}

#[cfg(target_os = "linux")]
fn default_browser() -> Result<Browser> {
    let desktop_id = command_stdout("xdg-settings", &["get", "default-web-browser"])
        .or_else(|| command_stdout("xdg-mime", &["query", "default", "x-scheme-handler/https"]))
        .ok_or_else(|| {
            SessionError::invalid_request(
                "cannot determine the Linux default browser; set --browser",
            )
        })?;
    let desktop_path = find_desktop_entry(&desktop_id).ok_or_else(|| {
        SessionError::invalid_request(format!(
            "cannot find default browser desktop entry `{desktop_id}`"
        ))
    })?;
    let content = fs::read_to_string(&desktop_path).map_err(|error| {
        SessionError::helper_failure(format!(
            "cannot read default browser desktop entry `{}`: {error}",
            desktop_path.display()
        ))
    })?;
    let exec = desktop_entry_exec(&content).ok_or_else(|| {
        SessionError::helper_failure(format!(
            "default browser desktop entry `{}` has no Exec line",
            desktop_path.display()
        ))
    })?;
    let executable = desktop_exec_command(exec).ok_or_else(|| {
        SessionError::helper_failure(format!(
            "cannot parse Exec line from default browser desktop entry `{}`",
            desktop_path.display()
        ))
    })?;
    supported_default_browser(PathBuf::from(executable))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn default_browser() -> Result<Browser> {
    Err(SessionError::invalid_request(
        "cannot determine the default browser on this platform; set --browser",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command_stdout(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn supported_default_browser(executable: PathBuf) -> Result<Browser> {
    let Some(kind) = supported_browser(&executable) else {
        return Err(SessionError::invalid_request(format!(
            "default browser `{}` is not supported for session capture; set --browser to {BROWSER_CHOICES}",
            executable.display()
        )));
    };
    Ok(browser_path(kind, executable))
}

fn supported_browser(executable: &Path) -> Option<BrowserKind> {
    let path = executable.to_string_lossy().to_ascii_lowercase();
    let file_name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let file = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if path.contains("google chrome")
        || matches!(
            file.as_str(),
            "google-chrome" | "google-chrome-stable" | "chrome"
        )
    {
        Some(BrowserKind::Chrome)
    } else if path.contains("brave browser") || file == "brave-browser" || file == "brave" {
        Some(BrowserKind::Brave)
    } else if path.contains("microsoft edge")
        || matches!(file.as_str(), "microsoft-edge" | "msedge")
    {
        Some(BrowserKind::Edge)
    } else if path.contains("chromium") || matches!(file.as_str(), "chromium" | "chromium-browser")
    {
        Some(BrowserKind::Chromium)
    } else if path.contains("firefox") || file == "firefox" {
        Some(BrowserKind::Firefox)
    } else if matches!(file.as_str(), "zen" | "zen-browser")
        || (file_name.ends_with(".appimage") && file.starts_with("zen-"))
    {
        Some(BrowserKind::Zen)
    } else if path.contains("/dia.app/") || file == "dia" {
        Some(BrowserKind::Dia)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn find_desktop_entry(desktop_id: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(desktop_id);
    if direct.exists() {
        return Some(direct);
    }
    desktop_application_dirs()
        .into_iter()
        .map(|directory| directory.join(desktop_id))
        .find(|path| path.exists())
}

#[cfg(target_os = "linux")]
fn desktop_application_dirs() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(xdg_data_home) = std::env::var_os("XDG_DATA_HOME") {
        directories.push(PathBuf::from(xdg_data_home).join("applications"));
    } else if let Some(home) = std::env::var_os("HOME") {
        directories.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let xdg_data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    directories.extend(
        xdg_data_dirs
            .split(':')
            .filter(|directory| !directory.is_empty())
            .map(|directory| PathBuf::from(directory).join("applications")),
    );
    directories
}

#[cfg(target_os = "linux")]
fn desktop_entry_exec(content: &str) -> Option<&str> {
    let mut in_desktop_entry = false;
    for line in content.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if in_desktop_entry {
            if let Some(exec) = line.strip_prefix("Exec=") {
                return Some(exec.trim());
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn desktop_exec_command(exec: &str) -> Option<String> {
    let tokens = split_desktop_exec(exec);
    let mut index = 0;
    if tokens.get(index).is_some_and(|token| token == "env") {
        index += 1;
        while let Some(token) = tokens.get(index) {
            if token.starts_with('-') || token.contains('=') {
                index += 1;
                continue;
            }
            break;
        }
    }
    tokens
        .get(index)
        .filter(|token| !token.contains('%'))
        .cloned()
}

#[cfg(target_os = "linux")]
fn split_desktop_exec(exec: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in exec.chars() {
        if escaped {
            token.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            _ => token.push(character),
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn browser_candidates() -> Vec<Browser> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "macos")]
    {
        candidates.extend([
            browser(
                BrowserKind::Chrome,
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            ),
            browser(
                BrowserKind::Brave,
                "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            ),
            browser(
                BrowserKind::Edge,
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            ),
            browser(
                BrowserKind::Chromium,
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
            ),
            browser(
                BrowserKind::Firefox,
                "/Applications/Firefox.app/Contents/MacOS/firefox",
            ),
            browser(BrowserKind::Zen, "/Applications/Zen.app/Contents/MacOS/zen"),
            browser(BrowserKind::Dia, DEFAULT_DIA_BROWSER_PATH),
        ]);
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            candidates.extend([
                browser_path(
                    BrowserKind::Chrome,
                    home.join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
                ),
                browser_path(
                    BrowserKind::Brave,
                    home.join("Applications/Brave Browser.app/Contents/MacOS/Brave Browser"),
                ),
                browser_path(
                    BrowserKind::Edge,
                    home.join("Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
                ),
                browser_path(
                    BrowserKind::Chromium,
                    home.join("Applications/Chromium.app/Contents/MacOS/Chromium"),
                ),
                browser_path(
                    BrowserKind::Firefox,
                    home.join("Applications/Firefox.app/Contents/MacOS/firefox"),
                ),
                browser_path(
                    BrowserKind::Zen,
                    home.join("Applications/Zen.app/Contents/MacOS/zen"),
                ),
            ]);
        }
    }
    #[cfg(target_os = "linux")]
    {
        candidates.extend([
            browser(BrowserKind::Chrome, "google-chrome"),
            browser(BrowserKind::Chrome, "google-chrome-stable"),
            browser(BrowserKind::Brave, "brave-browser"),
            browser(BrowserKind::Edge, "microsoft-edge"),
            browser(BrowserKind::Chromium, "chromium"),
            browser(BrowserKind::Chromium, "chromium-browser"),
            browser(BrowserKind::Firefox, "firefox"),
            browser(BrowserKind::Firefox, "firefox-developer-edition"),
            browser(BrowserKind::Zen, "zen"),
            browser(BrowserKind::Zen, "zen-browser"),
        ]);
    }
    #[cfg(target_os = "windows")]
    {
        candidates.extend(windows_browser_candidates());
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        candidates.extend([
            browser(BrowserKind::Chrome, "google-chrome"),
            browser(BrowserKind::Chrome, "chrome"),
            browser(BrowserKind::Brave, "brave"),
            browser(BrowserKind::Edge, "msedge"),
            browser(BrowserKind::Chromium, "chromium"),
            browser(BrowserKind::Firefox, "firefox"),
            browser(BrowserKind::Zen, "zen"),
        ]);
    }
    candidates
}

#[cfg(target_os = "windows")]
fn windows_browser_candidates() -> Vec<Browser> {
    let mut candidates = vec![
        browser(BrowserKind::Chrome, "chrome.exe"),
        browser(BrowserKind::Brave, "brave.exe"),
        browser(BrowserKind::Edge, "msedge.exe"),
        browser(BrowserKind::Chromium, "chromium.exe"),
        browser(BrowserKind::Firefox, "firefox.exe"),
        browser(BrowserKind::Zen, "zen.exe"),
    ];
    for root in windows_program_roots() {
        candidates.extend([
            browser_path(
                BrowserKind::Chrome,
                root.join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            ),
            browser_path(
                BrowserKind::Brave,
                root.join("BraveSoftware")
                    .join("Brave-Browser")
                    .join("Application")
                    .join("brave.exe"),
            ),
            browser_path(
                BrowserKind::Edge,
                root.join("Microsoft")
                    .join("Edge")
                    .join("Application")
                    .join("msedge.exe"),
            ),
            browser_path(
                BrowserKind::Firefox,
                root.join("Mozilla Firefox").join("firefox.exe"),
            ),
            browser_path(BrowserKind::Zen, root.join("Zen Browser").join("zen.exe")),
            browser_path(BrowserKind::Zen, root.join("Zen").join("zen.exe")),
        ]);
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let root = PathBuf::from(local_app_data);
        candidates.extend([
            browser_path(
                BrowserKind::Chrome,
                root.join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            ),
            browser_path(
                BrowserKind::Brave,
                root.join("BraveSoftware")
                    .join("Brave-Browser")
                    .join("Application")
                    .join("brave.exe"),
            ),
            browser_path(
                BrowserKind::Edge,
                root.join("Microsoft")
                    .join("Edge")
                    .join("Application")
                    .join("msedge.exe"),
            ),
            browser_path(
                BrowserKind::Zen,
                root.join("Programs").join("Zen Browser").join("zen.exe"),
            ),
            browser_path(
                BrowserKind::Zen,
                root.join("Programs").join("Zen").join("zen.exe"),
            ),
        ]);
    }
    candidates
}

#[cfg(target_os = "windows")]
fn windows_program_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(value) = std::env::var_os(variable) {
            roots.push(PathBuf::from(value));
        }
    }
    dedup_paths(roots)
}

#[cfg(target_os = "windows")]
fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.iter().any(|known| known == &path) {
            deduped.push(path);
        }
    }
    deduped
}

fn browser(kind: BrowserKind, executable: &str) -> Browser {
    browser_path(kind, PathBuf::from(executable))
}

fn browser_path(kind: BrowserKind, executable: PathBuf) -> Browser {
    Browser { kind, executable }
}

fn browser_is_available(browser: &Browser) -> bool {
    if browser.executable.components().count() > 1 || browser.executable.is_absolute() {
        return browser.executable.exists();
    }
    Command::new(&browser.executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn bidi_websocket_urls(client: &Client, port: u16) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(url) = browser_websocket_url(client, port) {
        urls.push(url);
    }
    urls.push(format!("ws://127.0.0.1:{port}/session"));
    urls.push(format!("ws://127.0.0.1:{port}"));
    dedup_strings(urls)
}

fn browser_websocket_url(client: &Client, port: u16) -> Option<String> {
    client
        .get(format!("{}/json/version", cdp_base_url(port)))
        .send()
        .ok()?
        .json::<CdpVersion>()
        .ok()?
        .browser_websocket_url
}

fn dedup_strings(values: Vec<String>) -> Vec<String> {
    let mut result = Vec::new();
    for value in values {
        if !result.contains(&value) {
            result.push(value);
        }
    }
    result
}

#[derive(Deserialize)]
struct BidiCookie {
    name: String,
    value: BidiBytesValue,
    domain: String,
    #[serde(default = "root_cookie_path")]
    path: String,
    #[serde(default)]
    expiry: Option<f64>,
    #[serde(default)]
    secure: bool,
    #[serde(rename = "httpOnly", default)]
    http_only: bool,
}

impl BidiCookie {
    fn into_browser_cookie(self) -> Result<BrowserCookie> {
        Ok(BrowserCookie {
            name: self.name,
            value: self.value.into_cookie_value()?,
            domain: self.domain,
            path: self.path,
            expires: self.expiry.unwrap_or_default(),
            secure: self.secure,
            http_only: self.http_only,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum BidiBytesValue {
    #[serde(rename = "string")]
    String { value: String },
    #[serde(rename = "base64")]
    Base64 {
        #[serde(rename = "value")]
        _value: String,
    },
}

impl BidiBytesValue {
    fn into_cookie_value(self) -> Result<String> {
        match self {
            Self::String { value } => Ok(value),
            Self::Base64 { .. } => Err(SessionError::helper_failure(
                "WebDriver BiDi returned a base64 cookie value; text cookies are required",
            )),
        }
    }
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                vec![byte as char]
            } else {
                format!("%{byte:02X}").chars().collect()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use std::thread;

    #[cfg(unix)]
    static PROFILE_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[cfg(unix)]
    fn profile_test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "browser-session-profile-test-{}-{}",
            std::process::id(),
            PROFILE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn stalled_websocket() -> (String, thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _socket = tungstenite::accept(stream).unwrap();
            sleep(Duration::from_millis(400));
        });
        (format!("ws://127.0.0.1:{port}/devtools/page/test"), handle)
    }

    #[test]
    fn websocket_call_obeys_deadline() {
        let (url, server) = stalled_websocket();
        let cancelled = AtomicBool::new(false);
        let control = RequestControl::new(Duration::from_millis(100), &cancelled);
        let error = cdp_call(&url, 1, "Network.getAllCookies", control).unwrap_err();
        assert_eq!(error.code, "timeout");
        server.join().unwrap();
    }

    #[test]
    fn websocket_call_observes_cancellation_while_reading() {
        let (url, server) = stalled_websocket();
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let canceller = thread::spawn(move || {
            sleep(Duration::from_millis(50));
            signal.store(true, Ordering::Release);
        });
        let control = RequestControl::new(Duration::from_secs(2), cancelled.as_ref());
        let error = cdp_call(&url, 1, "Network.getAllCookies", control).unwrap_err();
        assert_eq!(error.code, "cancelled");
        canceller.join().unwrap();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires an installed Firefox with headless WebDriver BiDi support"]
    fn firefox_captures_page_callback_results() {
        struct FirefoxProfile {
            child: Option<Child>,
            path: PathBuf,
        }
        impl Drop for FirefoxProfile {
            fn drop(&mut self) {
                terminate_spawned_browser(&mut self.child);
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        let browser = resolve_browser(Some("firefox")).unwrap();
        let mut profile = FirefoxProfile {
            child: None,
            path: profile_test_root(),
        };
        prepare_automatic_profile_directory(&profile.path).unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        profile.child = Some(
            Command::new(browser.executable())
                .arg("--headless")
                .args(bidi_browser_arguments(Some(&profile.path), port))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let cancelled = AtomicBool::new(false);
        let control = RequestControl::new(Duration::from_secs(30), &cancelled);
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let mut connection =
            wait_for_bidi_connection(&client, port, Duration::from_secs(8), control)
                .unwrap()
                .expect("Firefox did not start its WebDriver BiDi endpoint");
        let context = open_bidi_context(&mut connection, "about:blank", control).unwrap();
        let script = r#"// Provider expressions may include comments and a trailing semicolon.
            (() => {
                if (!window.captureCallback) {
                    window.captureCallback = proof => { window.capturedProof = proof; };
                }
                return window.capturedProof || null;
            })(); // Keep the provider's source intact.
        "#;
        assert_eq!(
            evaluate_bidi(&mut connection, &context, script, control).unwrap(),
            Value::Null
        );
        let expected = json!({ "ticket": "fixture-ticket", "randstr": "fixture-random", "sid": "fixture-sid" });
        evaluate_bidi(
            &mut connection,
            &context,
            &format!("window.captureCallback({expected});"),
            control,
        )
        .unwrap();
        assert_eq!(
            evaluate_bidi(&mut connection, &context, script, control).unwrap(),
            expected
        );
        assert_eq!(
            evaluate_bidi(
                &mut connection,
                &context,
                "Promise.resolve({proof: 'fixture-proof'});",
                control,
            )
            .unwrap(),
            json!({ "proof": "fixture-proof" })
        );
    }

    #[test]
    fn uses_automatic_profiles_for_chrome_and_firefox_family() {
        let root = Path::new("/tmp/browser-session");
        assert_eq!(
            effective_profile_dir(None, Some(root), &browser(BrowserKind::Chrome, "chrome")),
            Some(root.join("login-profile"))
        );
        assert_eq!(
            effective_profile_dir(None, Some(root), &browser(BrowserKind::Firefox, "firefox")),
            Some(root.join("login-profile-firefox"))
        );
        assert_eq!(
            effective_profile_dir(None, Some(root), &browser(BrowserKind::Zen, "zen")),
            Some(root.join("login-profile-zen"))
        );
        assert_eq!(
            effective_profile_dir(None, Some(root), &browser(BrowserKind::Brave, "brave")),
            None
        );
    }

    #[test]
    fn explicit_profile_overrides_automatic_profile() {
        let profile = Path::new("/tmp/custom-profile");
        assert_eq!(
            effective_profile_dir(
                Some(profile),
                Some(Path::new("/tmp/browser-session")),
                &browser(BrowserKind::Chrome, "chrome")
            ),
            Some(profile.to_path_buf())
        );
    }

    #[cfg(unix)]
    #[test]
    fn automatic_profiles_are_private_and_repair_wide_permissions() {
        let root = profile_test_root();
        let profile = root.join("login-profile");
        prepare_profile_directory(&profile, true).unwrap();
        assert_eq!(
            fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
            0o700
        );

        fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();
        prepare_profile_directory(&profile, true).unwrap();
        assert_eq!(
            fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_profiles_keep_user_selected_permissions() {
        let root = profile_test_root();
        let profile = root.join("explicit-profile");
        fs::create_dir_all(&profile).unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();
        prepare_profile_directory(&profile, false).unwrap();
        assert_eq!(
            fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn automatic_profiles_reject_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = profile_test_root();
        let target = root.join("target");
        let profile = root.join("login-profile");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &profile).unwrap();
        let error = prepare_profile_directory(&profile, true).unwrap_err();
        assert_eq!(error.code, "insecure-profile");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn waits_for_a_spawned_browser_to_exit_gracefully() {
        let child = Command::new("sh")
            .args(["-c", "sleep 0.05"])
            .spawn()
            .unwrap();
        let mut child = Some(child);
        assert!(wait_for_spawned_browser_exit(
            &mut child,
            Duration::from_secs(1)
        ));
        assert!(child.is_none());
    }

    #[test]
    fn cdp_arguments_include_proxy_and_profile() {
        assert_eq!(
            cdp_browser_arguments(
                Some(Path::new("/tmp/profile")),
                29999,
                Some("http://127.0.0.1:7890")
            ),
            vec![
                "--remote-debugging-port=29999",
                "--remote-debugging-address=127.0.0.1",
                "--proxy-server=http://127.0.0.1:7890",
                "--user-data-dir=/tmp/profile",
                "--no-first-run",
                "--new-window"
            ]
        );
    }

    #[test]
    fn bidi_arguments_include_profile() {
        assert_eq!(
            bidi_browser_arguments(Some(Path::new("/tmp/profile")), 29999),
            vec![
                "--remote-debugging-port=29999",
                "--profile",
                "/tmp/profile",
                "--no-remote",
                "--new-window",
                "about:blank"
            ]
        );
    }

    #[test]
    fn profile_arguments_do_not_enable_remote_control() {
        assert_eq!(
            profile_browser_arguments(Path::new("/tmp/profile"), "https://example.com"),
            vec![
                "--profile",
                "/tmp/profile",
                "--no-remote",
                "--new-window",
                "https://example.com"
            ]
        );
    }

    #[test]
    fn recognizes_browser_paths_and_protocols() {
        assert_eq!(
            supported_browser(Path::new(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
            )),
            Some(BrowserKind::Chrome)
        );
        assert_eq!(
            supported_browser(Path::new("/usr/bin/firefox")),
            Some(BrowserKind::Firefox)
        );
        assert_eq!(
            supported_browser(Path::new("/opt/zen-x86_64.AppImage")),
            Some(BrowserKind::Zen)
        );
        assert_eq!(
            browser(BrowserKind::Firefox, "firefox").protocol(),
            BrowserProtocol::Bidi
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_desktop_exec_command() {
        assert_eq!(
            desktop_exec_command(r#""/opt/google/chrome/google-chrome" %U"#),
            Some("/opt/google/chrome/google-chrome".to_string())
        );
        assert_eq!(
            desktop_exec_command("env FOO=bar brave-browser %U"),
            Some("brave-browser".to_string())
        );
        assert_eq!(desktop_exec_command("%U"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_desktop_entry_exec_from_primary_section() {
        let content = concat!(
            "[Desktop Entry]\n",
            "Name=Browser\n",
            "Exec=chromium-browser %U\n",
            "\n",
            "[Desktop Action NewWindow]\n",
            "Exec=ignored\n"
        );
        assert_eq!(desktop_entry_exec(content), Some("chromium-browser %U"));
    }
}
