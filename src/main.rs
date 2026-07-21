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
//! - Webhook
//!
//! ## アクセス制御(2026-07-21、[open-easy-web]と同じOTP認証をベースに拡張)
//!
//! - **管理者**(`RGIT_ADMIN_EMAIL`)は常に全操作可能。
//! - **登録アカウント**([`accounts`]、管理者が個別に許可したメール
//!   アドレス)は、[`auth`]の同じOTP機構で**自分のメールアドレス宛に**
//!   ログインでき、リポジトリごとに管理者が許可した範囲
//!   (閲覧/ダウンロード/push、[`access::AccountPermission`])のみ操作可能。
//! - **`public`**(誰でも)・**`group`**(共有招待トークン保持者)の
//!   範囲も、アカウントとは独立にリポジトリごと設定できる
//!   ([`access::AccessConfig`])。
//! - Web UI操作(README/ファイル一覧/個別ダウンロード/ZIP)だけでなく、
//!   **`git clone`/`git pull`(git-upload-pack)・`git push`
//!   (git-receive-pack)自体もこの権限に従う**(`git_get`/`git_post`が
//!   ディスパッチ前に[`check_access`]を呼ぶ)。git CLIからの認証は
//!   HTTP Basic(ユーザー名=メールアドレス、パスワード=セッション
//!   トークン)で行う。
//!
//! [open-easy-web]: https://github.com/aon-co-jp/open-easy-web
//!
//! 実装済みなのは「`git clone`/`git push`が実際に成功する」という
//! 最小限のGitサーバー機能のみ。`git http-backend`(gitに標準同梱される
//! CGIプログラム)をサブプロセスとして起動し、HTTPリクエストをCGI環境変数
//! (`PATH_INFO`/`QUERY_STRING`/`REQUEST_METHOD`/`CONTENT_TYPE`)に変換して
//! 橋渡しするだけで、Gitプロトコル自体の再実装は行っていない
//! (実装難易度と実績のバランスを取った判断)。

mod access;
mod accounts;
mod auth;
mod capacity;
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
    /// `true`の間、`admin_email`以外のメールアドレスを新規アカウント
    /// (`POST /api/accounts`・自己申請の承認)として受け付けない
    /// (ユーザー指示、2026-07-21:「norukia.jp@gmail.com以外のメール
    /// アカウントは現状は受け入れないで」)。`RGIT_ACCOUNTS_LOCKED`
    /// 環境変数(既定`true`)で制御、`false`にすれば従来通り誰でも
    /// 自己申請→承認できる状態に戻せる。
    accounts_locked: bool,
}

/// `Authorization`ヘッダからログイン中のメールアドレスを特定する。
/// 2通りの認証方式を受け付ける:
/// - `Bearer <token>`(Web UI/APIクライアント向け)
/// - `Basic base64(email:token)`(git CLI向け——`git`は`http.extraHeader`
///   無しでも標準でBasic認証をサポートするため、ユーザー名にメール
///   アドレス、パスワードに[`verify_otp`]で得たセッショントークンを
///   使う運用にすることで、追加のツール無しに`git clone`/`git push`を
///   認証付きで行える)。
fn session_identity(req: &Request, state: &AppState) -> Option<String> {
    let header = req.header(poem::http::header::AUTHORIZATION)?;
    if let Some(token) = header.strip_prefix("Bearer ") {
        return state.auth.session_email(token);
    }
    if let Some(encoded) = header.strip_prefix("Basic ") {
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (_email, token) = text.split_once(':')?;
        return state.auth.session_email(token);
    }
    None
}

/// 管理者本人としてログイン済みかを検証する。無効なら`401`。
fn require_admin_session(req: &Request, state: &AppState) -> PoemResult<()> {
    match session_identity(req, state) {
        Some(email) if email == state.admin_email => Ok(()),
        _ => Err(poem::Error::from_string("admin login required", poem::http::StatusCode::UNAUTHORIZED)),
    }
}

