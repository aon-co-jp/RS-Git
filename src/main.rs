//! # RGit (v0.1.0)
//!
//! Gitea(Go製)のRust版を目指す、自己ホスト型Git forge。
//!
//! ## 正直な開示(最重要、aruaru-llm/aruaru-bertと同じ流儀)
//!
//! **v0.1.0時点では、git smart HTTPプロトコルによるclone/push/fetchのみ**
//! を実装している。GitBucket/Giteaが持つ以下の機能は**まだ一切無い**:
//!
//! - Web UI(リポジトリ閲覧・diffの整形表示等) — README表示のみ実装済み
//! - Issue・Pull Request
//! - Wiki
//! - git smart HTTP経由のclone/push自体への認証(`REMOTE_USER`は固定値の
//!   まま) — Web UI操作(リポジトリ新規作成)のみ[`auth`]モジュールで
//!   OTPログインを要求する(2026-07-21、[open-easy-web]と同じ設計)
//! - Webhook
//!
//! [open-easy-web]: https://github.com/aon-co-jp/open-easy-web
//!
//! 実装済みなのは「`git clone`/`git push`が実際に成功する」という
//! 最小限のGitサーバー機能のみ。`git http-backend`(gitに標準同梱される
//! CGIプログラム)をサブプロセスとして起動し、HTTPリクエストをCGI環境変数
//! (`PATH_INFO`/`QUERY_STRING`/`REQUEST_METHOD`/`CONTENT_TYPE`)に変換して
//! 橋渡しするだけで、Gitプロトコル自体の再実装は行っていない
//! (実装難易度と実績のバランスを取った判断)。

mod auth;
mod mail;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use poem::listener::TcpListener;
use poem::middleware::Tracing;
use poem::web::Data;
use poem::{
    handler, get, post, put,
    web::Path as PathExtractor,
    Body, EndpointExt, Request, Response, Result as PoemResult, Route, Server,
};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Clone)]
struct AppState {
    repos_root: PathBuf,
    auth: Arc<auth::AuthStore>,
    admin_email: String,
    smtp: Option<mail::SmtpConfig>,
}

/// `Authorization: Bearer <token>`ヘッダから有効なセッショントークンを
/// 検証する。無効なら`401`エラーを返す。
fn require_session(req: &Request, state: &AppState) -> PoemResult<()> {
    let header = req.header(poem::http::header::AUTHORIZATION).unwrap_or("");
    let token = header.strip_prefix("Bearer ").unwrap_or("");
    if token.is_empty() || !state.auth.is_valid(token) {
        return Err(poem::Error::from_string("not logged in", poem::http::StatusCode::UNAUTHORIZED));
    }
    Ok(())
}

fn env_data_dir() -> PathBuf {
    std::env::var("RGIT_DATA_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("./data/repos"))
}

/// リポジトリ名を`.git`サフィックス付きの安全なディレクトリ名に正規化する。
/// パストラバーサル(`..`・`/`・空文字)を拒否する。
fn sanitize_repo_name(name: &str) -> PoemResult<String> {
    if name.is_empty() || name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(poem::Error::from_string("invalid repository name", poem::http::StatusCode::BAD_REQUEST));
    }
    if name.ends_with(".git") {
        Ok(name.to_string())
    } else {
        Ok(format!("{name}.git"))
    }
}

#[handler]
async fn list_repos(state: Data<&AppState>) -> PoemResult<poem::web::Json<Vec<String>>> {
    let mut names = Vec::new();
    if state.repos_root.exists() {
        let mut entries = tokio::fs::read_dir(&state.repos_root)
            .await
            .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?
        {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    Ok(poem::web::Json(names))
}

#[derive(serde::Serialize)]
struct ReadmeResponse {
    branch: String,
    content: String,
}

/// `GET /api/repos/:name/readme` — リポジトリのデフォルトブランチにある
/// `README.md`をそのまま返す(WASMフロント側でMarkdown→HTML変換する)。
/// `git show <branch>:README.md`をサブプロセス実行して取得する
/// (bareリポジトリにはワーキングツリーが無いため、`git show`でblobを
/// 直接読む——`git http-backend`橋渡しと同じ「gitコマンドに任せる」方針)。
#[handler]
async fn get_readme(PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }

    let head_out = Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .arg("symbolic-ref")
        .arg("--short")
        .arg("HEAD")
        .output()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    let branch = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
    if branch.is_empty() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository has no commits yet"));
    }

    let show_out = Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .arg("show")
        .arg(format!("{branch}:README.md"))
        .output()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;

    if !show_out.status.success() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("README.md not found in default branch"));
    }

    let content = String::from_utf8_lossy(&show_out.stdout).to_string();
    let payload = ReadmeResponse { branch, content };
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&payload).unwrap_or_default()))
}

