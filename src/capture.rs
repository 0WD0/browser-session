// SPDX-License-Identifier: MIT

use crate::error::{Result, SessionError};
use reqwest::blocking::Client;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::{connect, Message};

pub const CAPTURE_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CDP_PORT: u16 = 29317;
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

const DEFAULT_BROWSER_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub url: String,
    pub cookie_names: Vec<String>,
    pub output: PathBuf,
    pub browser: Option<String>,
    pub profile_dir: Option<PathBuf>,
    pub port: u16,
    pub timeout: Duration,
    pub proxy: Option<String>,
    pub script_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureFile {
    pub schema: u32,
    pub source: CaptureSource,
    pub cookies: Vec<CapturedCookie>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureSource {
    pub browser: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapturedCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: f64,
    pub secure: bool,
    #[serde(rename = "httpOnly")]
    pub http_only: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CaptureResult {
    pub browser: String,
    pub output: PathBuf,
    pub url: String,
    pub cookies: Vec<String>,
    #[serde(rename = "page-captured")]
    pub page_captured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoginBrowser {
    name: String,
    executable: PathBuf,
}

#[derive(Debug, Deserialize)]
struct CdpVersion {
    #[serde(rename = "webSocketDebuggerUrl")]
    browser_websocket_url: Option<String>,
    #[serde(rename = "User-Agent")]
    user_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CdpTarget {
    #[serde(default)]
    id: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: String,
}

#[derive(Debug, Deserialize)]
struct CdpCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    #[serde(default)]
    expires: f64,
    #[serde(default)]
    secure: bool,
    #[serde(rename = "httpOnly", default)]
    http_only: bool,
}

pub fn capture(options: &CaptureOptions) -> Result<CaptureResult> {
    validate_options(options)?;
    let browser = resolve_browser(options.browser.as_deref())?;
    reject_unsupported_browser(&browser)?;
    let script = options
        .script_file
        .as_deref()
        .map(fs::read_to_string)
        .transpose()
        .map_err(|error| {
            SessionError::helper_failure(format!(
                "cannot read browser-session script `{}`: {error}",
                options
                    .script_file
                    .as_deref()
                    .unwrap_or_else(|| Path::new("<unknown>"))
                    .display()
            ))
        })?;
    let client = Client::builder()
        .timeout(Duration::from_millis(900))
        .build()
        .map_err(|error| {
            SessionError::helper_failure(format!("cannot create HTTP client: {error}"))
        })?;
    let base_url = cdp_base_url(options.port);
    let mut child = None;
    let version = match cdp_version(&client, &base_url) {
        Ok(version) => version,
        Err(_) => {
            if login_browser_is_running(&browser) {
                return Err(SessionError::browser_restart_required(format!(
                    "browser `{}` is already running without DevTools on 127.0.0.1:{}; close it and run browser-session again",
                    browser.executable.display(),
                    options.port
                )));
            }
            let spawned = spawn_browser(&browser, options)?;
            child = Some(spawned);
            let Some(version) = wait_for_cdp_version(&client, &base_url, Duration::from_secs(8))
            else {
                terminate_child(&mut child);
                return Err(SessionError::network(format!(
                    "cannot reach browser DevTools on 127.0.0.1:{}",
                    options.port
                )));
            };
            version
        }
    };

    let result = (|| {
        let target = open_target(&client, &base_url, &options.url)?;
        let started = Instant::now();
        let mut last_error = None;
        loop {
            if started.elapsed() >= options.timeout {
                return Err(last_error.unwrap_or_else(|| {
                    SessionError::auth_required(
                        "browser session did not expose the requested cookies before timeout",
                    )
                }));
            }
            match capture_once(
                &target.websocket_url,
                &options.url,
                &options.cookie_names,
                script.as_deref(),
                &version,
                &browser.name,
            ) {
                Ok(capture) => {
                    write_private_json(&options.output, &capture)?;
                    return Ok(CaptureResult {
                        browser: browser.name.clone(),
                        output: options.output.clone(),
                        url: options.url.clone(),
                        cookies: options.cookie_names.clone(),
                        page_captured: capture.page.is_some(),
                    });
                }
                Err(error) => last_error = Some(error),
            }
            sleep(Duration::from_millis(700));
        }
    })();

    if child.is_some() {
        close_browser(version.browser_websocket_url.as_deref());
        terminate_child(&mut child);
    }
    result
}

fn validate_options(options: &CaptureOptions) -> Result<()> {
    let url = Url::parse(&options.url)
        .map_err(|error| SessionError::invalid_request(format!("invalid capture URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(SessionError::invalid_request(
            "capture URL must be an http or https URL",
        ));
    }
    if options.cookie_names.is_empty() {
        return Err(SessionError::invalid_request(
            "at least one cookie name is required",
        ));
    }
    if options
        .cookie_names
        .iter()
        .any(|name| name.trim().is_empty() || name.contains(['\r', '\n', '=']))
    {
        return Err(SessionError::invalid_request(
            "cookie names must be nonblank and must not contain header characters",
        ));
    }
    if options.port == 0 {
        return Err(SessionError::invalid_request(
            "capture port must not be zero",
        ));
    }
    if options.timeout.is_zero() {
        return Err(SessionError::invalid_request(
            "capture timeout must be greater than zero",
        ));
    }
    if options
        .proxy
        .as_deref()
        .is_some_and(|proxy| proxy.trim().is_empty())
    {
        return Err(SessionError::invalid_request("proxy URL must not be empty"));
    }
    Ok(())
}

fn resolve_browser(requested: Option<&str>) -> Result<LoginBrowser> {
    if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        if requested.contains('/') || requested.contains('\\') {
            let executable = PathBuf::from(requested);
            if !executable.is_file() {
                return Err(SessionError::invalid_request(format!(
                    "cannot find browser executable `{requested}`"
                )));
            }
            return Ok(LoginBrowser {
                name: browser_name(&executable),
                executable,
            });
        }
        if let Some(executable) = find_executable(requested) {
            return Ok(LoginBrowser {
                name: requested.to_string(),
                executable,
            });
        }
        return Err(SessionError::invalid_request(format!(
            "cannot find browser `{requested}`"
        )));
    }

    for candidate in browser_candidates() {
        if let Some(executable) = find_executable(candidate) {
            return Ok(LoginBrowser {
                name: candidate.to_string(),
                executable,
            });
        }
    }
    Err(SessionError::invalid_request(
        "cannot find a Chromium-compatible browser; set --browser",
    ))
}

fn browser_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &["chrome.exe", "msedge.exe", "brave.exe", "chromium.exe"]
    }
    #[cfg(not(target_os = "windows"))]
    {
        &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "brave-browser",
            "microsoft-edge",
        ]
    }
}