/// リクエストから、グループ限定公開の突破に使うトークンを取り出す。
/// ダウンロードリンクは素のURLとして共有される想定のため、まず
/// クエリ文字列`?token=...`を見て、次にAPI呼び出し向けの
/// `X-RGit-Group-Token`ヘッダを見る。
fn extract_group_token(req: &Request) -> Option<String> {
    if let Some(query) = req.uri().query() {
        for kv in query.split('&') {
            if let Some(v) = kv.strip_prefix("token=") {
                return Some(urlencoding_decode(v));
            }
        }
    }
    req.header("X-RGit-Group-Token").map(str::to_string)
}

/// 読み取り・ダウンロード・push系エンドポイントへのアクセス可否。
/// 管理者は常に許可。それ以外は[`access`]モジュールのリポジトリ単位
/// 設定(`private`/`public`/`group`、ログイン中アカウント個別許可)に従う。
/// Web UI/API向け——資格情報が無ければ`403`(WASM側は`401`/`403`を
/// 同じ扱いでログイン画面へ誘導する想定)。
async fn check_access(req: &Request, state: &AppState, repo_path: &Path, need: access::Need) -> PoemResult<()> {
    if is_allowed_for_git(req, state, repo_path, need).await {
        Ok(())
    } else {
        Err(poem::Error::from_string("access denied", poem::http::StatusCode::FORBIDDEN))
    }
}

async fn is_allowed_for_git(req: &Request, state: &AppState, repo_path: &Path, need: access::Need) -> bool {
    let identity = session_identity(req, state);
    if identity.as_deref() == Some(state.admin_email.as_str()) {
        return true;
    }
    let config = access::load(repo_path).await;
    let groups = access::load_groups(&state.repos_root).await;
    let token = extract_group_token(req);
    access::is_allowed(&config, need, &groups, token.as_deref(), identity.as_deref())
}

/// git smart HTTP(clone/pull/push)専用のアクセス確認。**`git`クライアント
/// は`401`+`WWW-Authenticate`ヘッダを受け取って初めてBasic認証の資格
/// 情報を送り直す仕様**(`403`では再送しない)ため、`poem::Error`
/// (ヘッダを付けられない)ではなく`Response`を直接組み立てて返す。
/// 許可されていれば`None`、拒否なら返すべき`Response`を`Some`で返す。
async fn git_access_error(req: &Request, state: &AppState, repo_path: &Path, need: access::Need) -> Option<Response> {
    if is_allowed_for_git(req, state, repo_path, need).await {
        return None;
    }
    if req.header(poem::http::header::AUTHORIZATION).is_none() {
        return Some(
            Response::builder()
                .status(poem::http::StatusCode::UNAUTHORIZED)
                .header("WWW-Authenticate", "Basic realm=\"RGit\"")
                .body("authentication required"),
        );
    }
    Some(Response::builder().status(poem::http::StatusCode::FORBIDDEN).body("access denied"))
}

/// bareリポジトリのデフォルトブランチ名を解決する。コミットが1つも
/// 無い(HEADが未設定の)リポジトリでは空文字列を返す。
async fn resolve_default_branch(repo_path: &Path) -> PoemResult<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("symbolic-ref")
        .arg("--short")
        .arg("HEAD")
        .output()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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

/// `GET /api/repos` — リポジトリ一覧。ログイン済みなら全リポジトリ、
/// 未ログインなら[`access`]で閲覧許可(`public`または、提示された
/// トークンに一致する`group`)されたリポジトリのみ返す。
#[handler]
async fn list_repos(req: &Request, state: Data<&AppState>) -> PoemResult<poem::web::Json<Vec<String>>> {
    let identity = session_identity(req, &state);
    let is_admin = identity.as_deref() == Some(state.admin_email.as_str());
    let groups = access::load_groups(&state.repos_root).await;
    let token = extract_group_token(req);
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
                    let allowed = is_admin || {
                        let config = access::load(&entry.path()).await;
                        access::is_allowed(&config, access::Need::View, &groups, token.as_deref(), identity.as_deref())
                    };
                    if allowed {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    Ok(poem::web::Json(names))
}

/// `GET /api/repos/:name/access` — 現在のアクセス設定を返す(ログイン
/// 必須——`group`名やON/OFF状態は管理者向け情報であり、トークン自体は
/// 含まないが第三者に見せる理由も無いため)。
#[handler]
async fn get_access(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    let config = access::load(&repo_path).await;
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&config).unwrap_or_default()))
}

