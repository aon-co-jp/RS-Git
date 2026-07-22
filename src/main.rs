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
//! - Webhook
//!
//! **Wiki**は実装済み(2026-07-22)。GitHub/GitLab/Gitea同様、各リポジトリ
//! `<name>.git`の兄弟として`<name>.wiki.git`というbareリポジトリを持つ
//! だけの設計——Wikiページの実体はそのリポジトリ内の`.md`ファイルであり、
//! 編集はWeb UIからではなく`git clone`/`git push`で行う(このリポジトリ
//! 自体がまだWeb版ファイルエディタを持たないことと一貫させた判断)。
//! アクセス制御は`<name>.git`本体と**同じ**[`access::AccessConfig`]を
//! 共有する(Wiki専用の権限体系は持たない、閲覧=View/push=Push)。
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
mod issues;
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
use serde::{Deserialize, Serialize};
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

/// デモ用アカウントの固定識別子。実在のメールアドレスと衝突しないよう
/// `.invalid`(RFC 2606で予約された、実在解決されないTLD)を使う。
/// このIDでログインしたセッションは、閲覧(View)・ダウンロード(Download)
/// のみ常に許可し、push(Push)は各リポジトリの設定に関わらず**常に拒否**
/// する(ユーザー指示、2026-07-22: 「だれでもログイン出来ても...pushなどは
/// 出来ない仕様」)。管理操作(アカウント/グループ管理等)は
/// `require_admin_session`が別途`admin_email`と一致するかを見るため、
/// デモIDでは通らない(追加の分岐不要)。
const DEMO_IDENTITY: &str = "demo@rgit.invalid";

async fn is_allowed_for_git(req: &Request, state: &AppState, repo_path: &Path, need: access::Need) -> bool {
    let identity = session_identity(req, state);
    if identity.as_deref() == Some(state.admin_email.as_str()) {
        return true;
    }
    if identity.as_deref() == Some(DEMO_IDENTITY) {
        return matches!(need, access::Need::View | access::Need::Download);
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

/// `<name>.git`から兄弟Wikiリポジトリ`<name>.wiki.git`のディレクトリ名を
/// 導出する。`repo_dir_name`は[`sanitize_repo_name`]済み(`.git`サフィックス
/// 保証済み)であることが前提。
fn wiki_dir_name(repo_dir_name: &str) -> String {
    let base = repo_dir_name.strip_suffix(".git").unwrap_or(repo_dir_name);
    format!("{base}.wiki.git")
}

/// git smart HTTP経路(`git_get`/`git_post`)向け——リクエストされた
/// リポジトリディレクトリ名(`<name>.git`または`<name>.wiki.git`)から、
/// アクセス制御設定([`access::AccessConfig`])をどのディレクトリから
/// 読むべきかを解決する。Wikiは本体リポジトリと**同じ**権限を共有する
/// ため、`<name>.wiki.git`は常に`<name>.git`側の設定を見る。
fn access_config_dir(repo_dir_name: &str, repos_root: &Path) -> PathBuf {
    match repo_dir_name.strip_suffix(".wiki.git") {
        Some(base) => repos_root.join(format!("{base}.git")),
        None => repos_root.join(repo_dir_name),
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
                    // `<name>.wiki.git`はWikiの実体であり、通常のリポジトリ
                    // 一覧には出さない(README表示等の対象ではないため、
                    // 管理者から見ても紛らわしいだけ——admin判定より前に弾く)。
                    if name.ends_with(".wiki.git") {
                        continue;
                    }
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

/// `GET /api/repos/:name/wiki` — Wikiページ名の一覧(`<name>.wiki.git`の
/// デフォルトブランチにあるファイルの`git ls-tree`)。アクセス権は本体
/// リポジトリ`<name>.git`の[`access::Need::View`]と共有する。**Wikiリポジトリ
/// にコミットが1つも無い場合(まだ誰も`git push`していない場合)はエラー
/// ではなく空配列を返す**(要件通り、`PUT /repos/:name`で自動作成される
/// 直後のWikiは常にこの状態のため)。
#[handler]
async fn get_wiki_pages(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::View).await?;

    let wiki_path = state.repos_root.join(wiki_dir_name(&repo_dir_name));
    if !wiki_path.exists() {
        return Ok(Response::builder()
            .status(poem::http::StatusCode::OK)
            .content_type("application/json")
            .body(serde_json::to_vec::<Vec<String>>(&Vec::new()).unwrap_or_default()));
    }

    let branch = resolve_default_branch(&wiki_path).await?;
    if branch.is_empty() {
        return Ok(Response::builder()
            .status(poem::http::StatusCode::OK)
            .content_type("application/json")
            .body(serde_json::to_vec::<Vec<String>>(&Vec::new()).unwrap_or_default()));
    }

    let out = Command::new("git")
        .arg("-C")
        .arg(&wiki_path)
        .arg("ls-tree")
        .arg("-r")
        .arg("--name-only")
        .arg(&branch)
        .output()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    if !out.status.success() {
        // `symbolic-ref`はHEADが指す**ブランチ名**を返すだけで、その
        // ブランチが実在する(コミットがある)かは保証しない
        // (bareリポジトリ作成直後はHEADが`refs/heads/master`等を指す
        // ものの、コミットは0個)。この場合`ls-tree`は失敗するが、
        // それも「まだページが無い」という正常な状態として扱う
        // (要件通り、エラーではなく空配列)。
        return Ok(Response::builder()
            .status(poem::http::StatusCode::OK)
            .content_type("application/json")
            .body(serde_json::to_vec::<Vec<String>>(&Vec::new()).unwrap_or_default()));
    }
    let files: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap_or("").lines().collect();
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&files).unwrap_or_default()))
}