fn browser_name(executable: &Path) -> String {
    executable
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("browser")
        .to_string()
}

fn find_executable(program: &str) -> Option<PathBuf> {
    let status = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    status.success().then(|| PathBuf::from(program))
}

fn reject_unsupported_browser(browser: &LoginBrowser) -> Result<()> {
    let name = browser.name.to_ascii_lowercase();
    if name.contains("firefox") || name == "zen" || name == "zen-browser" {
        return Err(SessionError::invalid_request(
            "this browser-session build supports Chromium CDP; Firefox-family BiDi is not implemented yet",
        ));
    }
    Ok(())
}

fn cdp_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn cdp_version(client: &Client, base_url: &str) -> Result<CdpVersion> {
    client
        .get(format!("{base_url}/json/version"))
        .send()
        .map_err(|error| SessionError::network(format!("cannot connect to DevTools: {error}")))?
        .error_for_status()
        .map_err(|error| {
            SessionError::helper_failure(format!("DevTools rejected version request: {error}"))
        })?
        .json()
        .map_err(|error| {
            SessionError::helper_failure(format!("invalid DevTools version response: {error}"))
        })
}

fn wait_for_cdp_version(client: &Client, base_url: &str, timeout: Duration) -> Option<CdpVersion> {
    let started = Instant::now();
    loop {
        if let Ok(version) = cdp_version(client, base_url) {
            return Some(version);
        }
        if started.elapsed() >= timeout {
            return None;
        }
        sleep(Duration::from_millis(150));
    }
}

