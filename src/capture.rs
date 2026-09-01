// SPDX-License-Identifier: MIT

use crate::browser::{
    bidi_cookies, browser_is_running, cdp_base_url, cdp_cookies, cdp_version, close_bidi_browser,
    close_cdp_browser, connect_bidi, effective_profile_dir, evaluate_bidi, evaluate_cdp,
    open_bidi_context, open_cdp_target, prepare_profile_with_cancel, resolve_browser,
    restart_running_browser, running_browser_conflict, spawn_remote_browser,
    terminate_spawned_browser, wait_for_bidi_connection, wait_for_cdp_version,
    wait_for_spawned_browser_exit, Browser, BrowserCookie, BrowserProtocol, CdpVersion,
    PreparedProfile, RequestControl,
};
use crate::error::{Result, SessionError};
use reqwest::blocking::Client;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::{Debug, Formatter};
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CAPTURE_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CDP_PORT: u16 = 0;
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

const DEFAULT_BROWSER_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Clone)]
pub struct CaptureOptions {
    pub url: String,
    pub cookie_names: Vec<String>,
    pub any_cookie_names: Vec<String>,
    pub all_origin_cookies: bool,
    pub output: PathBuf,
    pub browser: Option<String>,
    pub profile_dir: Option<PathBuf>,
    pub profile_root: Option<PathBuf>,
    pub port: u16,
    pub timeout: Duration,
    pub proxy: Option<String>,
    pub script: Option<String>,
    pub restart_running: bool,
    pub attach_existing: bool,
}

#[derive(Debug, Clone)]
pub struct PrepareProfileOptions {
    pub url: String,
    pub browser: Option<String>,
    pub profile_dir: Option<PathBuf>,
    pub profile_root: Option<PathBuf>,
    pub timeout: Duration,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Clone, Serialize, Deserialize, PartialEq)]
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

impl Debug for CaptureOptions {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureOptions")
            .field("url", &self.url)
            .field("cookie_names", &self.cookie_names)
            .field("any_cookie_names", &self.any_cookie_names)
            .field("all_origin_cookies", &self.all_origin_cookies)
            .field("output", &self.output)
            .field("browser", &self.browser)
            .field("profile_dir", &self.profile_dir)
            .field("profile_root", &self.profile_root)
            .field("port", &self.port)
            .field("timeout", &self.timeout)
            .field("proxy", &self.proxy)
            .field("script", &self.script.as_ref().map(|_| "<redacted>"))
            .field("restart_running", &self.restart_running)
            .field("attach_existing", &self.attach_existing)
            .finish()
    }
}