/// `PUT /api/repos/:name/access` — アクセス設定(モード・グループ・
/// 閲覧/ダウンロード許可)を更新する(ログイン必須)。`mode: "group"`の
/// 場合、指定した`group`が[`access::GroupStore`]に存在しなければ
/// `400`を返す(存在しないグループへの割り当てを防ぐ)。
#[handler]
async fn set_access(
    req: &Request,
    PathExtractor(name): PathExtractor<String>,
    state: Data<&AppState>,
    body: poem::web::Json<access::AccessConfig>,
) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    if body.mode == access::Mode::Group {
        let Some(group_name) = &body.group else {
            return Ok(Response::builder().status(poem::http::StatusCode::BAD_REQUEST).body("group name required for group mode"));
        };
        let groups = access::load_groups(&state.repos_root).await;
        if !groups.groups.contains_key(group_name) {
            return Ok(Response::builder().status(poem::http::StatusCode::BAD_REQUEST).body("unknown group"));
        }
    }
    access::save(&repo_path, &body).await.map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Response::builder().status(poem::http::StatusCode::OK).body("ok"))
}

#[derive(Deserialize)]
struct CreateGroupRequest {
    name: String,
}

#[derive(serde::Serialize)]
struct CreateGroupResponse {
    name: String,
    token: String,
}

/// `POST /api/groups` — 新しいグループ(チーム・クラス等)を作成し、
/// 招待トークンを発行する(ログイン必須)。**トークンはこのレスポンス
/// でしか返さない**(1回きりの表示、GitBucketの招待リンクと同じ発想)
/// ——`GroupStore`には保存されるが、以後のAPIから読み出す経路は無い。
#[handler]
async fn create_group(req: &Request, state: Data<&AppState>, body: poem::web::Json<CreateGroupRequest>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    if body.name.is_empty() || body.name.contains(['/', '\\', '.']) {
        return Ok(Response::builder().status(poem::http::StatusCode::BAD_REQUEST).body("invalid group name"));
    }
    let mut groups = access::load_groups(&state.repos_root).await;
    if groups.groups.contains_key(&body.name) {
        return Ok(Response::builder().status(poem::http::StatusCode::CONFLICT).body("group already exists"));
    }
    let token = access::generate_token();
    groups.groups.insert(body.name.clone(), access::GroupInfo { token: token.clone() });
    access::save_groups(&state.repos_root, &groups)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Response::builder()
        .status(poem::http::StatusCode::CREATED)
        .content_type("application/json")
        .body(serde_json::to_vec(&CreateGroupResponse { name: body.name.clone(), token }).unwrap_or_default()))
}

/// `GET /api/groups` — グループ名の一覧を返す(トークンは含まない、
/// ログイン必須)。
#[handler]
async fn list_groups(req: &Request, state: Data<&AppState>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let groups = access::load_groups(&state.repos_root).await;
    let mut names: Vec<&String> = groups.groups.keys().collect();
    names.sort();
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&names).unwrap_or_default()))
}

/// `DELETE /api/groups/:name` — グループを削除する(ログイン必須)。
/// このグループを参照していたリポジトリのアクセス設定はそのまま残る
/// (`group`名が存在しなくなるため、以後は`is_allowed`が常に拒否する
/// ——`private`へ暗黙に戻るのと同じ安全側の挙動)。
#[handler]
async fn delete_group(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let mut groups = access::load_groups(&state.repos_root).await;
    if groups.groups.remove(&name).is_none() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("group not found"));
    }
    access::save_groups(&state.repos_root, &groups)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Response::builder().status(poem::http::StatusCode::OK).body("ok"))
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
async fn get_readme(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::View).await?;

    let branch = resolve_default_branch(&repo_path).await?;
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