fn spawn_browser(browser: &LoginBrowser, options: &CaptureOptions) -> Result<Child> {
    if let Some(profile_dir) = options.profile_dir.as_deref() {
        fs::create_dir_all(profile_dir).map_err(|error| {
            SessionError::helper_failure(format!(
                "cannot create browser profile `{}`: {error}",
                profile_dir.display()
            ))
        })?;
    }
    let mut arguments = vec![
        format!("--remote-debugging-port={}", options.port),
        "--remote-debugging-address=127.0.0.1".to_string(),
        "--no-first-run".to_string(),
        "--new-window".to_string(),
        "about:blank".to_string(),
    ];
    if let Some(proxy) = options.proxy.as_deref() {
        arguments.insert(2, format!("--proxy-server={proxy}"));
    }
    if let Some(profile_dir) = options.profile_dir.as_deref() {
        arguments.insert(2, format!("--user-data-dir={}", profile_dir.display()));
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

fn login_browser_is_running(browser: &LoginBrowser) -> bool {
    let Some(process_name) = browser
        .executable
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    #[cfg(unix)]
    {
        Command::new("pgrep")
            .args(["-x", process_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        let process_name = if process_name.ends_with(".exe") {
            process_name.to_string()
        } else {
            format!("{process_name}.exe")
        };
        Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {process_name}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|output| {
                String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line.to_ascii_lowercase()
                        .contains(&process_name.to_ascii_lowercase())
                })
            })
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn open_target(client: &Client, base_url: &str, url: &str) -> Result<CdpTarget> {
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
            .send();
    }
    Ok(target)
}

fn capture_once(
    websocket_url: &str,
    url: &str,
    requested_names: &[String],
    script: Option<&str>,
    version: &CdpVersion,
    browser_name: &str,
) -> Result<CaptureFile> {
    let cookies = cdp_cookies(websocket_url)?;
    let cookies = select_cookies(cookies, url, requested_names)?;
    let page = script
        .map(|expression| evaluate_script(websocket_url, expression))
        .transpose()?;
    Ok(CaptureFile {
        schema: CAPTURE_SCHEMA_VERSION,
        source: CaptureSource {
            browser: browser_name.to_string(),
            url: url.to_string(),
            user_agent: Some(
                version
                    .user_agent
                    .clone()
                    .unwrap_or_else(|| DEFAULT_BROWSER_USER_AGENT.to_string()),
            ),
        },
        cookies,
        page,
    })
}

fn cdp_cookies(websocket_url: &str) -> Result<Vec<CdpCookie>> {
    let response = cdp_call(websocket_url, 1, "Network.getAllCookies")?;
    let cookies = response
        .pointer("/result/cookies")
        .cloned()
        .ok_or_else(|| SessionError::helper_failure("DevTools cookie response was incomplete"))?;
    serde_json::from_value(cookies).map_err(|error| {
        SessionError::helper_failure(format!("invalid DevTools cookie response: {error}"))
    })
}

fn select_cookies(
    cookies: Vec<CdpCookie>,
    page_url: &str,
    requested_names: &[String],
) -> Result<Vec<CapturedCookie>> {
    let url = Url::parse(page_url)
        .map_err(|error| SessionError::invalid_request(format!("invalid capture URL: {error}")))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| SessionError::helper_failure(format!("system clock error: {error}")))?
        .as_secs_f64();
    let requested = requested_names.iter().collect::<BTreeSet<_>>();
    let mut found = BTreeSet::new();
    let mut selected = Vec::new();
    for cookie in cookies {
        if !requested.contains(&cookie.name)
            || !cookie_domain_matches(cookie.domain.as_str(), url.host_str().unwrap_or_default())
            || !cookie_path_matches(cookie.path.as_str(), url.path())
            || (cookie.expires > 0.0 && cookie.expires <= now)
        {
            continue;
        }
        found.insert(cookie.name.clone());
        selected.push(CapturedCookie {
            name: cookie.name,
            value: cookie.value,
            domain: cookie.domain,
            path: cookie.path,
            expires: cookie.expires,
            secure: cookie.secure,
            http_only: cookie.http_only,
        });
    }
    let missing = requested_names
        .iter()
        .filter(|name| !found.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(SessionError::auth_required(format!(
            "browser session is missing requested cookie(s): {}",
            missing.join(", ")
        )));
    }
    Ok(selected)
}

fn cookie_domain_matches(cookie_domain: &str, host: &str) -> bool {
    let domain = cookie_domain.trim_start_matches('.');
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn cookie_path_matches(cookie_path: &str, request_path: &str) -> bool {
    cookie_path == "/" || request_path.starts_with(cookie_path)
}

fn evaluate_script(websocket_url: &str, expression: &str) -> Result<Value> {
    let response = cdp_call_with_params(
        websocket_url,
        2,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true
        }),
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

fn cdp_call(websocket_url: &str, id: u64, method: &str) -> Result<Value> {
    cdp_call_with_params(websocket_url, id, method, Value::Null)
}

fn cdp_call_with_params(
    websocket_url: &str,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let (mut socket, _) = connect(websocket_url).map_err(|error| {
        SessionError::network(format!("cannot connect to DevTools websocket: {error}"))
    })?;
    let mut payload = json!({"id": id, "method": method});
    if !params.is_null() {
        payload
            .as_object_mut()
            .expect("CDP payload is an object")
            .insert("params".to_string(), params);
    }
    socket
        .send(Message::Text(payload.to_string()))
        .map_err(|error| SessionError::network(format!("cannot send DevTools request: {error}")))?;
    loop {
        let message = socket.read().map_err(|error| {
            SessionError::network(format!("cannot read DevTools response: {error}"))
        })?;
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

fn close_browser(websocket_url: Option<&str>) {
    if let Some(websocket_url) = websocket_url {
        let _ = cdp_call(websocket_url, 99, "Browser.close");
    }
}

fn terminate_child(child: &mut Option<Child>) {
    if let Some(child) = child.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
    *child = None;
}

fn write_private_json(path: &Path, capture: &CaptureFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            SessionError::helper_failure(format!(
                "cannot create capture directory `{}`: {error}",
                parent.display()
            ))
        })?;
    }
    let content = serde_json::to_vec_pretty(capture).map_err(|error| {
        SessionError::helper_failure(format!("cannot encode capture file: {error}"))
    })?;
    let temporary = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("capture"),
        std::process::id()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| {
                SessionError::helper_failure(format!(
                    "cannot create capture file `{}`: {error}",
                    temporary.display()
                ))
            })?;
        file.write_all(&content).map_err(|error| {
            SessionError::helper_failure(format!(
                "cannot write capture file `{}`: {error}",
                temporary.display()
            ))
        })?;
    }
    #[cfg(not(unix))]
    fs::write(&temporary, content).map_err(|error| {
        SessionError::helper_failure(format!(
            "cannot write capture file `{}`: {error}",
            temporary.display()
        ))
    })?;
    set_private_permissions(&temporary)?;
    fs::rename(&temporary, path).map_err(|error| {
        SessionError::helper_failure(format!(
            "cannot install capture file `{}`: {error}",
            path.display()
        ))
    })
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        SessionError::helper_failure(format!(
            "cannot protect capture file `{}`: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn percent_encodes_target_urls() {
        assert_eq!(
            percent_encode("https://x.com/a b?c=d"),
            "https%3A%2F%2Fx.com%2Fa%20b%3Fc%3Dd"
        );
    }

    #[test]
    fn matches_parent_domains_without_matching_siblings() {
        assert!(cookie_domain_matches(".x.com", "x.com"));
        assert!(cookie_domain_matches(".x.com", "mobile.x.com"));
        assert!(!cookie_domain_matches(".x.com", "notx.com"));
    }

    #[test]
    fn filters_requested_live_cookies_by_page_domain() {
        let cookies = vec![
            CdpCookie {
                name: "auth_token".to_string(),
                value: format!("test-{}", std::process::id()),
                domain: ".x.com".to_string(),
                path: "/".to_string(),
                expires: 0.0,
                secure: true,
                http_only: true,
            },
            CdpCookie {
                name: "ct0".to_string(),
                value: format!("csrf-{}", std::process::id()),
                domain: ".example.com".to_string(),
                path: "/".to_string(),
                expires: 0.0,
                secure: true,
                http_only: false,
            },
        ];
        let result =
            select_cookies(cookies, "https://x.com/home", &["auth_token".to_string()]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "auth_token");
    }

    #[test]
    fn rejects_missing_requested_cookie() {
        let error = select_cookies(Vec::new(), "https://x.com", &["ct0".to_string()]).unwrap_err();
        assert_eq!(error.code, "auth-required");
        assert!(error.message.contains("ct0"));
    }

    #[cfg(unix)]
    #[test]
    fn writes_private_capture_files() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "browser-session-test-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("capture.json");
        let capture = CaptureFile {
            schema: CAPTURE_SCHEMA_VERSION,
            source: CaptureSource {
                browser: "chrome".to_string(),
                url: "https://x.com".to_string(),
                user_agent: None,
            },
            cookies: vec![CapturedCookie {
                name: "ct0".to_string(),
                value: format!("test-{}", std::process::id()),
                domain: ".x.com".to_string(),
                path: "/".to_string(),
                expires: 0.0,
                secure: true,
                http_only: false,
            }],
            page: None,
        };
        write_private_json(&path, &capture).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
