# 開発方針＆開発環境ルール(RGit)

作業ドライブは`F:\runo`。この節は[`open-raid-z`](https://github.com/aon-co-jp/open-raid-z)の
`CLAUDE.md`を正本とし、各プロジェクトへコピーして同期する方針に準じる。
GitHubリポジトリ: [aon-co-jp/RGit](https://github.com/aon-co-jp/RGit)。

> ⚠️ **正直な開示(最重要)**: v0.1.0時点では、git smart HTTPプロトコル
> によるclone/push/fetchのみを実装。Gitea/GitBucketが持つWeb UI・Issue・
> Pull Request・Wiki・ユーザー認証・Webhookは一切無い。`README.md`参照。

## このプロジェクトの役割

Gitea(Go製)のRust版を目指す、自己ホスト型Git forge。GitHub上の
`aon-co-jp`組織の全リポジトリをバックアップ目的で自己ホスト環境へ
ミラーする用途を最初の実用シナリオとする(GitBucket/Gitea導入の代替)。

## 技術スタック

`aruaru-llm`・`e-gov.info`と同じ方針: `poem`クレートを直接利用する
単純なHTTPサービス。DB非依存(Gitリポジトリ自体がデータストア)。

## 実装方式

Gitプロトコル自体を再実装せず、`git http-backend`(gitに標準同梱される
CGIプログラム)をサブプロセスとして起動し、HTTPリクエストをCGI環境変数
(`PATH_INFO`/`QUERY_STRING`/`REQUEST_METHOD`/`CONTENT_TYPE`)へ変換して
橋渡しする(`src/main.rs`の`git_http_backend`関数)。認証は未実装
(`REMOTE_USER`は固定値"rgit")。

## HANDOFF

- **2026-07-21 新規作成・実機検証**: `runo-forge`という仮称で開発を
  開始した後、`aon-co-jp/RGit`という既存の空リポジトリ(説明文
  「Gitea(Go製)のRust版」)が見つかったため、正式名称を`RGit`に統一。
  ローカルで実機検証済み: `PUT /repos/:name`でbareリポジトリ作成→
  `git clone`→ファイル追加・commit→`git push`→別ディレクトリへ再clone
  →push内容が正しく取得できることを確認(モックではなく実際の`git`
  コマンドとの相互運用性を確認)。
  - 次にすべきこと: (1) GitHubの空リポジトリへ初回push、(2) VPS
    (conoha)へのデプロイ(systemdサービス化)、(3) `aon-co-jp`組織の
    全リポジトリをバックアップ目的でミラーする同期スクリプトとの接続。