/// `GET /api/repos/:name/tree` — デフォルトブランチの全ファイル一覧
/// (`git ls-tree -r --name-only`)。個別ダウンロード・ZIP選択ダウンロード
/// のUIがファイル一覧を得るためのAPI。
#[handler]
async fn get_tree(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::View).await?;

    let branch = resolve_default_branch(&repo_path).await?;
    if branch.is_empty() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository has no commits yet"));
    }

    let out = Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .arg("ls-tree")
        .arg("-r")
        .arg("--name-only")
        .arg(&branch)
        .output()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    if !out.status.success() {
        return Err(poem::Error::from_string("git ls-tree failed", poem::http::StatusCode::INTERNAL_SERVER_ERROR));
    }
    let files: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap_or("").lines().collect();
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&files).unwrap_or_default()))
}

/// パストラバーサル(`..`セグメント)を拒否する、ツリー内相対パスの
/// 簡易サニタイズ(`git show`/`git archive`はツリー内でしか解決されない
/// ため実害は無いが、明らかに悪意のある入力を早期に弾く)。
fn sanitize_tree_path(path: &str) -> PoemResult<()> {
    if path.split('/').any(|seg| seg == "..") {
        return Err(poem::Error::from_string("invalid path", poem::http::StatusCode::BAD_REQUEST));
    }
    Ok(())
}

/// `GET /api/repos/:name/raw/<path>` — デフォルトブランチ内の1ファイルを
/// そのままダウンロードする(`Content-Disposition: attachment`)。
/// `path`はワイルドカードではなく、`main.rs`のルーティングで
/// `/api/repos/:name/raw/*filepath`として登録し、`req.uri()`から
/// プレフィックスを剥がして取り出す(git_get/git_postと同じ手法)。
#[handler]
async fn get_raw_file(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::Download).await?;

    let prefix = format!("/api/repos/{name}/raw/");
    let full_path = req.uri().path();
    let Some(file_path) = full_path.strip_prefix(&prefix) else {
        return Err(poem::Error::from_string("invalid path", poem::http::StatusCode::BAD_REQUEST));
    };
    let file_path = urlencoding_decode(file_path);
    sanitize_tree_path(&file_path)?;
    if file_path.is_empty() {
        return Err(poem::Error::from_string("file path required", poem::http::StatusCode::BAD_REQUEST));
    }

    let branch = resolve_default_branch(&repo_path).await?;
    if branch.is_empty() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository has no commits yet"));
    }

    let out = Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .arg("show")
        .arg(format!("{branch}:{file_path}"))
        .output()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    if !out.status.success() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("file not found"));
    }

    let file_name = file_path.rsplit('/').next().unwrap_or(&file_path);
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/octet-stream")
        .header("Content-Disposition", format!("attachment; filename=\"{file_name}\""))
        .body(out.stdout))
}