impl Debug for CaptureFile {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureFile")
            .field("schema", &self.schema)
            .field("source", &self.source)
            .field("cookies", &self.cookies)
            .field("page", &self.page.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Debug for CapturedCookie {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CapturedCookie")
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CaptureResult {
    pub browser: String,
    pub output: PathBuf,
    pub url: String,
    pub cookies: Vec<String>,
    #[serde(rename = "captured-cookie-count")]
    pub captured_cookie_count: usize,
    #[serde(rename = "page-captured")]
    pub page_captured: bool,
    #[serde(rename = "profile-dir", skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<PathBuf>,
}

pub fn capture(options: &CaptureOptions) -> Result<CaptureResult> {
    let cancelled = AtomicBool::new(false);
    capture_with_cancel(options, &cancelled)
}

pub fn capture_with_cancel(
    options: &CaptureOptions,
    cancelled: &AtomicBool,
) -> Result<CaptureResult> {
    validate_capture_options(options)?;
    let browser = resolve_browser(options.browser.as_deref())?;
    let profile_dir = effective_profile_dir(
        options.profile_dir.as_deref(),
        options.profile_root.as_deref(),
        &browser,
    );
    let script = options.script.as_deref();
    let port = control_port(options.port, options.attach_existing)?;
    let control = RequestControl::new(options.timeout, cancelled);
    let client = new_client()?;
    let capture = match browser.protocol() {
        BrowserProtocol::Cdp => capture_cdp(
            &client,
            options,
            port,
            &browser,
            profile_dir.as_deref(),
            script,
            control,
        )?,
        BrowserProtocol::Bidi => capture_bidi(
            &client,
            options,
            port,
            &browser,
            profile_dir.as_deref(),
            script,
            control,
        )?,
    };
    write_private_json(&options.output, &capture)?;
    Ok(CaptureResult {
        browser: capture.source.browser.clone(),
        output: options.output.clone(),
        url: options.url.clone(),
        cookies: captured_cookie_names(options),
        captured_cookie_count: capture.cookies.len(),
        page_captured: capture.page.is_some(),
        profile_dir,
    })
}

pub fn prepare_login_profile(options: &PrepareProfileOptions) -> Result<PreparedProfile> {
    let cancelled = AtomicBool::new(false);
    prepare_login_profile_with_cancel(options, &cancelled)
}

pub fn prepare_login_profile_with_cancel(
    options: &PrepareProfileOptions,
    cancelled: &AtomicBool,
) -> Result<PreparedProfile> {
    validate_url(&options.url)?;
    if options.profile_dir.is_some() && options.profile_root.is_some() {
        return Err(SessionError::invalid_request(
            "profile-dir and profile-root cannot be used together",
        ));
    }
    if options.timeout.is_zero() {
        return Err(SessionError::invalid_request(
            "profile preparation timeout must be greater than zero",
        ));
    }
    let browser = resolve_browser(options.browser.as_deref())?;
    prepare_profile_with_cancel(
        &options.url,
        &browser,
        options.profile_dir.as_deref(),
        options.profile_root.as_deref(),
        options.timeout,
        cancelled,
    )
}

fn validate_capture_options(options: &CaptureOptions) -> Result<()> {
    validate_url(&options.url)?;
    if options.cookie_names.is_empty()
        && options.any_cookie_names.is_empty()
        && options.script.is_none()
    {
        return Err(SessionError::invalid_request(
            "at least one cookie requirement or page script is required",
        ));
    }
    if options
        .script
        .as_deref()
        .is_some_and(|script| script.trim().is_empty())
    {
        return Err(SessionError::invalid_request(
            "page script must not be blank",
        ));
    }
    validate_cookie_names(&options.cookie_names)?;
    validate_cookie_names(&options.any_cookie_names)?;
    if options.profile_dir.is_some() && options.profile_root.is_some() {
        return Err(SessionError::invalid_request(
            "profile-dir and profile-root cannot be used together",
        ));
    }
    if options.attach_existing && options.port == 0 {
        return Err(SessionError::invalid_request(
            "attach-existing requires an explicit nonzero capture port",
        ));
    }
    if options.attach_existing
        && (options.profile_dir.is_some()
            || options.profile_root.is_some()
            || options.proxy.is_some()
            || options.restart_running)
    {
        return Err(SessionError::invalid_request(
            "attach-existing cannot be combined with profile, proxy, or restart options",
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

fn validate_url(url: &str) -> Result<Url> {
    let url = Url::parse(url)
        .map_err(|error| SessionError::invalid_request(format!("invalid capture URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(SessionError::invalid_request(
            "capture URL must be an http or https URL",
        ));
    }
    Ok(url)
}

fn validate_cookie_names(names: &[String]) -> Result<()> {
    if names
        .iter()
        .any(|name| name.trim().is_empty() || name.contains(['\r', '\n', '=']))
    {
        return Err(SessionError::invalid_request(
            "cookie names must be nonblank and must not contain header characters",
        ));
    }
    Ok(())
}

fn new_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_millis(900))
        .build()
        .map_err(|error| {
            SessionError::helper_failure(format!("cannot create HTTP client: {error}"))
        })
}

fn control_port(requested: u16, attach_existing: bool) -> Result<u16> {
    if attach_existing {
        return Ok(requested);
    }
    let listener = TcpListener::bind(("127.0.0.1", requested)).map_err(|error| {
        SessionError::endpoint_in_use(format!(
            "cannot reserve browser control port {requested}: {error}"
        ))
    })?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| {
            SessionError::helper_failure(format!("cannot inspect reserved browser port: {error}"))
        })
}

fn capture_cdp(
    client: &Client,
    options: &CaptureOptions,
    port: u16,
    browser: &Browser,
    profile_dir: Option<&Path>,
    script: Option<&str>,
    control: RequestControl<'_>,
) -> Result<CaptureFile> {
    let base_url = cdp_base_url(port);
    let mut child = None;
    let mut spawned = false;
    let version = if options.attach_existing {
        control.check()?;
        cdp_version(client, &base_url)?
    } else {
        prepare_remote_browser(browser, options, port, profile_dir)?;
        child = Some(spawn_remote_browser(
            browser,
            profile_dir,
            options.profile_dir.is_none()
                && options.profile_root.is_some()
                && profile_dir.is_some(),
            port,
            options.proxy.as_deref(),
        )?);
        spawned = true;
        let Some(version) =
            wait_for_cdp_version(client, &base_url, Duration::from_secs(8), control)?
        else {
            if should_close_spawned_browser(spawned, profile_dir) {
                terminate_spawned_browser(&mut child);
            }
            return Err(SessionError::network(format!(
                "cannot reach browser DevTools on 127.0.0.1:{port}"
            )));
        };
        version
    };
    let source_browser = if options.attach_existing {
        version.browser_name().unwrap_or("attached-cdp")
    } else {
        browser.name()
    };
    let result = (|| {
        let target = open_cdp_target(client, &base_url, &options.url)?;
        wait_for_capture(control, || {
            capture_cdp_once(
                options,
                source_browser,
                &version,
                target.websocket_url(),
                script,
                control,
            )
        })
    })();
    if should_close_spawned_browser(spawned, profile_dir) {
        close_cdp_browser(
            version.browser_websocket_url(),
            RequestControl::cleanup(Duration::from_secs(2)),
        );
        if !wait_for_spawned_browser_exit(&mut child, Duration::from_secs(5)) {
            terminate_spawned_browser(&mut child);
        }
    }
    result
}

fn capture_bidi(
    client: &Client,
    options: &CaptureOptions,
    port: u16,
    browser: &Browser,
    profile_dir: Option<&Path>,
    script: Option<&str>,
    control: RequestControl<'_>,
) -> Result<CaptureFile> {
    let mut child = None;
    let mut spawned = false;
    let mut connection = if options.attach_existing {
        connect_bidi(client, port, control)?
    } else {
        prepare_remote_browser(browser, options, port, profile_dir)?;
        child = Some(spawn_remote_browser(
            browser,
            profile_dir,
            options.profile_dir.is_none()
                && options.profile_root.is_some()
                && profile_dir.is_some(),
            port,
            options.proxy.as_deref(),
        )?);
        spawned = true;
        let Some(connection) =
            wait_for_bidi_connection(client, port, Duration::from_secs(8), control)?
        else {
            if should_close_spawned_browser(spawned, profile_dir) {
                terminate_spawned_browser(&mut child);
            }
            return Err(SessionError::network(format!(
                "cannot reach browser WebDriver BiDi endpoint on 127.0.0.1:{port}"
            )));
        };
        connection
    };
    let source_browser = if options.attach_existing {
        connection
            .browser_name()
            .unwrap_or("attached-bidi")
            .to_string()
    } else {
        browser.name().to_string()
    };
    let result = (|| {
        let context = open_bidi_context(&mut connection, &options.url, control)?;
        wait_for_capture(control, || {
            capture_bidi_once(
                options,
                &source_browser,
                &mut connection,
                &context,
                script,
                control,
            )
        })
    })();
    if should_close_spawned_browser(spawned, profile_dir) {
        close_bidi_browser(
            &mut connection,
            RequestControl::cleanup(Duration::from_secs(2)),
        );
        if !wait_for_spawned_browser_exit(&mut child, Duration::from_secs(5)) {
            terminate_spawned_browser(&mut child);
        }
    }
    result
}

fn should_close_spawned_browser(spawned: bool, profile_dir: Option<&Path>) -> bool {
    spawned && profile_dir.is_some()
}

fn prepare_remote_browser(
    browser: &Browser,
    options: &CaptureOptions,
    port: u16,
    profile_dir: Option<&Path>,
) -> Result<()> {
    if browser_is_running(browser) {
        if options.restart_running {
            restart_running_browser(browser)?;
        } else if let Some(error) = running_browser_conflict(browser, port, profile_dir.is_some()) {
            return Err(error);
        }
    }
    Ok(())
}

fn wait_for_capture(
    control: RequestControl<'_>,
    mut capture_once: impl FnMut() -> Result<CaptureFile>,
) -> Result<CaptureFile> {
    loop {
        control.check()?;
        match capture_once() {
            Ok(capture) => return Ok(capture),
            Err(error) if matches!(error.code, "cancelled" | "timeout") => return Err(error),
            Err(_) => {}
        }
        sleep(Duration::from_millis(200));
    }
}

fn capture_cdp_once(
    options: &CaptureOptions,
    browser_name: &str,
    version: &CdpVersion,
    websocket_url: &str,
    script: Option<&str>,
    control: RequestControl<'_>,
) -> Result<CaptureFile> {
    let cookies = select_cookies(
        cdp_cookies(websocket_url, control)?,
        &options.url,
        &options.cookie_names,
        &options.any_cookie_names,
        options.all_origin_cookies,
    )?;
    let page = script
        .map(|expression| evaluate_cdp(websocket_url, expression, control))
        .transpose()?;
    capture_file(
        browser_name,
        &options.url,
        version.user_agent(),
        cookies,
        page,
    )
}

fn capture_bidi_once(
    options: &CaptureOptions,
    browser_name: &str,
    connection: &mut crate::browser::BidiConnection,
    context: &str,
    script: Option<&str>,
    control: RequestControl<'_>,
) -> Result<CaptureFile> {
    let cookies = select_cookies(
        bidi_cookies(connection, context, control)?,
        &options.url,
        &options.cookie_names,
        &options.any_cookie_names,
        options.all_origin_cookies,
    )?;
    let page = script
        .map(|expression| evaluate_bidi(connection, context, expression, control))
        .transpose()?;
    capture_file(
        browser_name,
        &options.url,
        connection.user_agent(),
        cookies,
        page,
    )
}

fn capture_file(
    browser_name: &str,
    url: &str,
    user_agent: Option<&str>,
    cookies: Vec<CapturedCookie>,
    page: Option<Value>,
) -> Result<CaptureFile> {
    if page.as_ref().is_some_and(Value::is_null) {
        return Err(SessionError::auth_required(
            "browser-session page script is not ready",
        ));
    }
    Ok(CaptureFile {
        schema: CAPTURE_SCHEMA_VERSION,
        source: CaptureSource {
            browser: browser_name.to_string(),
            url: url.to_string(),
            user_agent: Some(user_agent.unwrap_or(DEFAULT_BROWSER_USER_AGENT).to_string()),
        },
        cookies,
        page,
    })
}

fn select_cookies(
    cookies: Vec<BrowserCookie>,
    page_url: &str,
    required_names: &[String],
    any_names: &[String],
    all_origin_cookies: bool,
) -> Result<Vec<CapturedCookie>> {
    let url = validate_url(page_url)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| SessionError::helper_failure(format!("system clock error: {error}")))?
        .as_secs_f64();
    let required = required_names.iter().collect::<BTreeSet<_>>();
    let alternatives = any_names.iter().collect::<BTreeSet<_>>();
    let requested = required
        .union(&alternatives)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut found_required = BTreeSet::new();
    let mut found_alternative = false;
    let mut selected = Vec::new();
    for cookie in cookies {
        let origin_cookie = cookie_matches_origin(&cookie, &url, now);
        let request_cookie = origin_cookie && cookie_path_matches(cookie.path.as_str(), url.path());
        let requested_cookie = requested.contains(&cookie.name) && request_cookie;
        if !((all_origin_cookies && origin_cookie) || requested_cookie) {
            continue;
        }
        if required.contains(&cookie.name) && request_cookie {
            found_required.insert(cookie.name.clone());
        }
        if alternatives.contains(&cookie.name) && request_cookie {
            found_alternative = true;
        }
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
    let missing = required_names
        .iter()
        .filter(|name| !found_required.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(SessionError::auth_required(format!(
            "browser session is missing required cookie(s): {}",
            missing.join(", ")
        )));
    }
    if !any_names.is_empty() && !found_alternative {
        return Err(SessionError::auth_required(format!(
            "browser session is missing every alternative cookie: {}",
            any_names.join(", ")
        )));
    }
    Ok(selected)
}

fn cookie_matches_origin(cookie: &BrowserCookie, url: &Url, now: f64) -> bool {
    cookie_domain_matches(cookie.domain.as_str(), url.host_str().unwrap_or_default())
        && !(cookie.expires > 0.0 && cookie.expires <= now)
        && (!cookie.secure || url.scheme() == "https")
}

fn cookie_domain_matches(cookie_domain: &str, host: &str) -> bool {
    let domain = cookie_domain.trim_start_matches('.');
    if domain.is_empty() {
        return false;
    }
    if host.eq_ignore_ascii_case(domain) {
        return true;
    }
    let host = host.as_bytes();
    let domain = domain.as_bytes();
    host.len() > domain.len()
        && host[host.len() - domain.len() - 1] == b'.'
        && host[host.len() - domain.len()..].eq_ignore_ascii_case(domain)
}

fn cookie_path_matches(cookie_path: &str, request_path: &str) -> bool {
    if cookie_path.is_empty() || cookie_path == "/" || request_path == cookie_path {
        return true;
    }
    request_path
        .strip_prefix(cookie_path)
        .is_some_and(|suffix| cookie_path.ends_with('/') || suffix.starts_with('/'))
}

fn captured_cookie_names(options: &CaptureOptions) -> Vec<String> {
    let mut names = options
        .cookie_names
        .iter()
        .chain(&options.any_cookie_names)
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
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
    let temporary = temporary_path(path);
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

fn temporary_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_file_name(format!(
        ".{}.{}.{stamp}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("capture"),
        std::process::id()
    ))
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
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn cookie(name: &str, domain: &str) -> BrowserCookie {
        BrowserCookie {
            name: name.to_string(),
            value: format!("test-{}", std::process::id()),
            domain: domain.to_string(),
            path: "/".to_string(),
            expires: 0.0,
            secure: true,
            http_only: true,
        }
    }

    fn script_capture_options() -> CaptureOptions {
        CaptureOptions {
            url: "https://example.com/challenge".to_string(),
            cookie_names: Vec::new(),
            any_cookie_names: Vec::new(),
            all_origin_cookies: false,
            output: PathBuf::from("/tmp/browser-session-page.json"),
            browser: Some("chrome".to_string()),
            profile_dir: Some(PathBuf::from("/tmp/browser-session-page-profile")),
            profile_root: None,
            port: DEFAULT_CDP_PORT,
            timeout: Duration::from_secs(1),
            proxy: None,
            script: Some("(() => null)()".to_string()),
            restart_running: false,
            attach_existing: false,
        }
    }

    #[test]
    fn secret_bearing_debug_output_is_redacted() {
        let secret = format!("secret-{}", std::process::id());
        let browser_cookie = BrowserCookie {
            name: "sid".to_string(),
            value: secret.clone(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            expires: 0.0,
            secure: true,
            http_only: true,
        };
        let captured_cookie = CapturedCookie {
            name: "sid".to_string(),
            value: secret.clone(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            expires: 0.0,
            secure: true,
            http_only: true,
        };
        let capture = CaptureFile {
            schema: CAPTURE_SCHEMA_VERSION,
            source: CaptureSource {
                browser: "chrome".to_string(),
                url: "https://example.com".to_string(),
                user_agent: None,
            },
            cookies: vec![captured_cookie],
            page: Some(serde_json::json!({ "session": secret.clone() })),
        };
        let mut options = script_capture_options();
        options.script = Some(secret.clone());

        for rendered in [
            format!("{browser_cookie:?}"),
            format!("{capture:?}"),
            format!("{options:?}"),
        ] {
            assert!(
                !rendered.contains(&secret),
                "secret-bearing debug output was not redacted"
            );
            assert!(rendered.contains("<redacted>"));
        }
    }

    #[test]
    fn matches_parent_domains_without_matching_siblings() {
        assert!(cookie_domain_matches(".x.com", "x.com"));
        assert!(cookie_domain_matches(".x.com", "mobile.x.com"));
        assert!(!cookie_domain_matches(".x.com", "notx.com"));
    }

    #[test]
    fn matches_cookie_domains_case_insensitively() {
        assert!(cookie_domain_matches(".EXAMPLE.COM", "sub.example.com"));
    }

    #[test]
    fn applies_cookie_path_boundary_rules() {
        assert!(!cookie_path_matches("/foo", "/foobar"));
        assert!(cookie_path_matches("/foo", "/foo"));
        assert!(cookie_path_matches("/foo", "/foo/bar"));
        assert!(cookie_path_matches("/foo/", "/foo/bar"));
    }

    #[test]
    fn secure_cookies_require_https() {
        let secure_cookie = cookie("sid", "example.com");
        let error = select_cookies(
            vec![secure_cookie.clone()],
            "http://example.com/",
            &["sid".to_string()],
            &[],
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, "auth-required");

        let selected = select_cookies(
            vec![secure_cookie],
            "https://example.com/",
            &["sid".to_string()],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn all_origin_cookies_still_respect_secure_scheme() {
        let selected = select_cookies(
            vec![cookie("sid", "example.com")],
            "http://example.com/",
            &[],
            &[],
            true,
        )
        .unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn requires_every_required_cookie() {
        let error = select_cookies(
            vec![cookie("auth_token", ".x.com")],
            "https://x.com/home",
            &["auth_token".to_string(), "ct0".to_string()],
            &[],
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, "auth-required");
        assert!(error.message.contains("ct0"));
    }

    #[test]
    fn accepts_one_alternative_cookie() {
        let selected = select_cookies(
            vec![cookie("SAPISID", ".youtube.com")],
            "https://music.youtube.com",
            &[],
            &["__Secure-3PAPISID".to_string(), "SAPISID".to_string()],
            false,
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "SAPISID");
    }

    #[test]
    fn all_origin_cookies_include_path_scoped_origin_cookies() {
        let mut account_cookie = cookie("ACCOUNT", ".youtube.com");
        account_cookie.path = "/account".to_string();
        let selected = select_cookies(
            vec![
                cookie("SAPISID", ".youtube.com"),
                cookie("PREF", ".youtube.com"),
                account_cookie,
                cookie("foreign", ".example.com"),
            ],
            "https://music.youtube.com",
            &[],
            &["SAPISID".to_string()],
            true,
        )
        .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|cookie| cookie.name.as_str())
                .collect::<Vec<_>>(),
            vec!["SAPISID", "PREF", "ACCOUNT"]
        );
    }

    #[test]
    fn closes_only_spawned_isolated_browsers() {
        assert!(!should_close_spawned_browser(
            false,
            Some(Path::new("/tmp/profile"))
        ));
        assert!(!should_close_spawned_browser(true, None));
        assert!(should_close_spawned_browser(
            true,
            Some(Path::new("/tmp/profile"))
        ));
    }

    #[test]
    fn allocates_dynamic_ports_and_rejects_occupied_fixed_ports() {
        let dynamic = control_port(0, false).unwrap();
        assert_ne!(dynamic, 0);

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let occupied = listener.local_addr().unwrap().port();
        let error = control_port(occupied, false).unwrap_err();
        assert_eq!(error.code, "endpoint-in-use");
        assert_eq!(control_port(occupied, true).unwrap(), occupied);
    }

    #[test]
    fn page_script_is_a_capture_completion_requirement() {
        validate_capture_options(&script_capture_options()).unwrap();
    }

    #[test]
    fn capture_requires_a_cookie_or_page_script() {
        let mut options = script_capture_options();
        options.script = None;
        let error = validate_capture_options(&options).unwrap_err();
        assert_eq!(error.code, "invalid-request");
        assert!(error.message.contains("page script"));
    }

    #[test]
    fn cancellation_stops_capture_waiting() {
        let cancelled = AtomicBool::new(true);
        let control = RequestControl::new(Duration::from_secs(1), &cancelled);
        let error = wait_for_capture(control, || {
            panic!("cancelled capture evaluated provider state")
        })
        .unwrap_err();
        assert_eq!(error.code, "cancelled");
    }

    #[test]
    fn null_page_script_result_is_not_ready() {
        let error = capture_file(
            "test-browser",
            "https://x.com",
            None,
            Vec::new(),
            Some(Value::Null),
        )
        .unwrap_err();
        assert_eq!(error.code, "auth-required");
    }

    #[test]
    fn replaces_an_existing_capture_file() {
        let directory = std::env::temp_dir().join(format!(
            "browser-session-replace-test-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("capture.json");
        fs::write(&path, b"existing destination").unwrap();
        let capture = CaptureFile {
            schema: CAPTURE_SCHEMA_VERSION,
            source: CaptureSource {
                browser: "test-browser".to_string(),
                url: "https://example.com".to_string(),
                user_agent: None,
            },
            cookies: Vec::new(),
            page: None,
        };

        write_private_json(&path, &capture).unwrap();

        let installed: CaptureFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed.source.browser, "test-browser");
        fs::remove_dir_all(directory).unwrap();
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
