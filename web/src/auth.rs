//! ログインUI(メールOTP)。`POST /api/auth/{request-otp,verify-otp,logout}`を
//! `fetch`で叩き、セッショントークンを`localStorage`(キー`rgit_token`・
//! `rgit_email`)へ保存する。以後の認証付きリクエストは
//! `Authorization: Bearer <token>`ヘッダを付与する(`authorized_fetch`)。
//!
//! JSONパースは`web/src/lib.rs`と同じ方針で`rust_json::parse_light`を使う。
//! `RequestInit`+`Headers`で`POST`+`Content-Type: application/json`の
//! リクエストを組み立てる。

use rust_json::parse_light;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Document, Headers, Request, RequestInit, RequestMode, Response, Storage};

const TOKEN_KEY: &str = "rgit_token";
const EMAIL_KEY: &str = "rgit_email";

/// このデプロイでRGitがマウントされているパス接頭辞
/// (`https://runo.tokyo/rgit`、2026-07-21)。絶対パスの`fetch`呼び出しは
/// ブラウザの現在ページのパスと無関係にオリジン直下へ飛ぶため、nginx側で
/// `/rgit`を剥がしてバックエンドへプロキシしていても、フロントエンドの
/// 側でこの接頭辞を明示的に付けないと`/api/...`がドメイン直下(接頭辞無し)
/// を叩いてしまう。**正直な開示**: 現状はこの1箇所にハードコードして
/// おり、複数のマウント先(例: 別ドメイン・別パス)で同じビルドを使い
/// 回すことは想定していない——将来必要になれば、ビルド時環境変数や
/// `index.html`側の設定注入に置き換えること。
const BASE_PATH: &str = "/rs-git";

pub fn api_url(path: &str) -> String {
    format!("{BASE_PATH}{path}")
}

fn document() -> Document {
    web_sys::window().expect("no window").document().expect("no document")
}

fn local_storage() -> Option<Storage> {
    web_sys::window()?.local_storage().ok()?
}

pub fn stored_token() -> Option<String> {
    local_storage()?.get_item(TOKEN_KEY).ok()?
}

pub fn stored_email() -> Option<String> {
    local_storage()?.get_item(EMAIL_KEY).ok()?
}

fn store_session(email: &str, token: &str) {
    if let Some(storage) = local_storage() {
        storage.set_item(TOKEN_KEY, token).ok();
        storage.set_item(EMAIL_KEY, email).ok();
    }
}

fn clear_session() {
    if let Some(storage) = local_storage() {
        storage.remove_item(TOKEN_KEY).ok();
        storage.remove_item(EMAIL_KEY).ok();
    }
}

fn set_text(id: &str, text: &str) {
    if let Some(el) = document().get_element_by_id(id) {
        el.set_text_content(Some(text));
    }
}

fn show(id: &str, visible: bool) {
    if let Some(el) = document().get_element_by_id(id) {
        if visible {
            el.class_list().remove_1("hidden").ok();
        } else {
            el.class_list().add_1("hidden").ok();
        }
    }
}

fn input_value(id: &str) -> String {
    document()
        .get_element_by_id(id)
        .and_then(|el| el.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|input| input.value())
        .unwrap_or_default()
}

/// 認証付き`fetch`。ログイン済みなら`Authorization: Bearer <token>`を付与する。
/// `body`が`Some`ならJSON POST、`None`ならGET相当のメソッド指定なし。
pub async fn authorized_fetch(url: &str, method: &str, body: Option<&str>) -> Result<(u16, String), JsValue> {
    let opts = RequestInit::new();
    opts.set_method(method);
    opts.set_mode(RequestMode::SameOrigin);

    let headers = Headers::new()?;
    if body.is_some() {
        headers.set("Content-Type", "application/json")?;
    }
    if let Some(token) = stored_token() {
        headers.set("Authorization", &format!("Bearer {token}"))?;
    }
    opts.set_headers(&headers);
    if let Some(b) = body {
        opts.set_body(&JsValue::from_str(b));
    }

    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let request = Request::new_with_str_and_init(&api_url(url), &opts)?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request)).await?;
    let resp: Response = resp_value.dyn_into()?;
    let status = resp.status();
    let text_value = JsFuture::from(resp.text()?).await?;
    Ok((status, text_value.as_string().unwrap_or_default()))
}

fn show_auth_error(msg: &str) {
    set_text("auth-error", msg);
}

/// ログイン状態(メールアドレス)に応じて`#auth-anon`/`#auth-logged-in`の
/// 表示を切り替える。
pub fn refresh_auth_ui() {
    match stored_email() {
        Some(email) => {
            set_text("auth-email", &format!("ログイン中: {email}"));
            show("auth-anon", false);
            show("auth-logged-in", true);
        }
        None => {
            show("auth-anon", true);
            show("auth-logged-in", false);
        }
    }
}

async fn request_otp() {
    show_auth_error("");
    let email = input_value("login-email");
    if !email.contains('@') {
        show_auth_error("メールアドレスを入力してください");
        return;
    }
    let body = format!(r#"{{"email":"{}"}}"#, json_escape(&email));
    match authorized_fetch("/api/auth/request-otp", "POST", Some(&body)).await {
        Ok((200, _)) => {
            show("login-code", true);
            show("btn-verify-otp", true);
            show_auth_error("OTPを送信しました。届いたコードを入力してください。");
        }
        Ok((403, _)) => show_auth_error("このメールアドレスは登録されていません"),
        Ok((503, _)) => show_auth_error("サーバーのメール設定が未完了です"),
        Ok((status, _)) => show_auth_error(&format!("OTP送信に失敗しました(status {status})")),
        Err(_) => show_auth_error("OTP送信に失敗しました(通信エラー)"),
    }
}

async fn verify_otp() {
    show_auth_error("");
    let email = input_value("login-email");
    let code = input_value("login-code");
    if code.is_empty() {
        show_auth_error("コードを入力してください");
        return;
    }
    let body = format!(r#"{{"email":"{}","code":"{}"}}"#, json_escape(&email), json_escape(&code));
    match authorized_fetch("/api/auth/verify-otp", "POST", Some(&body)).await {
        Ok((200, text)) => match parse_light(&text).ok().and_then(|v| v.get("token")?.as_str().map(str::to_string)) {
            Some(token) => {
                store_session(&email, &token);
                refresh_auth_ui();
                show_auth_error("");
                super::reload_after_login();
            }
            None => show_auth_error("ログインに失敗しました(応答解析エラー)"),
        },
        Ok((_, _)) => show_auth_error("コードが正しくありません"),
        Err(_) => show_auth_error("ログインに失敗しました(通信エラー)"),
    }
}

async fn logout() {
    let _ = authorized_fetch("/api/auth/logout", "POST", None).await;
    clear_session();
    refresh_auth_ui();
}

pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn wire_click(id: &str, f: impl Fn() + 'static) {
    let doc = document();
    let Some(el) = doc.get_element_by_id(id) else { return };
    let closure = Closure::<dyn FnMut()>::new(f);
    el.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref()).ok();
    closure.forget();
}

/// ログインフォーム(OTP送信・ログイン・ログアウトボタン)のイベント
/// リスナーを配線する。`start()`から一度だけ呼ぶ。
pub fn wire_auth_ui() {
    refresh_auth_ui();
    wire_click("btn-request-otp", || wasm_bindgen_futures::spawn_local(request_otp()));
    wire_click("btn-verify-otp", || wasm_bindgen_futures::spawn_local(verify_otp()));
    wire_click("btn-logout", || wasm_bindgen_futures::spawn_local(logout()));
}