/// `%XX`パーセントエンコーディングの最小限デコード(外部crateを増やさず、
/// このエンドポイントが受け取るASCII中心のファイルパス用途に絞った実装)。
fn urlencoding_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET /api/repos/:name/zip?paths=a,b,c` — ZIPダウンロード。
/// `paths`省略時はリポジトリ全体、指定時は`git archive`のpathspecとして
/// 渡し、指定したファイル/フォルダのみを含むZIPを生成する
/// (フォルダ・複数ファイルの選択ダウンロードを1つの経路で実現)。
#[handler]
async fn get_zip(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::Download).await?;

    let branch = resolve_default_branch(&repo_path).await?;
    if branch.is_empty() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository has no commits yet"));
    }

    let query = req.uri().query().unwrap_or("");
    let paths: Vec<String> = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("paths="))
        .map(|v| urlencoding_decode(v).split(',').map(str::to_string).collect())
        .unwrap_or_default();
    for p in &paths {
        sanitize_tree_path(p)?;
    }

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(&repo_path).arg("archive").arg("--format=zip").arg(&branch);
    if !paths.is_empty() {
        cmd.arg("--");
        for p in &paths {
            cmd.arg(p);
        }
    }
    let out = cmd.output().await.map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    if !out.status.success() {
        return Err(poem::Error::from_string(
            format!("git archive failed: {}", String::from_utf8_lossy(&out.stderr)),
            poem::http::StatusCode::BAD_REQUEST,
        ));
    }

    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/zip")
        .header("Content-Disposition", format!("attachment; filename=\"{name}.zip\""))
        .body(out.stdout))
}

#[derive(Deserialize)]
struct RequestOtpRequest {
    email: String,
}

/// `POST /api/auth/request-otp` — 指定メール宛にOTPを送信する。
/// 管理者メール、または管理者が[`accounts`]で許可登録したメール
/// アドレスのみ受け付ける(未登録なら`403`)。SMTP未設定の場合は`503`
/// (open-easy-webと同じグレースフルデグレード方針)。
#[handler]
async fn request_otp(state: Data<&AppState>, body: poem::web::Json<RequestOtpRequest>) -> PoemResult<Response> {
    let email = body.email.trim().to_string();
    if email != state.admin_email {
        let registered = accounts::load(&state.repos_root).await;
        if !registered.emails.contains(&email) {
            return Ok(Response::builder().status(poem::http::StatusCode::FORBIDDEN).body("email not registered"));
        }
    }
    let Some(smtp) = state.smtp.clone() else {
        return Ok(Response::builder().status(poem::http::StatusCode::SERVICE_UNAVAILABLE).body("SMTP not configured"));
    };
    let auth::RequestOtpOutcome::Issued(code) = state.auth.request_otp(&email);
    match mail::send_otp(smtp, email, code).await {
        Ok(()) => Ok(Response::builder().status(poem::http::StatusCode::OK).body("otp sent")),
        Err(e) => {
            tracing::warn!("failed to send OTP mail: {e}");
            Ok(Response::builder().status(poem::http::StatusCode::BAD_GATEWAY).body("failed to send mail"))
        }
    }
}

#[derive(Deserialize)]
struct VerifyOtpRequest {
    email: String,
    code: String,
}

#[derive(serde::Serialize)]
struct VerifyOtpResponse {
    token: String,
}

/// `POST /api/auth/verify-otp` — OTPコードを検証し、成功すれば
/// (提示したメールアドレスに対する)セッショントークンを発行する。
#[handler]
async fn verify_otp(state: Data<&AppState>, body: poem::web::Json<VerifyOtpRequest>) -> PoemResult<Response> {
    match state.auth.consume_otp(&body.email, &body.code) {
        Ok(()) => {
            let token = state.auth.create_session(&body.email);
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

#[derive(Deserialize)]
struct AddAccountRequest {
    email: String,
}

/// `POST /api/accounts` — ログイン可能なメールアドレスを1件登録する
/// (管理者のみ)。
#[handler]
async fn add_account(req: &Request, state: Data<&AppState>, body: poem::web::Json<AddAccountRequest>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let email = body.email.trim().to_string();
    if !email.contains('@') {
        return Ok(Response::builder().status(poem::http::StatusCode::BAD_REQUEST).body("invalid email"));
    }
    if state.accounts_locked && email != state.admin_email {
        return Ok(Response::builder()
            .status(poem::http::StatusCode::FORBIDDEN)
            .body("account registration is currently restricted to the administrator email only"));
    }
    let mut store = accounts::load(&state.repos_root).await;
    store.emails.insert(email);
    accounts::save(&state.repos_root, &store)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Response::builder().status(poem::http::StatusCode::CREATED).body("ok"))
}

/// `GET /api/accounts` — 登録済みメールアドレス一覧(管理者のみ)。
#[handler]
async fn list_accounts(req: &Request, state: Data<&AppState>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let store = accounts::load(&state.repos_root).await;
    let mut emails: Vec<&String> = store.emails.iter().collect();
    emails.sort();
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&emails).unwrap_or_default()))
}

/// `DELETE /api/accounts/:email` — アカウント登録を解除する(管理者のみ)。
/// 既存のセッション自体は自然失効まで有効(即時失効はv0.1.0では未実装)。
#[handler]
async fn remove_account(req: &Request, PathExtractor(email): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let mut store = accounts::load(&state.repos_root).await;
    if !store.emails.remove(&email) {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("account not found"));
    }
    accounts::save(&state.repos_root, &store)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Response::builder().status(poem::http::StatusCode::OK).body("ok"))
}

#[derive(Deserialize)]
struct AccessRequestPayload {
    email: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// `POST /api/accounts/request` — **認証不要、誰でも申請可能**。
/// 「このメールアドレスに、このリポジトリへのアクセスを許可してほしい」
/// という申請を保留リストへ追加し、管理者へメール通知する。
/// 申請しただけではまだ何も見えるようにならない——管理者が
/// [`decide_access_request`]で許可するまでは無効。
#[handler]
async fn request_access(state: Data<&AppState>, body: poem::web::Json<AccessRequestPayload>) -> PoemResult<Response> {
    let email = body.email.trim().to_string();
    if !email.contains('@') {
        return Ok(Response::builder().status(poem::http::StatusCode::BAD_REQUEST).body("invalid email"));
    }
    let mut store = accounts::load(&state.repos_root).await;
    let id = accounts::generate_request_id();
    store.pending_requests.push(accounts::AccessRequest {
        id,
        email: email.clone(),
        repo: body.repo.clone(),
        message: body.message.clone(),
        is_create_repo_request: false,
    });
    accounts::save(&state.repos_root, &store)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;

    if let Some(smtp) = state.smtp.clone() {
        if let Err(e) = mail::send_access_request_notice(smtp, state.admin_email.clone(), email, body.repo.clone(), body.message.clone()).await {
            tracing::warn!("failed to notify admin of access request: {e}");
        }
    }
    Ok(Response::builder().status(poem::http::StatusCode::CREATED).body("request submitted"))
}

/// `GET /api/accounts/requests` — 保留中の申請一覧(管理者のみ)。
#[handler]
async fn list_access_requests(req: &Request, state: Data<&AppState>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let store = accounts::load(&state.repos_root).await;
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&store.pending_requests).unwrap_or_default()))
}

#[derive(Deserialize)]
struct DecideAccessRequestPayload {
    approve: bool,
    #[serde(default)]
    allow_view: bool,
    #[serde(default)]
    allow_download: bool,
    #[serde(default)]
    allow_push: bool,
}

/// `POST /api/accounts/requests/:id/decide` — 申請を審査する(管理者のみ)。
/// **閲覧・ダウンロード・push を個別に選んで許可**できる(一部だけの
/// 許可も、全部拒否〈却下〉も可能)。承認時、対象リポジトリが
/// 指定されていればそのリポジトリの`access::AccessConfig::accounts`に
/// 書き込む(リポジトリ指定が無い申請は、アカウント登録
/// 〈ログイン自体の許可〉のみ行う——個別リポジトリへの権限は別途
/// [`set_access`]で付与する)。
#[handler]
async fn decide_access_request(
    req: &Request,
    PathExtractor(id): PathExtractor<String>,
    state: Data<&AppState>,
    body: poem::web::Json<DecideAccessRequestPayload>,
) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let mut store = accounts::load(&state.repos_root).await;
    let Some(pos) = store.pending_requests.iter().position(|r| r.id == id) else {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("request not found"));
    };
    let request = store.pending_requests.remove(pos);

    if body.approve && state.accounts_locked && request.email != state.admin_email {
        // 申請自体は削除済みだが、まだ`emails`へは登録していない状態で
        // 保存し直す(却下と同じ扱い)。管理者へは理由を明示して返す。
        accounts::save(&state.repos_root, &store)
            .await
            .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
        return Ok(Response::builder()
            .status(poem::http::StatusCode::FORBIDDEN)
            .body("account registration is currently restricted to the administrator email only"));
    }

    if body.approve {
        store.emails.insert(request.email.clone());
    }
    accounts::save(&state.repos_root, &store)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;

    if body.approve {
        if let Some(repo_name) = &request.repo {
            let repo_dir_name = sanitize_repo_name(repo_name)?;
            let repo_path = state.repos_root.join(&repo_dir_name);
            if repo_path.exists() {
                let mut config = access::load(&repo_path).await;
                config.accounts.insert(
                    request.email.clone(),
                    access::AccountPermission { allow_view: body.allow_view, allow_download: body.allow_download, allow_push: body.allow_push },
                );
                access::save(&repo_path, &config)
                    .await
                    .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
            }
        }
    }

    if let Some(smtp) = state.smtp.clone() {
        if let Err(e) = mail::send_access_decision(smtp, request.email.clone(), body.approve, request.repo.clone()).await {
            tracing::warn!("failed to notify requester of decision: {e}");
        }
    }
    Ok(Response::builder().status(poem::http::StatusCode::OK).body(if body.approve { "approved" } else { "denied" }))
}

/// `PUT /repos/:name` — bareリポジトリを新規作成する(`git init --bare`)。
/// ログイン必須。**管理者、または`can_create_repos`許可を持つ登録
/// アカウント**が対象(ユーザー要件: 作成権限自体は誰にでも開放しない)。
/// さらに**管理者自身の作成も含め**、[`capacity::decide`]による
/// ディスク空き容量の自動判定を必ず通す(空き容量不足なら`507
/// Insufficient Storage`)。既に存在する場合は`409 Conflict`を返す。
#[handler]
async fn create_repo(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let identity = session_identity(req, &state);
    let is_admin = identity.as_deref() == Some(state.admin_email.as_str());
    if !is_admin {
        let allowed = match &identity {
            Some(email) => {
                let accounts = accounts::load(&state.repos_root).await;
                accounts.emails.contains(email) && accounts.can_create_repos.contains(email)
            }
            None => false,
        };
        if !allowed {
            return Err(poem::Error::from_string("repository creation not permitted for this account", poem::http::StatusCode::FORBIDDEN));
        }
    }

    let decision = capacity::decide(&state.repos_root);
    if !decision.allowed {
        tracing::warn!("repository creation denied by capacity policy: {decision:?}");
        return Ok(Response::builder()
            .status(poem::http::StatusCode::INSUFFICIENT_STORAGE)
            .content_type("application/json")
            .body(serde_json::to_vec(&decision).unwrap_or_default()));
    }

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

#[derive(Deserialize)]
struct SetCreatePermissionRequest {
    allow: bool,
}

/// `PUT /api/accounts/:email/create-permission` — そのアカウントに
/// リポジトリ新規作成を許可するか(管理者のみ)。
#[handler]
async fn set_create_permission(
    req: &Request,
    PathExtractor(email): PathExtractor<String>,
    state: Data<&AppState>,
    body: poem::web::Json<SetCreatePermissionRequest>,
) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let mut store = accounts::load(&state.repos_root).await;
    if !store.emails.contains(&email) {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("account not found"));
    }
    if body.allow {
        store.can_create_repos.insert(email);
    } else {
        store.can_create_repos.remove(&email);
    }
    accounts::save(&state.repos_root, &store)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Response::builder().status(poem::http::StatusCode::OK).body("ok"))
}

