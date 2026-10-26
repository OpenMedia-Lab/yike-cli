//! Protected session storage and explicitly dedicated, loopback-only browser login.
use crate::model::Session;
use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Url};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

const MAX_SESSION: u64 = 128 * 1024;
const LOGIN_EXPRESSION: &str = r#"(async()=>{const bdstoken=window.templateData?.bdstoken;if(typeof bdstoken!=="string"||!bdstoken)return null;const r=await fetch("/youai/user/v1/getuinfo?clienttype=70&bdstoken="+encodeURIComponent(bdstoken),{credentials:"include",headers:{"X-Requested-With":"XMLHttpRequest"}});if(!r.ok)return null;const text=await r.text();let obj;try{obj=JSON.parse(text)}catch{return null}if(obj.errno!==0)return null;const m=text.match(/"youa_id"\s*:\s*(?:"([^"\r\n]+)"|(\d+))/);if(!m)return null;return {bdstoken,account_id:m[1]||m[2]};})()"#;
fn auth_error() -> anyhow::Error {
    anyhow!(
        "dedicated browser authentication failed; check the Yike profile and local debugging port"
    )
}
fn storage_error() -> anyhow::Error {
    anyhow!("protected session storage is unavailable")
}
fn valid_session(session: &Session) -> Result<()> {
    if session.cookie.is_empty()
        || session.cookie.len() > 65536
        || session.bdstoken.is_empty()
        || session.bdstoken.len() > 4096
        || session.account_id.is_empty()
        || session.account_id.len() > 256
        || reqwest::header::HeaderValue::from_str(&session.cookie).is_err()
        || session.bdstoken.contains(['\r', '\n'])
    {
        return Err(auth_error());
    }
    Ok(())
}
fn check_regular_private(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|_| storage_error())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_SESSION {
        return Err(storage_error());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(storage_error());
        }
    }
    Ok(metadata)
}
fn private_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(storage_error());
        }
    } else {
        fs::create_dir_all(path).map_err(|_| storage_error())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|_| storage_error())?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path)
            .map_err(|_| storage_error())?
            .permissions()
            .mode()
            & 0o077
            != 0
        {
            return Err(storage_error());
        }
    }
    #[cfg(windows)]
    protect_windows(path)?;
    Ok(())
}
#[cfg(windows)]
fn protect_windows(path: &Path) -> Result<()> {
    // Replace the DACL, retaining only this user's full control. No credential bytes in arguments.
    let script = r#"$ErrorActionPreference='Stop';try{$p=$env:YIKE_PRIVATE_PATH;$u=[Security.Principal.WindowsIdentity]::GetCurrent().User;$a=Get-Acl -LiteralPath $p;$a.SetAccessRuleProtection($true,$false);foreach($r in @($a.Access)){$a.RemoveAccessRuleSpecific($r)};$a.SetOwner($u);if(Test-Path -LiteralPath $p -PathType Container){$r=[Security.AccessControl.FileSystemAccessRule]::new($u,'FullControl','ContainerInherit,ObjectInherit','None','Allow')}else{$r=[Security.AccessControl.FileSystemAccessRule]::new($u,'FullControl','Allow')};$a.AddAccessRule($r);Set-Acl -LiteralPath $p -AclObject $a;exit 0}catch{exit 1}"#;
    let status = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .env("YIKE_PRIVATE_PATH", path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| storage_error())?;
    if !status.success() {
        return Err(storage_error());
    }
    Ok(())
}
fn native_storage(path: &Path) -> Result<()> {
    crate::state::require_native_path(path).map_err(|_| storage_error())
}
pub fn load_session(path: &Path) -> Result<Session> {
    native_storage(path)?;
    let metadata = check_regular_private(path)?;
    private_directory(path.parent().ok_or_else(storage_error)?)?;
    // Restored sessions can retain explicit file ACLs even after the parent
    // directory is protected. Restrict the credential file before reading it.
    #[cfg(windows)]
    protect_windows(path)?;
    let mut file = fs::File::open(path).map_err(|_| storage_error())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata().map_err(|_| storage_error())?;
        if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
            return Err(storage_error());
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_SESSION + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| storage_error())?;
    if bytes.len() as u64 > MAX_SESSION {
        return Err(storage_error());
    }
    let session: Session = serde_json::from_slice(&bytes).map_err(|_| storage_error())?;
    valid_session(&session)?;
    Ok(session)
}
pub fn save_session(path: &Path, session: &Session) -> Result<()> {
    valid_session(session)?;
    native_storage(path)?;
    let parent = path.parent().ok_or_else(storage_error)?;
    private_directory(parent)?;
    if path.exists() || fs::symlink_metadata(path).is_ok() {
        check_regular_private(path)?;
    }
    let temp = parent.join(format!(".session-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&temp).map_err(|_| storage_error())?;
        #[cfg(windows)]
        protect_windows(&temp)?;
        let bytes = serde_json::to_vec(session).map_err(|_| storage_error())?;
        output.write_all(&bytes).map_err(|_| storage_error())?;
        output.sync_all().map_err(|_| storage_error())?;
        drop(output);
        #[cfg(windows)]
        {
            // Windows rename does not replace a destination. The short gap is safe: a crash
            // requires login again, and never makes credentials visible to another account.
            if path.exists() {
                fs::remove_file(path).map_err(|_| storage_error())?;
            }
        }
        fs::rename(&temp, path).map_err(|_| storage_error())?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|_| storage_error())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
pub fn logout(path: &Path) -> Result<()> {
    if !path.exists() && fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    check_regular_private(path)?;
    fs::remove_file(path).map_err(|_| storage_error())
}
fn is_wsl() -> bool {
    cfg!(target_os = "linux")
        && (std::env::var_os("WSL_INTEROP").is_some()
            || fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|v| v.to_ascii_lowercase().contains("microsoft")))
}
pub async fn login(path: &Path, port: u16, attach: bool) -> Result<Session> {
    if port < 1024 {
        return Err(auth_error());
    }
    native_storage(path)?;
    private_directory(path.parent().ok_or_else(storage_error)?)?;
    let session = if cfg!(windows) || is_wsl() {
        windows_bridge(port, attach).await?
    } else {
        native_login(path, port, attach).await?
    };
    valid_session(&session)?;
    // This probe confirms ownership using the captured session without exporting it.
    crate::api::Api::new(session.clone())?
        .capabilities()
        .await?;
    save_session(path, &session)?;
    Ok(session)
}
async fn windows_bridge(port: u16, attach: bool) -> Result<Session> {
    let executable = if is_wsl() {
        "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"
    } else {
        "powershell.exe"
    };
    let script =
        powershell_stdin_script(include_str!("../scripts/browser-bridge.ps1"), port, attach);
    let mut child = Command::new(executable)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| auth_error())?;
    let mut input = child.stdin.take().ok_or_else(auth_error)?;
    input
        .write_all(script.as_bytes())
        .await
        .map_err(|_| auth_error())?;
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(360), child.wait_with_output())
        .await
        .map_err(|_| auth_error())?
        .map_err(|_| auth_error())?;
    if !output.status.success() || output.stdout.len() as u64 > MAX_SESSION {
        return Err(auth_error());
    }
    serde_json::from_slice(&output.stdout).map_err(|_| auth_error())
}
fn powershell_stdin_script(source: &str, port: u16, attach: bool) -> String {
    // Windows PowerShell 5.1 -Command - waits for a blank line before executing
    // a multiline script block. EOF after one newline exits 0 without running it.
    format!(
        "& {{\n{source}\n}} -Port {port} {}\n\n",
        if attach { "-Attach" } else { "" }
    )
}
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
async fn cdp_connect(address: &str, port: u16) -> Result<Socket> {
    let url = Url::parse(address).map_err(|_| auth_error())?;
    if url.scheme() != "ws"
        || !matches!(
            url.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        )
        || url.port() != Some(port)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(auth_error());
    }
    tokio_tungstenite::connect_async(address)
        .await
        .map(|(socket, _)| socket)
        .map_err(|_| auth_error())
}
async fn cdp(socket: &mut Socket, method: &str, params: Value) -> Result<Value> {
    // Only one request is outstanding on a connection; event frames are ignored.
    socket
        .send(Message::Text(
            json!({"id":1,"method":method,"params":params})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|_| auth_error())?;
    loop {
        let message = tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await
            .map_err(|_| auth_error())?
            .ok_or_else(auth_error)?
            .map_err(|_| auth_error())?;
        if let Message::Text(text) = message {
            if text.len() > 16 * 1024 * 1024 {
                return Err(auth_error());
            }
            let value: Value = serde_json::from_str(&text).map_err(|_| auth_error())?;
            if value.get("id").and_then(Value::as_u64) == Some(1) {
                if value.get("error").is_some() {
                    return Err(auth_error());
                }
                return value.get("result").cloned().ok_or_else(auth_error);
            }
        }
    }
}
async fn local_json(client: &Client, port: u16, endpoint: &str) -> Result<Value> {
    let response = client
        .get(format!("http://127.0.0.1:{port}/{endpoint}"))
        .send()
        .await
        .map_err(|_| auth_error())?;
    if !response.status().is_success() {
        return Err(auth_error());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| auth_error())?;
        if bytes.len().saturating_add(chunk.len()) > 16 * 1024 * 1024 {
            return Err(auth_error());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| auth_error())
}
fn dedicated_arguments(arguments: &[Value], port: u16) -> bool {
    let args: Vec<_> = arguments.iter().filter_map(Value::as_str).collect();
    let profile = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--user-data-dir="));
    let dedicated = profile
        .and_then(|p| Path::new(p).file_name())
        .and_then(|s| s.to_str())
        .is_some_and(|leaf| {
            leaf.eq_ignore_ascii_case("yike-browser")
                || leaf.eq_ignore_ascii_case("YikeBackupLogin")
        });
    dedicated
        && args.contains(&format!("--remote-debugging-port={port}").as_str())
        && args.contains(&"--remote-debugging-address=127.0.0.1")
}
async fn native_login(path: &Path, port: u16, attach: bool) -> Result<Session> {
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| auth_error())?;
    if !attach && local_json(&client, port, "json/version").await.is_err() {
        launch_native(path, port)?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        if let Ok(version) = local_json(&client, port, "json/version").await {
            let address = version
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .ok_or_else(auth_error)?;
            let mut browser = cdp_connect(address, port).await?;
            let command = cdp(&mut browser, "Browser.getBrowserCommandLine", json!({})).await?;
            if !command
                .get("arguments")
                .and_then(Value::as_array)
                .is_some_and(|args| dedicated_arguments(args, port))
            {
                return Err(auth_error());
            }
            drop(browser);
            let targets = local_json(&client, port, "json/list").await?;
            if let Some(target) = targets
                .as_array()
                .ok_or_else(auth_error)?
                .iter()
                .find(|target| {
                    target.get("type").and_then(Value::as_str) == Some("page")
                        && target
                            .get("url")
                            .and_then(Value::as_str)
                            .and_then(|v| Url::parse(v).ok())
                            .is_some_and(|u| {
                                u.scheme() == "https"
                                    && u.host_str() == Some("photo.baidu.com")
                                    && u.port_or_known_default() == Some(443)
                            })
                })
            {
                let address = target
                    .get("webSocketDebuggerUrl")
                    .and_then(Value::as_str)
                    .ok_or_else(auth_error)?;
                let mut socket = cdp_connect(address, port).await?;
                let evaluated = cdp(
                    &mut socket,
                    "Runtime.evaluate",
                    json!({"expression":LOGIN_EXPRESSION,"awaitPromise":true,"returnByValue":true}),
                )
                .await?;
                if let Some(value) = evaluated
                    .get("result")
                    .and_then(|v| v.get("value"))
                    .filter(|v| v.is_object())
                {
                    let cookies = cdp(
                        &mut socket,
                        "Network.getCookies",
                        json!({"urls":["https://photo.baidu.com/"]}),
                    )
                    .await?;
                    let entries = cookies
                        .get("cookies")
                        .and_then(Value::as_array)
                        .ok_or_else(auth_error)?;
                    if entries.len() > 256 {
                        return Err(auth_error());
                    }
                    let cookie = entries
                        .iter()
                        .map(|v| {
                            Ok(format!(
                                "{}={}",
                                v.get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(auth_error)?,
                                v.get("value")
                                    .and_then(Value::as_str)
                                    .ok_or_else(auth_error)?
                            ))
                        })
                        .collect::<Result<Vec<String>>>()?
                        .join("; ");
                    return Ok(Session {
                        cookie,
                        bdstoken: value
                            .get("bdstoken")
                            .and_then(Value::as_str)
                            .ok_or_else(auth_error)?
                            .into(),
                        account_id: value
                            .get("account_id")
                            .and_then(Value::as_str)
                            .ok_or_else(auth_error)?
                            .into(),
                    });
                }
            }
        } else if attach {
            return Err(auth_error());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "browser login timed out; complete sign-in in the dedicated Yike window and retry"
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
fn launch_native(path: &Path, port: u16) -> Result<()> {
    let profile = path.parent().ok_or_else(auth_error)?.join("yike-browser");
    private_directory(&profile)?;
    #[cfg(target_os = "macos")]
    let candidates = [
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    #[cfg(not(target_os = "macos"))]
    let candidates = [
        "/usr/bin/microsoft-edge",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];
    let browser = candidates
        .iter()
        .find(|candidate| Path::new(candidate).is_file())
        .ok_or_else(|| anyhow!("a Chromium browser is required for the dedicated login window"))?;
    std::process::Command::new(browser)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .args([
            "--remote-debugging-address=127.0.0.1",
            "--enable-automation",
            "--no-first-run",
            "--new-window",
            "https://photo.baidu.com/",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| auth_error())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> Session {
        Session {
            cookie: "fixture=anonymous".into(),
            bdstoken: "fixture-token".into(),
            account_id: "12345678901234567".into(),
        }
    }
    #[test]
    fn private_session_roundtrip_and_logout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/session.json");
        save_session(&path, &session()).unwrap();
        assert_eq!(
            load_session(&path).unwrap().account_id,
            session().account_id
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        save_session(&path, &session()).unwrap();
        logout(&path).unwrap();
        assert!(!path.exists());
        logout(&path).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn restored_session_explicit_acl_is_restricted_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/session.json");
        save_session(&path, &session()).unwrap();
        let run = |script: &str| {
            std::process::Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    script,
                ])
                .env("YIKE_PRIVATE_PATH", &path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        // Simulate an imported file carrying an explicit Everyone read rule.
        // Protecting only its parent does not remove this file-specific rule.
        assert!(run(
            r#"$ErrorActionPreference='Stop';try{$p=$env:YIKE_PRIVATE_PATH;$s=[Security.Principal.SecurityIdentifier]::new('S-1-1-0');$a=Get-Acl -LiteralPath $p;$r=[Security.AccessControl.FileSystemAccessRule]::new($s,'Read','Allow');$a.AddAccessRule($r);Set-Acl -LiteralPath $p -AclObject $a;$a=Get-Acl -LiteralPath $p;$rules=@($a.Access|Where-Object{$_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -eq 'S-1-1-0'});if($rules.Count -eq 0){exit 1};exit 0}catch{exit 1}"#
        ));
        assert_eq!(
            load_session(&path).unwrap().account_id,
            session().account_id
        );
        assert!(run(
            r#"$ErrorActionPreference='Stop';try{$a=Get-Acl -LiteralPath $env:YIKE_PRIVATE_PATH;$u=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value;$rules=@($a.Access);if(-not $a.AreAccessRulesProtected -or $rules.Count -ne 1){exit 1};$r=$rules[0];if($r.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -ne $u -or $r.AccessControlType -ne 'Allow' -or $r.FileSystemRights -ne 'FullControl'){exit 1};exit 0}catch{exit 1}"#
        ));
    }
    #[cfg(unix)]
    #[test]
    fn rejects_public_permissions_and_symlinks_without_exposing_secrets() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/session.json");
        save_session(&path, &session()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_session(&path).err().unwrap().to_string();
        assert!(!error.contains("fixture"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = path.parent().unwrap().join("link");
        symlink(&path, &link).unwrap();
        assert!(load_session(&link).is_err());
        assert!(save_session(&link, &session()).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn session_storage_resolves_existing_ancestors_before_creation() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let native = directory.path().join("native");
        fs::create_dir(&native).unwrap();
        let alias = directory.path().join("alias");
        symlink(&native, &alias).unwrap();
        let path = alias.join("private/session.json");
        let resolved = crate::state::resolved_future_path(&path).unwrap();
        assert_eq!(
            resolved,
            native.canonicalize().unwrap().join("private/session.json")
        );
        assert!(!resolved.exists());
        save_session(&path, &session()).unwrap();
        assert_eq!(
            load_session(&resolved).unwrap().account_id,
            session().account_id
        );
    }
    #[test]
    fn only_dedicated_loopback_browser_arguments_are_accepted() {
        let mut args = vec![
            json!("edge"),
            json!("--user-data-dir=/private/yike-browser"),
            json!("--remote-debugging-port=19227"),
            json!("--remote-debugging-address=127.0.0.1"),
        ];
        assert!(dedicated_arguments(&args, 19227));
        args[1] = json!("--user-data-dir=/private/Default");
        assert!(!dedicated_arguments(&args, 19227));
        args[1] = json!("--user-data-dir=/private/yike-browser");
        args[3] = json!("--remote-debugging-address=0.0.0.0");
        assert!(!dedicated_arguments(&args, 19227));
    }
    #[test]
    fn windows_pipe_helper_suppresses_task_results_and_bounds_cdp_waits() {
        // Windows PowerShell 5.1 otherwise emits VoidTaskResult into the private JSON pipe.
        let helper = include_str!("../scripts/browser-bridge.ps1");
        assert!(helper.contains("[void]$socket.ConnectAsync"));
        assert!(helper.contains("[void]$Socket.SendAsync"));
        assert!(!helper.contains("[Threading.CancellationToken]::None"));
        assert_eq!(
            helper.matches("$cancellation.CancelAfter(15000)").count(),
            2
        );
    }
    #[test]
    fn windows_multiline_stdin_script_has_required_terminating_blank_line() {
        let script = powershell_stdin_script(
            "param([int]$Port,[switch]$Attach)\nWrite-Output 'anonymous'",
            19227,
            true,
        );
        assert!(script.ends_with("} -Port 19227 -Attach\n\n"));
    }
}
