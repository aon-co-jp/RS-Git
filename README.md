# RS-Git

Giteaが持つ機能のうち、まずgit clone/push・OTPログイン・アクセス制御・README閲覧機能だけを実装した、Go言語版GiteaのRust＋RPoem版です。元の機能を素直に移植致しました。自己ホスト型Git forge。

**開発開始日: 2026-07-21**(このリポジトリのGitHub作成日、2026-07-22に`RGit`から`RS-Git`へ改名)

## 現状(v0.1.0)

> ⚠️ **正直な開示**: Gitea/GitBucketが持つIssue・Pull Request・
> Webhookは**まだ実装していません**。Wikiは実装しました(2026-07-22)。
> それ以外(Web UI・認証・アクセス制御)は実装済みです。実装済みなのは
> 以下の通りです。

- `git clone` / `git push` / `git fetch`(git smart HTTPプロトコル、`git http-backend`をCGI橋渡し、アクセス権限に応じてpush可否を判定) — 実機確認済み
- `PUT /repos/:name` によるbareリポジトリの新規作成(ディスク容量に応じた自動判定付き)
- `GET /repos` によるリポジトリ一覧取得
- `GET /healthz` ヘルスチェック
- **OTPメールログイン**(管理者+管理者が許可した登録アカウント)
- **だれでもログイン(デモ用)**: OTP不要、閲覧・ダウンロード・README閲覧のみ可能、pushは管理者のみ
- **アクセス制御**: private/public/groupの3モード、アカウント単位で閲覧・ダウンロード・push権限を個別設定
- **自己申請フロー**: 誰でもアクセス許可を申請でき、管理者がメールで気づいて承認/却下(閲覧/DL/push個別許可)
- **Wiki**: 各リポジトリ`<name>.git`の兄弟として`<name>.wiki.git`という素のbareリポジトリを自動作成(GitHub/GitLab/Gitea同様の設計)。閲覧はWeb UI(`GET /api/repos/:name/wiki`・`/wiki/:page`)、編集は`git clone`/`git push`で行う(Web版ページエディタは無い、正直な開示)。アクセス権限は本体リポジトリと共有(別権限系統は持たない)。
- **WASM製Web UI**(`/ui/`): リポジトリ一覧・README閲覧・Wiki閲覧・ログイン・管理パネル(申請一覧・アカウント管理・グループ管理・リポジトリ別アクセス設定)

## 起動方法

```
RGIT_DATA_DIR=./data/repos RGIT_PORT=8090 cargo run --release
```

## なぜRustで作り直すか

Gitea(Go製)は512MBのメモリでも動作する実績があるが、それでもJVM製の
GitBucket等と比べて軽量。RS-Gitはさらに、Rustの所有権システムによる
メモリ安全性・省メモリ性を活かし、小規模VPSでの自己ホストをより
現実的にすることを目指す。

## ライセンス

Apache-2.0
