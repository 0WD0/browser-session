// SPDX-License-Identifier: MIT

use browser_session::capture::{
    capture, prepare_login_profile, CaptureOptions, CaptureResult, PrepareProfileOptions,
    DEFAULT_CDP_PORT, DEFAULT_TIMEOUT_SECS,
};
use browser_session::error::{Result, SessionError};
use serde::Serialize;
use serde_json::json;
use std::env;
use std::path::PathBuf;
use std::process;
use std::time::Duration;

const PROTOCOL_VERSION: u32 = 1;
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Capture,
    PrepareProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    command: Command,
    url: Option<String>,
    cookie_names: Vec<String>,
    any_cookie_names: Vec<String>,
    all_origin_cookies: bool,
    output: Option<PathBuf>,
    browser: Option<String>,
    profile_dir: Option<PathBuf>,
    profile_root: Option<PathBuf>,
    port: u16,
    timeout_secs: u64,
    proxy: Option<String>,
    script_file: Option<PathBuf>,
    restart_running: bool,
}

#[derive(Serialize)]
struct SuccessEnvelope<T> {
    ok: bool,
    schema: u32,
    protocol: u32,
    version: &'static str,
    data: T,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ErrorPayload {
    code: &'static str,
    message: String,
    retryable: bool,
    #[serde(rename = "auth-required")]
    auth_required: bool,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    ok: bool,
    schema: u32,
    protocol: u32,
    version: &'static str,
    error: ErrorPayload,
    warnings: Vec<String>,
}

fn main() {
    match run(env::args().skip(1)) {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!("{error}");
            println!("{}", encode_error(&error));
            process::exit(1);
        }
    }
}

fn encode_error(error: &SessionError) -> String {
    serde_json::to_string(&ErrorEnvelope {
        ok: false,
        schema: browser_session::capture::CAPTURE_SCHEMA_VERSION,
        protocol: PROTOCOL_VERSION,
        version: VERSION,
        error: ErrorPayload {
            code: error.code,
            message: error.message.clone(),
            retryable: error.retryable,
            auth_required: error.auth_required,
        },
        warnings: Vec::new(),
    })
    .expect("error envelope is serializable")
}

fn run<I>(arguments: I) -> Result<String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let options = parse_args(arguments)?;
    let data = match options.command {
        Command::Help => json!({"usage": usage()}),
        Command::Version => json!({
            "schema": browser_session::capture::CAPTURE_SCHEMA_VERSION,
            "protocol": PROTOCOL_VERSION,
            "version": VERSION
        }),
        Command::Capture => {
            let capture = capture(&CaptureOptions {
                url: options
                    .url
                    .expect("capture URL was validated by parse_args"),
                cookie_names: options.cookie_names,
                any_cookie_names: options.any_cookie_names,
                all_origin_cookies: options.all_origin_cookies,
                output: options
                    .output
                    .expect("capture output was validated by parse_args"),
                browser: options.browser,
                profile_dir: options.profile_dir,
                profile_root: options.profile_root,
                port: options.port,
                timeout: Duration::from_secs(options.timeout_secs),
                proxy: options.proxy,
                script_file: options.script_file,
                restart_running: options.restart_running,
            })?;
            capture_result_json(capture)
        }
        Command::PrepareProfile => {
            let prepared = prepare_login_profile(&PrepareProfileOptions {
                url: options
                    .url
                    .expect("profile URL was validated by parse_args"),
                browser: options.browser,
                profile_dir: options.profile_dir,
                profile_root: options.profile_root,
                timeout: Duration::from_secs(options.timeout_secs),
            })?;
            json!({
                "profile": {
                    "prepared": true,
                    "browser": prepared.browser,
                    "path": prepared.path
                }
            })
        }
    };
    serde_json::to_string(&SuccessEnvelope {
        ok: true,
        schema: browser_session::capture::CAPTURE_SCHEMA_VERSION,
        protocol: PROTOCOL_VERSION,
        version: VERSION,
        data,
        warnings: Vec::new(),
    })
    .map_err(|error| SessionError::helper_failure(format!("cannot encode response: {error}")))
}

fn capture_result_json(result: CaptureResult) -> serde_json::Value {
    serde_json::to_value(result).expect("capture result is serializable")
}

