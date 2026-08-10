// SPDX-License-Identifier: MIT

use crate::browser::{
    bidi_cookies, browser_is_running, cdp_base_url, cdp_cookies, cdp_version, close_bidi_browser,
    close_cdp_browser, connect_bidi, effective_profile_dir, evaluate_bidi, evaluate_cdp,
    open_bidi_context, open_cdp_target, prepare_profile, resolve_browser, restart_running_browser,
    running_browser_conflict, spawn_remote_browser, terminate_spawned_browser,
    wait_for_bidi_connection, wait_for_cdp_version, Browser, BrowserCookie, BrowserProtocol,
    CdpVersion, PreparedProfile,
};
use crate::error::{Result, SessionError};
use reqwest::blocking::Client;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CAPTURE_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CDP_PORT: u16 = 29317;
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

const DEFAULT_BROWSER_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
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
    pub script_file: Option<PathBuf>,
    pub restart_running: bool,
}

#[derive(Debug, Clone)]
pub struct PrepareProfileOptions {
    pub url: String,
    pub browser: Option<String>,
    pub profile_dir: Option<PathBuf>,
    pub profile_root: Option<PathBuf>,
    pub timeout: Duration,
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
    #[serde(rename = "captured-cookie-count")]
    pub captured_cookie_count: usize,
    #[serde(rename = "page-captured")]
    pub page_captured: bool,
    #[serde(rename = "profile-dir", skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<PathBuf>,
}

pub fn capture(options: &CaptureOptions) -> Result<CaptureResult> {
    validate_capture_options(options)?;
    let browser = resolve_browser(options.browser.as_deref())?;
    let profile_dir = effective_profile_dir(
        options.profile_dir.as_deref(),
        options.profile_root.as_deref(),
        &browser,
    );
    let script = read_script(options.script_file.as_deref())?;
    let client = new_client()?;
    let capture = match browser.protocol() {
        BrowserProtocol::Cdp => capture_cdp(
            &client,
            options,
            &browser,
            profile_dir.as_deref(),
            script.as_deref(),
        )?,
        BrowserProtocol::Bidi => capture_bidi(
            &client,
            options,
            &browser,
            profile_dir.as_deref(),
            script.as_deref(),
        )?,
    };
    write_private_json(&options.output, &capture)?;
    Ok(CaptureResult {
        browser: browser.name().to_string(),
        output: options.output.clone(),
        url: options.url.clone(),
        cookies: captured_cookie_names(options),
        captured_cookie_count: capture.cookies.len(),
        page_captured: capture.page.is_some(),
        profile_dir,
    })
}

pub fn prepare_login_profile(options: &PrepareProfileOptions) -> Result<PreparedProfile> {
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
    prepare_profile(
        &options.url,
        &browser,
        options.profile_dir.as_deref(),
        options.profile_root.as_deref(),
        options.timeout,
    )
}

