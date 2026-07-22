//! RGit WASMフロントエンド(v0.1.0)。
//!
//! `/api/repos`・`/api/repos/:name/readme`(RGit本体、poem/tokio/hyper製)を
//! `fetch`で叩き、Markdown→HTML変換をブラウザ側(WASM)で行う。サーバー側は
//! JSONを返すだけで済むため、GitHubのREADME表示相当の機能をサーバー負荷
//! 最小(計算をクライアントに逃がす)で実現する狙い。
//!
//! **省メモリ最適化(このパスで実施)**: `serde`/`serde_json`はWASM
//! バイナリサイズへの影響が大きいため使わない。JSONパースも当初は
//! ブラウザ組み込みの`JSON.parse`(`js_sys::JSON`)へ委譲する案だったが、
//! それだと`Reflect::get`でフィールドを読むたびにWASM↔JS境界を1回ずつ
//! 跨ぐ。以前はこのクレート内に自作の最小JSONパーサを持っていたが、
//! [aon-co-jp/RJSON](https://github.com/aon-co-jp/RJSON)(`rust-json`
//! クレート)の`light`モジュールへ統合した(2026-07-21)——依存ゼロの
//! ブラウザ`JSON.parse`相当を1回パースし、以降はネイティブRust値
//! (`String`/`Vec`)として扱う——境界越えの呼び出し回数そのものを削減する。
//! `rust-json`は`default-features = false`(`Cargo.toml`参照)で依存し、
//! `serde_json`を要求する`full` featureはビルド対象に含まれない。
//! 加えて`opt-level="z"`+LTO+`panic=abort`+`strip=true`
//! (`Cargo.toml`参照)でバイナリを極小化している。
//!
//! **正直な開示**: v0.1.0はリポジトリ一覧+README表示のみ。GitHubにある
//! ディレクトリツリー表示・コミット履歴・シンタックスハイライト等は未実装。

mod admin;
mod auth;
mod wiki;

use rust_json::{parse_light, LightValue};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Document, Element};

fn document() -> Document {
    web_sys::window().expect("no window").document().expect("no document")
}

/// `auth::api_url`と同じ接頭辞規約(`/rgit`マウント、モジュールdoc参照)。
pub(crate) async fn fetch_text(url: &str) -> Result<String, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let resp_value = JsFuture::from(window.fetch_with_str(&auth::api_url(url))).await?;
    let resp: web_sys::Response = resp_value.dyn_into()?;
    let text_value = JsFuture::from(resp.text()?).await?;
    Ok(text_value.as_string().unwrap_or_default())
}

pub(crate) fn markdown_to_html(src: &str) -> String {
    let parser = pulldown_cmark::Parser::new(src);
    let mut html_out = String::new();
    pulldown_cmark::html::push_html(&mut html_out, parser);
    html_out
}

fn show_status(msg: &str) {
    if let Some(el) = document().get_element_by_id("status") {
        el.set_text_content(Some(msg));
    }
}

/// `rust_json::parse_light`(RJSONの`light`モジュール)で文字列配列を
/// パースする。
pub(crate) fn parse_string_array(text: &str) -> Vec<String> {
    let Ok(value) = parse_light(text) else { return Vec::new() };
    let Some(items) = value.as_array() else { return Vec::new() };
    items.iter().filter_map(LightValue::as_str).map(str::to_string).collect()
}

/// `{"branch": "...", "content": "..."}`から2フィールドを
/// `rust_json::parse_light`で直接読む(型を作らず、必要な2値だけを
/// その場で取り出す)。
fn parse_readme_fields(text: &str) -> Option<(String, String)> {
    let value = parse_light(text).ok()?;
    let branch = value.get("branch")?.as_str()?.to_string();
    let content = value.get("content")?.as_str()?.to_string();
    Some((branch, content))
}