/// `POST /api/auth/request-otp` — 固定管理者メール宛にOTPを送信する。
/// SMTP未設定の場合は`503`を返す(open-easy-webと同じグレースフル
/// デグレード方針)。
#[handler]
async fn request_otp(state: Data<&AppState>) -> PoemResult<Response> {
    let Some(smtp) = state.smtp.clone() else {
        return Ok(Response::builder().status(poem::http::StatusCode::SERVICE_UNAVAILABLE).body("SMTP not configured"));
    };
    let auth::RequestOtpOutcome::Issued(code) = state.auth.request_otp(&state.admin_email);
    match mail::send_otp(smtp, state.admin_email.clone(), code).await {
        Ok(()) => Ok(Response::builder().status(poem::http::StatusCode::OK).body("otp sent")),
        Err(e) => {
            tracing::warn!("failed to send OTP mail: {e}");
            Ok(Response::builder().status(poem::http::StatusCode::BAD_GATEWAY).body("failed to send mail"))
        }
    }
}

#[derive(Deserialize)]
struct VerifyOtpRequest {
    code: String,
}

#[derive(serde::Serialize)]
struct VerifyOtpResponse {
    token: String,
}

/// `POST /api/auth/verify-otp` — OTPコードを検証し、成功すればセッション
/// トークンを発行する。
#[handler]
async fn verify_otp(state: Data<&AppState>, body: poem::web::Json<VerifyOtpRequest>) -> PoemResult<Response> {
    match state.auth.consume_otp(&state.admin_email, &body.code) {
        Ok(()) => {
            let token = state.auth.create_session();
            Ok(Response::builder()
                .status(poem::http::StatusCode::OK)
                .content_type("application/json")
                .body(serde_json::to_vec(&VerifyOtpResponse { token }).unwrap_or_default()))
        }
        Err(e) => Ok(Response::builder().status(poem::http::StatusCode::FORBIDDEN).body(e.message())),
    }
}

/// `POST /api/auth/logout` — セッショントークンを失効させる。
#[handler]
async fn logout(req: &Request, state: Data<&AppState>) -> PoemResult<Response> {
    let header = req.header(poem::http::header::AUTHORIZATION).unwrap_or("");
    if let Some(token) = header.strip_prefix("Bearer ") {
        state.auth.logout(token);
    }
    Ok(Response::builder().status(poem::http::StatusCode::OK).body("logged out"))
}

/// `PUT /repos/:name` — bareリポジトリを新規作成する(`git init --bare`)。
/// ログイン必須(`Authorization: Bearer <token>`)。既に存在する場合は
/// `409 Conflict`を返す。
#[handler]
async fn create_repo(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    require_session(req, &state)?;
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::CONFLICT).body("repository already exists"));
    }
    tokio::fs::create_dir_all(&state.repos_root)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;

    let status = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&repo_path)
        .status()
        .await
        .map_err(|e| poem::Error::from_string(format!("failed to spawn git init: {e}"), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    if !status.success() {
        return Err(poem::Error::from_string("git init --bare failed", poem::http::StatusCode::INTERNAL_SERVER_ERROR));
    }
    Ok(Response::builder().status(poem::http::StatusCode::CREATED).body(repo_dir_name))
}

#[handler]
async fn healthz() -> &'static str {
    "ok"
}

/// git smart HTTPプロトコルの全経路(`info/refs`・`git-upload-pack`・
/// `git-receive-pack`)を`git http-backend`へCGI形式で橋渡しする。
#[allow(clippy::too_many_arguments)]
async fn git_http_backend(
    path_info: &str,
    query_string: &str,
    method: &str,
    content_type: &str,
    body: Body,
    repos_root: &Path,
) -> PoemResult<Response> {
    let body_bytes = body
        .into_bytes()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::BAD_REQUEST))?;

    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", repos_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query_string)
        .env("REQUEST_METHOD", method)
        .env("CONTENT_TYPE", content_type)
        .env("REMOTE_USER", "rgit") // 認証未実装(v0.1.0の既知の制限、モジュールdoc参照)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| poem::Error::from_string(format!("failed to spawn git http-backend: {e}"), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&body_bytes)
            .await
            .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;

    if !output.status.success() {
        tracing::warn!("git http-backend exited with {:?}: {}", output.status, String::from_utf8_lossy(&output.stderr));
    }

    parse_cgi_response(&output.stdout)
}