/// `GET /api/repos/:name/wiki/:page` — Wikiの1ページの中身を返す
/// (`git show <branch>:<page>`、README表示と同じ「gitコマンドに任せる」
/// 方針)。`page`はツリー内のファイル名そのもの(通常は`Home.md`等)。
#[handler]
async fn get_wiki_page(
    req: &Request,
    PathExtractor((name, page)): PathExtractor<(String, String)>,
    state: Data<&AppState>,
) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::View).await?;

    let page = urlencoding_decode(&page);
    sanitize_tree_path(&page)?;
    if page.is_empty() {
        return Err(poem::Error::from_string("page name required", poem::http::StatusCode::BAD_REQUEST));
    }

    let wiki_path = state.repos_root.join(wiki_dir_name(&repo_dir_name));
    if !wiki_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("wiki has no pages yet"));
    }

    let branch = resolve_default_branch(&wiki_path).await?;
    if branch.is_empty() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("wiki has no pages yet"));
    }

    let show_out = Command::new("git")
        .arg("-C")
        .arg(&wiki_path)
        .arg("show")
        .arg(format!("{branch}:{page}"))
        .output()
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    if !show_out.status.success() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("wiki page not found"));
    }

    let content = String::from_utf8_lossy(&show_out.stdout).to_string();
    let payload = ReadmeResponse { branch, content };
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&payload).unwrap_or_default()))
}

/// `GET /api/repos/:name/issues` — Issue一覧。Wikiと同じく、本体
/// リポジトリの[`access::Need::View`]で権限判定する(Issue専用の権限
/// 系統は持たない)。**シリアライズは[`rust_json::full`](RJSON/RS-JSON、
/// `serde_json::Value`のこのエコシステム版ラッパー)を使う**——ユーザー
/// 指示により、このリポジトリのHTTP層で`poem::web::Json`(内部で
/// serde_jsonへ直結)を使わず、自前のJSONクレートに揃える。
#[handler]
async fn list_issues(req: &Request, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::View).await?;

    let issues = issues::list(&repo_path).await;
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(rust_json::full::to_vec_strict(&issues).unwrap_or_default()))
}

#[derive(serde::Deserialize)]
struct CreateIssueRequest {
    title: String,
    body: String,
}

/// リクエストボディの生バイト列を読み取り、[`rust_json::full`]
/// (RJSON/RS-JSON)経由でパースする。`poem::web::Json`抽出子は使わない
/// (ユーザー指示、このエコシステム自前のJSONクレートへ統一するため)。
async fn read_json_body<T: serde::de::DeserializeOwned>(body: poem::Body) -> PoemResult<T> {
    let bytes = body.into_vec().await.map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::BAD_REQUEST))?;
    rust_json::full::from_slice_strict(&bytes).map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::BAD_REQUEST))
}

