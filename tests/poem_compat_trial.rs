//! `open-runo-poem-compat`(RPoem)をRS-Gitの依存グラフの中で実際に
//! 試用する統合テスト。
//!
//! **正直な開示・スコープ**: RS-Gitに`[lib]`ターゲットが無く
//! (`main.rs`のみのバイナリクレート)、`src/issues.rs`等の既存モジュールは
//! この`tests/`統合テストから直接importできない。そのため本テストは
//! 「既存の本番ハンドラをそのまま移植した」ものではなく、RS-Gitが実際に
//! 使っている依存(`open-runo-poem-compat`・`rust-json`のfullモジュール)
//! **だけ**を使い、Issue一覧・作成に相当する最小限のロジックを
//! その場で組み立てて、実TCP経由で動くことを確認する試用
//! (トライアル)である。本番`main.rs`のpoem実装をRPoemへ置き換える
//! 作業そのものは、この試用結果を見てから判断する——今回はまだ実施
//! していない。
use open_runo_poem_compat::{get, Method, Params, Request, Response, Route, Server, StatusCode, TcpListener};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrialIssue {
    id: u64,
    title: String,
}

// テスト用の簡易共有状態(本番のRS-Git AppStateはData抽出子経由で渡すが、
// 本ファサードはまだ型駆動のData抽出子を実装していないため、この試用
// テストではRust標準のstaticで代用する——正直な簡略化)。
fn issues_store() -> &'static Mutex<Vec<TrialIssue>> {
    static STORE: OnceLock<Mutex<Vec<TrialIssue>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Vec::new()))
}

#[open_runo_poem_compat_macro::handler]
async fn list_issues_trial(_req: Request, _params: Params) -> Response {
    let issues = issues_store().lock().unwrap().clone();
    hyper::Response::builder()
        .status(StatusCode::OK)
        .body(open_runo_poem_compat::fixed_body(bytes::Bytes::from(
            rust_json::full::to_vec_strict(&issues).unwrap_or_default(),
        )))
        .unwrap()
}

#[open_runo_poem_compat_macro::handler]
async fn create_issue_trial(req: Request, _params: Params) -> Response {
    let created: Result<TrialIssue, Response> = open_runo_poem_compat::hyper_compat::read_json_body(req).await;
    match created {
        Ok(mut issue) => {
            let mut issues = issues_store().lock().unwrap();
            issue.id = issues.len() as u64;
            issues.push(issue.clone());
            hyper::Response::builder()
                .status(StatusCode::CREATED)
                .body(open_runo_poem_compat::fixed_body(bytes::Bytes::from(
                    rust_json::full::to_vec_strict(&issue).unwrap_or_default(),
                )))
                .unwrap()
        }
        Err(resp) => resp,
    }
}

async fn spawn(app: Route) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    Server::new(TcpListener::bind(([127, 0, 0, 1], 0))).run(app).await.expect("bind ephemeral port")
}

#[tokio::test]
async fn rs_json_and_poem_compat_serve_issue_crud_over_real_tcp() {
    // RS-Git本体のIssue機能と同じ2クレート(open-runo-poem-compat・
    // rust-json)の組み合わせが、実際にビルド・実TCP経由で動作することを
    // 確認する(RS-Git本体のハンドラそのものではない、上記docコメント
    // 参照)。
    let app = Route::new()
        .at("/trial/issues", get(list_issues_trial()).post(create_issue_trial()))
        .with_cors();
    let (addr, handle) = spawn(app).await;

    // 作成前は空配列。
    let empty = get_body(addr, "/trial/issues").await;
    assert_eq!(empty, b"[]");

    // 1件作成。
    let created = post_body(addr, "/trial/issues", br#"{"id":0,"title":"hello"}"#).await;
    assert!(String::from_utf8_lossy(&created).contains("hello"));

    // 一覧に反映されている。
    let after = get_body(addr, "/trial/issues").await;
    assert!(String::from_utf8_lossy(&after).contains("hello"));

    handle.abort();
}

async fn get_body(addr: SocketAddr, path: &str) -> Vec<u8> {
    send(addr, Method::GET, path, None).await
}
async fn post_body(addr: SocketAddr, path: &str, body: &[u8]) -> Vec<u8> {
    send(addr, Method::POST, path, Some(body.to_vec())).await
}

async fn send(addr: SocketAddr, method: Method, path: &str, body: Option<Vec<u8>>) -> Vec<u8> {
    use http_body_util::BodyExt;
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(conn);
    let body_bytes = body.unwrap_or_default();
    let req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .body(http_body_util::Full::new(bytes::Bytes::from(body_bytes)))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    resp.into_body().collect().await.unwrap().to_bytes().to_vec()
}