fn parse_args<I>(arguments: I) -> Result<Options>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let args = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    if args.is_empty() {
        return Ok(default_options(Command::Help));
    }
    let command = match args[0].as_str() {
        "help" | "--help" | "-h" => Command::Help,
        "version" | "--version" => Command::Version,
        "capture" => Command::Capture,
        "prepare-profile" => Command::PrepareProfile,
        other => {
            return Err(SessionError::invalid_request(format!(
                "unknown command `{other}`"
            )))
        }
    };
    if matches!(command, Command::Help | Command::Version) {
        if args.len() > 1 {
            return Err(SessionError::invalid_request(
                "help and version do not accept options",
            ));
        }
        return Ok(default_options(command));
    }

    let mut options = default_options(command);
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--url" => options.url = Some(option_value(&args, &mut index, "url")?),
            "--cookie" => options
                .cookie_names
                .push(option_value(&args, &mut index, "cookie")?),
            "--any-cookie" => {
                options
                    .any_cookie_names
                    .push(option_value(&args, &mut index, "any-cookie")?)
            }
            "--all-origin-cookies" => options.all_origin_cookies = true,
            "--output" => {
                options.output = Some(PathBuf::from(option_value(&args, &mut index, "output")?))
            }
            "--browser" => options.browser = Some(option_value(&args, &mut index, "browser")?),
            "--profile-dir" => {
                options.profile_dir = Some(PathBuf::from(option_value(
                    &args,
                    &mut index,
                    "profile-dir",
                )?))
            }
            "--profile-root" => {
                options.profile_root = Some(PathBuf::from(option_value(
                    &args,
                    &mut index,
                    "profile-root",
                )?))
            }
            "--port" => {
                let value = option_value(&args, &mut index, "port")?;
                options.port = value.parse().map_err(|_| {
                    SessionError::invalid_request(format!("invalid port `{value}`"))
                })?;
            }
            "--timeout-secs" => {
                let value = option_value(&args, &mut index, "timeout-secs")?;
                options.timeout_secs = value.parse().map_err(|_| {
                    SessionError::invalid_request(format!("invalid timeout `{value}`"))
                })?;
            }
            "--proxy" => options.proxy = Some(option_value(&args, &mut index, "proxy")?),
            "--script-file" => {
                options.script_file = Some(PathBuf::from(option_value(
                    &args,
                    &mut index,
                    "script-file",
                )?))
            }
            "--restart-running" => options.restart_running = true,
            other => {
                return Err(SessionError::invalid_request(format!(
                    "unknown option `{other}`"
                )))
            }
        }
        index += 1;
    }
    validate_command_options(&options)?;
    Ok(options)
}

fn default_options(command: Command) -> Options {
    Options {
        command,
        url: None,
        cookie_names: Vec::new(),
        any_cookie_names: Vec::new(),
        all_origin_cookies: false,
        output: None,
        browser: None,
        profile_dir: None,
        profile_root: None,
        port: DEFAULT_CDP_PORT,
        timeout_secs: DEFAULT_TIMEOUT_SECS,
        proxy: None,
        script_file: None,
        restart_running: false,
    }
}

fn validate_command_options(options: &Options) -> Result<()> {
    let url = options
        .url
        .as_deref()
        .ok_or_else(|| SessionError::invalid_request("missing --url URL"))?;
    if url.trim().is_empty() {
        return Err(SessionError::invalid_request(
            "capture URL must not be empty",
        ));
    }
    if options.profile_dir.is_some() && options.profile_root.is_some() {
        return Err(SessionError::invalid_request(
            "profile-dir and profile-root cannot be used together",
        ));
    }
    match options.command {
        Command::Capture => {
            if options.output.is_none() {
                return Err(SessionError::invalid_request("missing --output FILE"));
            }
            if options.cookie_names.is_empty() && options.any_cookie_names.is_empty() {
                return Err(SessionError::invalid_request(
                    "capture requires at least one --cookie or --any-cookie NAME",
                ));
            }
        }
        Command::PrepareProfile => {
            if options.output.is_some()
                || !options.cookie_names.is_empty()
                || !options.any_cookie_names.is_empty()
                || options.all_origin_cookies
                || options.port != DEFAULT_CDP_PORT
                || options.proxy.is_some()
                || options.script_file.is_some()
                || options.restart_running
            {
                return Err(SessionError::invalid_request(
                    "prepare-profile accepts only url, browser, profile, and timeout options",
                ));
            }
        }
        Command::Help | Command::Version => unreachable!("non-request command was returned"),
    }
    Ok(())
}

