# tiny_http 0.12.0 への修正

元: https://crates.io/crates/tiny_http/0.12.0 (MIT OR Apache-2.0。ライセンスは `LICENSE-*`)

1. `src/util/equal_reader.rs`: 読み残した本文を読み捨てるとき、`Content-Length` の大きさの配列を一度に確保していた。
   巨大な `Content-Length`（例: 1TB）を付けた要求で、メモリの確保に失敗してプロセスごと異常終了する。固定サイズ（8KB）の配列で、繰り返し読み捨てるようにした。
2. `src/request.rs`: `Transfer-Encoding: chunked` の本文を読み残したまま要求を終えると、残りが次の要求として解釈されていた。
   読み残しは、終わりまで読み捨てるようにした（`DrainOnDrop`）。
