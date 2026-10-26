//! Independently implemented Baidu Photo web protocol adapter.
//! Credentials are scoped to the API and its fixed first-party upload endpoint.
//! Original-media downloads and their redirects use a separate cookie-free client.
use crate::model::{Album, Capabilities, PlannedFile, RemoteFile, Session, UploadTicket};
use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use reqwest::{Client, Method, Response, Url, header};
use serde_json::{Value, json};
use std::{collections::HashSet, fmt, time::Duration};
use tokio::sync::OnceCell;

const API_ROOT: &str = "https://photo.baidu.com/youai/";
const UPLOAD_ROOT: &str = "https://c3.pcs.baidu.com/rest/2.0/pcs/superfile2";
const MAX_JSON: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    Network,
    Timeout,
    Authentication,
    Throttled,
    Rejected,
    Protocol,
    AccountMismatch,
}
#[derive(Debug)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub operation: &'static str,
    pub status: Option<u16>,
    pub errno: Option<i64>,
    pub retry_after: Option<Duration>,
}
impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "photo service {:?} at {}", self.kind, self.operation)?;
        if let Some(status) = self.status {
            write!(f, " (HTTP {status})")?;
        }
        if let Some(errno) = self.errno {
            write!(f, " (code {errno})")?;
        }
        Ok(())
    }
}
impl std::error::Error for ApiError {}
fn failure(kind: ApiErrorKind) -> anyhow::Error {
    ApiError {
        kind,
        operation: "request",
        status: None,
        errno: None,
        retry_after: None,
    }
    .into()
}
fn protocol() -> anyhow::Error {
    failure(ApiErrorKind::Protocol)
}
fn at(error: anyhow::Error, operation: &'static str) -> anyhow::Error {
    match error.downcast::<ApiError>() {
        Ok(mut error) => {
            error.operation = operation;
            error.into()
        }
        Err(error) => error,
    }
}
fn network(error: reqwest::Error) -> anyhow::Error {
    failure(if error.is_timeout() {
        ApiErrorKind::Timeout
    } else {
        ApiErrorKind::Network
    })
}
pub fn retry_delay(error: &anyhow::Error) -> Option<Duration> {
    let e = error.downcast_ref::<ApiError>()?;
    matches!(
        e.kind,
        ApiErrorKind::Network | ApiErrorKind::Timeout | ApiErrorKind::Throttled
    )
    .then_some(e.retry_after.unwrap_or(Duration::from_secs(1)))
}
pub fn is_auth_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ApiError>().is_some_and(|e| {
        matches!(
            e.kind,
            ApiErrorKind::Authentication | ApiErrorKind::AccountMismatch
        )
    })
}