/// `POST /api/repos/:name/issues` — Issue新規作成。作成には
/// `access::Need::Push`(このリポジトリへの書き込み権)を要求する
/// ——誰でも作れる掲示板にはしない、という判断(要件で明示されて
/// いないため、既存の最も近い権限概念〈push〉を流用した)。
#[handler]
async fn create_issue(req: &Request, body: poem::Body, PathExtractor(name): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::Push).await?;
    let body: CreateIssueRequest = read_json_body(body).await?;
    if body.title.trim().is_empty() {
        return Ok(Response::builder().status(poem::http::StatusCode::BAD_REQUEST).body("title must not be empty"));
    }
    let author = session_identity(req, &state).unwrap_or_else(|| "anonymous".to_string());
    let issue = issues::create(&repo_path, body.title.clone(), body.body.clone(), author)
        .await
        .map_err(|e| poem::Error::from_string(e.to_string(), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Response::builder()
        .status(poem::http::StatusCode::CREATED)
        .content_type("application/json")
        .body(rust_json::full::to_vec_strict(&issue).unwrap_or_default()))
}

#[handler]
async fn get_issue(req: &Request, PathExtractor((name, id)): PathExtractor<(String, u64)>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::View).await?;

    match issues::get(&repo_path, id).await {
        Some(issue) => Ok(Response::builder()
            .status(poem::http::StatusCode::OK)
            .content_type("application/json")
            .body(rust_json::full::to_vec_strict(&issue).unwrap_or_default())),
        None => Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("issue not found")),
    }
}

#[derive(serde::Deserialize)]
struct SetIssueStatusRequest {
    status: issues::IssueStatus,
}

#[handler]
async fn set_issue_status(req: &Request, body: poem::Body, PathExtractor((name, id)): PathExtractor<(String, u64)>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::Push).await?;
    let body: SetIssueStatusRequest = read_json_body(body).await?;

    match issues::set_status(&repo_path, id, body.status).await {
        Ok(()) => Ok(Response::builder().status(poem::http::StatusCode::OK).content_type("application/json").body("{\"ok\":true}")),
        Err(issues::SetStatusError::NotFound) => Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("issue not found")),
    }
}

#[derive(serde::Deserialize)]
struct CreateCommentRequest {
    body: String,
}

#[handler]
async fn create_issue_comment(req: &Request, body: poem::Body, PathExtractor((name, id)): PathExtractor<(String, u64)>, state: Data<&AppState>) -> PoemResult<Response> {
    let repo_dir_name = sanitize_repo_name(&name)?;
    let repo_path = state.repos_root.join(&repo_dir_name);
    if !repo_path.exists() {
        return Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("repository not found"));
    }
    check_access(req, &state, &repo_path, access::Need::Push).await?;
    let body: CreateCommentRequest = read_json_body(body).await?;
    if body.body.trim().is_empty() {
        return Ok(Response::builder().status(poem::http::StatusCode::BAD_REQUEST).body("comment body must not be empty"));
    }
    let author = session_identity(req, &state).unwrap_or_else(|| "anonymous".to_string());
    match issues::add_comment(&repo_path, id, author, body.body.clone()).await {
        Some(comment) => Ok(Response::builder()
            .status(poem::http::StatusCode::CREATED)
            .content_type("application/json")
            .body(rust_json::full::to_vec_strict(&comment).unwrap_or_default())),
        None => Ok(Response::builder().status(poem::http::StatusCode::NOT_FOUND).body("issue not found")),
    }
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

/// `POST /api/auth/demo-login` — OTP不要で誰でも即座にログインできる
/// デモ用エンドポイント。一般公開デモンストレーション向け(ユーザー指示、
/// 2026-07-22)。発行されるセッションは[`DEMO_IDENTITY`]に紐づき、
/// 閲覧・ダウンロード・README閲覧は可能だが**push・管理操作は一切
/// できない**(`is_allowed_for_git`・`require_admin_session`で拒否)。
#[handler]
async fn demo_login(state: Data<&AppState>) -> PoemResult<Response> {
    let token = state.auth.create_session(DEMO_IDENTITY);
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&VerifyOtpResponse { token }).unwrap_or_default()))
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

#[derive(Serialize)]
struct AccountDetail {
    email: String,
    registered: bool,
    can_create_repos: bool,
}

