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

By default the implementation caps video upload at 5 GiB and honors any lower reported account limit. Unlimited storage does not imply an unlimited single-file size. The observed web bundle and account configuration report 5 GiB, but a controlled real-file precreate larger than 5 GiB was accepted through the normal upload protocol. This proves precreate acceptance only, not successful commit or verified original readback.

An unlimited account can explicitly opt into a positive byte ceiling using `upload plan --attempt-upload-limit BYTES`. The selected attempt ceiling is persisted in a new immutable plan and checked against live capabilities before execution. Missing/zero reported limits and unknown or absent unlimited entitlement do not authorize the attempt. Legacy plans retain their original policy. This option neither forges client identity nor promises acceptance; upload, commit, and complete original-stream verification must all succeed for each object. A declared 10 GiB download limit remains unproven for larger originals until their full readback succeeds.

Uploaded, metadata-matched and verified states remain distinct. Verified backup requires a full original-stream SHA-256 comparison against the immutable source plan. Download publication additionally checks size/MD5 and never overwrites an existing destination. No deletion endpoint is implemented.