pub struct Api {
    session: Session,
    api_client: Client,
    upload_client: Client,
    media_client: Client,
    upload_cookie: Option<String>,
    root: Url,
    upload_root: Url,
    checked_account: OnceCell<()>,
    #[cfg(test)]
    test_origin: Option<Url>,
}
impl Api {
    pub fn new(session: Session) -> Result<Self> {
        Self::build(session, Url::parse(API_ROOT).map_err(|_| protocol())?)
    }
    fn build(session: Session, root: Url) -> Result<Self> {
        if session.cookie.is_empty()
            || session.bdstoken.is_empty()
            || session.account_id.is_empty()
            || session.cookie.len() > 65536
            || session.bdstoken.len() > 4096
            || header::HeaderValue::from_str(&session.cookie).is_err()
        {
            return Err(failure(ApiErrorKind::Authentication));
        }
        let make = || {
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(20))
                .read_timeout(Duration::from_secs(90))
                .user_agent("Mozilla/5.0 YikeCLI/0.1")
                .build()
                .map_err(network)
        };
        let upload_cookie = first_party_upload_cookie(&session.cookie);
        Ok(Self {
            session,
            api_client: make()?,
            upload_client: make()?,
            media_client: make()?,
            upload_cookie,
            root,
            upload_root: Url::parse(UPLOAD_ROOT).map_err(|_| protocol())?,
            checked_account: OnceCell::new(),
            #[cfg(test)]
            test_origin: None,
        })
    }
    #[cfg(test)]
    pub(crate) fn for_test(session: Session, base: Url) -> Result<Self> {
        if base.scheme() != "http"
            || !matches!(
                base.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
            )
        {
            return Err(protocol());
        }
        let mut api = Self::build(session, base.clone())?;
        api.upload_root = base
            .join("rest/2.0/pcs/superfile2")
            .map_err(|_| protocol())?;
        api.test_origin = Some(base);
        Ok(api)
    }
    pub fn account(&self) -> String {
        self.session.fingerprint()
    }
    async fn ensure_account(&self) -> Result<()> {
        self.checked_account
            .get_or_try_init(|| async {
                let value = self
                    .json(Method::GET, "user/v1/getuinfo", &[], None)
                    .await?;
                let owner = field(&value, "youa_id")
                    .or_else(|| field(&value, "uk"))
                    .ok_or_else(protocol)
                    .and_then(id)?;
                if owner != self.session.account_id {
                    return Err(failure(ApiErrorKind::AccountMismatch));
                }
                Ok(())
            })
            .await?;
        Ok(())
    }
    async fn json(
        &self,
        method: Method,
        path: &'static str,
        query: &[(&str, String)],
        form: Option<&[(&str, String)]>,
    ) -> Result<Value> {
        let url = self.root.join(path).map_err(|_| protocol())?;
        // `path` is private and constant at call sites; no externally supplied API URL.
        if url.origin() != self.root.origin() {
            return Err(protocol());
        }
        let mut request = self
            .api_client
            .request(method, url)
            .header(header::COOKIE, &self.session.cookie)
            .header("X-Requested-With", "XMLHttpRequest")
            .header(header::REFERER, "https://photo.baidu.com/")
            .query(&[
                ("clienttype", "70"),
                ("bdstoken", self.session.bdstoken.as_str()),
            ])
            .query(query);
        if let Some(form) = form {
            request = request.form(form);
        }
        let response = request
            .send()
            .await
            .map_err(|error| at(network(error), path))?;
        decode_json(response, true)
            .await
            .map_err(|error| at(error, path))
    }
    pub async fn capabilities(&self) -> Result<Capabilities> {
        self.ensure_account().await?;
        let value = self.json(Method::GET, "user/v1/filecfg", &[], None).await?;
        let size = |key| {
            field(&value, key)
                .and_then(|v| v.get(1).or_else(|| v.get("1")))
                .and_then(unsigned)
        };
        // Quota describes unlimited storage separately from the per-file transfer limits.
        let unlimited = match self.json(Method::GET, "user/v1/quotainfo", &[], None).await {
            Ok(quota) => field(&quota, "is_unlimit").and_then(boolean),
            Err(error) if is_auth_error(&error) => return Err(error),
            Err(_) => None,
        };
        Ok(Capabilities {
            upload_limit: size("fudsize"),
            download_limit: size("fddsize"),
            unlimited,
        })
    }
    pub async fn files(&self, album: Option<&Album>) -> Result<Vec<RemoteFile>> {
        self.ensure_account().await?;
        let mut query = vec![
            ("need_thumbnail", "0".into()),
            ("need_filter_hidden", "0".into()),
            ("limit", "1000".into()),
        ];
        let endpoint = if let Some(album) = album {
            query.extend([
                ("album_id", album.id.clone()),
                ("tid", album.tid.clone()),
                ("need_amount", "1".into()),
            ]);
            "album/v1/listfile"
        } else {
            "file/v1/list"
        };
        let mut files = Vec::new();
        for value in self.pages(endpoint, query).await? {
            files.push(parse_file(&value)?);
        }
        let mut ids = HashSet::new();
        if files.iter().any(|file| !ids.insert(file.fsid.clone())) {
            return Err(protocol());
        }
        Ok(files)
    }
    pub async fn albums(&self) -> Result<Vec<Album>> {
        self.ensure_account().await?;
        let albums = self
            .pages(
                "album/v1/list",
                vec![("need_amount", "1".into()), ("limit", "100".into())],
            )
            .await?
            .iter()
            .map(parse_album)
            .collect::<Result<Vec<_>>>()?;
        let mut ids = HashSet::new();
        if albums.iter().any(|album| !ids.insert(album.id.clone())) {
            return Err(protocol());
        }
        Ok(albums)
    }
    async fn pages(
        &self,
        endpoint: &'static str,
        mut query: Vec<(&str, String)>,
    ) -> Result<Vec<Value>> {
        let mut cursor = String::new();
        let mut cursors = HashSet::new();
        let mut page_signatures = HashSet::new();
        let mut output = Vec::new();
        for _ in 0..100_000 {
            query.retain(|(key, _)| *key != "cursor");
            query.push(("cursor", cursor.clone()));
            let value = self.json(Method::GET, endpoint, &query, None).await?;
            let list = value
                .get("list")
                .and_then(Value::as_array)
                .ok_or_else(protocol)?;
            let more = value
                .get("has_more")
                .and_then(boolean)
                .ok_or_else(protocol)?;
            if !list.is_empty()
                && !page_signatures.insert(serde_json::to_string(list).map_err(|_| protocol())?)
            {
                return Err(protocol());
            }
            output.extend(list.iter().cloned());
            if !more {
                return Ok(output);
            }
            let next = value.get("cursor").ok_or_else(protocol).and_then(id)?;
            if next.is_empty() || next == cursor || !cursors.insert(next.clone()) {
                return Err(protocol());
            }
            cursor = next;
        }
        Err(protocol())
    }
    pub async fn album_create(&self, title: &str) -> Result<Album> {
        self.ensure_account().await?;
        if title.trim().is_empty() || title.chars().count() > 50 {
            return Err(anyhow!("album title must contain 1 to 50 characters"));
        }
        let entropy = uuid::Uuid::new_v4().as_u128() % 9_000_000 + 1_000_000;
        let tid = format!("3{}{}", chrono::Utc::now().timestamp(), entropy);
        let value = self
            .json(
                Method::POST,
                "album/v1/create",
                &[
                    ("title", title.into()),
                    ("tid", tid),
                    ("source", "0".into()),
                ],
                None,
            )
            .await?;
        let album_id = field(&value, "album_id")
            .ok_or_else(protocol)
            .and_then(id)?;
        // Do not retry create if detail fails: the caller's commit intent remains unresolved.
        let detail = self
            .json(
                Method::GET,
                "album/v1/detail",
                &[("album_id", album_id.clone())],
                None,
            )
            .await?;
        let album = parse_album(detail.get("data").unwrap_or(&detail))?;
        if album.id != album_id {
            return Err(protocol());
        }
        Ok(album)
    }
    pub async fn add_to_album(&self, album: &Album, file: &RemoteFile) -> Result<()> {
        self.ensure_account().await?;
        let fsid = integer_id(&file.fsid)?;
        let list = serde_json::to_string(&json!([{ "fsid": fsid }])).map_err(|_| protocol())?;
        self.json(
            Method::GET,
            "album/v1/addfile",
            &[
                ("album_id", album.id.clone()),
                ("tid", album.tid.clone()),
                ("list", list),
            ],
            None,
        )
        .await?;
        Ok(())
    }
    pub async fn precreate(&self, file: &PlannedFile) -> Result<UploadTicket> {
        self.ensure_account().await?;
        let params = upload_params(file)?;
        let value = self
            .json(Method::POST, "file/v1/precreate", &[], Some(&params))
            .await?;
        let return_type = field(&value, "return_type")
            .and_then(unsigned)
            .and_then(|n| u8::try_from(n).ok())
            .ok_or_else(protocol)?;
        let path = field(&value, "path")
            .and_then(Value::as_str)
            .unwrap_or(&params[4].1)
            .to_owned();
        let upload_id = field(&value, "uploadid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let remote = value
            .get("data")
            .filter(|v| v.get("fs_id").is_some() || v.get("fsid").is_some())
            .map(parse_file)
            .transpose()?;
        let missing = if return_type == 1 {
            let array = field(&value, "block_list")
                .and_then(Value::as_array)
                .ok_or_else(protocol)?;
            let mut seen = HashSet::new();
            array
                .iter()
                .map(|value| {
                    let part = unsigned(value)
                        .and_then(|i| usize::try_from(i).ok())
                        .ok_or_else(protocol)?;
                    if part >= file.parts.len() || !seen.insert(part) {
                        return Err(protocol());
                    }
                    Ok(part)
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        match return_type {
            1 if upload_id.is_some() => {}
            2 if upload_id.is_some() => {}
            3 if remote.is_some() => {}
            _ => return Err(protocol()),
        }
        Ok(UploadTicket {
            return_type,
            path,
            upload_id,
            missing,
            file: remote,
        })
    }
    pub async fn upload_part(
        &self,
        file: &PlannedFile,
        ticket: &UploadTicket,
        index: usize,
        bytes: Vec<u8>,
    ) -> Result<()> {
        use md5::Digest;
        self.ensure_account().await?;
        let expected = file.parts.get(index).ok_or_else(protocol)?;
        if bytes.len() != expected.len
            || hex::encode(md5::Md5::digest(&bytes)) != expected.md5
            || hex::encode(sha2::Sha256::digest(&bytes)) != expected.sha256
        {
            return Err(at(protocol(), "upload_part.source_digest"));
        }
        let upload_id = ticket.upload_id.as_ref().ok_or_else(protocol)?;
        self.validate_upload_endpoint()?;
        let upload_cookie = self
            .upload_cookie
            .as_ref()
            .ok_or_else(|| at(failure(ApiErrorKind::Authentication), "upload_part.cookie"))?;
        let response = self
            .upload_client
            .post(self.upload_root.clone())
            .header(header::COOKIE, upload_cookie)
            .query(&[
                ("method", "upload".to_owned()),
                ("path", ticket.path.clone()),
                ("partseq", index.to_string()),
                ("uploadid", upload_id.clone()),
                ("app_id", "16051585".to_owned()),
            ])
            .multipart(reqwest::multipart::Form::new().part(
                "file",
                reqwest::multipart::Part::bytes(bytes).file_name(file.name().to_owned()),
            ))
            .send()
            .await
            .map_err(|error| at(network(error), "upload_part.transport"))?;
        let value = decode_json(response, false)
            .await
            .map_err(|error| at(error, "upload_part.response"))?;
        let digest = value
            .get("md5")
            .and_then(Value::as_str)
            .ok_or_else(|| at(protocol(), "upload_part.md5_missing"))?;
        if normalize_md5(digest)
            .map_err(|error| at(error, "upload_part.md5_format"))?
            .as_str()
            != expected.md5
        {
            return Err(at(protocol(), "upload_part.md5_mismatch"));
        }
        Ok(())
    }
    fn validate_upload_endpoint(&self) -> Result<()> {
        #[cfg(test)]
        if self.test_origin.as_ref().is_some_and(|base| {
            base.join("rest/2.0/pcs/superfile2")
                .is_ok_and(|expected| expected == self.upload_root)
        }) {
            return Ok(());
        }
        if self.upload_root.as_str() == UPLOAD_ROOT {
            return Ok(());
        }
        Err(at(protocol(), "upload_part.endpoint"))
    }
    pub async fn create_file(
        &self,
        file: &PlannedFile,
        ticket: &UploadTicket,
    ) -> Result<RemoteFile> {
        self.ensure_account().await?;
        let mut params = upload_params(file)?;
        params[4].1.clone_from(&ticket.path);
        params.push(("uploadid", ticket.upload_id.clone().ok_or_else(protocol)?));
        let value = self
            .json(Method::POST, "file/v1/create", &[], Some(&params))
            .await?;
        parse_file(value.get("data").ok_or_else(protocol)?)
    }
    pub async fn download_response(
        &self,
        file: &RemoteFile,
        offset: u64,
        etag: Option<&str>,
    ) -> Result<Response> {
        self.ensure_account().await?;
        let value = self
            .json(
                Method::GET,
                "file/v2/download",
                &[("fsid", file.fsid.clone())],
                None,
            )
            .await?;
        let dlink = field(&value, "dlink")
            .and_then(Value::as_str)
            .ok_or_else(protocol)?;
        let mut url = Url::parse(dlink).map_err(|_| protocol())?;
        for _ in 0..6 {
            self.validate_media_url(&url)?;
            let mut request = self
                .media_client
                .get(url.clone())
                .header(header::REFERER, "https://photo.baidu.com/");
            if offset > 0 {
                request = request.header(header::RANGE, format!("bytes={offset}-"));
            }
            if let Some(etag) = etag {
                request = request.header(header::IF_RANGE, etag);
            }
            let response = request.send().await.map_err(network)?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(protocol)?;
                url = url.join(location).map_err(|_| protocol())?;
                continue;
            }
            // 416 is returned for the transfer engine to validate against its immutable size.
            if !response.status().is_success() && response.status().as_u16() != 416 {
                return Err(http_error(&response));
            }
            return Ok(response);
        }
        Err(protocol())
    }
    fn validate_media_url(&self, url: &Url) -> Result<()> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err(protocol());
        }
        #[cfg(test)]
        if self
            .test_origin
            .as_ref()
            .is_some_and(|origin| url.origin() == origin.origin())
        {
            return Ok(());
        }
        let trusted = url.host_str().is_some_and(|host| {
            ["baidu.com", "baidupcs.com", "bcebos.com"]
                .iter()
                .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        });
        if url.scheme() != "https" || url.port_or_known_default() != Some(443) || !trusted {
            return Err(protocol());
        }
        Ok(())
    }
}
fn http_error(response: &Response) -> anyhow::Error {
    let status = response.status().as_u16();
    let kind = match status {
        401 | 403 => ApiErrorKind::Authentication,
        429 | 500 | 502 | 503 | 504 => ApiErrorKind::Throttled,
        _ => ApiErrorKind::Rejected,
    };
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                chrono::DateTime::parse_from_rfc2822(s)
                    .ok()
                    .and_then(|date| {
                        (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
                            .to_std()
                            .ok()
                    })
            })
        });
    ApiError {
        kind,
        operation: "response",
        status: Some(status),
        errno: None,
        retry_after,
    }
    .into()
}
async fn decode_json(response: Response, require_errno: bool) -> Result<Value> {
    if !response.status().is_success() {
        return Err(http_error(&response));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(network)?;
        if body.len().saturating_add(chunk.len()) > MAX_JSON {
            return Err(protocol());
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| protocol())?;
    if !value.is_object() {
        return Err(protocol());
    }
    let errno = value
        .get("errno")
        .or_else(|| value.get("error_code"))
        .and_then(signed);
    if require_errno && errno.is_none() {
        return Err(protocol());
    }
    if let Some(errno) = errno.filter(|n| *n != 0) {
        let kind = match errno {
            -6 | -7 | 111 | 401 => ApiErrorKind::Authentication,
            429 => ApiErrorKind::Throttled,
            _ => ApiErrorKind::Rejected,
        };
        return Err(ApiError {
            kind,
            operation: "response",
            status: None,
            errno: Some(errno),
            retry_after: None,
        }
        .into());
    }
    Ok(value)
}
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .get(key)
        .or_else(|| value.get("data").and_then(|v| v.get(key)))
        .or_else(|| value.get("result").and_then(|v| v.get(key)))
}
fn first_party_upload_cookie(cookie: &str) -> Option<String> {
    cookie.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == "BDUSS" && !value.is_empty()).then(|| format!("BDUSS={value}"))
    })
}
fn id(value: &Value) -> Result<String> {
    match value {
        Value::String(s) if !s.is_empty() => Ok(s.clone()),
        Value::Number(n) if n.is_u64() || n.is_i64() => Ok(n.to_string()),
        _ => Err(protocol()),
    }
}
fn integer_id(value: &str) -> Result<Value> {
    value
        .parse::<u64>()
        .map(|n| json!(n))
        .map_err(|_| protocol())
}
fn unsigned(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}
fn signed(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}
fn boolean(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| match unsigned(value) {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    })
}
fn parse_album(value: &Value) -> Result<Album> {
    Ok(Album {
        id: value.get("album_id").ok_or_else(protocol).and_then(id)?,
        tid: value.get("tid").ok_or_else(protocol).and_then(id)?,
        title: value
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(protocol)?
            .to_owned(),
    })
}
fn parse_file(value: &Value) -> Result<RemoteFile> {
    let fsid = value
        .get("fsid")
        .or_else(|| value.get("fs_id"))
        .ok_or_else(protocol)
        .and_then(id)?;
    let path = value
        .get("path")
        .or_else(|| value.get("server_filename"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(protocol)?
        .to_owned();
    let size = value.get("size").and_then(unsigned).ok_or_else(protocol)?;
    let md5 = value
        .get("md5")
        .or_else(|| value.get("server_md5"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(normalize_md5)
        .transpose()?;
    Ok(RemoteFile {
        fsid,
        path,
        size,
        md5,
        ctime: value.get("ctime").and_then(signed).unwrap_or(0),
        mtime: value.get("mtime").and_then(signed).unwrap_or(0),
    })
}
fn upload_params(file: &PlannedFile) -> Result<Vec<(&'static str, String)>> {
    if file.stamp.len == 0
        || file.parts.is_empty()
        || file.name().is_empty()
        || file.name().contains(['/', '\\', '\0'])
    {
        return Err(protocol());
    }
    normalize_md5(&file.md5)?;
    normalize_md5(&file.slice_md5)?;
    let blocks: Vec<_> = file.parts.iter().map(|p| p.md5.as_str()).collect();
    Ok(vec![
        ("autoinit", "1".into()),
        ("isdir", "0".into()),
        ("rtype", "1".into()),
        ("ctype", "11".into()),
        ("path", format!("/{}", file.name())),
        ("size", file.stamp.len.to_string()),
        ("slice-md5", file.slice_md5.clone()),
        ("content-md5", file.md5.clone()),
        (
            "block_list",
            serde_json::to_string(&blocks).map_err(|_| protocol())?,
        ),
    ])
}
/// The web service sometimes permutes MD5 groups and masks each nibble with its index.
/// The marker at nibble 9 distinguishes that representation from a normal hex MD5.
pub fn normalize_md5(input: &str) -> Result<String> {
    if input.len() != 32 || !input.is_ascii() {
        return Err(protocol());
    }
    if input.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(input.to_ascii_lowercase());
    }
    let bytes = input.as_bytes();
    let mut decoded = [0_u8; 32];
    for (position, target) in decoded.iter_mut().enumerate() {
        let character = bytes[position].to_ascii_lowercase();
        let nibble = if position == 9 && (b'g'..=b'v').contains(&character) {
            character - b'g'
        } else {
            (character as char)
                .to_digit(16)
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(protocol)?
        };
        *target = nibble ^ (position as u8 & 15);
    }
    let order = [
        8, 9, 10, 11, 12, 13, 14, 15, 0, 1, 2, 3, 4, 5, 6, 7, 24, 25, 26, 27, 28, 29, 30, 31, 16,
        17, 18, 19, 20, 21, 22, 23,
    ];
    let hex = b"0123456789abcdef";
    Ok(order
        .iter()
        .map(|&index| hex[decoded[index] as usize] as char)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileStamp, PartDigest};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn session() -> Session {
        Session {
            cookie: "test-cookie=private-fixture; BDUSS=upload-fixture; STOKEN=excluded-fixture"
                .into(),
            bdstoken: "private-fixture-token".into(),
            account_id: "42".into(),
        }
    }
    fn owner() -> Value {
        json!({"errno":0,"youa_id":"42"})
    }
    fn remote(fsid: Value) -> Value {
        json!({"fsid":fsid,"path":"/原片 fixture.mp4","size":3,"md5":"900150983cd24fb0d6963f7d28e17f72"})
    }
    fn planned() -> PlannedFile {
        PlannedFile {
            id: "fixture".into(),
            path: "fixture.mp4".into(),
            relative_path: "fixture.mp4".into(),
            stamp: FileStamp {
                len: 3,
                modified_ns: 0,
                identity: "fixture".into(),
            },
            md5: "900150983cd24fb0d6963f7d28e17f72".into(),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            slice_md5: "900150983cd24fb0d6963f7d28e17f72".into(),
            parts: vec![PartDigest {
                md5: "900150983cd24fb0d6963f7d28e17f72".into(),
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
                len: 3,
            }],
            blocked: None,
            album_title: "fixture".into(),
        }
    }
    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }
    impl From<Value> for Reply {
        fn from(value: Value) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&value).unwrap(),
            }
        }
    }
    async fn fixture(replies: Vec<Reply>) -> (Api, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base =
            Url::parse(&format!("http://{}/youai/", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end;
                loop {
                    let mut buffer = [0_u8; 8192];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(index) = bytes.windows(4).position(|slice| slice == b"\r\n\r\n") {
                        header_end = index + 4;
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|n| n.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut buffer = [0_u8; 8192];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                requests.push(String::from_utf8_lossy(&bytes).into_owned());
                let extra = reply
                    .headers
                    .iter()
                    .map(|(key, value)| format!("{key}: {value}\r\n"))
                    .collect::<String>();
                let response = format!(
                    "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n{}\r\n",
                    reply.status,
                    reply.body.len(),
                    extra
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&reply.body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            requests
        });
        (Api::for_test(session(), base).unwrap(), task)
    }
    #[tokio::test]
    async fn paginates_every_page_and_keeps_large_integer_ids_lossless() {
        let (api, requests) = fixture(vec![owner().into(),json!({"errno":0,"list":[remote(json!(18446744073709551615_u64))],"has_more":1,"cursor":"next 中文"}).into(),json!({"errno":0,"list":[remote(json!("9007199254740993"))],"has_more":0,"cursor":""}).into()]).await;
        let files = api.files(None).await.unwrap();
        assert_eq!(
            files.iter().map(|f| f.fsid.as_str()).collect::<Vec<_>>(),
            ["18446744073709551615", "9007199254740993"]
        );
        let requests = requests.await.unwrap();
        assert!(requests[2].contains("cursor=next+%E4%B8%AD%E6%96%87"));
        assert!(requests[1].contains("clienttype=70"));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-requested-with: xmlhttprequest")
        );
    }
    #[tokio::test]
    async fn rejects_repeated_pagination_and_unknown_shapes() {
        let (api, requests) = fixture(vec![
            owner().into(),
            json!({"errno":0,"list":[remote(json!("1"))],"has_more":1,"cursor":"same"}).into(),
            json!({"errno":0,"list":[remote(json!("2"))],"has_more":1,"cursor":"same"}).into(),
        ])
        .await;
        assert!(api.files(None).await.is_err());
        requests.await.unwrap();
        let (api, requests) = fixture(vec![
            owner().into(),
            json!({"errno":0,"list":[],"cursor":""}).into(),
        ])
        .await;
        assert!(api.files(None).await.is_err());
        requests.await.unwrap();
    }
    #[tokio::test]
    async fn account_mismatch_stops_before_mutations() {
        let (api, requests) = fixture(vec![
            json!({"errno":0,"youa_id":"different-account"}).into(),
        ])
        .await;
        let error = api.album_create("fixture").await.err().unwrap();
        assert!(is_auth_error(&error));
        assert!(!error.to_string().contains("different-account"));
        assert_eq!(requests.await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn reports_limits_and_safe_retry_errors() {
        let (api, requests) = fixture(vec![
            owner().into(),
            json!({"errno":0,"fudsize":[1,5368709120_u64],"fddsize":[2,"10737418240"]}).into(),
            json!({"errno":0,"is_unlimit":1}).into(),
            Reply {
                status: 429,
                headers: vec![("Retry-After".into(), "7".into())],
                body: b"private-fixture-token https://private.invalid/signed?secret=fixture"
                    .to_vec(),
            },
        ])
        .await;
        let caps = api.capabilities().await.unwrap();
        assert_eq!(caps.upload_limit, Some(5368709120));
        assert_eq!(caps.download_limit, Some(10737418240));
        assert_eq!(caps.unlimited, Some(true));
        let error = api.albums().await.err().unwrap();
        assert_eq!(retry_delay(&error), Some(Duration::from_secs(7)));
        assert!(!format!("{error:?}").contains("fixture"));
        requests.await.unwrap();
    }
    #[tokio::test]
    async fn original_download_and_redirects_never_receive_api_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base =
            Url::parse(&format!("http://{}/youai/", listener.local_addr().unwrap())).unwrap();
        let api = Api::for_test(session(), base.clone()).unwrap();
        let task = tokio::spawn(async move {
            let replies = [
                (200,"",serde_json::to_vec(&owner()).unwrap()),
                (200,"",serde_json::to_vec(&json!({"errno":0,"dlink":base.join("media?secret=fixture").unwrap().as_str()})).unwrap()),
                (302,"Location: /redirected?secret=fixture\r\n",Vec::new()),
                (200,"",b"abc".to_vec()),
            ];
            let mut captured = Vec::new();
            for (status, headers, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 16384];
                let count = socket.read(&mut bytes).await.unwrap();
                captured.push(String::from_utf8_lossy(&bytes[..count]).into_owned());
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                            body.len(),
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                socket.write_all(&body).await.unwrap();
            }
            captured
        });
        let file = parse_file(&remote(json!("1"))).unwrap();
        let response = api
            .download_response(&file, 1, Some("\"fixture-etag\""))
            .await
            .unwrap();
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"abc");
        let requests = task.await.unwrap();
        assert!(requests[1].to_ascii_lowercase().contains("cookie:"));
        for request in &requests[2..] {
            assert!(!request.to_ascii_lowercase().contains("cookie:"));
            assert!(!request.contains("private-fixture-token"));
            assert!(request.to_ascii_lowercase().contains("range: bytes=1-"));
        }
    }
    #[test]
    fn rejects_untrusted_media_origins_and_malformed_md5() {
        let api = Api::new(session()).unwrap();
        for url in [
            "https://evil.invalid/file",
            "http://d.pcs.baidu.com/file",
            "https://baidu.com.evil.invalid/file",
            "https://user:secret@d.pcs.baidu.com/file",
            "https://d.pcs.baidu.com:444/file",
        ] {
            assert!(api.validate_media_url(&Url::parse(url).unwrap()).is_err());
        }
        assert!(normalize_md5("not-a-digest").is_err());
        assert_eq!(
            normalize_md5("900150983CD24FB0D6963F7D28E17F72").unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            normalize_md5("3df10ad71paa9d7729c23a155f3df292").unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }
    #[tokio::test]
    async fn precreate_uses_hashes_and_rejects_out_of_range_parts() {
        let (api, requests)=fixture(vec![owner().into(),json!({"errno":0,"return_type":1,"path":"/fixture.mp4","uploadid":"anonymous-upload","block_list":[9]}).into()]).await;
        let file = planned();
        assert!(api.precreate(&file).await.is_err());
        let requests = requests.await.unwrap();
        assert!(requests[1].contains("ctype=11"));
        assert!(requests[1].contains("content-md5=900150983cd24fb0d6963f7d28e17f72"));
        assert!(requests[1].contains("block_list=%5B%22900150983cd24fb0d6963f7d28e17f72%22%5D"));
    }
    #[tokio::test]
    async fn chunk_upload_uses_multipart_and_only_first_party_auth_cookie() {
        let (api, requests) = fixture(vec![
            owner().into(),
            json!({"md5":"900150983cd24fb0d6963f7d28e17f72"}).into(),
        ])
        .await;
        let ticket = UploadTicket {
            return_type: 1,
            path: "/fixture.mp4".into(),
            upload_id: Some("anonymous-upload".into()),
            missing: vec![0],
            file: None,
        };
        api.upload_part(&planned(), &ticket, 0, b"abc".to_vec())
            .await
            .unwrap();
        let requests = requests.await.unwrap();
        let upload = requests[1].to_ascii_lowercase();
        assert!(upload.contains("partseq=0"));
        assert!(upload.contains("app_id=16051585"));
        assert!(upload.contains("multipart/form-data"));
        assert!(upload.contains("name=\"file\""));
        assert!(upload.contains("cookie: bduss=upload-fixture"));
        assert!(!upload.contains("test-cookie="));
        assert!(!upload.contains("stoken="));
        assert!(!upload.contains("private-fixture-token"));
    }
    #[tokio::test]
    async fn first_party_upload_auth_is_never_forwarded_on_redirect() {
        let (api, requests) = fixture(vec![
            owner().into(),
            Reply {
                status: 302,
                headers: vec![("Location".into(), "https://evil.invalid/upload".into())],
                body: Vec::new(),
            },
        ])
        .await;
        let ticket = UploadTicket {
            return_type: 1,
            path: "/fixture.mp4".into(),
            upload_id: Some("anonymous-upload".into()),
            missing: vec![0],
            file: None,
        };
        let error = api
            .upload_part(&planned(), &ticket, 0, b"abc".to_vec())
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<ApiError>().unwrap().status, Some(302));
        assert_eq!(requests.await.unwrap().len(), 2);
    }
    #[test]
    fn upload_auth_requires_exact_fixed_endpoint_and_only_bduss() {
        assert_eq!(
            first_party_upload_cookie("other=excluded; BDUSS=anonymous; STOKEN=excluded"),
            Some("BDUSS=anonymous".into())
        );
        assert_eq!(first_party_upload_cookie("BDUSS_BFESS=excluded"), None);
        let mut api = Api::new(session()).unwrap();
        for endpoint in [
            "https://c3.pcs.baidu.com:444/rest/2.0/pcs/superfile2",
            "https://evil.invalid/rest/2.0/pcs/superfile2",
            "https://c3.pcs.baidu.com/other",
        ] {
            api.upload_root = Url::parse(endpoint).unwrap();
            assert!(api.validate_upload_endpoint().is_err());
        }
    }
    #[tokio::test]
    async fn pcs_upload_errors_keep_only_numeric_code_and_fixed_operation() {
        let (api, requests) = fixture(vec![owner().into(), json!({"error_code":31045,"error_msg":"private-fixture-token https://private.invalid/signed"}).into()]).await;
        let ticket = UploadTicket {
            return_type: 1,
            path: "/fixture.mp4".into(),
            upload_id: Some("anonymous-upload".into()),
            missing: vec![0],
            file: None,
        };
        let error = api
            .upload_part(&planned(), &ticket, 0, b"abc".to_vec())
            .await
            .unwrap_err();
        let typed = error.downcast_ref::<ApiError>().unwrap();
        assert_eq!(typed.errno, Some(31045));
        assert_eq!(typed.operation, "upload_part.response");
        assert!(!format!("{error:?}").contains("private-fixture"));
        requests.await.unwrap();
    }
}