async fn load_readme(repo: String) {
    show_status(&format!("{repo} のREADMEを読み込み中..."));
    wasm_bindgen_futures::spawn_local(wiki::load_wiki_list(repo.clone()));
    let url = format!("/api/repos/{repo}/readme");
    match fetch_text(&url).await {
        Ok(text) => match parse_readme_fields(&text) {
            Some((branch, content)) => {
                if let Some(el) = document().get_element_by_id("readme") {
                    el.set_inner_html(&markdown_to_html(&content));
                }
                show_status(&format!("{repo} (branch: {branch})"));
            }
            None => {
                if let Some(el) = document().get_element_by_id("readme") {
                    el.set_inner_html("<p><em>README.md が見つかりませんでした。</em></p>");
                }
                show_status(&format!("{repo}: README.md無し"));
            }
        },
        Err(_) => show_status(&format!("{repo}: 読み込みに失敗しました")),
    }
}

fn render_repo_list(names: &[String]) {
    let doc = document();
    let Some(list) = doc.get_element_by_id("repo-list") else { return };
    list.set_inner_html("");
    for name in names {
        let li = doc.create_element("li").unwrap();
        let a = doc.create_element("a").unwrap();
        a.set_attribute("href", "#").ok();
        a.set_attribute("data-repo", name).ok();
        a.set_class_name("repo-link");
        a.set_text_content(Some(name));
        li.append_child(&a).ok();
        list.append_child(&li).ok();
    }
}

/// `#repo-list`へのクリックをイベント委譲で拾い、`data-repo`属性から
/// リポジトリ名を取り出して`load_readme`を起動する。
fn wire_repo_list_clicks() {
    let doc = document();
    let Some(list) = doc.get_element_by_id("repo-list") else { return };

    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        let Some(target) = event.target() else { return };
        let Ok(el) = target.dyn_into::<Element>() else { return };
        // クリックされたのが<a>内の子要素でも、data-repo属性を持つ祖先まで遡る。
        let mut node: Option<Element> = Some(el);
        while let Some(current) = node {
            if let Some(repo) = current.get_attribute("data-repo") {
                event.prevent_default();
                wasm_bindgen_futures::spawn_local(load_readme(repo));
                return;
            }
            node = current.parent_element();
        }
    });
    list.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref()).ok();
    closure.forget(); // リスナーはページ寿命全体で有効にするため意図的にリーク(SPA1ページのみのv0.1.0では許容)
}

async fn load_repo_list() {
    show_status("リポジトリ一覧を読み込み中...");
    match fetch_text("/api/repos").await {
        Ok(text) => {
            let names = parse_string_array(&text);
            render_repo_list(&names);
            wire_repo_list_clicks();
            show_status(&format!("{}件のリポジトリ", names.len()));
        }
        Err(_) => show_status("リポジトリ一覧の読み込みに失敗しました"),
    }
}

/// `{"allowed": bool, "free_bytes": u64, "min_free_bytes": u64}`を
/// `rust_json::parse_light`で直接読む。数値は`LightValue::as_f64`経由
/// (JSONに浮動小数として保持される、`u64`最大値付近の誤差は表示用途では
/// 許容)。
async fn load_capacity() {
    let Ok((_, text)) = auth::authorized_fetch("/api/capacity", "GET", None).await else { return };
    let Some(value) = parse_light(&text).ok() else { return };
    let allowed = value.get("allowed").and_then(LightValue::as_bool).unwrap_or(false);
    let free_bytes = value.get("free_bytes").and_then(LightValue::as_f64).unwrap_or(0.0);
    let free_gb = free_bytes / 1_073_741_824.0;
    let msg = if allowed {
        format!("空き容量: {free_gb:.1}GB (作成可)")
    } else {
        format!("空き容量: {free_gb:.1}GB (残量不足のため新規作成不可)")
    };
    if let Some(el) = document().get_element_by_id("capacity-status") {
        el.set_text_content(Some(&msg));
    }
}

/// ログイン成功後に呼ばれる。ログイン状態でリポジトリ一覧・容量表示が
/// 変わりうるため再読み込みする(v0.1.0はアクセス制御ありのプライベート
/// リポジトリ一覧切り替えを想定)。
fn reload_after_login() {
    wasm_bindgen_futures::spawn_local(load_repo_list());
    wasm_bindgen_futures::spawn_local(load_capacity());
    admin::refresh_all();
}

#[wasm_bindgen(start)]
pub fn start() {
    auth::wire_auth_ui();
    admin::wire_admin_ui();
    wasm_bindgen_futures::spawn_local(load_repo_list());
    wasm_bindgen_futures::spawn_local(load_capacity());
    admin::refresh_all();
}
