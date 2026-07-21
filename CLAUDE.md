# 開発方針＆開発環境ルール(RGit)

作業ドライブは`F:\runo`。この節は[`open-raid-z`](https://github.com/aon-co-jp/open-raid-z)の
`CLAUDE.md`を正本とし、各プロジェクトへコピーして同期する方針に準じる。
GitHubリポジトリ: [aon-co-jp/RGit](https://github.com/aon-co-jp/RGit)。

> ⚠️ **正直な開示(最重要、2026-07-21更新)**: git smart HTTPプロトコル
> によるclone/push/fetch、OTPログイン(管理者+登録アカウント)、
> リポジトリ単位のアクセス制御(private/public/group/アカウント個別、
> 閲覧・ダウンロード・push個別許可)、自己申請フローまで実装済み。
> Gitea/GitBucketが持つIssue・Pull Request・Wiki・Webhookはまだ無い。
> Web UI側もログイン画面・アクセス許可設定画面はまだ無い(現状APIのみ、
> `curl`での動作確認止まり)。`README.md`参照。

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

- **2026-07-21 GitHub初回push・VPSデプロイ完了、README表示機能に着手
  (未検証部分あり、雷雨のため中断・チェックポイント)**:
  1. **完了・実機検証済み**: GitHubへの初回push成功
     ([aon-co-jp/RGit](https://github.com/aon-co-jp/RGit))。VPS(conoha)
     上でclone→`cargo build --release`→systemdサービス化
     (`/etc/systemd/system/rgit.service`)し、`healthz`で稼働確認済み
     (メモリ使用量1.5MB)。
  2. **完了・実機検証済み**: バックエンドに`GET /api/repos`
     (リポジトリ一覧、既存`list_repos`を再利用)・
     `GET /api/repos/:name/readme`(`git show <branch>:README.md`を
     サブプロセス実行してJSON化)を追加、`cargo build`成功を確認。
     `poem`の`static-files` feature有効化、`/ui`配下で`static/`を配信する
     設定を追加。
  3. **未検証(雷雨のため中断)**: GitHub README表示機能をWASMフロント
     エンド(`web/`、新規crate`rgit-web`)として実装。ユーザー指示により
     「省メモリ・ハイスピード」を追求する方向で、以下の判断を経た:
     - 当初`serde`/`serde_json`を使う設計→WASMバイナリサイズへの影響が
       大きいとユーザー指摘を受け撤回。
     - 次に`js_sys::JSON::parse`(ブラウザ組み込み)+`Reflect`でのJSON
       パースに変更→「JSON.parseをRJSON.parseとして開発して」という
       ユーザー指示を受け、自作の最小JSONパーサ`web/src/rjson.rs`
       (`RJson`、文字列エスケープ・`\uXXXX`・サロゲートペア対応の
       再帰下降パーサ、単体テスト4件同梱)を新規実装し、
       `js_sys`/`Reflect`依存も撤去。WASM↔JS境界を跨ぐ呼び出し回数の
       削減が狙い。
     - `web/Cargo.toml`に`opt-level="z"`+LTO+`panic=abort`+`strip=true`の
       release profileを追加(バイナリ極小化)。
     - **`cargo build --target wasm32-unknown-unknown --release`は
       雷雨によるシャットダウンのため未実行**。`rjson`の単体テスト
       (ネイティブターゲットでの`cargo test`)も未実行。次回セッション
       開始時に最優先で検証すること(型チェックだけで「完了」と
       報告しない、というこのエコシステム共通のルール通り)。
  - 次にすべきこと: (1) `web/`のネイティブテスト実行(`rjson`パーサの
    正しさ検証)、(2) `wasm32-unknown-unknown`ターゲットでのビルド、
    (3) `wasm-bindgen` CLIでJSグルーコード生成し`static/`へ配置、
    (4) 実ブラウザでリポジトリ一覧・README表示が実際に動くことを確認、
    (5) VPSへの再デプロイ、(6) 外部バックアップ同期スクリプトへの
    RGit自身の組み込み(同期先の詳細はVPS上の設定のみで管理し、
    このリポジトリには記載しない方針、次項参照)。

> ⚠️ **運用ルール(2026-07-21追記)**: 外部バックアップ先(アカウント名・
> ホスト名・トークン等)は、このリポジトリを含むいかなるGitリポジトリの
> コミット・ドキュメントにも記載しない。関連設定はVPS上の環境変数・
> 認証情報ファイル(`/root/.secrets/`等)のみで管理する。

- **2026-07-21(続き) WASM実ビルド検証・[RJSON](https://github.com/aon-co-jp/RJSON)への
  JSONパーサ統合・open-easy-web方式のOTP認証を追加**:
  1. **WASM実ビルド・実機検証完了**: `cargo build --target
     wasm32-unknown-unknown --release`成功、`wasm-bindgen`でJSグルー
     生成、`.wasm`は234KB。実際に`rgit`サーバーを起動しリポジトリを
     push、`/api/repos`・`/api/repos/:name/readme`のJSON応答を確認。
  2. **`web/src/rjson.rs`(独自最小JSONパーサ)を撤去し、
     [aon-co-jp/RJSON](https://github.com/aon-co-jp/RJSON)(`rust-json`
     クレート)の`light`モジュールへ統合**(ユーザー指示「統廃合して
     融合して」)。RJSON側に`serde_json`依存ゼロの`light`モジュールを
     新設してもらい(`full` featureで既存のserde_json依存コードと分離、
     `default-features = false`で完全排除可能)、`web/Cargo.toml`で
     `rust-json = { path = "../../RJSON", default-features = false }`
     として依存。旧`web/src/rjson.rs`は削除、`lib.rs`は
     `rust_json::{parse_light, LightValue}`を使うよう書き換え。
     ビルド後の`.wasm`サイズは234KBのまま(serde_json非混入を確認済み)。
  3. **open-easy-webと同じOTP認証を追加**(ユーザー承認: フル実装、
     SMTP設定込み): `src/auth.rs`(open-easy-webの`server/src/auth.rs`
     から、RGitは単一管理者アカウントのみのため`UserStore`相当・
     連絡先変更機能を省いて移植)・`src/mail.rs`(同`mail.rs`から
     `send_otp`のみ移植、`lettre`)。`RGIT_ADMIN_EMAIL`・
     `RGIT_SMTP_{HOST,PORT,USERNAME,PASSWORD,FROM}`環境変数で設定。
     `POST /api/auth/{request-otp,verify-otp,logout}`、
     `PUT /repos/:name`(リポジトリ新規作成)に`Authorization: Bearer`
     必須化(`require_session`)。**実SMTP(既存open-easy-webと同じGmail
     アカウントを再利用)で実際にOTPメールを送受信し、
     未ログイン→401・OTP送信→200・OTP検証→トークン発行→
     トークン付き作成→201・無効トークン→401・ログアウト後の同一
     トークン→401という一連のフローを実HTTPで確認済み**(モックでは
     なく実メール到達・実コード入力による検証)。
     `cargo test`—auth関連5件green。
  - 次にすべきこと: (1) WASMフロントエンド側にログインUI(メール
    OTP入力フォーム)がまだ無い(現状はcurlでの検証のみ、サーバー側
    APIは完成)、(2) git smart HTTP(clone/push)自体への認証は未着手
    (現状はWeb UI操作のみ保護、モジュールdoc参照)、(3) VPSへの
    再デプロイ(認証・RJSON統合を反映した最新版)、(4) 保留中の
    外部バックアップ同期スクリプトへの組み込み。

- **2026-07-21(続き) アクセス制御の大幅拡張: private/public/group/
  アカウント個別許可、閲覧・ダウンロード・push個別許可、自己申請フロー、
  git push自体への認証を実装・実機検証**(ユーザー指示の積み重ね:
  「管理者が許可すればREADME/ファイルを誰でも閲覧・DL・ZIP可能に」→
  「グループ/チーム単位でも」→「登録アカウント制+push権限も」→
  「誰でも申請できて管理者がメールで気づいて許可・不許可を選べる」)。
  1. **`src/access.rs`新設**: `AccessConfig`(`mode: private/public/group`、
     `allow_view`/`allow_download`/`allow_push`、`accounts:
     HashMap<email, AccountPermission>`)。管理者ログイン済みは常に許可、
     それ以外は`mode`のルール(public=誰でも、group=共有招待トークン
     一致)またはアカウント個別許可のどちらかで判定
     (`access::is_allowed`、単体テスト9件でprivate/public/group/
     アカウント個別/push許可の組み合わせを検証)。
  2. **`src/accounts.rs`新設**: 登録メールアドレス管理
     (`.rgit-accounts.json`)+自己申請(`AccessRequest`、
     `POST /api/accounts/request`は認証不要で誰でも送れる)。
  3. **`src/auth.rs`拡張**: `Session`にメールアドレスを持たせ、
     `create_session(email)`/`session_email(token)`に変更(旧:
     管理者1名専用→どのメールでもログインできる汎用OTP機構に)。
  4. **管理者専用API**: グループ作成/一覧/削除(`/api/groups*`)、
     アカウント追加/一覧/削除(`/api/accounts`)、申請一覧・審査
     (`/api/accounts/requests*`、`decide`で閲覧/DL/push を個別に選んで
     承認・却下)。すべて`require_admin_session`(セッションのメールが
     `RGIT_ADMIN_EMAIL`と一致)でガード。
  5. **git smart HTTP自体への認証を実装**(これまでの既知の制限を解消):
     `git_get`/`git_post`が`PATH_INFO`からリポジトリ名と
     clone/pull(`git-upload-pack`→`Need::Download`)/push
     (`git-receive-pack`→`Need::Push`)を判定し、ディスパッチ前に権限
     チェックする。**実装中に発見した重要な罠**: gitクライアントは
     `403`では認証情報を送り直さず、`401`+`WWW-Authenticate`ヘッダを
     受け取って初めてBasic認証を試みる仕様——最初`403`を返す実装にして
     しまい、認証情報付きpushが延々`403`になるバグを実機検証で発見・
     修正(`git_access_error`関数、資格情報無し→`401`+
     `WWW-Authenticate: Basic realm="RGit"`、資格情報ありで権限不足
     →`403`、と使い分けた)。
  6. **git CLI向け認証方式**: `Authorization: Basic
     base64(email:セッショントークン)`をサポート(`session_identity`が
     `Bearer`と`Basic`両方を解釈)。`git remote set-url`でURLに
     `email:token@host`を埋め込む運用で、追加ツール無しに
     `git clone`/`git push`が認証付きで行える。
  7. **実機E2E検証(モックではなく実際の`git`コマンド・実SMTP)**:
     非公開リポジトリへの匿名`git push`→`401`(WWW-Authenticate付き)、
     管理者Basic認証での`git push`→成功→別クローンで内容確認、
     リポジトリを`public`(閲覧・DL許可・push不許可)に変更→匿名`git
     clone`は成功・匿名`git push`は依然`401`拒否、を確認。
     **検証中に発生した紛らわしい現象**: 一度Basic認証成功後、Windows
     Git Credential Manager(`credential.helper=manager`)が資格情報を
     キャッシュし、別ディレクトリでの「匿名のはずのclone」が
     管理者権限で成功してしまい、一瞬「権限チェックが機能していない」
     ように見えた——原因はサーバー側ではなくクライアント側のGCM
     キャッシュと特定し、`git -c credential.helper=`で無効化してから
     再検証し、正しく拒否されることを確認した(この教訓を記録:
     このエコシステムで今後同様のテストをする際、GCM等の資格情報
     キャッシュを疑うこと)。
  8. **未検証のまま保留(ユーザーが離席中はメール送信を控える指示のため)**:
     自己申請→管理者審査(`decide_access_request`)のフルE2Eは、
     管理者ログイン自体が実OTPメール送信を要するため、このパスでは
     実行しなかった。申請の保存(`POST /api/accounts/request`、認証
     不要でメールも飛ばないSMTP未設定インスタンスで検証済み)と
     `decide_access_request`のコードレビュー(承認時のみアカウント
     登録+リポジトリ`access`設定への書き込み、却下時は申請削除のみ、
     SMTP未設定なら送信をスキップ)までは確認済み。
  - 次にすべきこと: (1) `decide_access_request`の実ログイン込みE2E検証
    (次回、メール送信が許容されるタイミングで)、(2) WASM側UI
    (ログイン・アクセス許可設定・申請一覧・グループ管理の画面が
    すべて未着手)、(3) VPSへの再デプロイ、(4) 保留中の外部バックアップ
    同期スクリプトへのRGit組み込み。

- **2026-07-21(続き) 自己申請フローのフルE2E検証(実SMTP)・VPS本番
  デプロイ・容量ベースの新規リポジトリ作成自動判定を追加**:
  1. **自己申請→承認のフルE2E、実SMTP・実ログインで検証完了**
     (前回保留していた項目): 匿名で`POST /api/accounts/request`
     →管理者へ実際に通知メール到達を確認(ユーザーがメール本文を
     提示して確認)→管理者が実OTPログイン→`GET
     /api/accounts/requests`で申請確認→`POST .../decide`で
     閲覧+ダウンロード許可・push不許可を選んで承認→`GET
     /api/accounts`・`GET /api/repos/:name/access`で、アカウント登録と
     `access::AccessConfig::accounts`への権限書き込みが正確に反映
     されていることを確認。
  2. **VPS本番デプロイ**: `git pull`→`cargo build --release`→
     `systemctl restart rgit`で最新版(アクセス制御・RJSON統合)を反映、
     `healthz`で稼働確認。systemdユニットに`RGIT_ADMIN_EMAIL`・
     `RGIT_SMTP_*`を追加(VPS上のみ、Gitには含めない)し、本番でも
     ログイン機能が使える状態にした。
  3. **`src/capacity.rs`新設(ユーザー指示: 「HDDの限界に応じて新規
     リポジトリ作成を許可するか、管理者でも他人やチームに対しても
     AIが自動で考慮する機能」)**: `fs2::available_space`で実際の
     ディスク空き容量を計測し、閾値(`RGIT_MIN_FREE_DISK_MB`、既定
     1GB)を下回れば`507 Insufficient Storage`で拒否する自動判定。
     **「AI」という言葉が指すのは機械学習モデルではなく、実測値に基づく
     ルールベースの自動判定である旨をモジュールdocに明記**(誇張表示を
     避けるこのエコシステムの方針通り)。
  4. **リポジトリ作成権限をアカウント単位に拡張**:
     `accounts::AccountStore.can_create_repos`(登録アカウントのうち、
     新規リポジトリ作成が許可された集合)を追加、
     `PUT /api/accounts/:email/create-permission`(管理者のみ)で
     付与・剥奪。`create_repo`ハンドラは「管理者、または`emails`かつ
     `can_create_repos`両方に含まれるアカウント」のみ許可し、**管理者
     自身の作成要求にも`capacity::decide`を必ず適用**(要件通り、
     管理者だからといって容量判定を素通りしない)。
  5. **検証**: `cargo test` **15件全green**(新規: `capacity`モジュール
     2件、実際のボリュームで非ゼロの空き容量を計測できることと、
     存在しないパスでは安全側〈不許可〉に倒れることを確認)。実機でも
     `GET /api/capacity`が実際のディスク空き容量(検証時2.6TB)を返す
     こと、`RGIT_MIN_FREE_DISK_MB`を意図的に極端な値にすると
     `allowed:false`になることを確認済み。
  - 次にすべきこと: (1) WASM側UI(ログイン・アクセス許可・申請一覧・
    グループ管理・容量表示のいずれも未着手)、(2) 保留中の外部
    バックアップ同期スクリプトへのRGit組み込み、(3) 今回の変更を
    VPS本番へ再デプロイ(現在のVPSはアクセス制御拡張版までで、
    容量判定機能はまだ反映していない)。

- **2026-07-21(続き) WASMフロントエンドにログインUI・容量表示を追加、
  実機検証済み**: 上記(1)のログインUI着手分。
  1. **`web/src/auth.rs`新設**: `POST /api/auth/{request-otp,verify-otp,
     logout}`をfetchで叩くログインフォームロジック。メール入力→
     「OTP送信」ボタン→コード入力欄出現→「ログイン」ボタンで
     `verify-otp`→成功したら`localStorage`(キー`rgit_token`/
     `rgit_email`)へトークン保存。JSONパースは既存方針通り
     `rust_json::parse_light`(RJSON)のみ、`serde`は使わず自前で
     JSONエスケープ関数を実装(メールアドレス等をリクエストボディへ
     埋め込む際の最小限のエスケープ)。認証付きリクエストは
     `authorized_fetch`(`RequestInit`+`Headers`で`Authorization:
     Bearer <token>`を付与)に一本化。
  2. **`web/src/lib.rs`**: `load_capacity()`を追加し`GET
     /api/capacity`の結果(空き容量GB換算・作成可否)を`#capacity-status`
     に表示。`start()`で`auth::wire_auth_ui()`を呼び、ログイン成功時
     `reload_after_login()`でリポジトリ一覧・容量表示を再取得。
  3. **`static/index.html`**: `#auth-bar`(メール入力・OTP送信ボタン・
     コード入力・ログインボタン・ログイン中表示・ログアウトボタン・
     エラー表示・容量表示)を追加。
  4. **`web/Cargo.toml`**: `web-sys` featuresに`Headers`・`Storage`・
     `HtmlInputElement`・`DomTokenList`を追加(既存の
     `opt-level="z"`+LTO+`panic=abort`+`strip`構成は維持)。
  5. **実機検証(モックではなく実サーバー・実ブラウザ)**:
     `cargo build --target wasm32-unknown-unknown --release`警告0件で
     成功、`.wasm`は262KB(旧234KBから微増、認証UI分)。`wasm-bindgen
     --target web`でJSグルー再生成し`static/`へ配置。実際に`rgit`
     サーバーを起動(`RGIT_ADMIN_EMAIL`設定・SMTP未設定)し、Claude
     Browser paneで`http://127.0.0.1:8095/ui/index.html`を開いて
     ログインフォーム・容量表示(「空き容量: 2546.3GB (作成可)」)・
     リポジトリ一覧が実際にレンダリングされることを確認。
     コンソールエラー無し。メールアドレス入力→「OTP送信」を実クリック
     →SMTP未設定のため`503`が返り、UI上に「サーバーのメール設定が
     未完了です」と正しく表示されることまで確認(実SMTPでのOTP送受信
     自体は今回未実施、メール設定が無い環境での検証のみ)。
  - 次にすべきこと: (1) 実SMTP環境でのOTPログインE2E(コード入力→
    ログイン成功→ログアウトの一連)、(2) アクセス許可設定・申請一覧・
    グループ管理のWASM UIは依然未着手、(3) VPS本番への再デプロイ
    (今回の変更はローカル検証のみ、VPSは未反映)、(4) 保留中の外部
    バックアップ同期スクリプトへのRGit組み込み。
---

## エコシステム全体マップ(2026-07-21追記)

同時並行開発の対象プロジェクト一覧・各リポジトリの現況は
[`open-raid-z`のCLAUDE.md](https://github.com/aon-co-jp/open-raid-z/blob/main/CLAUDE.md)
「関連プロジェクト」節を参照。**どのリポジトリから読み始めても、
この節を起点に他プロジェクトへ辿れる**ようにしてある(このリポジトリ
自身の状況はこの上のHANDOFF節を参照)。