fn validate_capture_options(options: &CaptureOptions) -> Result<()> {
    validate_url(&options.url)?;
    if options.cookie_names.is_empty() && options.any_cookie_names.is_empty() {
        return Err(SessionError::invalid_request(
            "at least one required or alternative cookie name is required",
        ));
    }
    validate_cookie_names(&options.cookie_names)?;
    validate_cookie_names(&options.any_cookie_names)?;
    if options.profile_dir.is_some() && options.profile_root.is_some() {
        return Err(SessionError::invalid_request(
            "profile-dir and profile-root cannot be used together",
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

fn read_script(path: Option<&Path>) -> Result<Option<String>> {
    path.map(fs::read_to_string).transpose().map_err(|error| {
        SessionError::helper_failure(format!("cannot read browser-session script: {error}"))
    })
}

fn new_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_millis(900))
        .build()
        .map_err(|error| {
            SessionError::helper_failure(format!("cannot create HTTP client: {error}"))
        })
}

fn capture_cdp(
    client: &Client,
    options: &CaptureOptions,
    browser: &Browser,
    profile_dir: Option<&Path>,
    script: Option<&str>,
) -> Result<CaptureFile> {
    let base_url = cdp_base_url(options.port);
    let mut child = None;
    let mut spawned = false;
    let version = match cdp_version(client, &base_url) {
        Ok(version) => version,
        Err(_) => {
            prepare_remote_browser(browser, options, profile_dir)?;
            child = Some(spawn_remote_browser(
                browser,
                profile_dir,
                options.port,
                options.proxy.as_deref(),
            )?);
            spawned = true;
            let Some(version) = wait_for_cdp_version(client, &base_url, Duration::from_secs(8))
            else {
                if should_close_spawned_browser(spawned, profile_dir) {
                    terminate_spawned_browser(&mut child);
                }
                return Err(SessionError::network(format!(
                    "cannot reach browser DevTools on 127.0.0.1:{}",
                    options.port
                )));
            };
            version
        }
    };
    let result = (|| {
        let target = open_cdp_target(client, &base_url, &options.url)?;
        wait_for_capture(options.timeout, || {
            capture_cdp_once(options, browser, &version, target.websocket_url(), script)
        })
    })();
    if should_close_spawned_browser(spawned, profile_dir) {
        close_cdp_browser(version.browser_websocket_url());
        terminate_spawned_browser(&mut child);
    }
    result
}

fn capture_bidi(
    client: &Client,
    options: &CaptureOptions,
    browser: &Browser,
    profile_dir: Option<&Path>,
    script: Option<&str>,
) -> Result<CaptureFile> {
    let mut child = None;
    let mut spawned = false;
    let mut connection = match connect_bidi(client, options.port) {
        Ok(connection) => connection,
        Err(_) => {
            prepare_remote_browser(browser, options, profile_dir)?;
            child = Some(spawn_remote_browser(
                browser,
                profile_dir,
                options.port,
                options.proxy.as_deref(),
            )?);
            spawned = true;
            let Some(connection) =
                wait_for_bidi_connection(client, options.port, Duration::from_secs(8))
            else {
                if should_close_spawned_browser(spawned, profile_dir) {
                    terminate_spawned_browser(&mut child);
                }
                return Err(SessionError::network(format!(
                    "cannot reach browser WebDriver BiDi endpoint on 127.0.0.1:{}",
                    options.port
                )));
            };
            connection
        }
    };
    let result = (|| {
        let context = open_bidi_context(&mut connection, &options.url)?;
        wait_for_capture(options.timeout, || {
            capture_bidi_once(options, browser, &mut connection, &context, script)
        })
    })();
    if should_close_spawned_browser(spawned, profile_dir) {
        close_bidi_browser(&mut connection);
        terminate_spawned_browser(&mut child);
    }
    result
}

fn should_close_spawned_browser(spawned: bool, profile_dir: Option<&Path>) -> bool {
    spawned && profile_dir.is_some()
}

fn prepare_remote_browser(
    browser: &Browser,
    options: &CaptureOptions,
    profile_dir: Option<&Path>,
) -> Result<()> {
    if browser_is_running(browser) {
        if options.restart_running {
            restart_running_browser(browser)?;
        } else if let Some(error) =
            running_browser_conflict(browser, options.port, profile_dir.is_some())
        {
            return Err(error);
        }
    }
    Ok(())
}

fn wait_for_capture(
    timeout: Duration,
    mut capture_once: impl FnMut() -> Result<CaptureFile>,
) -> Result<CaptureFile> {
    let started = Instant::now();
    let mut last_error = None;
    loop {
        if started.elapsed() >= timeout {
            return Err(last_error.unwrap_or_else(|| {
                SessionError::auth_required(
                    "browser session did not expose the requested state before timeout",
                )
            }));
        }
        match capture_once() {
            Ok(capture) => return Ok(capture),
            Err(error) => last_error = Some(error),
        }
        sleep(Duration::from_secs(1));
    }
}

fn capture_cdp_once(
    options: &CaptureOptions,
    browser: &Browser,
    version: &CdpVersion,
    websocket_url: &str,
    script: Option<&str>,
) -> Result<CaptureFile> {
    let cookies = select_cookies(
        cdp_cookies(websocket_url)?,
        &options.url,
        &options.cookie_names,
        &options.any_cookie_names,
        options.all_origin_cookies,
    )?;
    let page = script
        .map(|expression| evaluate_cdp(websocket_url, expression))
        .transpose()?;
    capture_file(
        browser.name(),
        &options.url,
        version.user_agent(),
        cookies,
        page,
    )
}

fn capture_bidi_once(
    options: &CaptureOptions,
    browser: &Browser,
    connection: &mut crate::browser::BidiConnection,
    context: &str,
    script: Option<&str>,
) -> Result<CaptureFile> {
    let cookies = select_cookies(
        bidi_cookies(connection, context)?,
        &options.url,
        &options.cookie_names,
        &options.any_cookie_names,
        options.all_origin_cookies,
    )?;
    let page = script
        .map(|expression| evaluate_bidi(connection, context, expression))
        .transpose()?;
    capture_file(
        browser.name(),
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
}

fn cookie_domain_matches(cookie_domain: &str, host: &str) -> bool {
    let domain = cookie_domain.trim_start_matches('.');
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn cookie_path_matches(cookie_path: &str, request_path: &str) -> bool {
    cookie_path.is_empty() || cookie_path == "/" || request_path.starts_with(cookie_path)
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

    #[test]
    fn matches_parent_domains_without_matching_siblings() {
        assert!(cookie_domain_matches(".x.com", "x.com"));
        assert!(cookie_domain_matches(".x.com", "mobile.x.com"));
        assert!(!cookie_domain_matches(".x.com", "notx.com"));
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