/// `GET /api/capacity` — 現在のディスク容量判定結果(誰でも参照可能、
/// 秘匿情報ではない——「今リポジトリを作れるか」はUI側が事前に案内する
/// のに有用)。
#[handler]
async fn get_capacity(state: Data<&AppState>) -> PoemResult<Response> {
    let decision = capacity::decide(&state.repos_root);
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&decision).unwrap_or_default()))
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

/// git smart HTTPの`PATH_INFO`から、対象リポジトリのディレクトリ名と、
/// clone/pull(`git-upload-pack`)かpush(`git-receive-pack`)かを判定する。
/// `/{repo}.git/info/refs?service=git-upload-pack`(ハンドシェイクGET)・
/// `/{repo}.git/git-upload-pack`・`/{repo}.git/git-receive-pack`
/// (実データPOST)のいずれの形も扱う。判定できない経路(想定外の
/// PATH_INFO)は`None`を返し、呼び出し側はアクセス制御をスキップする
/// (`git_http_backend`自体が404を返すため実害は無い)。
fn parse_git_repo_and_need(path_info: &str, query_string: &str) -> Option<(String, access::Need)> {
    let trimmed = path_info.trim_start_matches('/');
    let (repo_segment, rest) = trimmed.split_once('/')?;
    if !repo_segment.ends_with(".git") {
        return None;
    }
    let need = if rest == "git-receive-pack" || query_string.contains("service=git-receive-pack") {
        access::Need::Push
    } else {
        access::Need::Download
    };
    Some((repo_segment.to_string(), need))
}