/// `GET /api/accounts/:email` — 個別アカウントの登録状態を返す
/// (管理者のみ)。WASM管理UIが`can_create_repos`の現在値を反映した
/// チェックボックス/インジケータを描画できるよう、既存の
/// `list_accounts`(メール一覧のみ)を補完するために追加
/// (2026-07-21、CLAUDE.mdのHANDOFF記載の宿題への対応)。
/// 未登録のメールアドレスでも`404`にはせず
/// `registered: false, can_create_repos: false`を返す
/// (「まだ登録されていない」という状態も呼び出し側が扱いやすいように)。
#[handler]
async fn get_account(req: &Request, PathExtractor(email): PathExtractor<String>, state: Data<&AppState>) -> PoemResult<Response> {
    require_admin_session(req, &state)?;
    let store = accounts::load(&state.repos_root).await;
    let detail = AccountDetail {
        registered: store.emails.contains(&email),
        can_create_repos: store.can_create_repos.contains(&email),
        email,
    };
    Ok(Response::builder()
        .status(poem::http::StatusCode::OK)
        .content_type("application/json")
        .body(serde_json::to_vec(&detail).unwrap_or_default()))
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

    // Wikiリポジトリを兄弟として自動作成する(要件5: 「enable wiki」の
    // ような別ステップ無しに、作成直後から`git clone .../<name>.wiki.git`
    // できる状態にする)。コミットは1つも無い空リポジトリのままで良く、
    // 一覧・閲覧API側(get_wiki_pages/get_wiki_page)がその状態を
    // エラーではなく「ページ0件」として扱う。
    let wiki_path = state.repos_root.join(wiki_dir_name(&repo_dir_name));
    let wiki_status = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&wiki_path)
        .status()
        .await
        .map_err(|e| poem::Error::from_string(format!("failed to spawn git init for wiki: {e}"), poem::http::StatusCode::INTERNAL_SERVER_ERROR))?;
    if !wiki_status.success() {
        tracing::warn!("git init --bare failed for wiki repo {:?}", wiki_path);
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
            let access_path = access_config_dir(&repo_dir, &state.repos_root);
            if let Some(err_resp) = git_access_error(req, &state, &access_path, need).await {
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
            let access_path = access_config_dir(&repo_dir, &state.repos_root);
            if let Some(err_resp) = git_access_error(req, &state, &access_path, need).await {
                return Ok(err_resp);
            }
        }
    }
    git_http_backend(&path_info, &query_string, "POST", &content_type, body, &state.repos_root).await
}

/// ルーティング定義を`main()`とテスト(`poem::test::TestClient`)の両方から
/// 再利用できるように切り出したもの(2026-07-21追記)。
fn build_routes(state: AppState, static_dir: &str) -> impl poem::Endpoint {
    Route::new()
        .at("/healthz", get(healthz))
        .at("/repos", get(list_repos))
        .at("/repos/:name", put(create_repo))
        .at("/api/repos", get(list_repos))
        .at("/api/repos/:name/readme", get(get_readme))
        .at("/api/repos/:name/wiki", get(get_wiki_pages))
        .at("/api/repos/:name/wiki/:page", get(get_wiki_page))
        .at("/api/repos/:name/issues", get(list_issues).post(create_issue))
        .at("/api/repos/:name/issues/:id", get(get_issue).put(set_issue_status))
        .at("/api/repos/:name/issues/:id/comments", post(create_issue_comment))
        .at("/api/repos/:name/access", get(get_access).put(set_access))
        .at("/api/repos/:name/tree", get(get_tree))
        .at("/api/repos/:name/zip", get(get_zip))
        .at("/api/repos/:name/raw/*filepath", get(get_raw_file))
        .at("/api/auth/request-otp", post(request_otp))
        .at("/api/auth/verify-otp", post(verify_otp))
        .at("/api/auth/demo-login", post(demo_login))
        .at("/api/auth/logout", post(logout))
        .at("/api/groups", get(list_groups).post(create_group))
        .at("/api/groups/:name", poem::delete(delete_group))
        .at("/api/accounts", get(list_accounts).post(add_account))
        .at("/api/accounts/:email", get(get_account).delete(remove_account))
        .at("/api/accounts/request", post(request_access))
        .at("/api/accounts/requests", get(list_access_requests))
        .at("/api/accounts/requests/:id/decide", post(decide_access_request))
        .at("/api/accounts/:email/create-permission", put(set_create_permission))
        .at("/api/capacity", get(get_capacity))
        .nest(
            "/ui",
            poem::endpoint::StaticFilesEndpoint::new(static_dir).index_file("index.html"),
        )
        .at("/*path", get(git_get).post(git_post))
        .data(state)
        .with(Tracing)
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

    let app = build_routes(state, &static_dir);

    let port = std::env::var("RGIT_PORT").unwrap_or_else(|_| "8090".to_string());
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("listening on {addr}");
    Server::new(TcpListener::bind(addr)).run(app).await?;
    Ok(())
}

