# Protocol adapter

The adapter is independently implemented against observed Baidu Photo web wire contracts. It is a private service API and may change. This project is not affiliated with Baidu.

References:

- Official service: https://photo.baidu.com/
- Public web-client reference: https://nd-static.bdstatic.com/photo-static/yike-web/js/chunk-common.ad887183.js
- MIT protocol comparison: https://github.com/HengyueLi/baiduphoto
- OpenList protocol comparison (AGPL, reference only): https://github.com/OpenListTeam/OpenList/tree/main/drivers/baidu_photo
- Driver maintenance notes: https://doc.oplist.org.cn/guide/drivers/baidu_photo

Normal API requests use the fixed `https://photo.baidu.com/youai/` origin, the actual page token, normal client parameters and the authenticated session. The adapter verifies the session owner and reads account capabilities.

Chunk upload requires BDUSS only at the exact first-party HTTPS endpoint `c3.pcs.baidu.com:443/rest/2.0/pcs/superfile2`. Redirects are refused. Original downloads and their validated redirects use a separate cookie-free client. Credentials, raw responses and signed URLs are never logged.

Parts are 4 MiB; slice MD5 covers the first 256 KiB. Precreate supplies the server's missing-part indices and may implicitly return an instant object. Both precreate and final create therefore have durable intent before submission. Ambiguous responses are reconciled by the same account, complete digest, size and original-byte proof rather than retried blindly.

The implementation caps video upload at the validated web route's 5 GiB and also honors any lower account limit. That does not establish a permanent limit for every official client. Unknown capabilities remain unknown.

Uploaded, metadata-matched and verified states remain distinct. Verified backup requires a full original-stream SHA-256 comparison against the immutable source plan. Download publication additionally checks size/MD5 and never overwrites an existing destination. No deletion endpoint is implemented.
