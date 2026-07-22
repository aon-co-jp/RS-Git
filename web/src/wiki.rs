//! Wiki表示UI(2026-07-22追加)。`GET /api/repos/:name/wiki`(ページ名一覧)・
//! `GET /api/repos/:name/wiki/:page`(1ページの内容)を`fetch`し、README表示
//! (`web/src/lib.rs`の`load_readme`)と同じ「Markdown→HTMLはブラウザ側で
//! 変換」「JSONパースは`rust_json::parse_light`のみ」という方針をそのまま
//! 踏襲する。
//!
//! **正直な開示(v0.1.0)**: Web版のページ編集機能(GitHubのWiki編集画面
//! 相当)は無い。Wikiの実体は`<repo>.wiki.git`という素のbareリポジトリで、
//! 編集は`git clone`してMarkdownファイルを書き、`git push`するだけ
//! (通常のリポジトリと全く同じフロー、権限も本体リポジトリと共有)。
//! このモジュールはあくまで**閲覧**専用。

use crate::{fetch_text, markdown_to_html, parse_string_array};
use rust_json::parse_light;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Document, Element};

fn document() -> Document {
    web_sys::window().expect("no window").document().expect("no document")
}

fn set_html(id: &str, html: &str) {
    if let Some(el) = document().get_element_by_id(id) {
        el.set_inner_html(html);
    }
}

/// `{"branch": "...", "content": "..."}`(READMEと同じ形)を読む。
fn parse_page_fields(text: &str) -> Option<(String, String)> {
    let value = parse_light(text).ok()?;
    let branch = value.get("branch")?.as_str()?.to_string();
    let content = value.get("content")?.as_str()?.to_string();
    Some((branch, content))
}

/// クローン/push手順の案内(v0.1.0はWeb版エディタが無いことの説明)。
fn edit_instructions_html(repo: &str) -> String {
    format!(
        "<p class=\"wiki-edit-note\">✏️ このWikiはリポジトリと同じgitで管理されています。編集するには:</p>\
         <pre><code>git clone &lt;サーバーURL&gt;/{repo}.wiki.git\n\
cd {repo}.wiki\n\
# ページ(.mdファイル)を追加・編集\n\
git add . &amp;&amp; git commit -m \"update wiki\"\n\
git push</code></pre>\
         <p class=\"wiki-edit-note\">アクセス権限は本体リポジトリの閲覧/ダウンロード/push許可と共有されます。</p>"
    )
}

async fn load_page(repo: String, page: String) {
    set_html("wiki-content", "<p><em>読み込み中...</em></p>");
    let url = format!("/api/repos/{repo}/wiki/{page}");
    match fetch_text(&url).await {
        Ok(text) => match parse_page_fields(&text) {
            Some((_branch, content)) => {
                set_html("wiki-content", &markdown_to_html(&content));
            }
            None => set_html("wiki-content", "<p><em>ページを読み込めませんでした。</em></p>"),
        },
        Err(_) => set_html("wiki-content", "<p><em>ページの読み込みに失敗しました。</em></p>"),
    }
}

fn render_page_list(repo: &str, pages: &[String]) {
    let doc = document();
    let Some(list) = doc.get_element_by_id("wiki-list") else { return };
    list.set_inner_html("");
    for page in pages {
        let li = doc.create_element("li").unwrap();
        let a = doc.create_element("a").unwrap();
        a.set_attribute("href", "#").ok();
        a.set_attribute("data-wiki-repo", repo).ok();
        a.set_attribute("data-wiki-page", page).ok();
        a.set_class_name("wiki-link");
        a.set_text_content(Some(page));
        li.append_child(&a).ok();
        list.append_child(&li).ok();
    }
    if pages.is_empty() {
        let li = doc.create_element("li").unwrap();
        li.set_text_content(Some("(まだページがありません)"));
        list.append_child(&li).ok();
    }
}

/// `#wiki-list`へのクリックをイベント委譲で拾う(`web/src/lib.rs`の
/// `wire_repo_list_clicks`と同じ手法)。リポジトリを切り替えるたびに
/// 呼ばれるので、リスナーは複数貼られる想定で問題ない(要素自体を
/// `render_page_list`で毎回作り直しているため、古いリスナーは古い要素
/// ごと破棄される)。
fn wire_page_list_clicks() {
    let doc = document();
    let Some(list) = doc.get_element_by_id("wiki-list") else { return };
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        let Some(target) = event.target() else { return };
        let Ok(el) = target.dyn_into::<Element>() else { return };
        let mut node: Option<Element> = Some(el);
        while let Some(current) = node {
            if let (Some(repo), Some(page)) = (current.get_attribute("data-wiki-repo"), current.get_attribute("data-wiki-page")) {
                event.prevent_default();
                wasm_bindgen_futures::spawn_local(load_page(repo, page));
                return;
            }
            node = current.parent_element();
        }
    });
    list.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref()).ok();
    closure.forget();
}

/// リポジトリを選んだとき(`load_readme`と同時)に呼ぶ。ページ名一覧を
/// 取得して`#wiki-list`へ描画し、編集手順の案内も更新する。
pub async fn load_wiki_list(repo: String) {
    set_html("wiki-content", "<p><em>左のページ名をクリックすると内容を表示します。</em></p>");
    set_html("wiki-edit-instructions", &edit_instructions_html(&repo));
    let url = format!("/api/repos/{repo}/wiki");
    match fetch_text(&url).await {
        Ok(text) => {
            let pages = parse_string_array(&text);
            render_page_list(&repo, &pages);
            wire_page_list_clicks();
        }
        Err(_) => {
            if let Some(el) = document().get_element_by_id("wiki-list") {
                el.set_inner_html("<li>Wiki一覧の読み込みに失敗しました。</li>");
            }
        }
    }
}