#[cfg(test)]
mod handler_tests {
    //! `poem::test::TestClient`を使ったハンドラレベルのテスト
    //! (2026-07-21追記、`GET /api/accounts/:email`〈新規追加分〉の検証)。

    use super::*;
    use poem::test::TestClient;

    const ADMIN_EMAIL: &str = "admin@example.com";

    async fn make_state(label: &str) -> AppState {
        let unique = format!("{:?}-{label}", std::time::Instant::now());
        let repos_root = std::env::temp_dir().join(format!("rgit-handler-test-{}", unique.replace(['{', '}', ':', ' ', '.'], "-")));
        tokio::fs::create_dir_all(&repos_root).await.unwrap();
        AppState { repos_root, auth: Arc::new(auth::AuthStore::default()), admin_email: ADMIN_EMAIL.to_string(), smtp: None, accounts_locked: false }
    }

    #[tokio::test]
    async fn get_account_reflects_can_create_repos_state() {
        let state = make_state("get-account").await;
        let repos_root = state.repos_root.clone();
        let token = state.auth.create_session(ADMIN_EMAIL);
        let app = build_routes(state, "./static");
        let client = TestClient::new(app);

        // 未登録のメールアドレスは404ではなく registered:false を返す。
        let resp = client
            .get("/api/accounts/nobody@example.com")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await;
        resp.assert_status_is_ok();
        let body: serde_json::Value = serde_json::from_slice(&resp.0.into_body().into_bytes().await.unwrap()).unwrap();
        assert_eq!(body["registered"], false);
        assert_eq!(body["can_create_repos"], false);

        // アカウント登録+作成許可を付与した状態で反映されることを確認。
        let mut store = accounts::load(&repos_root).await;
        store.emails.insert("member@example.com".to_string());
        store.can_create_repos.insert("member@example.com".to_string());
        accounts::save(&repos_root, &store).await.unwrap();

        let resp2 = client
            .get("/api/accounts/member@example.com")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await;
        resp2.assert_status_is_ok();
        let body2: serde_json::Value = serde_json::from_slice(&resp2.0.into_body().into_bytes().await.unwrap()).unwrap();
        assert_eq!(body2["registered"], true);
        assert_eq!(body2["can_create_repos"], true);
    }

    #[tokio::test]
    async fn get_account_requires_admin_session() {
        let state = make_state("get-account-unauth").await;
        let app = build_routes(state, "./static");
        let client = TestClient::new(app);

        let resp = client.get("/api/accounts/member@example.com").send().await;
        resp.assert_status(poem::http::StatusCode::UNAUTHORIZED);
    }

    // --- Wiki機能(2026-07-22追記) ---

    /// `PUT /repos/:name`実行時、本体`<name>.git`だけでなく兄弟の
    /// `<name>.wiki.git`(空のbareリポジトリ)も自動作成されることを確認。
    #[tokio::test]
    async fn create_repo_also_creates_wiki_sibling() {
        let state = make_state("create-repo-wiki").await;
        let repos_root = state.repos_root.clone();
        let token = state.auth.create_session(ADMIN_EMAIL);
        let app = build_routes(state, "./static");
        let client = TestClient::new(app);

        let resp = client
            .put("/repos/demo")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await;
        resp.assert_status(poem::http::StatusCode::CREATED);

        assert!(repos_root.join("demo.git").is_dir(), "main bare repo should exist");
        assert!(repos_root.join("demo.wiki.git").is_dir(), "wiki bare repo should exist alongside it");

        // 空リポジトリ(コミット無し)なので、一覧APIはエラーではなく
        // 空配列を返す(要件通り)。
        let wiki_resp = client
            .get("/api/repos/demo/wiki")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await;
        wiki_resp.assert_status_is_ok();
        let body: serde_json::Value = serde_json::from_slice(&wiki_resp.0.into_body().into_bytes().await.unwrap()).unwrap();
        assert_eq!(body, serde_json::json!([]));
    }

