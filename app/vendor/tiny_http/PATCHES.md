# tiny_http 0.12.0 への修正

元: https://crates.io/crates/tiny_http/0.12.0 (MIT OR Apache-2.0。ライセンスは `LICENSE-*`)。
上流の更新（セキュリティ修正を含む）は、自動では入りません。GitHub Actions (`test.yml`) が毎週、上流の新しい版の有無と、この写しの差分を確かめます。新しい版が出たら、差分を確認して、この修正を当て直してください。

1. `src/util/equal_reader.rs`: 読み残した本文を読み捨てるとき、`Content-Length` の大きさの配列を一度に確保していた。
   巨大な `Content-Length`（例: 1TB）を付けた要求で、メモリの確保に失敗してプロセスごと異常終了する。固定サイズ（8KB）の配列にした。
2. 読み残した本文の後始末を、要求を処理するスレッド（ワーカー）ではなく、接続ごとのスレッドに移した。
   元の実装は、ワーカーが、上限も期限もなく、終わりまで読み捨てていた。1バイトずつ送り続ける相手に、ワーカーを占有されてしまう。
   - `Content-Length` の読み残し: `EqualReader::with_state` が、読み捨てるバイト数を `ConnState` に記録する。
     次の要求を読む前に、`client.rs` の `finish_previous` が、1MB・10秒を上限に読み捨てる。超えたら接続を閉じる。
   - `Transfer-Encoding: chunked` の読み残し（`DrainOnDrop`）: 残りが次の要求として解釈されていた。読み捨てずに、接続を閉じる印（`poisoned`）を付ける。