/// `git http-backend`のCGI出力(`Header: value\r\n`の並び + 空行 + body)を
/// poemの`Response`へ変換する。`Status:`ヘッダがあれば対応するステータス
/// コードを設定し、無ければ200とみなす(CGI仕様の慣例)。
fn parse_cgi_response(raw: &[u8]) -> PoemResult<Response> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| poem::Error::from_string("malformed CGI output from git http-backend", poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    let (header_bytes, rest) = raw.split_at(sep);
    let body = &rest[4..];

    let mut status = poem::http::StatusCode::OK;
    let mut builder = Response::builder();
    for line in String::from_utf8_lossy(header_bytes).split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("Status") {
                if let Some(code_str) = value.split_whitespace().next() {
                    if let Ok(code) = code_str.parse::<u16>() {
                        if let Ok(parsed) = poem::http::StatusCode::from_u16(code) {
                            status = parsed;
                        }
                    }
                }
            } else {
                builder = builder.header(name, value);
            }
        }
    }
    Ok(builder.status(status).body(body.to_vec()))
}

#[handler]
async fn git_get(req: &Request, state: Data<&AppState>) -> PoemResult<Response> {
    let path_info = req.uri().path().to_string();
    let query_string = req.uri().query().unwrap_or("").to_string();
    git_http_backend(&path_info, &query_string, "GET", "", Body::empty(), &state.repos_root).await
}

#[handler]
async fn git_post(req: &Request, body: Body, state: Data<&AppState>) -> PoemResult<Response> {
    let path_info = req.uri().path().to_string();
    let query_string = req.uri().query().unwrap_or("").to_string();
    let content_type = req.header(poem::http::header::CONTENT_TYPE).unwrap_or("").to_string();
    git_http_backend(&path_info, &query_string, "POST", &content_type, body, &state.repos_root).await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let repos_root = env_data_dir();
    tokio::fs::create_dir_all(&repos_root).await?;
    tracing::info!("rgit v0.1.0 starting, repos_root={:?}", repos_root);

    let admin_email = std::env::var("RGIT_ADMIN_EMAIL").unwrap_or_else(|_| "admin@example.com".to_string());
    let smtp = mail::SmtpConfig::from_env();
    if smtp.is_none() {
        tracing::warn!("RGIT_SMTP_* not fully configured; /api/auth/request-otp will return 503");
    }
    let state = AppState { repos_root, auth: Arc::new(auth::AuthStore::default()), admin_email, smtp };

    // git smart HTTPの実際のURLパターン(`git clone http://host/repo.git`)は
    // `/{repo}.git/info/refs`・`/{repo}.git/git-upload-pack`・
    // `/{repo}.git/git-receive-pack`。`*path`ワイルドカードで一括受け、
    // git http-backend自身にPATH_INFOで経路を判断させる。
    let static_dir = std::env::var("RGIT_STATIC_DIR").unwrap_or_else(|_| "./static".to_string());

    let app = Route::new()
        .at("/healthz", get(healthz))
        .at("/repos", get(list_repos))
        .at("/repos/:name", put(create_repo))
        .at("/api/repos", get(list_repos))
        .at("/api/repos/:name/readme", get(get_readme))
        .at("/api/auth/request-otp", post(request_otp))
        .at("/api/auth/verify-otp", post(verify_otp))
        .at("/api/auth/logout", post(logout))
        .nest(
            "/ui",
            poem::endpoint::StaticFilesEndpoint::new(&static_dir).index_file("index.html"),
        )
        .at("/*path", get(git_get).post(git_post))
        .data(state)
        .with(Tracing);

    let port = std::env::var("RGIT_PORT").unwrap_or_else(|_| "8090".to_string());
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("listening on {addr}");
    Server::new(TcpListener::bind(addr)).run(app).await?;
    Ok(())
}