    async fn run_git(cwd: Option<&std::path::Path>, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new("git");
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.args(args);
        cmd.output().await.expect("failed to spawn git")
    }

    /// 実際に生きたHTTPサーバーを`127.0.0.1`のエフェメラルポートで起動し、
    /// テスト終了時に呼び出し側が`handle.abort()`できるようにする
    /// (モックではなく本物の`git http-backend`橋渡し経路を通す)。
    async fn spawn_real_server(state: AppState) -> (u16, tokio::task::JoinHandle<()>) {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = std_listener.local_addr().unwrap().port();
        drop(std_listener);
        let app = build_routes(state, "./static");
        let addr = format!("127.0.0.1:{port}");
        let handle = tokio::spawn(async move {
            Server::new(TcpListener::bind(addr)).run(app).await.ok();
        });
        // サーバーがacceptを開始するまでの短い猶予(ポーリングではなく
        // 固定の短いスリープ——このリポジトリの他のE2E検証と同水準)。
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        (port, handle)
    }

    /// 実際の`git clone`/`git push`コマンド(サブプロセス、モック無し)で
    /// `<name>.wiki.git`へページを書き込み、別ディレクトリへの再cloneで
    /// 中身が正しく取得できること、および`GET
    /// /api/repos/:name/wiki`・`/wiki/:page`が同じ内容を返すことを確認する。
    #[tokio::test]
    async fn wiki_repo_git_clone_push_roundtrip() {
        let state = make_state("wiki-roundtrip").await;
        let repos_root = state.repos_root.clone();
        let token = state.auth.create_session(ADMIN_EMAIL);
        let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{ADMIN_EMAIL}:{token}"));
        let auth_header = format!("http.extraheader=Authorization: Basic {basic}");

        // リポジトリ+Wikiを本物のAPI経由で作成。
        {
            let app = build_routes(state.clone(), "./static");
            let client = TestClient::new(app);
            let resp = client
                .put("/repos/proj")
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await;
            resp.assert_status(poem::http::StatusCode::CREATED);
        }
        assert!(repos_root.join("proj.wiki.git").is_dir());

        let (port, handle) = spawn_real_server(state).await;
        let wiki_url = format!("http://127.0.0.1:{port}/proj.wiki.git");

        let work_dir = std::env::temp_dir().join(format!("rgit-wiki-clone-{port}"));
        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        tokio::fs::create_dir_all(&work_dir).await.unwrap();

        // 1) clone(まだコミット無しの空リポジトリ)。
        let clone_out = run_git(None, &["-c", &auth_header, "clone", &wiki_url, work_dir.to_str().unwrap()]).await;
        assert!(clone_out.status.success(), "git clone failed: {}", String::from_utf8_lossy(&clone_out.stderr));

        // 2) ページを追加してpush。**ローカルの現在のブランチ名**
        // (clone直後、`init.defaultBranch`に従って決まる——`master`か
        // `main`かは環境依存)へpushする。空リポジトリのbareリポジトリの
        // `HEAD`シンボリック参照はこの同じ名前を既に指しているため、
        // 別名(例えば固定で`main`)へpushすると、コミットが存在しない
        // 側の名前をHEADが指したままになり、再clone時にワークツリーが
        // 空になってしまう(実機検証で発見)。
        let branch_out = run_git(Some(&work_dir), &["symbolic-ref", "--short", "HEAD"]).await;
        assert!(branch_out.status.success(), "failed to read local default branch");
        let local_branch = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();

        tokio::fs::write(work_dir.join("Home.md"), "# Welcome\n\nRGit Wiki roundtrip test.\n").await.unwrap();
        let add_out = run_git(Some(&work_dir), &["add", "Home.md"]).await;
        assert!(add_out.status.success());
        let commit_out = run_git(
            Some(&work_dir),
            &["-c", "user.email=test@example.com", "-c", "user.name=RGit Test", "commit", "-m", "add Home page"],
        )
        .await;
        assert!(commit_out.status.success(), "git commit failed: {}", String::from_utf8_lossy(&commit_out.stderr));
        let push_out = run_git(Some(&work_dir), &["-c", &auth_header, "push", "origin", &format!("HEAD:refs/heads/{local_branch}")]).await;
        assert!(push_out.status.success(), "git push failed: {}", String::from_utf8_lossy(&push_out.stderr));

        // 3) 別ディレクトリへ再cloneし、実際にpushされた内容が取得できることを確認。
        let reclone_dir = std::env::temp_dir().join(format!("rgit-wiki-reclone-{port}"));
        let _ = tokio::fs::remove_dir_all(&reclone_dir).await;
        let reclone_out = run_git(None, &["-c", &auth_header, "clone", &wiki_url, reclone_dir.to_str().unwrap()]).await;
        assert!(reclone_out.status.success(), "git re-clone failed: {}", String::from_utf8_lossy(&reclone_out.stderr));
        let recloned_content = tokio::fs::read_to_string(reclone_dir.join("Home.md")).await.unwrap();
        assert!(recloned_content.contains("RGit Wiki roundtrip test."));

        // 4) HTTP APIからも同じ内容が見えることを確認(GET /api/repos/:name/wiki, /wiki/:page)。
        let list_resp = reqwest_like_get(port, "/api/repos/proj/wiki", Some(&token)).await;
        assert!(list_resp.contains("Home.md"), "wiki page list should contain Home.md, got: {list_resp}");
        let page_resp = reqwest_like_get(port, "/api/repos/proj/wiki/Home.md", Some(&token)).await;
        assert!(page_resp.contains("RGit Wiki roundtrip test."), "wiki page content mismatch: {page_resp}");

        handle.abort();
        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        let _ = tokio::fs::remove_dir_all(&reclone_dir).await;
    }