fn option_value(args: &[String], index: &mut usize, name: &str) -> Result<String> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| SessionError::invalid_request(format!("missing value for --{name}")))
}

fn usage() -> String {
    [
        "usage:",
        "  browser-session version",
        "  browser-session capture --url URL (--cookie NAME|--any-cookie NAME)... --output FILE [options]",
        "  browser-session prepare-profile --url URL [options]",
        "",
        "capture options:",
        "  --browser NAME|PATH       supported browser or executable path",
        "  --profile-dir DIR         isolated profile directory",
        "  --profile-root DIR        automatic Chrome/Firefox/Zen profile root",
        "  --port N                  local remote-control port (default 29317)",
        "  --timeout-secs N          wait timeout (default 180)",
        "  --proxy URL               proxy for a Chromium browser started by the helper",
        "  --script-file FILE        provider-owned JavaScript expression",
        "  --any-cookie NAME         require one cookie from this alternative set",
        "  --all-origin-cookies      capture all unexpired cookies scoped to URL",
        "  --restart-running         permit one browser restart when supported",
        "",
        "prepare-profile options:",
        "  --browser NAME|PATH       Firefox or Zen browser",
        "  --profile-dir DIR         isolated Firefox-family profile directory",
        "  --profile-root DIR        automatic Firefox/Zen profile root",
        "  --timeout-secs N          wait for the user to close the profile",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_capture_options() {
        let options = parse_args([
            "capture",
            "--url",
            "https://x.com",
            "--cookie",
            "auth_token",
            "--any-cookie",
            "ct0",
            "--output",
            "/tmp/session.json",
            "--browser",
            "chrome",
            "--profile-root",
            "/tmp/profiles",
            "--port",
            "29999",
            "--timeout-secs",
            "12",
            "--all-origin-cookies",
            "--restart-running",
        ])
        .unwrap();
        assert_eq!(options.command, Command::Capture);
        assert_eq!(options.cookie_names, vec!["auth_token"]);
        assert_eq!(options.any_cookie_names, vec!["ct0"]);
        assert!(options.all_origin_cookies);
        assert_eq!(options.port, 29999);
        assert_eq!(options.timeout_secs, 12);
        assert!(options.restart_running);
    }

    #[test]
    fn parses_profile_preparation_options() {
        let options = parse_args([
            "prepare-profile",
            "--url",
            "https://music.youtube.com",
            "--browser",
            "firefox",
            "--profile-dir",
            "/tmp/profile",
            "--timeout-secs",
            "60",
        ])
        .unwrap();
        assert_eq!(options.command, Command::PrepareProfile);
        assert_eq!(options.browser, Some("firefox".to_string()));
        assert_eq!(options.profile_dir, Some(PathBuf::from("/tmp/profile")));
        assert_eq!(options.timeout_secs, 60);
    }

    #[test]
    fn rejects_capture_without_cookie_requirements() {
        let error =
            parse_args(["capture", "--url", "https://x.com", "--output", "/tmp/a"]).unwrap_err();
        assert!(error.message.contains("--cookie"));
    }

    #[test]
    fn rejects_profile_preparation_capture_options() {
        let error = parse_args([
            "prepare-profile",
            "--url",
            "https://x.com",
            "--cookie",
            "ct0",
        ])
        .unwrap_err();
        assert!(error.message.contains("prepare-profile"));
    }

    #[test]
    fn success_envelope_does_not_contain_cookie_values() {
        let output = capture_result_json(CaptureResult {
            browser: "chrome".to_string(),
            output: PathBuf::from("/tmp/session.json"),
            url: "https://x.com".to_string(),
            cookies: vec!["auth_token".to_string()],
            captured_cookie_count: 1,
            page_captured: false,
            profile_dir: None,
        });
        let text = serde_json::to_string(&output).unwrap();
        assert!(!text.contains("cookie-value"));
        assert!(text.contains("auth_token"));
    }
}
