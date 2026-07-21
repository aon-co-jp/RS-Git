# RGit

GitbucketのGo言語版のRust＋RPoem版の新規開発を開始。自己ホスト型Git forge。

**開発開始日: 2026-07-21**(このリポジトリのGitHub作成日)

## 現状(v0.1.0)

> ⚠️ **正直な開示**: 現時点ではGitea/GitBucketが持つWeb UI・Issue・
> Pull Request・Wiki・ユーザー認証・Webhookは**一切実装していません**。
> 実装済みなのは以下のみです。

- `git clone` / `git push` / `git fetch`(git smart HTTPプロトコル、`git http-backend`をCGI橋渡し) — 実機確認済み
- `PUT /repos/:name` によるbareリポジトリの新規作成
- `GET /repos` によるリポジトリ一覧取得
- `GET /healthz` ヘルスチェック

## 起動方法

```
RGIT_DATA_DIR=./data/repos RGIT_PORT=8090 cargo run --release
```

## なぜRustで作り直すか

Gitea(Go製)は512MBのメモリでも動作する実績があるが、それでもJVM製の
GitBucket等と比べて軽量。RGitはさらに、Rustの所有権システムによる
メモリ安全性・省メモリ性を活かし、小規模VPSでの自己ホストをより
現実的にすることを目指す。

## ライセンス

Apache-2.0