#[handler]
async fn git_get(req: &Request, state: Data<&AppState>) -> PoemResult<Response> {
    let path_info = req.uri().path().to_string();
    let query_string = req.uri().query().unwrap_or("").to_string();
    if let Some((repo_dir, need)) = parse_git_repo_and_need(&path_info, &query_string) {
        let repo_path = state.repos_root.join(&repo_dir);
        if repo_path.exists() {
            if let Some(err_resp) = git_access_error(req, &state, &repo_path, need).await {
                return Ok(err_resp);
            }
        }
    }
    git_http_backend(&path_info, &query_string, "GET", "", Body::empty(), &state.repos_root).await
}

#[handler]
async fn git_post(req: &Request, body: Body, state: Data<&AppState>) -> PoemResult<Response> {
    let path_info = req.uri().path().to_string();
    let query_string = req.uri().query().unwrap_or("").to_string();
    let content_type = req.header(poem::http::header::CONTENT_TYPE).unwrap_or("").to_string();
    if let Some((repo_dir, need)) = parse_git_repo_and_need(&path_info, &query_string) {
        let repo_path = state.repos_root.join(&repo_dir);
        if repo_path.exists() {
            if let Some(err_resp) = git_access_error(req, &state, &repo_path, need).await {
                return Ok(err_resp);
            }
        }
    }
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
    let accounts_locked = std::env::var("RGIT_ACCOUNTS_LOCKED").map(|v| v != "false" && v != "0").unwrap_or(true);
    if accounts_locked {
        tracing::info!("account registration is locked to the admin email only (RGIT_ACCOUNTS_LOCKED=false to lift)");
    }
    let state = AppState { repos_root, auth: Arc::new(auth::AuthStore::default()), admin_email, smtp, accounts_locked };

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
        .at("/api/repos/:name/access", get(get_access).put(set_access))
        .at("/api/repos/:name/tree", get(get_tree))
        .at("/api/repos/:name/zip", get(get_zip))
        .at("/api/repos/:name/raw/*filepath", get(get_raw_file))
        .at("/api/auth/request-otp", post(request_otp))
        .at("/api/auth/verify-otp", post(verify_otp))
        .at("/api/auth/logout", post(logout))
        .at("/api/groups", get(list_groups).post(create_group))
        .at("/api/groups/:name", poem::delete(delete_group))
        .at("/api/accounts", get(list_accounts).post(add_account))
        .at("/api/accounts/:email", poem::delete(remove_account))
        .at("/api/accounts/request", post(request_access))
        .at("/api/accounts/requests", get(list_access_requests))
        .at("/api/accounts/requests/:id/decide", post(decide_access_request))
        .at("/api/accounts/:email/create-permission", put(set_create_permission))
        .at("/api/capacity", get(get_capacity))
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