    /// 追加の外部HTTPクレートを増やさないための、テスト専用の最小限GET
    /// クライアント(生TCP + 手組みHTTP/1.1リクエスト)。本体側は
    /// `poem`/`tokio`のみなので、テストコードもここでは標準ライブラリの
    /// 範囲(`tokio::net::TcpStream`)で完結させる。
    async fn reqwest_like_get(port: u16, path: &str, bearer: Option<&str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        let auth_line = bearer.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{auth_line}\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        let text = String::from_utf8_lossy(&buf);
        text.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
    }

    /// Wikiのアクセス制御が本体リポジトリと**同じ**[`access::AccessConfig`]
    /// を共有すること(独立の権限系統を持たないこと)を確認する:
    /// private設定の本体リポジトリでは、Wiki一覧APIも(本体と同様)
    /// 未ログインで403になり、本体を`public`+`allow_view`にすればWiki
    /// 一覧も未ログインで見えるようになる。
    #[tokio::test]
    async fn wiki_access_control_mirrors_main_repo() {
        let state = make_state("wiki-access").await;
        let repos_root = state.repos_root.clone();
        let token = state.auth.create_session(ADMIN_EMAIL);
        let app = build_routes(state, "./static");
        let client = TestClient::new(app);

        let create_resp = client
            .put("/repos/secret")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await;
        create_resp.assert_status(poem::http::StatusCode::CREATED);

        // private(既定)のままなら、未ログインでのWiki一覧取得は拒否される。
        let anon_resp = client.get("/api/repos/secret/wiki").send().await;
        anon_resp.assert_status(poem::http::StatusCode::FORBIDDEN);

        // 本体リポジトリのアクセス設定をpublic+閲覧許可に変更する
        // (Wiki専用の設定APIは存在しない——本体と共有という設計通り)。
        let repo_path = repos_root.join("secret.git");
        let config = access::AccessConfig { mode: access::Mode::Public, group: None, allow_view: true, allow_download: false, allow_push: false, accounts: std::collections::HashMap::new() };
        access::save(&repo_path, &config).await.unwrap();

        let anon_resp2 = client.get("/api/repos/secret/wiki").send().await;
        anon_resp2.assert_status_is_ok();

        // git smart HTTP側(push)も同じ設定を見る: pushは許可していないため
        // 未認証pushへは401(WWW-Authenticate、gitクライアント再送仕様)になる。
        let git_push_resp = client
            .post("/secret.wiki.git/git-receive-pack")
            .content_type("application/x-git-receive-pack-request")
            .send()
            .await;
        git_push_resp.assert_status(poem::http::StatusCode::UNAUTHORIZED);
    }
}
