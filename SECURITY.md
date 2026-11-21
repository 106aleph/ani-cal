# セキュリティ

## 脆弱性の報告

公開の Issue ではなく、GitHub の「Security」タブの「Report a vulnerability」から、非公開で報告してください。再現の手順と影響の範囲があると助かります。個人用のアプリのため、対応までの期間は保証できません。

対象は、このリポジトリのコードと、`compose.yaml`・`app/Dockerfile` の設定です。しょぼいカレンダー、Cloudflare、Tailscale など外部サービスの問題は対象外です。

## 運用する方へ

インターネットに公開するときの設定は、[README](README.md) の「公開するときのセキュリティ」にあります。更新 (`git pull` と再ビルド) を定期的に行ってください。
